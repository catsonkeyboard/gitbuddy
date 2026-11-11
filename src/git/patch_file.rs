//! Checked canonical Git patch import as durable unstaged working-tree changes.
use super::{Repository, libgit::raw};
use anyhow::{Context, Result, ensure};

#[derive(Clone, Debug)]
pub struct ApplyPatchPlan {
    batch: super::patch_batch::BatchPlan,
}
impl ApplyPatchPlan {
    pub fn summary(&self) -> String {
        self.batch.summary()
    }
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
            super::conversion::working_diff(&repo, &mut options, &[], Some(2 * 1024 * 1024))?
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
        Ok(ApplyPatchPlan {
            batch: self.prepare_patch_batch(bytes)?,
        })
    }
    pub(super) fn apply_patch_file(&self, plan: &ApplyPatchPlan) -> Result<String> {
        self.apply_patch_batch(&plan.batch)
    }
}
