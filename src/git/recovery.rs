//! History edits only move references; index and working tree stay untouched.
use super::Repository;
use super::libgit::{author, oid, raw};
use anyhow::{Context, Result, ensure};
use chrono::{Local, TimeZone};
use git2::{ErrorCode, Oid, Repository as RawRepo, RepositoryState};
use std::path::PathBuf;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HeadState {
    pub id: Option<String>,
    pub reference: String,
    git_dir: PathBuf,
}

#[derive(Clone, Debug)]
pub struct CommitEdit {
    pub head: HeadState,
    pub message: String,
    pub author: String,
    pub parents: Vec<String>,
    index_tree: Oid,
}

#[derive(Clone, Debug)]
pub struct ReflogTarget {
    pub id: String,
    pub subject: String,
    pub available: bool,
    git_dir: PathBuf,
}

#[derive(Clone, Debug)]
pub struct ReflogEntry {
    pub selector: String,
    pub message: String,
    pub committer: String,
    pub date: String,
    pub before: Option<ReflogTarget>,
    pub after: Option<ReflogTarget>,
}

#[derive(Clone, Debug)]
pub struct ReflogPage {
    pub head: HeadState,
    pub entries: Vec<ReflogEntry>,
    pub total: usize,
}

pub(super) fn head_state(repo: &RawRepo) -> Result<HeadState> {
    let head = repo.find_reference("HEAD")?;
    let reference = head.symbolic_target()?.unwrap_or("HEAD").to_owned();
    ensure!(
        reference == "HEAD" || reference.starts_with("refs/heads/"),
        "HEAD must point to a local branch or commit."
    );
    let id = match head.resolve() {
        Ok(reference) => reference.target().map(|id| id.to_string()),
        Err(e) if e.code() == ErrorCode::NotFound || e.code() == ErrorCode::UnbornBranch => None,
        Err(e) => return Err(e.into()),
    };
    Ok(HeadState {
        id,
        reference,
        git_dir: repo.path().to_path_buf(),
    })
}

fn editable(repo: &RawRepo) -> Result<()> {
    ensure!(
        repo.state() == RepositoryState::Clean,
        "Finish or abort the merge, rebase, cherry-pick or revert before editing history."
    );
    ensure!(
        !repo.index()?.has_conflicts(),
        "Resolve index conflicts before editing history."
    );
    Ok(())
}

/// Keep HEAD locked while updating its branch so a concurrent checkout cannot
/// redirect the operation to a different branch. Recheck after taking locks.
fn rewrite_head(
    repo: &RawRepo,
    expected: &HeadState,
    message: &str,
    target: impl FnOnce() -> Result<Option<Oid>>,
) -> Result<()> {
    ensure!(
        repo.path() == expected.git_dir,
        "This selection belongs to a different repository."
    );
    let mut head_lock = repo.transaction()?;
    head_lock.lock_ref("HEAD")?;
    let mut branch_lock = if expected.reference != "HEAD" {
        ensure!(
            git2::Reference::is_valid_name(&expected.reference),
            "Invalid branch reference."
        );
        let mut lock = repo.transaction()?;
        lock.lock_ref(&expected.reference)?;
        Some(lock)
    } else {
        None
    };
    ensure!(
        head_state(repo)? == *expected,
        "HEAD changed. Reopen the action and review it again."
    );
    editable(repo)?;
    let signature = author(repo)?;
    let target = target()?;
    // Ensure recovery remains possible even when automatic reflogs are disabled.
    repo.reference_ensure_log("HEAD")?;
    repo.reference_ensure_log(&expected.reference)?;
    let transaction = branch_lock.as_mut().unwrap_or(&mut head_lock);
    if let Some(id) = target {
        transaction.set_target(&expected.reference, id, Some(&signature), message)?;
    } else {
        ensure!(
            expected.reference != "HEAD",
            "Cannot undo an initial commit while HEAD is detached. Create a branch first."
        );
        // Deleting the unborn branch's predecessor removes the branch log.
        // Keep its old commit in HEAD's log, including when logging was disabled.
        let mut log = repo.reflog("HEAD")?;
        if let Some(old) = &expected.id
            && log
                .get(0)
                .is_none_or(|entry| entry.id_new().to_string() != *old)
        {
            log.append(
                oid(old)?,
                &signature,
                Some("gitbuddy: before undo initial commit"),
            )?;
        }
        log.append(Oid::ZERO_SHA1, &signature, Some(message))?;
        log.write()?;
        transaction.remove(&expected.reference)?;
    }
    if let Some(transaction) = branch_lock {
        transaction.commit()?;
        // head_lock stays alive until the branch update (and HEAD reflog) finishes.
    } else {
        head_lock.commit()?;
    }
    Ok(())
}

impl Repository {
    pub fn commit_edit(&self) -> Result<CommitEdit> {
        let repo = raw(self)?;
        editable(&repo)?;
        let head = head_state(&repo)?;
        let id = head
            .id
            .as_deref()
            .context("There is no commit to edit yet.")?;
        let commit = repo.find_commit(oid(id)?)?;
        let signature = commit.author();
        Ok(CommitEdit {
            head,
            message: commit.message()?.to_owned(),
            author: format!(
                "{} <{}>",
                signature.name().unwrap_or_default(),
                signature.email().unwrap_or_default()
            ),
            parents: commit.parent_ids().map(|id| id.to_string()).collect(),
            index_tree: repo.index()?.write_tree()?,
        })
    }

    pub(super) fn amend(&self, context: &CommitEdit, message: &str) -> Result<String> {
        ensure!(!message.trim().is_empty(), "Please enter a commit message.");
        let repo = raw(self)?;
        rewrite_head(&repo, &context.head, "commit (amend): GitBuddy", || {
            let mut index = repo.index()?;
            index.read(true)?;
            ensure!(
                index.write_tree()? == context.index_tree,
                "Staged content changed. Reopen Amend and review it again."
            );
            let tree = repo.find_tree(context.index_tree)?;
            let commit = repo.find_commit(oid(context
                .head
                .id
                .as_deref()
                .context("No HEAD commit")?)?)?;
            Ok(Some(commit.amend(
                None,
                None,
                Some(&author(&repo)?),
                Some("UTF-8"),
                Some(message),
                Some(&tree),
            )?))
        })?;
        Ok("Amended latest commit; unstaged changes preserved".into())
    }

    pub(super) fn undo_last(&self, expected: &HeadState) -> Result<String> {
        let repo = raw(self)?;
        rewrite_head(
            &repo,
            expected,
            "reset: undo latest commit (GitBuddy, keep index/worktree)",
            || {
                let commit = repo.find_commit(oid(expected
                    .id
                    .as_deref()
                    .context("There is no commit to undo.")?)?)?;
                if commit.parent_count() == 0 {
                    ensure!(
                        expected.reference != "HEAD",
                        "Cannot undo an initial commit while HEAD is detached. Create a branch first."
                    );
                    Ok(None)
                } else {
                    // Merge commits are undone to their first parent.
                    Ok(Some(commit.parent_id(0)?))
                }
            },
        )?;
        Ok("Latest commit undone; index and working tree preserved".into())
    }

    pub fn reflog(&self, limit: usize) -> Result<ReflogPage> {
        let repo = raw(self)?;
        let head = head_state(&repo)?;
        let log = match repo.reflog("HEAD") {
            Ok(log) => log,
            Err(e) if e.code() == ErrorCode::NotFound => {
                return Ok(ReflogPage {
                    head,
                    entries: Vec::new(),
                    total: 0,
                });
            }
            Err(e) => return Err(e.into()),
        };
        let target = |id: Oid| {
            if id.is_zero() {
                return None;
            }
            let commit = repo.find_commit(id).ok();
            Some(ReflogTarget {
                id: id.to_string(),
                available: commit.is_some(),
                git_dir: repo.path().to_path_buf(),
                subject: commit
                    .as_ref()
                    .and_then(|c| c.summary().ok().flatten())
                    .unwrap_or("Commit no longer available")
                    .into(),
            })
        };
        let entries = log
            .iter()
            .take(limit)
            .enumerate()
            .map(|(i, entry)| {
                let signature = entry.committer();
                ReflogEntry {
                    selector: format!("HEAD@{{{i}}}"),
                    message: String::from_utf8_lossy(entry.message_bytes().unwrap_or_default())
                        .into_owned(),
                    committer: signature.name().unwrap_or_default().into(),
                    date: Local
                        .timestamp_opt(signature.when().seconds(), 0)
                        .single()
                        .map(|t| t.format("%Y-%m-%d %H:%M:%S").to_string())
                        .unwrap_or_default(),
                    before: target(entry.id_old()),
                    after: target(entry.id_new()),
                }
            })
            .collect();
        Ok(ReflogPage {
            head,
            entries,
            total: log.len(),
        })
    }

    pub(super) fn restore_reflog(
        &self,
        expected: &HeadState,
        target: &ReflogTarget,
    ) -> Result<String> {
        let repo = raw(self)?;
        ensure!(
            repo.path() == target.git_dir,
            "This reflog entry belongs to a different repository."
        );
        let commit = repo
            .find_commit(oid(&target.id)?)
            .context("This commit has expired or is unavailable.")?;
        rewrite_head(
            &repo,
            expected,
            "reset: restore from reflog (GitBuddy, keep index/worktree)",
            || Ok(Some(commit.id())),
        )?;
        Ok(format!(
            "Restored HEAD to {}; index and working tree preserved",
            &target.id[..8]
        ))
    }

    pub(super) fn recover_branch(&self, target: &ReflogTarget, name: &str) -> Result<String> {
        let repo = raw(self)?;
        ensure!(
            repo.path() == target.git_dir,
            "This reflog entry belongs to a different repository."
        );
        ensure!(
            !name.trim().is_empty() && !name.starts_with('-') && git2::Branch::name_is_valid(name)?,
            "Invalid recovery branch name."
        );
        let commit = repo
            .find_commit(oid(&target.id)?)
            .context("This commit has expired or is unavailable.")?;
        repo.branch(name, &commit, false)?;
        Ok(format!(
            "Created recovery branch {name}; current branch unchanged"
        ))
    }
}
