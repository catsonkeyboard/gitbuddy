//! Patch import keeps its fixed budget while using the shared conversion engine.
use crate::git::{
    conversion::{CleanPolicy, Limits, Snapshot as Shared},
    lfs::AttributeLookup,
};
use anyhow::Result;
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Snapshot(Shared);
impl Snapshot {
    pub(super) fn capture(repo: &git2::Repository, paths: &BTreeSet<String>) -> Result<Self> {
        let paths = paths.iter().map(PathBuf::from).collect();
        Ok(Self(Shared::capture(
            repo,
            &paths,
            AttributeLookup::WorktreeThenIndex,
        )?))
    }
    pub(super) fn validate(&self, repo: &git2::Repository) -> Result<()> {
        self.0.validate(repo)
    }
    pub(super) fn validate_path(&self, repo: &git2::Repository, path: &str) -> Result<()> {
        self.0.validate_path(repo, Path::new(path))
    }
    pub(super) fn clean(
        &self,
        repo: &git2::Repository,
        path: &str,
        bytes: &[u8],
        indexed: &[u8],
    ) -> Result<Vec<u8>> {
        self.0.clean(
            repo,
            Path::new(path),
            bytes,
            indexed,
            CleanPolicy::PatchApply,
            Limits::PATCH,
        )
    }
    pub(super) fn smudge(
        &self,
        repo: &git2::Repository,
        path: &str,
        bytes: &[u8],
        oid: git2::Oid,
    ) -> Result<Vec<u8>> {
        self.0
            .smudge(repo, Path::new(path), bytes, oid, Limits::PATCH)
    }
}
