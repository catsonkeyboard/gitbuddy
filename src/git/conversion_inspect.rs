//! Read-only directory reuse. These views never become write receipts.
use super::{ATTRS, CleanPolicy, Config, FileStamp, Limits, Snapshot, WorkingFile};
use crate::git::lfs::{
    self, AttributeDirectoryView, AttributeLookup, AttributeQuery, AttributeState,
};
use anyhow::{Context, Result, ensure};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

pub(in crate::git) struct NativeConfigGuard<'a> {
    repo: &'a git2::Repository,
    original: git2::Config,
}
impl Drop for NativeConfigGuard<'_> {
    fn drop(&mut self) {
        let _ = self.repo.set_config(&self.original);
    }
}
pub(in crate::git) struct Inspector {
    config: Config,
    directories: BTreeMap<PathBuf, AttributeDirectoryView>,
    lfs: Option<[u8; 32]>,
}
pub(in crate::git) struct ReadConversion {
    snapshot: Snapshot,
    native: bool,
}
impl Inspector {
    pub(in crate::git) fn capture(
        repo: &git2::Repository,
    ) -> Result<(Self, NativeConfigGuard<'_>)> {
        let fresh = git2::Repository::open(repo.path())?;
        let mut config = fresh.config()?;
        let frozen = config.snapshot()?;
        let config = Config::read(&frozen)?;
        let original = repo.config()?;
        repo.set_config(&frozen)?;
        Ok((
            Self {
                config,
                directories: BTreeMap::new(),
                lfs: None,
            },
            NativeConfigGuard { repo, original },
        ))
    }
    pub(in crate::git) fn for_path(
        &mut self,
        repo: &git2::Repository,
        path: &Path,
    ) -> Result<ReadConversion> {
        let parent = path
            .parent()
            .context("Working content has no parent")?
            .to_path_buf();
        if !self.directories.contains_key(&parent) {
            let view = AttributeDirectoryView::capture(
                repo,
                path,
                AttributeLookup::WorktreeThenIndex,
                ATTRS,
            )?;
            self.directories.insert(parent.clone(), view);
        }
        let values = self
            .directories
            .get(&parent)
            .context("Missing attribute directory view")?
            .values(path)?;
        let query = AttributeQuery {
            values,
            fingerprint: [0; 32],
        };
        let codec = super::codec_for(path, &query, &self.config)?;
        let has_lfs = matches!(query.values.get("filter"),Some(AttributeState::Value(value)) if value==b"lfs");
        let lfs = if has_lfs {
            let current = lfs::conversion_fingerprint(repo)?;
            if let Some(expected) = self.lfs {
                ensure!(
                    current == expected,
                    "LFS configuration changed during inspection"
                );
            } else {
                self.lfs = Some(current);
            }
            Some(current)
        } else {
            None
        };
        let snapshot = Snapshot {
            config: self.config.clone(),
            paths: BTreeMap::from([(path.to_path_buf(), query)]),
            lookup: AttributeLookup::WorktreeThenIndex,
            lfs,
            encodings: codec
                .map(|codec| BTreeMap::from([(path.to_path_buf(), codec)]))
                .unwrap_or_default(),
        };
        let native = snapshot.native_equivalent(repo, path)?;
        Ok(ReadConversion { snapshot, native })
    }
    pub(in crate::git) fn validate(&self, repo: &git2::Repository) -> Result<()> {
        ensure!(
            self.config == Config::capture(repo)?,
            "Conversion configuration changed during inspection"
        );
        if let Some(expected) = self.lfs {
            ensure!(
                lfs::conversion_fingerprint(repo)? == expected,
                "LFS configuration changed during inspection"
            );
        }
        for view in self.directories.values() {
            view.verify(repo)?;
        }
        Ok(())
    }
}
impl ReadConversion {
    pub(in crate::git) fn native(&self) -> bool {
        self.native
    }
    pub(in crate::git) fn has_encoding(&self, path: &Path) -> bool {
        self.snapshot.has_encoding(path)
    }
    pub(in crate::git) fn has_lfs(&self, path: &Path) -> bool {
        self.snapshot.has_lfs(path)
    }
    pub(in crate::git) fn mode(&self, actual: u32, indexed: Option<u32>) -> u32 {
        self.snapshot.canonical_mode(actual, indexed)
    }
    pub(in crate::git) fn canonical_file(
        &self,
        repo: &git2::Repository,
        path: &Path,
        indexed: &[u8],
        indexed_mode: Option<u32>,
        limits: Limits,
    ) -> Result<(Vec<u8>, u32)> {
        if self.snapshot.plain_lfs(path)? {
            let metadata = fs::symlink_metadata(repo.workdir().context("No worktree")?.join(path))?;
            let stamp = FileStamp::new(&metadata);
            let bytes = lfs::clean_file_content_at(repo, path)?;
            stamp.validate(repo, path)?;
            #[cfg(unix)]
            let actual = {
                use std::os::unix::fs::PermissionsExt;
                if metadata.permissions().mode() & 0o111 != 0 {
                    0o100755
                } else {
                    0o100644
                }
            };
            #[cfg(not(unix))]
            let actual = 0o100644;
            return Ok((bytes, self.mode(actual, indexed_mode)));
        }
        let working = WorkingFile::read(repo, path, limits.max_bytes)?;
        let bytes = self.snapshot.clean(
            repo,
            path,
            &working.bytes,
            indexed,
            CleanPolicy::PatchApply,
            limits,
        )?;
        working.validate(repo, path)?;
        Ok((bytes, self.mode(working.actual_mode, indexed_mode)))
    }
}
