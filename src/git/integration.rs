//! Explicit merge/pull policies and a durable, single-parent squash workflow.
use super::libgit::{author, file_changes, oid, raw};
use super::recovery::{editable, head_state};
use super::{HeadState, NetworkControl, RemoteInfo, Repository};
use anyhow::{Context, Result, ensure};
use git2::{Oid, RepositoryState, build::CheckoutBuilder};
use serde::{Deserialize, Serialize};
use std::{fs, io::Write, path::PathBuf};

pub(super) const SQUASH_STATE: &str = "gitbuddy-squash.json";
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MergeMode {
    #[default]
    Auto,
    NoFastForward,
    Squash,
}
impl MergeMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "Fast-forward or merge",
            Self::NoFastForward => "No fast-forward",
            Self::Squash => "Squash",
        }
    }
    pub fn impact(self) -> &'static str {
        match self {
            Self::Auto => {
                "Fast-forward when possible; otherwise create a merge commit with two parents."
            }
            Self::NoFastForward => {
                "Create a merge commit with two parents even when a fast-forward is possible."
            }
            Self::Squash => {
                "Stage the combined result without moving HEAD. Review and commit it as one single-parent commit."
            }
        }
    }
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PullStrategy {
    #[default]
    FastForwardOnly,
    Merge,
    Rebase,
}
impl PullStrategy {
    pub fn label(self) -> &'static str {
        match self {
            Self::FastForwardOnly => "Fast-forward only",
            Self::Merge => "Merge",
            Self::Rebase => "Rebase",
        }
    }
    pub fn impact(self) -> &'static str {
        match self {
            Self::FastForwardOnly => "Fetch upstream and fast-forward. Refuse diverged history.",
            Self::Merge => {
                "Fetch upstream; fast-forward when possible, otherwise create a two-parent merge commit."
            }
            Self::Rebase => {
                "Fetch upstream and replay local commits onto it. This rewrites local commit IDs; linear local history only."
            }
        }
    }
}
#[derive(Clone, Debug)]
pub struct MergePreview {
    pub head: HeadState,
    pub target: String,
    pub label: String,
    reference: String,
    fingerprint: u64,
}
#[derive(Clone, Debug)]
pub struct PullPreview {
    pub head: HeadState,
    pub upstream: String,
    pub remote: Option<RemoteInfo>,
    merge_ref: String,
    fingerprint: u64,
}
#[derive(Clone, Serialize, Deserialize)]
struct SquashState {
    version: u8,
    #[serde(with = "crate::session::path_encoding")]
    git_dir: PathBuf,
    reference: String,
    original: String,
    target: String,
    message: String,
    applied: bool,
    prepared: Option<String>,
}
pub(super) fn squash_active(repo: &git2::Repository) -> bool {
    repo.path().join(SQUASH_STATE).exists()
}
fn load(repo: &git2::Repository) -> Result<SquashState> {
    let path = repo.path().join(SQUASH_STATE);
    ensure!(
        fs::metadata(&path)?.len() <= 1024 * 1024,
        "Squash state is too large; preserve it and inspect the repository"
    );
    let bytes = fs::read(path)?;
    let state: SquashState = serde_json::from_slice(&bytes)
        .context("Unreadable squash state; preserve it before recovery")?;
    ensure!(
        state.version == 1 && state.git_dir == repo.path(),
        "Squash state belongs to another repository or version"
    );
    ensure!(
        state.reference == "HEAD"
            || (state.reference.starts_with("refs/heads/")
                && git2::Reference::is_valid_name(&state.reference)),
        "Invalid squash branch"
    );
    oid(&state.original)?;
    oid(&state.target)?;
    if let Some(id) = &state.prepared {
        oid(id)?;
    }
    Ok(state)
}
fn save(repo: &git2::Repository, state: &SquashState) -> Result<()> {
    let mut file = tempfile::NamedTempFile::new_in(repo.path())?;
    file.write_all(&serde_json::to_vec(state)?)?;
    file.as_file().sync_all()?;
    file.persist(repo.path().join(SQUASH_STATE))?;
    Ok(())
}
fn remove_if_exists(path: PathBuf) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}
fn clear_squash(repo: &git2::Repository) -> Result<()> {
    remove_if_exists(repo.path().join("SQUASH_MSG"))?;
    remove_if_exists(repo.path().join(SQUASH_STATE))
}
pub(super) fn squash_status(repo: &git2::Repository) -> Result<Option<(bool, String)>> {
    if !squash_active(repo) {
        return Ok(None);
    }
    let state = load(repo)?;
    Ok(Some((state.applied, state.message)))
}
fn clean(repo: &git2::Repository) -> Result<()> {
    editable(repo)?;
    ensure!(
        file_changes(repo)?.is_empty(),
        "Commit, stash or discard existing changes before merge/pull"
    );
    Ok(())
}
fn lock_head<'r>(repo: &'r git2::Repository, head: &HeadState) -> Result<git2::Transaction<'r>> {
    let mut transaction = repo.transaction()?;
    transaction.lock_ref("HEAD")?;
    if head.reference != "HEAD" {
        super::workspaces::branch_available(repo, &head.reference)?;
        transaction.lock_ref(&head.reference)?;
    }
    ensure!(
        head_state(repo)? == *head,
        "HEAD changed; reopen and review the operation"
    );
    Ok(transaction)
}
impl Repository {
    pub fn prepare_merge(&self, branch: &str) -> Result<MergePreview> {
        let repo = raw(self)?;
        clean(&repo)?;
        let head = head_state(&repo)?;
        ensure!(
            head.id.is_some(),
            "Create or check out a commit before merging"
        );
        let reference = super::libgit::merge_reference(&repo, branch)?;
        Ok(MergePreview {
            head,
            target: reference.peel_to_commit()?.id().to_string(),
            label: branch.into(),
            reference: reference.name()?.into(),
            fingerprint: self.fingerprint()?,
        })
    }
    pub fn prepare_pull(&self) -> Result<PullPreview> {
        let repo = raw(self)?;
        clean(&repo)?;
        let head = head_state(&repo)?;
        ensure!(
            head.reference != "HEAD" && head.id.is_some(),
            "Check out a local branch before pulling"
        );
        let (remote, upstream, merge_ref) = self.pull_configuration(&repo, &head)?;
        Ok(PullPreview {
            head,
            remote,
            upstream,
            merge_ref,
            fingerprint: self.fingerprint()?,
        })
    }
    fn pull_configuration(
        &self,
        repo: &git2::Repository,
        head: &HeadState,
    ) -> Result<(Option<RemoteInfo>, String, String)> {
        let remote_name = repo
            .branch_upstream_remote(&head.reference)
            .context("Current branch has no upstream remote")?
            .as_str()?
            .to_owned();
        let merge_ref = repo
            .branch_upstream_merge(&head.reference)
            .context("Current branch has no upstream branch")?
            .as_str()?
            .to_owned();
        ensure!(
            merge_ref.starts_with("refs/heads/") && git2::Reference::is_valid_name(&merge_ref),
            "Upstream is not a valid branch reference"
        );
        let (remote, upstream) = if remote_name == "." {
            (None, merge_ref.clone())
        } else {
            let remote = self
                .remote_info()?
                .into_iter()
                .find(|r| r.name == remote_name)
                .context("Upstream remote no longer exists")?;
            let upstream = super::remotes::tracking_ref(repo, &remote_name, &merge_ref)?;
            (Some(remote), upstream)
        };
        Ok((remote, upstream, merge_ref))
    }
    pub(super) fn merge_with(&self, preview: &MergePreview, mode: MergeMode) -> Result<String> {
        let repo = raw(self)?;
        clean(&repo)?;
        ensure!(
            head_state(&repo)? == preview.head && self.fingerprint()? == preview.fingerprint,
            "Repository changed; reopen Merge and review again"
        );
        ensure!(
            repo.find_reference(&preview.reference)?
                .peel_to_commit()?
                .id()
                == oid(&preview.target)?,
            "Merge target moved; reopen Merge"
        );
        self.integrate_target(oid(&preview.target)?, &preview.label, mode, false)
    }
    pub(super) fn pull_with(
        &self,
        preview: &PullPreview,
        strategy: PullStrategy,
        control: NetworkControl,
    ) -> Result<String> {
        control.check()?;
        let repo = raw(self)?;
        clean(&repo)?;
        ensure!(
            head_state(&repo)? == preview.head && self.fingerprint()? == preview.fingerprint,
            "Repository changed; reopen Pull and review again"
        );
        self.check_pull_configuration(preview)?;
        if let Some(remote) = &preview.remote {
            super::libgit::fetch_one(&repo, &remote.name, control.clone())?;
        }
        control.phase("Pull: reviewing fetched upstream…");
        control.check()?;
        clean(&repo)?;
        ensure!(
            head_state(&repo)? == preview.head,
            "HEAD changed during Fetch; upstream was fetched but no local integration was performed"
        );
        self.check_pull_configuration(preview)?;
        let target = repo
            .find_reference(&preview.upstream)?
            .peel_to_commit()?
            .id();
        let annotated = repo.find_annotated_commit(target)?;
        let (analysis, _) = repo.merge_analysis(&[&annotated])?;
        let rebase = if strategy == PullStrategy::Rebase
            && !analysis.is_fast_forward()
            && !analysis.is_up_to_date()
        {
            Some(self.rebase_preview(&target.to_string()).context("Pull rebase requires linear local history; choose Pull merge for a history with merges")?)
        } else {
            None
        };
        control.check()?;
        control.cancellation.finish()?;
        control.phase(match strategy {
            PullStrategy::FastForwardOnly => "Pull: applying fast-forward…",
            PullStrategy::Merge => "Pull: applying merge…",
            PullStrategy::Rebase => "Pull: applying rebase…",
        });
        // Progress callbacks and out-of-process clients may run during Fetch.
        clean(&repo)?;
        ensure!(
            head_state(&repo)? == preview.head,
            "HEAD changed before applying Pull; reopen Pull"
        );
        self.check_pull_configuration(preview)?;
        if let Some(context) = rebase {
            return self.with_lfs(|| self.start_rebase(&context, context.steps.clone()));
        }
        self.integrate_target(
            target,
            &preview.upstream,
            MergeMode::Auto,
            strategy == PullStrategy::FastForwardOnly,
        )
    }
    fn check_pull_configuration(&self, expected: &PullPreview) -> Result<()> {
        let repo = raw(self)?;
        let head = head_state(&repo)?;
        let (remote, upstream, merge_ref) = self.pull_configuration(&repo, &head)?;
        ensure!(
            head == expected.head
                && remote == expected.remote
                && merge_ref == expected.merge_ref
                && upstream == expected.upstream,
            "Upstream or remote configuration changed; reopen Pull"
        );
        Ok(())
    }
    fn with_lfs(&self, work: impl FnOnce() -> Result<String>) -> Result<String> {
        let repo = raw(self)?;
        let result = super::lfs::dehydrate(&repo).and_then(|_| work());
        let hydrated = super::lfs::hydrate(&raw(self)?);
        match (result, hydrated) {
            (Ok(text), Ok(missing)) => Ok(if missing > 0 { format!("{text}; {missing} LFS objects missing locally (use LFS Fetch)") } else { text }),
            (Err(error), _) => Err(error),
            (Ok(_), Err(error)) => Err(error.context("Integration changed the repository, but local LFS checkout failed; inspect the working tree")),
        }
    }
    fn integrate_target(
        &self,
        target: Oid,
        name: &str,
        mode: MergeMode,
        ff_only: bool,
    ) -> Result<String> {
        let repo = raw(self)?;
        clean(&repo)?;
        let original = head_state(&repo)?;
        let mut refs = lock_head(&repo, &original)?;
        let annotated = repo.find_annotated_commit(target)?;
        let (analysis, _) = repo.merge_analysis(&[&annotated])?;
        if analysis.is_up_to_date() {
            return Ok("Already up to date".into());
        }
        ensure!(
            !ff_only || analysis.is_fast_forward(),
            "Cannot fast-forward upstream; choose Pull merge or Pull rebase"
        );
        ensure!(
            analysis.is_fast_forward() || analysis.is_normal(),
            "Cannot merge these histories"
        );
        let head = repo.find_commit(oid(original
            .id
            .as_deref()
            .context("HEAD has no commit")?)?)?;
        let signature = if mode == MergeMode::Squash
            || (mode == MergeMode::Auto && analysis.is_fast_forward())
        {
            repo.signature()
                .or_else(|_| git2::Signature::now("GitBuddy", "gitbuddy@localhost"))?
        } else {
            author(&repo)?
        };
        repo.reference("ORIG_HEAD", head.id(), true, "gitbuddy: before integration")?;
        self.with_lfs(|| {
            if mode == MergeMode::Auto && analysis.is_fast_forward() {
                let commit = repo.find_commit(target)?;
                refs.set_target(&original.reference, target, Some(&signature), &format!("Fast-forward to {name}"))?;
                repo.checkout_tree(commit.as_object(), Some(CheckoutBuilder::new().safe().dry_run()))?;
                repo.checkout_tree(commit.as_object(), Some(CheckoutBuilder::new().safe()))?;
                refs.commit()?;
                return Ok(format!("Fast-forwarded to {name}"));
            }
            let mut squash = if mode == MergeMode::Squash {
                let state = SquashState { version: 1, git_dir: repo.path().into(), reference: original.reference.clone(), original: head.id().to_string(), target: target.to_string(), message: format!("Squash merge '{name}'"), applied: false, prepared: None };
                save(&repo, &state)?;
                Some(state)
            } else { None };
            repo.merge(&[&annotated], None, Some(CheckoutBuilder::new().safe().allow_conflicts(true).conflict_style_merge(true)))?;
            if let Some(state) = squash.as_mut() {
                // Squash must never advertise a second merge parent to commits.
                repo.cleanup_state()?;
                let mut message = tempfile::NamedTempFile::new_in(repo.path())?;
                message.write_all(state.message.as_bytes())?;
                message.as_file().sync_all()?;
                message.persist(repo.path().join("SQUASH_MSG"))?;
                state.applied = true;
                save(&repo, state)?;
                let mut index = repo.index()?;
                ensure!(!index.has_conflicts(), "Squash has conflicts; resolve them, then commit or abort squash");
                if index.write_tree()? == head.tree_id() {
                    clear_squash(&repo)?;
                    return Ok("Squash has no changes; HEAD unchanged".into());
                }
                return Ok("Squash result staged; review and commit to create a single-parent commit (HEAD unchanged)".into());
            }
            let mut index = repo.index()?;
            ensure!(!index.has_conflicts(), "Merge has conflicts; resolve or abort");
            let tree = repo.find_tree(index.write_tree()?)?;
            let other = repo.find_commit(target)?;
            let message = format!("Merge branch '{name}'");
            let id = repo.commit(None, &signature, &signature, &message, &tree, &[&head, &other])?;
            refs.set_target(&original.reference, id, Some(&signature), "gitbuddy: merge")?;
            refs.commit()?;
            repo.cleanup_state()?;
            Ok(format!("Created merge commit {id}"))
        })
    }
    pub(super) fn commit_squash(
        &self,
        message: &str,
        reviewed: Option<(&[u8], &[u8])>,
    ) -> Result<String> {
        let repo = raw(self)?;
        let mut state = load(&repo)?;
        ensure!(
            state.applied,
            "Squash application was interrupted; preserve edits and Abort squash before retrying"
        );
        let head = head_state(&repo)?;
        ensure!(
            head.reference == state.reference,
            "HEAD switched during squash; return to its original branch before continuing"
        );
        let mut refs = lock_head(&repo, &head)?;
        if let Some(prepared) = &state.prepared {
            let commit = repo.find_commit(oid(prepared)?)?;
            ensure!(
                commit.parent_count() == 1 && commit.parent_id(0)? == oid(&state.original)?,
                "Prepared squash commit has unexpected parents; preserve and inspect its state"
            );
        }
        if let Some(prepared) = &state.prepared
            && head.id.as_deref() == Some(prepared)
        {
            clear_squash(&repo)?;
            return Ok(format!(
                "Squash commit {prepared} already created; state cleaned"
            ));
        }
        ensure!(
            head.id.as_deref() == Some(&state.original) && repo.state() == RepositoryState::Clean,
            "Squash HEAD or operation changed; inspect it before continuing"
        );
        let mut index = repo.index()?;
        let path = index.path().context("Missing index")?.to_path_buf();
        let _index_lock = super::conflicts::IndexLock::acquire(&path)?;
        index.read(true)?;
        if let Some((bytes, marker)) = reviewed {
            ensure!(
                fs::read(&path)? == bytes && fs::read(repo.path().join(SQUASH_STATE))? == marker,
                "Squash or staged content changed; refresh before continuing"
            );
        }
        ensure!(
            !index.has_conflicts(),
            "Resolve all squash conflicts before committing"
        );
        ensure!(!message.trim().is_empty(), "Enter a squash commit message");
        let parent = repo.find_commit(oid(&state.original)?)?;
        let tree = repo.find_tree(index.write_tree()?)?;
        ensure!(
            tree.id() != parent.tree_id(),
            "No staged changes to commit; stage squash changes or Abort squash"
        );
        let signature = author(&repo)?;
        let id = if let Some(id) = &state.prepared {
            let commit = repo.find_commit(oid(id)?)?;
            ensure!(
                commit.tree_id() == tree.id(),
                "Prepared squash commit differs from the current index; restore the reviewed index or Abort squash"
            );
            commit.id()
        } else {
            let id = repo.commit(
                None,
                &signature,
                &signature,
                message.trim(),
                &tree,
                &[&parent],
            )?;
            state.prepared = Some(id.to_string());
            save(&repo, &state)?;
            id
        };
        repo.reference_ensure_log("HEAD")?;
        refs.set_target(
            &state.reference,
            id,
            Some(&signature),
            "gitbuddy: squash commit",
        )?;
        refs.commit()?;
        clear_squash(&repo).context(
            "Squash commit created, but state cleanup failed; Continue squash to finish cleanup",
        )?;
        Ok(format!("Created single-parent squash commit {id}"))
    }
    pub(super) fn abort_squash(&self) -> Result<String> {
        let repo = raw(self)?;
        let state = load(&repo)?;
        let head = head_state(&repo)?;
        ensure!(
            head.reference == state.reference && head.id.as_deref() == Some(&state.original),
            "Squash HEAD changed or commit already completed; Continue to clean up a completed squash"
        );
        let _refs = lock_head(&repo, &head)?;
        ensure!(
            matches!(
                repo.state(),
                RepositoryState::Clean | RepositoryState::Merge
            ),
            "Another Git operation is in progress; refusing to overwrite it"
        );
        if repo.state() == RepositoryState::Merge {
            ensure!(
                fs::read_to_string(repo.path().join("MERGE_HEAD"))?.trim() == state.target,
                "Merge target changed; preserve and inspect this operation"
            );
        }
        self.with_lfs(|| {
            let original = repo.find_commit(oid(&state.original)?)?;
            repo.checkout_tree(original.as_object(), Some(CheckoutBuilder::new().force()))?;
            if repo.state() == RepositoryState::Merge {
                repo.cleanup_state()?;
            }
            clear_squash(&repo)?;
            Ok("Squash aborted; original index and tracked files restored".into())
        })
    }
}
