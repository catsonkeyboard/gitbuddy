//! Checked import of a single-file patch as unstaged working tree changes.
use super::{Repository, libgit::raw};
use anyhow::{Context, Result, ensure};
use git2::{ApplyLocation, ApplyOptions, Delta, Diff, FileMode, RepositoryState};
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug)]
pub struct ApplyPatchPlan {
    batch: Option<super::patch_batch::BatchPlan>,
    git_dir: PathBuf,
    path: PathBuf,
    patch: Vec<u8>,
    digest: [u8; 32],
    fingerprint: u64,
}
impl ApplyPatchPlan {
    pub fn summary(&self) -> String {
        if let Some(batch) = &self.batch {
            return batch.summary();
        }
        let text = String::from_utf8_lossy(&self.patch);
        let preview = text
            .chars()
            .take(3000)
            .map(|c| {
                if c.is_control() && c != '\n' && c != '\t' {
                    '�'
                } else {
                    c
                }
            })
            .collect::<String>();
        format!(
            "Apply patch to {} as unstaged working-file changes? The index stays unchanged.\n\nPatch preview:\n{}{}",
            self.path.display(),
            preview,
            if text.chars().count() > 3000 {
                "\n… preview truncated"
            } else {
                ""
            }
        )
    }
}

fn target(repo: &git2::Repository, root: &Path, diff: &Diff<'_>) -> Result<PathBuf> {
    ensure!(
        diff.deltas().len() == 1,
        "Only one-file patches are supported"
    );
    let delta = diff.deltas().next().context("Patch has no changes")?;
    ensure!(
        delta.status() == Delta::Modified
            && delta.old_file().mode() == FileMode::Blob
            && delta.new_file().mode() == FileMode::Blob,
        "Only modifications to regular non-executable text files are supported"
    );
    let path = delta.new_file().path().context("Missing patch path")?;
    ensure!(
        delta.old_file().path() == Some(path) && !path.as_os_str().is_empty(),
        "Renames are not supported"
    );
    let mut absolute = root.to_path_buf();
    for part in path.components() {
        let std::path::Component::Normal(name) = part else {
            anyhow::bail!("Invalid patch path")
        };
        absolute.push(name);
        let metadata = fs::symlink_metadata(&absolute)?;
        ensure!(
            !metadata.file_type().is_symlink(),
            "Patch paths cannot include symbolic links"
        );
    }
    let meta = fs::symlink_metadata(&absolute)?;
    ensure!(
        meta.is_file() && meta.len() <= 16 * 1024 * 1024,
        "Patch target must be a regular file up to 16 MiB"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        ensure!(meta.nlink() == 1, "Patch target must not be hard-linked");
    }
    ensure!(
        repo.index()?.get_path(path, 0).is_some(),
        "Patch target must be tracked"
    );
    ensure!(
        repo.status_file(path)? == git2::Status::CURRENT,
        "Patch target has staged or unstaged changes; commit or stash them first"
    );
    ensure!(
        !super::lfs::tracked(repo, path)?,
        "LFS files are not supported by patch import"
    );
    Ok(path.to_owned())
}
impl Repository {
    /// Export the complete staged or unstaged diff, including Git binary payloads.
    pub fn export_patch(&self, staged: bool) -> Result<Vec<u8>> {
        let repo = raw(self)?;
        ensure!(
            !super::patch_batch::recovery_pending(&repo),
            "Recover the interrupted patch operation first"
        );
        let fingerprint = self.fingerprint()?;
        let mut options = git2::DiffOptions::new();
        options
            .include_typechange(true)
            .include_untracked(true)
            .recurse_untracked_dirs(true)
            .show_untracked_content(true)
            .show_binary(true);
        let index = repo.index()?;
        let mut diff = if staged {
            repo.diff_tree_to_index(
                super::libgit::head_tree(&repo)?.as_ref(),
                Some(&index),
                Some(&mut options),
            )?
        } else {
            repo.diff_index_to_workdir(Some(&index), Some(&mut options))?
        };
        diff.find_similar(Some(
            git2::DiffFindOptions::new()
                .renames(true)
                .for_untracked(true),
        ))?;
        let mut output = Vec::new();
        for i in 0..diff.deltas().len() {
            let mut patch = git2::Patch::from_diff(&diff, i)?
                .context("Cannot export a complete patch for this change")?;
            let buf = patch.to_buf()?;
            ensure!(
                output.len().saturating_add(buf.len()) <= 2 * 1024 * 1024,
                "Patch exceeds the 2 MiB export limit"
            );
            output.extend_from_slice(buf.as_ref());
        }
        ensure!(!output.is_empty(), "No changes to export");
        ensure!(
            self.fingerprint()? == fingerprint,
            "Repository changed while exporting; try again"
        );
        Ok(output)
    }
    pub fn prepare_patch_file(&self, bytes: Vec<u8>) -> Result<ApplyPatchPlan> {
        ensure!(
            !bytes.is_empty() && bytes.len() <= 2 * 1024 * 1024 && !bytes.contains(&0),
            "Patch must be nonempty text up to 2 MiB"
        );
        let repo = raw(self)?;
        ensure!(
            !super::patch_batch::recovery_pending(&repo),
            "Recover the interrupted patch operation first"
        );
        ensure!(
            repo.state() == RepositoryState::Clean
                && !super::integration::squash_active(&repo)
                && !super::rebase::active(&repo),
            "Finish the current repository operation first"
        );
        let diff = Diff::from_buffer(&bytes).context("Invalid patch")?;
        super::patch_batch::binary_limit(&diff)?;
        if diff.deltas().len() != 1
            || diff.deltas().next().is_some_and(|d| {
                d.status() != Delta::Modified
                    || d.old_file().mode() != FileMode::Blob
                    || d.new_file().mode() != FileMode::Blob
            })
        {
            let batch = self.prepare_patch_batch(bytes.clone())?;
            return Ok(ApplyPatchPlan {
                batch: Some(batch),
                git_dir: repo.path().into(),
                path: PathBuf::new(),
                patch: bytes,
                digest: [0; 32],
                fingerprint: 0,
            });
        }
        let path = target(&repo, &self.root, &diff)?;
        let data = fs::read(self.root.join(&path))?;
        let entry = repo
            .index()?
            .get_path(&path, 0)
            .context("Missing index entry")?;
        ensure!(
            repo.find_blob(entry.id)?.content() == data,
            "Target uses content filters or differs from the index"
        );
        let tree = repo
            .head()?
            .peel_to_tree()
            .context("Patch import requires a HEAD commit")?;
        ensure!(
            tree.get_path(&path)?.id() == entry.id,
            "Patch target has staged changes"
        );
        let result = repo
            .apply_to_tree(&tree, &diff, None)
            .context("Patch does not apply to HEAD")?;
        let changed = result
            .get_path(&path, 0)
            .context("Patch removed the target")?;
        ensure!(
            changed.id != entry.id,
            "Patch has no effective content (binary patches must include file data)"
        );
        repo.apply(
            &diff,
            ApplyLocation::WorkDir,
            Some(ApplyOptions::new().check(true)),
        )
        .context("Patch does not apply cleanly")?;
        Ok(ApplyPatchPlan {
            batch: None,
            git_dir: repo.path().into(),
            path,
            patch: bytes,
            digest: Sha256::digest(&data).into(),
            fingerprint: self.fingerprint()?,
        })
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    pub(super) fn apply_patch_file(&self, _plan: &ApplyPatchPlan) -> Result<String> {
        anyhow::bail!("Patch import currently supports macOS and Linux only")
    }
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    pub(super) fn apply_patch_file(&self, plan: &ApplyPatchPlan) -> Result<String> {
        if let Some(batch) = &plan.batch {
            return self.apply_patch_batch(batch);
        }
        use std::io::Write;
        let repo = raw(self)?;
        let index = repo.index()?;
        let _index_lock =
            super::conflicts::IndexLock::acquire(index.path().context("Missing index")?)?;
        ensure!(
            repo.path() == plan.git_dir && self.fingerprint()? == plan.fingerprint,
            "Repository changed; review the patch again"
        );
        ensure!(
            repo.state() == RepositoryState::Clean,
            "Repository operation changed"
        );
        let head = super::recovery::head_state(&repo)?;
        let diff = Diff::from_buffer(&plan.patch)?;
        ensure!(
            target(&repo, &self.root, &diff)? == plan.path,
            "Patch target changed"
        );
        let data = fs::read(self.root.join(&plan.path))?;
        ensure!(
            <[u8; 32]>::from(Sha256::digest(&data)) == plan.digest,
            "Target file changed; review the patch again"
        );
        repo.apply(
            &diff,
            ApplyLocation::WorkDir,
            Some(ApplyOptions::new().check(true)),
        )
        .context("Patch no longer applies")?;
        let tree = repo.head()?.peel_to_tree()?;
        let result_index = repo
            .apply_to_tree(&tree, &diff, None)
            .context("Patch no longer applies to HEAD")?;
        let entry = result_index
            .get_path(&plan.path, 0)
            .context("Patch removed the target")?;
        let (size, kind) = repo.odb()?.read_header(entry.id)?;
        ensure!(
            kind == git2::ObjectType::Blob && size <= 16 * 1024 * 1024,
            "Patched file exceeds the size limit"
        );
        let blob = repo.find_blob(entry.id)?;
        let path = self.root.join(&plan.path);
        let metadata = fs::symlink_metadata(&path)?;
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::ffi::OsStrExt;
            let cpath = std::ffi::CString::new(path.as_os_str().as_bytes())?;
            let attrs = unsafe { libc::llistxattr(cpath.as_ptr(), std::ptr::null_mut(), 0) };
            ensure!(
                attrs == 0,
                "Cannot preserve extended attributes on this platform"
            );
        }
        let mut temp =
            tempfile::NamedTempFile::new_in(path.parent().context("Missing parent directory")?)?;
        #[cfg(target_os = "macos")]
        {
            use std::os::fd::AsRawFd;
            let source = fs::File::open(&path)?;
            let copied = unsafe {
                libc::fcopyfile(
                    source.as_raw_fd(),
                    temp.as_file().as_raw_fd(),
                    std::ptr::null_mut(),
                    libc::COPYFILE_METADATA,
                )
            };
            ensure!(
                copied == 0,
                "Cannot preserve file metadata: {}",
                std::io::Error::last_os_error()
            );
        }
        temp.write_all(blob.content())?;
        temp.as_file().set_permissions(metadata.permissions())?;
        temp.as_file().sync_all()?;
        ensure!(
            target(&repo, &self.root, &diff)? == plan.path
                && fs::read(&path)? == data
                && super::recovery::head_state(&repo)? == head
                && repo.state() == RepositoryState::Clean,
            "Target changed; patch application cancelled"
        );
        temp.persist(&path).map_err(|e| e.error)?;
        Ok(format!(
            "Applied patch to {}; changes are unstaged",
            plan.path.display()
        ))
    }
}
