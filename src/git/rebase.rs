//! A resumable, editable sequencer. Commit replay and reference updates use libgit2.
use super::libgit::{author, oid, raw};
use super::recovery::head_state;
use super::{HeadState, Repository};
use anyhow::{Context, Result, bail, ensure};
use git2::{CherrypickOptions, Repository as Raw, RepositoryState, build::CheckoutBuilder};
use serde::{Deserialize, Serialize};
use std::{collections::HashSet, fs, io::Write};

const STATE: &str = "gitbuddy-rebase.json";
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RebaseAction {
    Pick,
    Reword,
    Squash,
    Fixup,
    Drop,
}
impl RebaseAction {
    pub fn label(self) -> &'static str {
        match self {
            Self::Pick => "Pick",
            Self::Reword => "Reword",
            Self::Squash => "Squash",
            Self::Fixup => "Fixup",
            Self::Drop => "Drop",
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RebaseStep {
    pub id: String,
    pub message: String,
    pub action: RebaseAction,
}
#[derive(Clone, Debug)]
pub struct RebasePreview {
    pub head: HeadState,
    pub onto: String,
    pub base: String,
    pub steps: Vec<RebaseStep>,
}
#[derive(Clone, Debug)]
pub struct RebaseStatus {
    pub branch: String,
    pub original: String,
    pub onto: String,
    pub completed: usize,
    pub total: usize,
    pub current: Option<RebaseStep>,
    pub backup: String,
}
#[derive(Serialize, Deserialize)]
struct State {
    version: u32,
    branch: String,
    original: String,
    onto: String,
    current: String,
    steps: Vec<RebaseStep>,
    cursor: usize,
    pending: bool,
    // CHERRY_PICK_HEAD is written before checkout/index by libgit2. Only
    // this durable acknowledgement proves the apply call completed.
    #[serde(default)]
    applied: bool,
    backup: String,
    // Journal the new object before moving HEAD, allowing either side of a crash
    // between the ref update and the next state write to resume exactly once.
    prepared: Option<String>,
    #[serde(default)]
    finishing: bool,
}
pub(super) fn active(repo: &Raw) -> bool {
    repo.path().join(STATE).exists()
}
fn load(repo: &Raw) -> Result<State> {
    let state: State = serde_json::from_slice(
        &fs::read(repo.path().join(STATE)).context("No GitBuddy rebase is in progress")?,
    )?;
    ensure!(
        state.version == 1 && state.cursor <= state.steps.len(),
        "Unsupported or invalid rebase state; preserve the state file for recovery"
    );
    oid(&state.original)?;
    oid(&state.onto)?;
    oid(&state.current)?;
    for step in &state.steps {
        oid(&step.id)?;
    }
    if let Some(id) = &state.prepared {
        oid(id)?;
    }
    ensure!(
        state.branch.starts_with("refs/heads/") && git2::Reference::is_valid_name(&state.branch),
        "Invalid rebase branch"
    );
    Ok(state)
}
fn save(repo: &Raw, state: &State) -> Result<()> {
    let mut temp = tempfile::NamedTempFile::new_in(repo.path())?;
    temp.write_all(&serde_json::to_vec_pretty(state)?)?;
    temp.as_file().sync_all()?;
    temp.persist(repo.path().join(STATE))?;
    Ok(())
}
struct Lock(fs::File);
impl Lock {
    fn acquire(repo: &Raw) -> Result<Self> {
        let path = repo.path().join("gitbuddy-rebase.lock");
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .context("Cannot open the rebase operation lock")?;
        file.try_lock()
            .context("Another rebase action is running")?;
        Ok(Self(file))
    }
}
impl Drop for Lock {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}
fn clean(repo: &Raw) -> Result<()> {
    ensure!(
        repo.state() == RepositoryState::Clean && !active(repo),
        "Finish or abort the current repository operation first"
    );
    ensure!(
        super::libgit::file_changes(repo)?.is_empty(),
        "Commit or stash all staged, unstaged and untracked files before rebasing"
    );
    Ok(())
}
fn checked_branch(repo: &Raw, state: &State) -> Result<()> {
    super::workspaces::branch_available(repo, &state.branch)?;
    let branch = repo.find_reference(&state.branch)?.target();
    ensure!(
        branch == Some(oid(&state.original)?)
            || (state.finishing && branch == Some(oid(&state.current)?)),
        "Original branch moved externally; rebase will not overwrite it"
    );
    let head = head_state(repo)?;
    if head.reference == state.branch {
        let starting = state.cursor == 0
            && !state.pending
            && state.prepared.is_none()
            && head.id.as_deref() == Some(&state.original);
        let finishing = state.finishing
            && (head.id.as_deref() == Some(&state.current)
                || head.id.as_deref() == Some(&state.original));
        ensure!(
            starting || finishing,
            "HEAD was switched externally; restore the detached rebase HEAD before continuing"
        );
        return Ok(());
    }
    ensure!(
        head.reference == "HEAD",
        "HEAD was switched externally; restore the detached rebase HEAD before continuing"
    );
    let expected = head.id.as_deref();
    ensure!(
        expected == Some(&state.current) || state.prepared.as_deref() == expected,
        "Rebase HEAD moved externally; no changes made"
    );
    Ok(())
}
fn move_head(repo: &Raw, expected: &str, target: &str) -> Result<()> {
    let mut lock = repo.transaction()?;
    lock.lock_ref("HEAD")?;
    let head = head_state(repo)?;
    ensure!(
        head.reference == "HEAD" && head.id.as_deref() == Some(expected),
        "Rebase HEAD changed"
    );
    repo.reference_ensure_log("HEAD")?;
    lock.set_target(
        "HEAD",
        oid(target)?,
        Some(&author(repo)?),
        "gitbuddy: rebase step",
    )?;
    lock.commit()?;
    Ok(())
}
impl Repository {
    pub fn rebase_preview(&self, upstream: &str) -> Result<RebasePreview> {
        let repo = raw(self)?;
        clean(&repo)?;
        let head = head_state(&repo)?;
        ensure!(
            head.reference != "HEAD",
            "Check out a local branch before rebasing"
        );
        let original = oid(head.id.as_deref().context("No commits to rebase")?)?;
        let onto = repo
            .revparse_single(upstream.trim())?
            .peel_to_commit()?
            .id();
        let base = repo
            .merge_base(original, onto)
            .context("Rebase requires a common ancestor")?;
        let mut current = original;
        let mut steps = Vec::new();
        while current != base {
            ensure!(steps.len() < 2000, "Rebase is limited to 2,000 commits");
            let commit = repo.find_commit(current)?;
            ensure!(
                commit.parent_count() == 1,
                "This range contains a merge commit; preserving merges is not supported"
            );
            steps.push(RebaseStep {
                id: current.to_string(),
                message: commit.message()?.into(),
                action: RebaseAction::Pick,
            });
            current = commit.parent_id(0)?;
        }
        ensure!(
            !steps.is_empty(),
            "There are no commits after the common ancestor"
        );
        steps.reverse();
        Ok(RebasePreview {
            head,
            onto: onto.to_string(),
            base: base.to_string(),
            steps,
        })
    }
    pub fn rebase_status(&self) -> Result<Option<RebaseStatus>> {
        let repo = raw(self)?;
        if !active(&repo) {
            return Ok(None);
        }
        let s = load(&repo)?;
        Ok(Some(RebaseStatus {
            branch: s.branch,
            original: s.original,
            onto: s.onto,
            completed: s.cursor,
            total: s.steps.len(),
            current: s.steps.get(s.cursor).cloned(),
            backup: s.backup,
        }))
    }
    pub(super) fn start_rebase(
        &self,
        preview: &RebasePreview,
        steps: Vec<RebaseStep>,
    ) -> Result<String> {
        let repo = raw(self)?;
        let _lock = Lock::acquire(&repo)?;
        clean(&repo)?;
        ensure!(
            head_state(&repo)? == preview.head,
            "HEAD changed; rebuild the rebase plan"
        );
        let expected: HashSet<_> = preview.steps.iter().map(|s| &s.id).collect();
        let chosen: HashSet<_> = steps.iter().map(|s| &s.id).collect();
        ensure!(
            steps.len() == preview.steps.len() && chosen == expected && chosen.len() == steps.len(),
            "Each original commit must appear exactly once; use Drop to omit a commit"
        );
        let mut first = true;
        let mut parent = oid(&preview.base)?;
        for step in &preview.steps {
            let commit = repo.find_commit(oid(&step.id)?)?;
            ensure!(
                commit.parent_count() == 1 && commit.parent_id(0)? == parent,
                "Original rebase range changed; rebuild the plan"
            );
            parent = commit.id();
        }
        ensure!(
            Some(parent.to_string()).as_deref() == preview.head.id.as_deref(),
            "Plan does not end at the original HEAD"
        );
        for step in &steps {
            if step.action == RebaseAction::Drop {
                continue;
            }
            ensure!(
                !first || !matches!(step.action, RebaseAction::Squash | RebaseAction::Fixup),
                "Squash / Fixup requires a preceding retained commit"
            );
            ensure!(
                !step.message.trim().is_empty(),
                "Retained commits need a message"
            );
            first = false;
        }
        let signature = author(&repo)?;
        let original = preview.head.id.clone().context("No HEAD")?;
        let backup = format!(
            "refs/gitbuddy/rewrites/{}-{}",
            &original[..8],
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        );
        repo.reference(&backup, oid(&original)?, false, "gitbuddy: before rebase")?;
        repo.reference(
            "ORIG_HEAD",
            oid(&original)?,
            true,
            "gitbuddy: before rebase",
        )?;
        let mut state = State {
            version: 1,
            branch: preview.head.reference.clone(),
            original: original.clone(),
            onto: preview.onto.clone(),
            current: preview.onto.clone(),
            steps,
            cursor: 0,
            pending: false,
            applied: false,
            backup,
            prepared: None,
            finishing: false,
        };
        // Lock both references through the initial checkout and detachment.
        let mut refs = repo.transaction()?;
        refs.lock_ref("HEAD")?;
        refs.lock_ref(&state.branch)?;
        ensure!(
            head_state(&repo)? == preview.head,
            "HEAD changed while starting rebase"
        );
        save(&repo, &state)?;
        let target = repo.find_commit(oid(&state.onto)?)?;
        let mut checkout = CheckoutBuilder::new();
        checkout.safe();
        if let Err(error) = repo.checkout_tree(target.as_object(), Some(&mut checkout)) {
            let _ = fs::remove_file(repo.path().join(STATE));
            return Err(error.into());
        }
        repo.reference_ensure_log("HEAD")?;
        refs.set_target(
            "HEAD",
            target.id(),
            Some(&signature),
            "gitbuddy: rebase start",
        )?;
        refs.commit()?;
        run(&repo, &mut state)
    }
    pub(super) fn continue_rebase(&self) -> Result<String> {
        let repo = raw(self)?;
        let _lock = Lock::acquire(&repo)?;
        let mut state = load(&repo)?;
        checked_branch(&repo, &state)?;
        run(&repo, &mut state)
    }
    pub(super) fn abort_rebase(&self) -> Result<String> {
        let repo = raw(self)?;
        let _lock = Lock::acquire(&repo)?;
        let state = load(&repo)?;
        checked_branch(&repo, &state)?;
        ensure!(
            repo.find_reference(&state.branch)?.target() == Some(oid(&state.original)?),
            "Rebase already finished; Continue rebase to clear its saved state"
        );
        let mut refs = repo.transaction()?;
        refs.lock_ref("HEAD")?;
        refs.lock_ref(&state.branch)?;
        checked_branch(&repo, &state)?;
        let original = repo.find_commit(oid(&state.original)?)?;
        let mut checkout = CheckoutBuilder::new();
        checkout.force();
        repo.checkout_tree(original.as_object(), Some(&mut checkout))?;
        refs.set_symbolic_target(
            "HEAD",
            &state.branch,
            Some(&author(&repo)?),
            "gitbuddy: rebase abort",
        )?;
        refs.commit()?;
        if repo.state() != RepositoryState::Clean {
            repo.cleanup_state()?;
        }
        fs::remove_file(repo.path().join(STATE))?;
        Ok("Rebase aborted; original branch and tracked files restored".into())
    }
}
fn run(repo: &Raw, state: &mut State) -> Result<String> {
    checked_branch(repo, state)?;
    if !state.finishing && head_state(repo)?.reference == state.branch {
        // Recovery if the process stopped after saving the initial journal but
        // before detaching HEAD. A safe checkout refuses newer working edits.
        let mut refs = repo.transaction()?;
        refs.lock_ref("HEAD")?;
        refs.lock_ref(&state.branch)?;
        checked_branch(repo, state)?;
        let target = repo.find_commit(oid(&state.onto)?)?;
        let mut checkout = CheckoutBuilder::new();
        checkout.safe();
        repo.checkout_tree(target.as_object(), Some(&mut checkout))?;
        refs.set_target(
            "HEAD",
            target.id(),
            Some(&author(repo)?),
            "gitbuddy: resume rebase start",
        )?;
        refs.commit()?;
    }
    loop {
        checked_branch(repo, state)?;
        if let Some(prepared) = state.prepared.clone() {
            let head = head_state(repo)?.id.context("No rebase HEAD")?;
            if head != prepared {
                move_head(repo, &state.current, &prepared)?;
            }
            state.current = prepared;
            state.cursor += 1;
            state.pending = false;
            state.applied = false;
            state.prepared = None;
            if repo.state() != RepositoryState::Clean {
                repo.cleanup_state()?;
            }
            save(repo, state)?;
        }
        let Some(step) = state.steps.get(state.cursor).cloned() else {
            break;
        };
        if step.action == RebaseAction::Drop {
            state.cursor += 1;
            save(repo, state)?;
            continue;
        }
        let picked = repo.find_commit(oid(&step.id)?)?;
        if !state.pending {
            ensure!(
                repo.state() == RepositoryState::Clean
                    && super::libgit::file_changes(repo)?.is_empty(),
                "Working tree changed during rebase; review it before continuing"
            );
            state.pending = true;
            state.applied = false;
            save(repo, state)?;
            let mut checkout = CheckoutBuilder::new();
            checkout
                .safe()
                .allow_conflicts(true)
                .conflict_style_merge(true);
            let mut options = CherrypickOptions::new();
            options.checkout_builder(checkout);
            repo.cherrypick(&picked, Some(&mut options))?;
            state.applied = true;
            save(repo, state)?;
        }
        ensure!(
            state.applied,
            "Interrupted while applying a rebase step; application cannot be verified. Preserve any resolutions, then Abort rebase and rebuild the plan. No commit was created"
        );
        ensure!(
            !repo.path().join("index.lock").exists(),
            "An index lock exists; verify no Git process is running before recovering the interrupted operation"
        );
        let marker = fs::read_to_string(repo.path().join("CHERRY_PICK_HEAD"))
            .context("Interrupted before applying a rebase step; abort and rebuild the plan")?;
        ensure!(
            marker.trim() == step.id,
            "The pending cherry-pick changed; refusing to continue"
        );
        let mut index = repo.index()?;
        index.read(true)?;
        if index.has_conflicts() {
            bail!(
                "Rebase paused at {}. Resolve conflicts, then Continue rebase or Abort.",
                &step.id[..8]
            );
        }
        ensure!(
            !super::libgit::file_changes(repo)?
                .iter()
                .any(|file| file.unstaged()),
            "Stage all conflict resolutions and other edits before continuing rebase"
        );
        let tree = repo.find_tree(index.write_tree()?)?;
        let previous = repo.find_commit(oid(&state.current)?)?;
        let combine = matches!(step.action, RebaseAction::Squash | RebaseAction::Fixup);
        let message = match step.action {
            RebaseAction::Squash => format!(
                "{}\n\n{}",
                previous.message()?.trim_end(),
                step.message.trim()
            ),
            RebaseAction::Fixup => previous.message()?.into(),
            RebaseAction::Pick => picked.message()?.into(),
            _ => step.message.clone(),
        };
        let parents = if combine {
            previous.parents().collect::<Vec<_>>()
        } else {
            vec![previous.clone()]
        };
        let parents = parents.iter().collect::<Vec<_>>();
        let original_author = if combine {
            previous.author()
        } else {
            picked.author()
        };
        let id = repo.commit(
            None,
            &original_author,
            &author(repo)?,
            &message,
            &tree,
            &parents,
        )?;
        state.prepared = Some(id.to_string());
        save(repo, state)?;
    }
    ensure!(
        repo.state() == RepositoryState::Clean,
        "Unexpected operation state at rebase completion"
    );
    let mut refs = repo.transaction()?;
    refs.lock_ref("HEAD")?;
    refs.lock_ref(&state.branch)?;
    checked_branch(repo, state)?;
    state.finishing = true;
    save(repo, state)?;
    repo.reference_ensure_log(&state.branch)?;
    let signature = author(repo)?;
    refs.set_target(
        &state.branch,
        oid(&state.current)?,
        Some(&signature),
        "gitbuddy: rebase finish",
    )?;
    refs.set_symbolic_target(
        "HEAD",
        &state.branch,
        Some(&signature),
        "gitbuddy: rebase finish",
    )?;
    refs.commit()?;
    fs::remove_file(repo.path().join(STATE))?;
    Ok(format!(
        "Rebase complete; original history kept at {}",
        state.backup
    ))
}

#[cfg(test)]
mod tests {
    use super::super::{ConflictResolution, Operation};
    use super::*;
    use std::sync::Arc;
    fn fixture() -> (tempfile::TempDir, Repository, RebasePreview) {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        repo.execute(Operation::SetIdentity(
            "Author".into(),
            "author@example.invalid".into(),
        ))
        .unwrap();
        let commit = |text: &str, message: &str| {
            fs::write(repo.root.join("file"), text).unwrap();
            repo.execute(Operation::StageAll).unwrap();
            repo.execute(Operation::Commit(message.into())).unwrap();
        };
        commit("base\n", "base");
        repo.execute(Operation::CreateBranch("feature".into()))
            .unwrap();
        commit("feature\n", "feature");
        repo.execute(Operation::Checkout("main".into())).unwrap();
        commit("main\n", "main");
        repo.execute(Operation::Checkout("feature".into())).unwrap();
        let preview = repo.rebase_preview("main").unwrap();
        (dir, repo, preview)
    }
    fn paused(repo: &Repository, preview: RebasePreview) {
        assert!(
            repo.execute(Operation::Rebase {
                steps: preview.steps.clone(),
                context: Arc::new(preview)
            })
            .is_err()
        );
        let context = repo.conflict_context(std::path::Path::new("file")).unwrap();
        repo.execute(Operation::ResolveConflict {
            context: Arc::new(context),
            resolution: ConflictResolution::Edited("resolved\n".into()),
        })
        .unwrap();
    }
    fn prepared(repo: &Raw, state: &State) -> String {
        let mut index = repo.index().unwrap();
        let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
        let parent = repo.find_commit(oid(&state.current).unwrap()).unwrap();
        repo.commit(
            None,
            &parent.author(),
            &author(repo).unwrap(),
            "prepared",
            &tree,
            &[&parent],
        )
        .unwrap()
        .to_string()
    }
    #[test]
    fn prepared_commit_resumes_once_before_or_after_head_update() {
        for moved in [false, true] {
            let (_dir, repo, preview) = fixture();
            paused(&repo, preview);
            let raw = raw(&repo).unwrap();
            let mut state = load(&raw).unwrap();
            let id = prepared(&raw, &state);
            state.prepared = Some(id.clone());
            save(&raw, &state).unwrap();
            if moved {
                move_head(&raw, &state.current, &id).unwrap();
            }
            repo.execute(Operation::ContinueRebase).unwrap();
            assert_eq!(repo.snapshot(10).unwrap().head_id, Some(id));
            assert!(!active(&raw));
        }
    }
    #[test]
    fn completed_reference_transaction_can_resume_cleanup() {
        let (_dir, repo, preview) = fixture();
        paused(&repo, preview);
        let raw = raw(&repo).unwrap();
        let mut state = load(&raw).unwrap();
        let id = prepared(&raw, &state);
        move_head(&raw, &state.current, &id).unwrap();
        raw.cleanup_state().unwrap();
        state.current = id.clone();
        state.cursor = state.steps.len();
        state.pending = false;
        state.finishing = true;
        save(&raw, &state).unwrap();
        raw.reference(&state.branch, oid(&id).unwrap(), true, "fixture finish")
            .unwrap();
        raw.set_head(&state.branch).unwrap();
        assert!(repo.execute(Operation::AbortRebase).is_err());
        repo.execute(Operation::ContinueRebase).unwrap();
        assert_eq!(repo.snapshot(10).unwrap().head_id, Some(id));
        assert!(!active(&raw));
    }
    #[test]
    fn original_branch_moved_externally_is_never_overwritten() {
        let (_dir, repo, preview) = fixture();
        paused(&repo, preview);
        let raw = raw(&repo).unwrap();
        let state = load(&raw).unwrap();
        raw.reference(
            &state.branch,
            oid(&state.onto).unwrap(),
            true,
            "external move",
        )
        .unwrap();
        assert!(repo.execute(Operation::ContinueRebase).is_err());
        assert!(repo.execute(Operation::AbortRebase).is_err());
        assert_eq!(
            raw.find_reference(&state.branch).unwrap().target(),
            Some(oid(&state.onto).unwrap())
        );
        assert!(active(&raw));
    }
    #[test]
    fn process_lock_releases_without_removing_its_inode() {
        let (_dir, repo, _) = fixture();
        let raw = raw(&repo).unwrap();
        let lock = Lock::acquire(&raw).unwrap();
        assert!(Lock::acquire(&raw).is_err());
        drop(lock);
        let _next = Lock::acquire(&raw).unwrap();
        assert!(raw.path().join("gitbuddy-rebase.lock").exists());
    }
    #[test]
    fn initial_journal_can_resume_before_detaching_head() {
        let (_dir, repo, preview) = fixture();
        let raw = raw(&repo).unwrap();
        let state = State {
            version: 1,
            branch: preview.head.reference,
            original: preview.head.id.unwrap(),
            onto: preview.onto.clone(),
            current: preview.onto,
            steps: preview.steps,
            cursor: 0,
            pending: false,
            applied: false,
            backup: "refs/gitbuddy/fixture".into(),
            prepared: None,
            finishing: false,
        };
        save(&raw, &state).unwrap();
        assert!(
            repo.execute(Operation::ContinueRebase)
                .unwrap_err()
                .to_string()
                .contains("paused")
        );
        assert!(raw.head_detached().unwrap());
        repo.execute(Operation::AbortRebase).unwrap();
        assert_eq!(repo.snapshot(10).unwrap().head_id, Some(state.original));
    }
}
