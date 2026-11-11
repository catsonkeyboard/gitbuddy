//! No-follow working-file reads and reusable stale-input receipts.
use anyhow::{Context, Result, ensure};
use std::{
    fs,
    io::Read,
    path::{Component, Path},
    time::SystemTime,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::git) struct FileStamp {
    length: u64,
    modified: Option<SystemTime>,
    readonly: bool,
    #[cfg(unix)]
    dev: u64,
    #[cfg(unix)]
    ino: u64,
    #[cfg(unix)]
    mode: u32,
    #[cfg(unix)]
    changed: (i64, i64),
}
impl FileStamp {
    pub(in crate::git) fn new(metadata: &fs::Metadata) -> Self {
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;
        Self {
            length: metadata.len(),
            modified: metadata.modified().ok(),
            readonly: metadata.permissions().readonly(),
            #[cfg(unix)]
            dev: metadata.dev(),
            #[cfg(unix)]
            ino: metadata.ino(),
            #[cfg(unix)]
            mode: metadata.mode(),
            #[cfg(unix)]
            changed: (metadata.ctime(), metadata.ctime_nsec()),
        }
    }
    pub(in crate::git) fn validate(&self, repo: &git2::Repository, path: &Path) -> Result<()> {
        let absolute = safe_regular_path(repo, path)?;
        ensure!(
            *self == Self::new(&fs::symlink_metadata(absolute)?),
            "Working file changed while converting: {path:?}"
        );
        Ok(())
    }
}

pub(in crate::git) struct WorkingFile {
    pub bytes: Vec<u8>,
    pub stamp: FileStamp,
    pub actual_mode: u32,
}
impl WorkingFile {
    pub(in crate::git) fn read(repo: &git2::Repository, path: &Path, limit: usize) -> Result<Self> {
        let absolute = safe_regular_path(repo, path)?;
        let path_metadata = fs::symlink_metadata(&absolute)?;
        let mut options = fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        }
        let mut file = options.open(&absolute)?;
        let before = file.metadata()?;
        ensure!(
            before.is_file() && before.len() <= limit as u64,
            "Managed content exceeds its conversion resource limit: {path:?}"
        );
        let stamp = FileStamp::new(&before);
        ensure!(
            stamp == FileStamp::new(&path_metadata),
            "Working file changed while opening: {path:?}"
        );
        let mut bytes = Vec::new();
        file.by_ref()
            .take(
                (limit as u64)
                    .checked_add(1)
                    .context("Invalid conversion resource limit")?,
            )
            .read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() <= limit && stamp == FileStamp::new(&file.metadata()?),
            "Working file changed or exceeded its conversion limit: {path:?}"
        );
        stamp.validate(repo, path)?;
        #[cfg(unix)]
        let actual_mode = {
            use std::os::unix::fs::PermissionsExt;
            if before.permissions().mode() & 0o111 != 0 {
                0o100755
            } else {
                0o100644
            }
        };
        #[cfg(not(unix))]
        let actual_mode = 0o100644;
        Ok(Self {
            bytes,
            stamp,
            actual_mode,
        })
    }
    pub(in crate::git) fn validate(&self, repo: &git2::Repository, path: &Path) -> Result<()> {
        self.stamp.validate(repo, path)?;
        let current = Self::read(repo, path, self.bytes.len())?;
        ensure!(
            current.bytes == self.bytes && current.stamp == self.stamp,
            "Working content changed while converting: {path:?}"
        );
        Ok(())
    }
}

fn safe_regular_path(repo: &git2::Repository, path: &Path) -> Result<std::path::PathBuf> {
    ensure!(
        !path.as_os_str().is_empty()
            && path
                .components()
                .all(|part| matches!(part, Component::Normal(_))),
        "Invalid working content path"
    );
    let mut absolute = repo
        .workdir()
        .context("Working content needs a worktree")?
        .to_path_buf();
    for component in path.components() {
        ensure!(
            !component
                .as_os_str()
                .to_string_lossy()
                .eq_ignore_ascii_case(".git"),
            "Working content cannot enter .git"
        );
        absolute.push(component);
        let metadata = fs::symlink_metadata(&absolute)?;
        ensure!(
            !metadata.file_type().is_symlink(),
            "Content conversion does not follow symbolic links: {path:?}"
        );
    }
    ensure!(
        fs::symlink_metadata(&absolute)?.is_file(),
        "Content conversion requires a regular file: {path:?}"
    );
    Ok(absolute)
}
