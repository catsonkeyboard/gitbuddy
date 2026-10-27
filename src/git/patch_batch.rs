//! Multi-file patch plans and a durable rollback journal.
use super::{Repository, libgit::raw};
use anyhow::{Context, Result, ensure};
use git2::{Delta, Diff, Oid, RepositoryState};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fs,
    io::Write,
    path::{Component, Path, PathBuf},
};

const JOURNAL: &str = "gitbuddy-patch-recovery";
const MAX_FILE: usize = 16 * 1024 * 1024;
pub(super) fn binary_limit(diff: &Diff<'_>) -> Result<()> {
    use std::io::Read;
    let mut error = None;
    let mut binary_count = 0usize;
    let mut total_inflated = 0usize;
    let mut total_result = 0usize;
    let mut binary = |_: git2::DiffDelta<'_>, binary: git2::DiffBinary<'_>| {
        binary_count += 1;
        let check = (|| -> Result<()> {
            ensure!(
                binary.contains_data(),
                "Binary patch does not contain file data"
            );
            for (side, file) in [binary.old_file(), binary.new_file()]
                .into_iter()
                .enumerate()
            {
                if matches!(file.kind(), git2::DiffBinaryKind::None) {
                    continue;
                }
                ensure!(
                    file.inflated_len() <= MAX_FILE,
                    "Binary patch exceeds the size limit"
                );
                total_inflated = total_inflated.saturating_add(file.inflated_len());
                ensure!(
                    total_inflated <= 64 * 1024 * 1024,
                    "Binary patch exceeds the total size limit"
                );
                let mut decoded = Vec::new();
                flate2::read::ZlibDecoder::new(file.data())
                    .take(MAX_FILE as u64 + 1)
                    .read_to_end(&mut decoded)?;
                ensure!(
                    decoded.len() == file.inflated_len() && decoded.len() <= MAX_FILE,
                    "Invalid or oversized binary patch data"
                );
                let result_size = if matches!(file.kind(), git2::DiffBinaryKind::Delta) {
                    let mut cursor = 0;
                    let mut read_varint = || -> Result<usize> {
                        let mut value = 0usize;
                        for shift in (0..usize::BITS).step_by(7) {
                            let byte =
                                *decoded.get(cursor).context("Invalid binary delta header")?;
                            cursor += 1;
                            ensure!(
                                (byte & 127) as usize <= (usize::MAX >> shift),
                                "Oversized binary delta"
                            );
                            value = value
                                .checked_add(
                                    ((byte & 127) as usize)
                                        .checked_shl(shift)
                                        .context("Oversized binary delta")?,
                                )
                                .context("Oversized binary delta")?;
                            if byte & 128 == 0 {
                                return Ok(value);
                            }
                        }
                        anyhow::bail!("Invalid binary delta header")
                    };
                    let source = read_varint()?;
                    let result = read_varint()?;
                    ensure!(
                        source <= MAX_FILE && result <= MAX_FILE,
                        "Binary delta result exceeds the size limit"
                    );
                    result
                } else {
                    file.inflated_len()
                };
                if side == 1 {
                    total_result = total_result.saturating_add(result_size);
                    ensure!(
                        total_result <= 64 * 1024 * 1024,
                        "Binary patch results exceed the total size limit"
                    );
                }
            }
            Ok(())
        })();
        if let Err(e) = check {
            error = Some(e);
            false
        } else {
            true
        }
    };
    let result = diff.foreach(&mut |_, _| true, Some(&mut binary), None, None);
    if let Some(error) = error {
        return Err(error);
    }
    result?;
    let expected = diff
        .deltas()
        .filter(|d| d.flags().contains(git2::DiffFlags::BINARY))
        .count();
    ensure!(
        binary_count >= expected,
        "Could not validate binary patch payload"
    );
    Ok(())
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
struct Version {
    oid: String,
    mode: u32,
    hash: [u8; 32],
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Change {
    path: String,
    before: Option<Version>,
    after: Option<Version>,
    metadata_source: Option<String>,
    temp: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Journal {
    version: u32,
    id: String,
    head_ref: String,
    head_oid: Option<String>,
    changes: Vec<Change>,
}
#[derive(Clone, Debug)]
pub(super) struct BatchPlan {
    git_dir: PathBuf,
    fingerprint: u64,
    changes: Vec<Change>,
    patch: Vec<u8>,
}
impl BatchPlan {
    pub(super) fn summary(&self) -> String {
        let files = self
            .changes
            .iter()
            .map(|c| {
                format!(
                    "{} {:?}",
                    if c.before.is_none() {
                        "Add"
                    } else if c.after.is_none() {
                        "Delete"
                    } else {
                        "Modify"
                    },
                    c.path
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        let raw = String::from_utf8_lossy(&self.patch);
        let preview = raw
            .chars()
            .take(2000)
            .map(|c| {
                if c.is_control() && !matches!(c, '\n' | '\t') {
                    '�'
                } else {
                    c
                }
            })
            .collect::<String>();
        format!(
            "Apply {} working-file changes? The index remains unchanged. A recovery journal protects interrupted writes.\n\n{}\n\nPatch preview:\n{}{}",
            self.changes.len(),
            files,
            preview,
            if raw.chars().count() > 2000 {
                "\n… preview truncated"
            } else {
                ""
            }
        )
    }
}
pub(super) fn recovery_pending(repo: &git2::Repository) -> bool {
    match fs::symlink_metadata(repo.path().join(JOURNAL)) {
        Ok(_) => true,
        Err(e) => e.kind() != std::io::ErrorKind::NotFound,
    }
}
fn safe_path(root: &Path, path: &str) -> Result<PathBuf> {
    let relative = Path::new(path);
    ensure!(
        !path.is_empty()
            && relative
                .components()
                .all(|c| matches!(c, Component::Normal(_))),
        "Invalid patch path"
    );
    ensure!(
        !relative
            .components()
            .any(|c| c.as_os_str().to_string_lossy().eq_ignore_ascii_case(".git")),
        "Patch path cannot enter .git"
    );
    let mut result = root.to_path_buf();
    for part in relative.components() {
        result.push(part);
        match fs::symlink_metadata(&result) {
            Ok(meta) => ensure!(
                !meta.file_type().is_symlink(),
                "Patch path contains a symbolic link"
            ),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(e) => return Err(e.into()),
        }
    }
    Ok(result)
}
fn version(repo: &git2::Repository, entry: Option<(Oid, u32)>) -> Result<Option<Version>> {
    entry
        .map(|(id, mode)| {
            ensure!(
                matches!(mode, 0o100644 | 0o100755),
                "Only ordinary files are supported"
            );
            let (size, kind) = repo.odb()?.read_header(id)?;
            ensure!(
                kind == git2::ObjectType::Blob && size <= MAX_FILE,
                "Patch file exceeds 16 MiB"
            );
            Ok(Version {
                oid: id.to_string(),
                mode,
                hash: Sha256::digest(repo.find_blob(id)?.content()).into(),
            })
        })
        .transpose()
}
fn tree_entry(tree: &git2::Tree<'_>, path: &Path) -> Result<Option<(Oid, u32)>> {
    match tree.get_path(path) {
        Ok(e) => Ok(Some((e.id(), e.filemode() as u32))),
        Err(e) if e.code() == git2::ErrorCode::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}
fn read_current(root: &Path, change: &Change) -> Result<Option<[u8; 32]>> {
    let path = safe_path(root, &change.path)?;
    let meta = match fs::symlink_metadata(&path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    ensure!(
        meta.is_file() && meta.len() <= MAX_FILE as u64,
        "Patch target is not an ordinary file or is too large"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let owned_temp = if meta.nlink() == 2 {
            change
                .temp
                .as_ref()
                .and_then(|p| safe_path(root, p).ok())
                .and_then(|p| fs::symlink_metadata(p).ok())
                .is_some_and(|m| m.is_file() && m.dev() == meta.dev() && m.ino() == meta.ino())
        } else {
            false
        };
        ensure!(
            meta.nlink() == 1 || owned_temp,
            "Hard-linked patch targets are not supported"
        );
    }
    Ok(Some(Sha256::digest(fs::read(path)?).into()))
}
fn hash_of(v: &Option<Version>) -> Option<[u8; 32]> {
    v.as_ref().map(|v| v.hash)
}
impl Repository {
    pub(super) fn prepare_patch_batch(&self, bytes: Vec<u8>) -> Result<BatchPlan> {
        let repo = raw(self)?;
        ensure!(
            !recovery_pending(&repo)
                && repo.state() == RepositoryState::Clean
                && !super::integration::squash_active(&repo)
                && !super::rebase::active(&repo),
            "Finish the current operation or recover the interrupted patch first"
        );
        let diff = Diff::from_buffer(&bytes)?;
        binary_limit(&diff)?;
        ensure!(
            diff.deltas().len() != 0 && diff.deltas().len() <= 128,
            "Patch must contain 1–128 files"
        );
        let tree = repo
            .head()?
            .peel_to_tree()
            .context("A HEAD commit is required")?;
        let index = repo.index()?;
        let mut paths = BTreeSet::new();
        let mut renames = std::collections::HashMap::new();
        for d in diff.deltas() {
            ensure!(
                matches!(
                    d.status(),
                    Delta::Added | Delta::Deleted | Delta::Modified | Delta::Renamed
                ),
                "Unsupported patch operation"
            );
            for path in [d.old_file().path(), d.new_file().path()]
                .into_iter()
                .flatten()
            {
                let s = path
                    .to_str()
                    .context("Patch paths must be UTF-8")?
                    .to_owned();
                safe_path(&self.root, &s)?;
                paths.insert(s);
            }
            if d.status() == Delta::Renamed {
                let old = d
                    .old_file()
                    .path()
                    .context("Missing rename source")?
                    .to_str()
                    .context("Invalid source")?;
                let new = d
                    .new_file()
                    .path()
                    .context("Missing rename destination")?
                    .to_str()
                    .context("Invalid destination")?;
                renames.insert(new.to_owned(), old.to_owned());
            }
        }
        ensure!(
            !paths.is_empty() && paths.len() <= 256,
            "Too many patch paths"
        );
        let names = paths.iter().collect::<Vec<_>>();
        for (i, a) in names.iter().enumerate() {
            for b in names.iter().skip(i + 1) {
                ensure!(
                    !Path::new(a).starts_with(b)
                        && !Path::new(b).starts_with(a)
                        && a.to_lowercase() != b.to_lowercase(),
                    "Overlapping or case-only patch paths are not supported"
                );
            }
        }
        let mut base_total = 0usize;
        for name in &paths {
            if let Some((id, mode)) = tree_entry(&tree, Path::new(name))? {
                ensure!(
                    matches!(mode, 0o100644 | 0o100755),
                    "Only ordinary files are supported"
                );
                let (size, kind) = repo.odb()?.read_header(id)?;
                ensure!(
                    kind == git2::ObjectType::Blob && size <= MAX_FILE,
                    "Patch file exceeds 16 MiB"
                );
                base_total = base_total.saturating_add(size);
                ensure!(
                    base_total <= 64 * 1024 * 1024,
                    "Patch source files exceed the total limit"
                );
            }
        }
        let result = repo
            .apply_to_tree(&tree, &diff, None)
            .context("Patch does not apply to HEAD (binary patches must contain data)")?;
        let mut changes = Vec::new();
        let mut total = 0usize;
        for path in paths {
            let p = Path::new(&path);
            let old = tree_entry(&tree, p)?;
            let staged = index.get_path(p, 0).map(|e| (e.id, e.mode));
            ensure!(
                staged == old && (1..=3).all(|stage| index.get_path(p, stage).is_none()),
                "Affected file has staged changes or conflicts: {path}"
            );
            ensure!(
                !super::lfs::tracked(&repo, p)?,
                "LFS files are not supported: {path}"
            );
            let before = version(&repo, old)?;
            let after = version(&repo, result.get_path(p, 0).map(|e| (e.id, e.mode)))?;
            if before == after {
                continue;
            }
            let change = Change {
                metadata_source: renames
                    .get(&path)
                    .cloned()
                    .or_else(|| before.as_ref().map(|_| path.clone())),
                path,
                before,
                after,
                temp: None,
            };
            if let (Some(source), Some(after)) = (&change.metadata_source, &change.after) {
                let original =
                    tree_entry(&tree, Path::new(source))?.context("Missing metadata source")?;
                ensure!(
                    original.1 == after.mode,
                    "File mode changes are not supported in a multi-file patch"
                );
            }
            ensure!(
                read_current(&self.root, &change)? == hash_of(&change.before),
                "Affected file has local changes or an untracked collision: {}",
                change.path
            );
            if change.before.is_some() {
                ensure!(
                    repo.status_file(Path::new(&change.path))? == git2::Status::CURRENT,
                    "Affected file is not clean: {}",
                    change.path
                );
            }
            for v in [&change.before, &change.after].into_iter().flatten() {
                total = total.saturating_add(repo.odb()?.read_header(Oid::from_str(&v.oid)?)?.0);
            }
            ensure!(
                total <= 64 * 1024 * 1024,
                "Patch exceeds the 64 MiB total limit"
            );
            changes.push(change);
        }
        ensure!(!changes.is_empty(), "Patch has no effective changes");
        Ok(BatchPlan {
            git_dir: repo.path().into(),
            fingerprint: self.fingerprint()?,
            changes,
            patch: bytes,
        })
    }
}

fn copy_metadata(source: &Path, destination: &fs::File) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        use std::os::fd::AsRawFd;
        let src = fs::File::open(source)?;
        ensure!(
            unsafe {
                libc::fcopyfile(
                    src.as_raw_fd(),
                    destination.as_raw_fd(),
                    std::ptr::null_mut(),
                    libc::COPYFILE_METADATA,
                )
            } == 0,
            "Cannot preserve metadata: {}",
            std::io::Error::last_os_error()
        );
    }
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::ffi::OsStrExt;
        let path = std::ffi::CString::new(source.as_os_str().as_bytes())?;
        ensure!(
            unsafe { libc::llistxattr(path.as_ptr(), std::ptr::null_mut(), 0) } == 0,
            "Cannot preserve extended attributes on this platform"
        );
        destination.set_permissions(fs::metadata(source)?.permissions())?;
    }
    Ok(())
}
fn make_parent(root: &Path, path: &str) -> Result<()> {
    let relative = Path::new(path).parent().context("Missing parent")?;
    let mut current = root.to_path_buf();
    for part in relative.components() {
        current.push(part);
        match fs::symlink_metadata(&current) {
            Ok(m) => ensure!(
                m.is_dir() && !m.file_type().is_symlink(),
                "Unsafe patch directory"
            ),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                fs::create_dir(&current)?;
                sync_dir(current.parent().context("Missing directory parent")?)?;
            }
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}
fn backup_path(dir: &Path, i: usize) -> PathBuf {
    dir.join(format!("{i}.backup"))
}
fn sync_dir(path: &Path) -> Result<()> {
    fs::File::open(path)?.sync_all()?;
    Ok(())
}
fn remove_journal(dir: &Path, journal: &Journal) -> Result<()> {
    let mut allowed = BTreeSet::from(["manifest.json".to_owned()]);
    for (i, change) in journal.changes.iter().enumerate() {
        if change.before.is_some() {
            allowed.insert(format!("{i}.backup"));
        }
    }
    for entry in fs::read_dir(dir)? {
        let name = entry?
            .file_name()
            .into_string()
            .map_err(|_| anyhow::anyhow!("Unexpected recovery file"))?;
        ensure!(
            allowed.contains(&name),
            "Unexpected file in recovery directory: {name}"
        );
    }
    for name in allowed {
        fs::remove_file(dir.join(name))?;
    }
    fs::remove_dir(dir)?;
    if let Some(parent) = dir.parent() {
        sync_dir(parent)?;
    }
    Ok(())
}
fn finish_journal(repo: &git2::Repository, journal: &Journal) -> Result<()> {
    let active = repo.path().join(JOURNAL);
    let complete = repo.path().join(format!("{}-completed", journal.id));
    ensure!(
        !complete.exists(),
        "Completed patch journal path already exists"
    );
    fs::rename(&active, &complete)?;
    sync_dir(repo.path())?;
    remove_journal(&complete, journal)
}
fn validate_journal(j: &Journal) -> Result<()> {
    ensure!(
        j.version == 1 && !j.changes.is_empty() && j.changes.len() <= 256,
        "Invalid patch recovery journal"
    );
    ensure!(
        j.id.starts_with("gitbuddy-patch-pending-")
            && j.id.len() <= 80
            && j.id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'),
        "Invalid recovery identifier"
    );
    let mut seen = BTreeSet::new();
    for (i, c) in j.changes.iter().enumerate() {
        ensure!(seen.insert(&c.path), "Duplicate recovery path");
        ensure!(
            c.before.is_some() || c.after.is_some(),
            "Empty recovery change"
        );
        if c.after.is_some() {
            let expected = Path::new(&c.path)
                .parent()
                .unwrap_or(Path::new(""))
                .join(format!(".{}-{i}.tmp", j.id));
            ensure!(
                c.temp.as_deref() == expected.to_str(),
                "Invalid recovery temporary path"
            );
        } else {
            ensure!(c.temp.is_none(), "Unexpected recovery temporary path");
        }
    }
    Ok(())
}
fn restore(repo: &git2::Repository, root: &Path) -> Result<()> {
    let dir = repo.path().join(JOURNAL);
    let metadata = fs::symlink_metadata(&dir)?;
    ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "Invalid patch recovery directory"
    );
    let manifest_path = dir.join("manifest.json");
    let meta = fs::symlink_metadata(&manifest_path)?;
    ensure!(
        meta.is_file() && !meta.file_type().is_symlink() && meta.len() <= 1024 * 1024,
        "Invalid patch recovery manifest"
    );
    let journal: Journal = serde_json::from_slice(&fs::read(manifest_path)?)?;
    validate_journal(&journal)?;
    let head = super::recovery::head_state(repo)?;
    ensure!(
        repo.state() == RepositoryState::Clean
            && head.reference == journal.head_ref
            && head.id == journal.head_oid,
        "HEAD or repository operation changed since the interruption; inspect the recovery data before restoring"
    );
    for (i, c) in journal.changes.iter().enumerate() {
        let actual = read_current(root, c)?;
        ensure!(
            actual == hash_of(&c.before) || actual == hash_of(&c.after),
            "File changed after the patch was interrupted; preserve it and inspect recovery data: {}",
            c.path
        );
        if let Some(before) = &c.before {
            let backup = backup_path(&dir, i);
            let m = fs::symlink_metadata(&backup)?;
            ensure!(
                m.is_file()
                    && !m.file_type().is_symlink()
                    && m.len() <= MAX_FILE as u64
                    && <[u8; 32]>::from(Sha256::digest(fs::read(&backup)?)) == before.hash,
                "Patch backup is missing or corrupt: {}",
                c.path
            );
        }
        if let Some(temp) = &c.temp {
            safe_path(root, temp)?;
        }
    }
    for (i, c) in journal.changes.iter().enumerate().rev() {
        let actual = read_current(root, c)?;
        if actual == hash_of(&c.before) {
            continue;
        }
        let path = safe_path(root, &c.path)?;
        match &c.before {
            None => fs::remove_file(&path)?,
            Some(_) => {
                make_parent(root, &c.path)?;
                let backup = backup_path(&dir, i);
                let mut temp =
                    tempfile::NamedTempFile::new_in(path.parent().context("Missing parent")?)?;
                copy_metadata(&backup, temp.as_file())?;
                std::io::copy(&mut fs::File::open(&backup)?, &mut temp)?;
                temp.as_file().sync_all()?;
                ensure!(
                    read_current(root, c)? == actual,
                    "File changed during recovery: {}",
                    c.path
                );
                temp.persist(&path).map_err(|e| e.error)?;
            }
        }
        sync_dir(path.parent().context("Missing parent")?)?;
    }
    for c in &journal.changes {
        if let Some(temp) = &c.temp {
            let path = safe_path(root, temp)?;
            if path.exists() {
                let meta = fs::symlink_metadata(&path)?;
                ensure!(
                    meta.is_file()
                        && !meta.file_type().is_symlink()
                        && meta.len() <= MAX_FILE as u64,
                    "Invalid patch temporary file: {}",
                    path.display()
                );
                let bytes = fs::read(&path)?;
                let expected = c.after.as_ref().context("Missing temporary file version")?;
                let blob = repo
                    .find_blob(Oid::from_str(&expected.oid)?)
                    .context("Temporary content object is unavailable")?;
                ensure!(
                    blob.content().starts_with(&bytes),
                    "Patch temporary file differs from expected content and was retained: {}",
                    path.display()
                );
                fs::remove_file(path)?;
            }
        }
    }
    finish_journal(repo, &journal)
        .context("Files restored, but completed journal cleanup failed")?;
    Ok(())
}

impl Repository {
    pub fn has_patch_recovery(&self) -> Result<bool> {
        Ok(recovery_pending(&raw(self)?))
    }
    pub(super) fn recover_patch(&self) -> Result<String> {
        let repo = raw(self)?;
        let index = repo.index()?;
        let _lock = super::conflicts::IndexLock::acquire(index.path().context("Missing index")?)?;
        restore(&repo, &self.root)?;
        Ok("Recovered files from the interrupted patch operation".into())
    }
    pub(super) fn apply_patch_batch(&self, plan: &BatchPlan) -> Result<String> {
        self.apply_patch_batch_inner(plan, &mut |_| Ok(()))
    }
    fn apply_patch_batch_inner(
        &self,
        plan: &BatchPlan,
        after_write: &mut dyn FnMut(usize) -> Result<()>,
    ) -> Result<String> {
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        anyhow::bail!("Multi-file patch application supports macOS and Linux only");
        let repo = raw(self)?;
        let index = repo.index()?;
        let _lock = super::conflicts::IndexLock::acquire(index.path().context("Missing index")?)?;
        ensure!(
            repo.path() == plan.git_dir
                && self.fingerprint()? == plan.fingerprint
                && !recovery_pending(&repo),
            "Repository changed; review the patch again"
        );
        let head = super::recovery::head_state(&repo)?;
        for c in &plan.changes {
            ensure!(
                read_current(&self.root, c)? == hash_of(&c.before),
                "File changed; review the patch again: {}",
                c.path
            );
        }
        let tempdir = tempfile::Builder::new()
            .prefix("gitbuddy-patch-pending-")
            .tempdir_in(repo.path())?;
        let label = tempdir
            .path()
            .file_name()
            .context("Missing journal name")?
            .to_string_lossy()
            .into_owned();
        let mut journal = Journal {
            version: 1,
            id: label.clone(),
            head_ref: head.reference.clone(),
            head_oid: head.id.clone(),
            changes: plan.changes.clone(),
        };
        for (i, c) in journal.changes.iter_mut().enumerate() {
            if c.before.is_some() {
                let source = safe_path(&self.root, &c.path)?;
                let mut backup = fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(backup_path(tempdir.path(), i))?;
                copy_metadata(&source, &backup)?;
                std::io::copy(&mut fs::File::open(source)?, &mut backup)?;
                backup.sync_all()?;
                ensure!(
                    <[u8; 32]>::from(Sha256::digest(fs::read(backup_path(tempdir.path(), i))?))
                        == c.before.as_ref().context("Missing original version")?.hash,
                    "File changed while creating backup: {}",
                    c.path
                );
            }
            if c.after.is_some() {
                let p = Path::new(&c.path);
                c.temp = Some(
                    p.parent()
                        .unwrap_or(Path::new(""))
                        .join(format!(".{label}-{i}.tmp"))
                        .to_str()
                        .context("Temporary path is not UTF-8")?
                        .into(),
                );
            }
        }
        let mut manifest = fs::File::create(tempdir.path().join("manifest.json"))?;
        manifest.write_all(&serde_json::to_vec(&journal)?)?;
        manifest.sync_all()?;
        sync_dir(tempdir.path())?;
        let persisted = tempdir.keep();
        if let Err(error) = fs::rename(&persisted, repo.path().join(JOURNAL)) {
            let _ = fs::remove_dir_all(&persisted);
            return Err(error.into());
        }
        sync_dir(repo.path())?;
        let work = (|| -> Result<()> {
            for c in &journal.changes {
                let Some(after) = &c.after else {
                    continue;
                };
                make_parent(&self.root, &c.path)?;
                let temp = safe_path(&self.root, c.temp.as_deref().context("Missing temp path")?)?;
                let mut file = fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(temp)?;
                if let Some(source) = &c.metadata_source {
                    copy_metadata(&safe_path(&self.root, source)?, &file)?;
                }
                #[cfg(unix)]
                if c.metadata_source.is_none() {
                    use std::os::unix::fs::PermissionsExt;
                    let mode = if after.mode == 0o100755 { 0o755 } else { 0o644 };
                    file.set_permissions(fs::Permissions::from_mode(mode))?;
                }
                file.write_all(repo.find_blob(Oid::from_str(&after.oid)?)?.content())?;
                file.sync_all()?;
            }
            ensure!(
                super::recovery::head_state(&repo)? == head
                    && repo.state() == RepositoryState::Clean,
                "Repository operation changed during patch preparation"
            );
            for c in &journal.changes {
                ensure!(
                    read_current(&self.root, c)? == hash_of(&c.before),
                    "File changed during patch preparation: {}",
                    c.path
                );
            }
            for (i, c) in journal.changes.iter().enumerate() {
                ensure!(
                    read_current(&self.root, c)? == hash_of(&c.before),
                    "File changed while applying patch: {}",
                    c.path
                );
                let target = safe_path(&self.root, &c.path)?;
                if c.after.is_none() {
                    fs::remove_file(target)?;
                } else {
                    let temp =
                        safe_path(&self.root, c.temp.as_deref().context("Missing temp path")?)?;
                    if c.before.is_none() {
                        fs::hard_link(&temp, &target)?;
                        fs::remove_file(temp)?;
                    } else {
                        fs::rename(temp, target)?;
                    }
                }
                sync_dir(
                    safe_path(&self.root, &c.path)?
                        .parent()
                        .context("Missing parent")?,
                )?;
                after_write(i)?;
            }
            Ok(())
        })();
        if let Err(error) = work {
            return match restore(&repo, &self.root) {
                Ok(()) => Err(error.context("Patch failed; affected files were restored")),
                Err(recovery) => Err(error.context(format!(
                    "Patch failed; recovery data retained at {}: {recovery:#}",
                    repo.path().join(JOURNAL).display()
                ))),
            };
        }
        match finish_journal(&repo, &journal) {
            Ok(()) => Ok(format!(
                "Applied patch to {} files; changes are unstaged",
                journal.changes.len()
            )),
            Err(error) => Ok(format!(
                "Applied patch to {} files; changes are unstaged. Journal cleanup needs attention: {error:#}",
                journal.changes.len()
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::Operation;
    use std::process::Command;

    #[test]
    fn interrupted_batch_recovery_refuses_external_edits_and_restores_originals() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        let raw = raw(&repo).unwrap();
        let mut config = raw.config().unwrap();
        config.set_bool("commit.gpgsign", false).unwrap();
        config.set_str("core.hooksPath", "/dev/null").unwrap();
        repo.execute(Operation::SetIdentity(
            "Test".into(),
            "test@example.invalid".into(),
        ))
        .unwrap();
        for name in ["a.txt", "b.txt"] {
            fs::write(repo.root.join(name), "before\n").unwrap();
        }
        repo.execute(Operation::StageAll).unwrap();
        repo.execute(Operation::Commit("base".into())).unwrap();
        for name in ["a.txt", "b.txt"] {
            fs::write(repo.root.join(name), "after\n").unwrap();
        }
        let patch = Command::new("git")
            .arg("-C")
            .arg(&repo.root)
            .args(["diff", "--binary"])
            .output()
            .unwrap();
        assert!(patch.status.success());
        for name in ["a.txt", "b.txt"] {
            fs::write(repo.root.join(name), "before\n").unwrap();
        }
        let plan = repo.prepare_patch_batch(patch.stdout).unwrap();
        let mut fail_after_first = |index| -> Result<()> {
            if index == 0 {
                anyhow::bail!("injected write failure");
            }
            Ok(())
        };
        assert!(
            repo.apply_patch_batch_inner(&plan, &mut fail_after_first)
                .is_err()
        );
        for name in ["a.txt", "b.txt"] {
            assert_eq!(
                fs::read_to_string(repo.root.join(name)).unwrap(),
                "before\n"
            );
        }
        assert!(!repo.has_patch_recovery().unwrap());
        let journal_dir = raw.path().join(JOURNAL);
        fs::create_dir(&journal_dir).unwrap();
        let id = "gitbuddy-patch-pending-test123".to_owned();
        let head = crate::git::recovery::head_state(&raw).unwrap();
        let mut journal = Journal {
            version: 1,
            id: id.clone(),
            head_ref: head.reference,
            head_oid: head.id,
            changes: plan.changes.clone(),
        };
        for (i, c) in journal.changes.iter_mut().enumerate() {
            if c.before.is_some() {
                fs::copy(repo.root.join(&c.path), backup_path(&journal_dir, i)).unwrap();
            }
            if c.after.is_some() {
                c.temp = Some(
                    Path::new(&c.path)
                        .parent()
                        .unwrap()
                        .join(format!(".{id}-{i}.tmp"))
                        .to_str()
                        .unwrap()
                        .into(),
                );
            }
        }
        fs::write(
            journal_dir.join("manifest.json"),
            serde_json::to_vec(&journal).unwrap(),
        )
        .unwrap();
        fs::write(repo.root.join("a.txt"), "after\n").unwrap();
        fs::write(repo.root.join("b.txt"), "external\n").unwrap();
        assert!(repo.execute(Operation::StageAll).is_err());
        assert!(repo.execute(Operation::RecoverPatch).is_err());
        assert_eq!(
            fs::read_to_string(repo.root.join("a.txt")).unwrap(),
            "after\n"
        );
        assert_eq!(
            fs::read_to_string(repo.root.join("b.txt")).unwrap(),
            "external\n"
        );
        fs::write(repo.root.join("b.txt"), "before\n").unwrap();
        repo.execute(Operation::RecoverPatch).unwrap();
        assert_eq!(
            fs::read_to_string(repo.root.join("a.txt")).unwrap(),
            "before\n"
        );
        assert_eq!(
            fs::read_to_string(repo.root.join("b.txt")).unwrap(),
            "before\n"
        );
        assert!(!repo.has_patch_recovery().unwrap());
    }
}
