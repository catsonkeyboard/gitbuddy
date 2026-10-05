//! Explicit history operations with pinned targets and stale-preview checks.
use super::libgit::{oid, raw};
use super::recovery::{editable, head_state, rewrite_head};
use super::{HeadState, Repository};
use anyhow::{Context, Result, ensure};
use git2::build::CheckoutBuilder;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResetMode {
    Soft,
    Mixed,
    Hard,
}
impl ResetMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Soft => "Soft",
            Self::Mixed => "Mixed",
            Self::Hard => "Hard",
        }
    }
    pub fn impact(self) -> &'static str {
        match self {
            Self::Soft => "Move HEAD; keep staged content and working files.",
            Self::Mixed => {
                "Move HEAD and reset the index; keep working files. Staged changes become unstaged."
            }
            Self::Hard => {
                "Move HEAD and reset the index and tracked files. Uncommitted tracked changes are discarded; untracked files that obstruct checkout may be overwritten."
            }
        }
    }
}
#[derive(Clone, Debug)]
pub struct ResetContext {
    pub head: HeadState,
    pub target: String,
    fingerprint: u64,
}
impl Repository {
    pub fn prepare_reset(&self, target: &str) -> Result<ResetContext> {
        let repo = raw(self)?;
        editable(&repo)?;
        let head = head_state(&repo)?;
        ensure!(head.id.is_some(), "There is no HEAD to reset");
        let target = repo.find_commit(oid(target)?)?.id().to_string();
        Ok(ResetContext {
            head,
            target,
            fingerprint: self.fingerprint()?,
        })
    }
    pub(super) fn reset_history(&self, context: &ResetContext, mode: ResetMode) -> Result<String> {
        let repo = raw(self)?;
        rewrite_head(
            &repo,
            &context.head,
            &format!("reset: GitBuddy {}", mode.label()),
            || {
                ensure!(
                    self.fingerprint()? == context.fingerprint,
                    "Repository changed; reopen Reset and review it again"
                );
                let target = repo.find_commit(oid(&context.target)?)?;
                let original = oid(context.head.id.as_deref().context("No HEAD to reset")?)?;
                let backup = format!(
                    "refs/gitbuddy/rewrites/reset-{}-{}",
                    &original.to_string()[..8],
                    chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
                );
                repo.reference(&backup, original, false, "gitbuddy: before reset")?;
                repo.reference("ORIG_HEAD", original, true, "gitbuddy: before reset")?;
                match mode {
                    ResetMode::Soft => {}
                    ResetMode::Mixed => {
                        let mut index = repo.index()?;
                        index.read_tree(&target.tree()?)?;
                        index.write()?;
                    }
                    ResetMode::Hard => {
                        super::lfs::dehydrate(&repo)?;
                        let mut checkout = CheckoutBuilder::new();
                        checkout.force();
                        repo.checkout_tree(target.as_object(), Some(&mut checkout))?;
                    }
                }
                Ok(Some(target.id()))
            },
        )?;
        Ok(format!(
            "{} reset to {}; original commit kept in reflog and a backup reference",
            mode.label(),
            &context.target[..8]
        ))
    }
    pub(super) fn create_branch_at(&self, name: &str, target: &str) -> Result<String> {
        let repo = raw(self)?;
        ensure!(
            git2::Branch::name_is_valid(name)? && !name.starts_with('-'),
            "Invalid branch name"
        );
        let commit = repo.find_commit(oid(target)?)?;
        repo.branch(name, &commit, false)?;
        Ok(format!(
            "Created branch {name} at {}; current checkout preserved",
            &target[..8]
        ))
    }
}
