//! Explicit merge/pull policies and a durable, single-parent squash workflow.
use super::libgit::{author, file_changes, oid, raw};
use super::recovery::{editable, head_state};
use super::worktree_conversion::{
    CheckoutHead, CheckoutOptions, acknowledge_checkout, checkout_tree_with_orig_head,
    guard_native_history_write, history_needs_encoded_checkout,
};
use super::{HeadState, NetworkControl, RemoteInfo, Repository};
use anyhow::{Context, Result, ensure};
use git2::{Oid, RepositoryState, build::CheckoutBuilder};
use serde::{Deserialize, Serialize};
use std::{collections::HashSet, fs, io::Write, path::PathBuf};

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
    fetch: Option<super::FetchPlan>,
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
/// Wrap only an already reviewed native write. Encoded checkout owns its raw
/// preimages and must not go through this LFS normalization first.
pub(super) fn with_native_lfs(
    repo: &git2::Repository,
    work: impl FnOnce() -> Result<String>,
) -> Result<String> {
    let result = super::lfs::dehydrate(&git2::Repository::open(repo.path())?).and_then(|_| work());
    let hydrated = super::lfs::hydrate(&git2::Repository::open(repo.path())?);
    match (result, hydrated) {
        (Ok(text), Ok(missing)) => Ok(with_missing_lfs(text, missing)),
        (Err(error), _) => Err(error),
        (Ok(_), Err(error)) => Err(error.context(
            "Integration changed the repository, but local LFS checkout failed; inspect the working tree",
        )),
    }
}
fn with_missing_lfs(text: String, missing: usize) -> String {
    if missing > 0 {
        format!("{text}; {missing} LFS objects missing locally (use LFS Fetch)")
    } else {
        text
    }
}
fn encoded_merge_policy(repo: &git2::Repository) -> Result<()> {
    if let Some(hook) = super::commit_policy::active_merge_hook(repo)? {
        anyhow::bail!(
            "Merge with working-tree-encoding cannot run the active {hook} hook against a staged merge result yet"
        );
    }
    let config = repo.config()?.snapshot()?;
    match config.get_bool("merge.renormalize") {
        Ok(true) => {
            anyhow::bail!("Merge with working-tree-encoding does not support merge.renormalize yet")
        }
        Ok(false) => (),
        Err(error) if error.code() == git2::ErrorCode::NotFound => (),
        Err(error) => return Err(error.into()),
    }
    if let Some(driver) = match config.get_entry("merge.default") {
        Ok(entry) => Some(entry),
        Err(error) if error.code() == git2::ErrorCode::NotFound => None,
        Err(error) => return Err(error.into()),
    } {
        ensure!(
            driver.has_value() && matches!(driver.value_bytes(), b"text" | b"binary" | b"union"),
            "Merge with working-tree-encoding does not support a custom default merge driver"
        );
    }
    let mut entries = config.entries(None)?;
    while let Some(entry) = entries.next() {
        let entry = entry?;
        let name = entry.name_bytes();
        ensure!(
            !(name.starts_with(b"merge.") && name.ends_with(b".driver")),
            "Merge with working-tree-encoding does not support configured custom merge drivers"
        );
    }
    Ok(())
}

fn prepare_encoded_merge_tree(repo: &git2::Repository, ours: Oid, theirs: Oid) -> Result<Oid> {
    // A separate handle retains the real repository's config/attributes, but
    // libgit2 writes any merged blobs to this handle's private memory backend.
    // In particular, another file's conflict cannot leave loose objects behind.
    let planner = git2::Repository::open(repo.path())?;
    let source = planner.odb()?;
    let _memory = source.add_new_mempack_backend(1000)?;
    let mut result = planner.merge_commits(
        &planner.find_commit(ours)?,
        &planner.find_commit(theirs)?,
        None,
    )?;
    ensure!(
        !result.has_conflicts(),
        "Merge with working-tree-encoding has conflicts; no merge was started"
    );
    let tree = result.write_tree_to(&planner)?;
    let destination = repo.odb()?;
    enum Pending {
        VisitTree(Oid),
        Write(Oid, git2::ObjectType),
    }
    let mut visited = HashSet::new();
    let mut objects = vec![Pending::VisitTree(tree)];
    while let Some(next) = objects.pop() {
        match next {
            Pending::VisitTree(id) => {
                if !visited.insert(id) {
                    continue;
                }
                // Persist dependencies before their parent. Visit existing
                // trees too: an older interrupted write may have installed
                // the tree before its newly merged children.
                objects.push(Pending::Write(id, git2::ObjectType::Tree));
                for entry in planner.find_tree(id)?.iter() {
                    match entry.kind() {
                        Some(git2::ObjectType::Tree) => {
                            objects.push(Pending::VisitTree(entry.id()));
                        }
                        Some(kind @ git2::ObjectType::Blob) => {
                            objects.push(Pending::Write(entry.id(), kind));
                        }
                        // A gitlink names an object in the submodule's database.
                        Some(git2::ObjectType::Commit) if entry.filemode() == 0o160000 => (),
                        _ => anyhow::bail!("Invalid entry in the planned merge tree"),
                    }
                }
            }
            Pending::Write(id, kind) => {
                if destination.exists(id) {
                    continue;
                }
                let object = source.read(id)?;
                ensure!(
                    object.kind() == kind,
                    "Invalid object in the planned merge tree"
                );
                ensure!(
                    destination.write(kind, object.data())? == id,
                    "Object ID changed while preparing the merge tree"
                );
            }
        }
    }
    Ok(tree)
}

fn publish_integrated_tree(
    repo: &git2::Repository,
    mut refs: git2::Transaction<'_>,
    original: &HeadState,
    commit: &git2::Commit<'_>,
    signature: &git2::Signature<'_>,
    reflog: &str,
    control: &NetworkControl,
) -> Result<usize> {
    let before = oid(original.id.as_deref().context("HEAD has no commit")?)?;
    let mut orig_ref = repo.transaction()?;
    orig_ref.lock_ref("ORIG_HEAD")?;
    orig_ref.set_target(
        "ORIG_HEAD",
        before,
        Some(signature),
        "gitbuddy: before integration",
    )?;
    refs.set_target(&original.reference, commit.id(), Some(signature), reflog)?;
    let receipt = checkout_tree_with_orig_head(
        repo,
        commit.as_object(),
        CheckoutOptions {
            publication: Some(CheckoutHead {
                reference: (original.reference != "HEAD").then(|| original.reference.clone()),
                id: commit.id(),
            }),
            ..CheckoutOptions::default()
        },
        control,
        before,
    )?;
    refs.commit()?;
    let original = orig_ref
        .commit()
        .context("Integration published HEAD, but ORIG_HEAD could not be updated");
    if super::worktree_conversion::pending(repo) {
        // The journal can finish or restore either side of this ref update.
        original?;
        acknowledge_checkout(repo, &receipt)
    } else {
        // Native checkout hydrates LFS only after publishing the ref. Even if
        // hydration fails the integration is committed and has its ORIG_HEAD.
        let hydrated = acknowledge_checkout(repo, &receipt);
        original?;
        hydrated
    }
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
    /// One-off Pull source. Branch upstream configuration is not changed.
    pub fn prepare_pull_from(&self, remote: &RemoteInfo, branch: &str) -> Result<PullPreview> {
        let repo = raw(self)?;
        clean(&repo)?;
        let head = head_state(&repo)?;
        ensure!(
            head.reference != "HEAD" && head.id.is_some(),
            "Check out a local branch before pulling"
        );
        let plan = self.prepare_fetch(remote, Some(branch))?;
        Ok(PullPreview {
            head,
            remote: Some(remote.clone()),
            upstream: plan
                .tracking
                .clone()
                .unwrap_or_else(|| plan.branch.clone().unwrap()),
            merge_ref: plan.branch.clone().unwrap(),
            fingerprint: self.fingerprint()?,
            fetch: Some(plan),
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
            fetch: None,
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
        self.integrate_target(
            oid(&preview.target)?,
            &preview.label,
            mode,
            false,
            &NetworkControl::default(),
        )
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
        let fetched = if let Some(plan) = &preview.fetch {
            self.fetch_selected(plan, control.clone())?
        } else if let Some(remote) = &preview.remote {
            super::libgit::fetch_one(&repo, &remote.name, control.clone())?;
            None
        } else {
            None
        };
        control.phase("Pull: reviewing fetched upstream…");
        control.check()?;
        clean(&repo)?;
        ensure!(
            head_state(&repo)? == preview.head,
            "HEAD changed during Fetch; upstream was fetched but no local integration was performed"
        );
        self.check_pull_configuration(preview)?;
        let target = if let Some(id) = fetched {
            oid(&id)?
        } else {
            repo.find_reference(&preview.upstream)?
                .peel_to_commit()?
                .id()
        };
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
            return self.start_rebase(&context, context.steps.clone());
        }
        self.integrate_target(
            target,
            &preview.upstream,
            MergeMode::Auto,
            strategy == PullStrategy::FastForwardOnly,
            &control,
        )
    }
    fn check_pull_configuration(&self, expected: &PullPreview) -> Result<()> {
        let repo = raw(self)?;
        let head = head_state(&repo)?;
        if let Some(plan) = &expected.fetch {
            ensure!(head == expected.head, "HEAD changed; review Pull again");
            return self.check_fetch(plan);
        }
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
    fn integrate_target(
        &self,
        target: Oid,
        name: &str,
        mode: MergeMode,
        ff_only: bool,
        control: &NetworkControl,
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
        if mode == MergeMode::Auto && analysis.is_fast_forward() {
            let signature = repo
                .signature()
                .or_else(|_| git2::Signature::now("GitBuddy", "gitbuddy@localhost"))?;
            let commit = repo.find_commit(target)?;
            let missing = publish_integrated_tree(
                &repo,
                refs,
                &original,
                &commit,
                &signature,
                &format!("Fast-forward to {name}"),
                control,
            )?;
            return Ok(with_missing_lfs(
                format!("Fast-forwarded to {name}"),
                missing,
            ));
        }

        if history_needs_encoded_checkout(&repo, Some(target))? {
            ensure!(
                mode != MergeMode::Squash,
                "Squash merge does not yet support working-tree-encoding"
            );
            encoded_merge_policy(&repo)?;
            let reviewed = self.fingerprint()?;
            let signature = author(&repo)?;
            let other = repo.find_commit(target)?;
            let tree = repo.find_tree(prepare_encoded_merge_tree(&repo, head.id(), target)?)?;
            let id = super::commit_policy::create_object(
                &repo,
                &signature,
                &signature,
                &format!("Merge branch '{name}'"),
                &tree,
                &[&head, &other],
            )?;
            // Signing may run an external tool before the checkout begins.
            // Revalidate the reviewed repository and the effective hooks.
            let fresh = raw(self)?;
            clean(&fresh)?;
            ensure!(
                self.fingerprint()? == reviewed && head_state(&fresh)? == original,
                "Repository changed while preparing the encoded merge; reopen Merge"
            );
            encoded_merge_policy(&fresh)?;
            let commit = repo.find_commit(id)?;
            let missing = publish_integrated_tree(
                &repo,
                refs,
                &original,
                &commit,
                &signature,
                "gitbuddy: merge",
                control,
            )?;
            return Ok(with_missing_lfs(
                format!("Created merge commit {id}"),
                missing,
            ));
        }

        let signature = if mode == MergeMode::Squash {
            repo.signature()
                .or_else(|_| git2::Signature::now("GitBuddy", "gitbuddy@localhost"))?
        } else {
            author(&repo)?
        };
        repo.reference("ORIG_HEAD", head.id(), true, "gitbuddy: before integration")?;
        with_native_lfs(&repo, || {
            let mut squash = if mode == MergeMode::Squash {
                let state = SquashState {
                    version: 1,
                    git_dir: repo.path().into(),
                    reference: original.reference.clone(),
                    original: head.id().to_string(),
                    target: target.to_string(),
                    message: format!("Squash merge '{name}'"),
                    applied: false,
                    prepared: None,
                };
                save(&repo, &state)?;
                Some(state)
            } else {
                None
            };
            repo.merge(
                &[&annotated],
                None,
                Some(
                    CheckoutBuilder::new()
                        .safe()
                        .allow_conflicts(true)
                        .conflict_style_merge(true),
                ),
            )?;
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
                ensure!(
                    !index.has_conflicts(),
                    "Squash has conflicts; resolve them, then commit or abort squash"
                );
                if index.write_tree()? == head.tree_id() {
                    clear_squash(&repo)?;
                    return Ok("Squash has no changes; HEAD unchanged".into());
                }
                return Ok("Squash result staged; review and commit to create a single-parent commit (HEAD unchanged)".into());
            }
            let mut index = repo.index()?;
            ensure!(
                !index.has_conflicts(),
                "Merge has conflicts; resolve or abort"
            );
            let message = super::commit_policy::prepare_merge_message(
                &repo,
                &format!("Merge branch '{name}'"),
            )?;
            let _index_lock =
                super::conflicts::IndexLock::acquire(index.path().context("Missing index")?)?;
            index.read(true)?;
            ensure!(
                !index.has_conflicts(),
                "A hook introduced conflicts; resolve before committing"
            );
            let tree = repo.find_tree(index.write_tree()?)?;
            let other = repo.find_commit(target)?;
            let id = super::commit_policy::create_object(
                &repo,
                &signature,
                &signature,
                &message,
                &tree,
                &[&head, &other],
            )?;
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
        let hook_state = fs::read(repo.path().join(SQUASH_STATE))?;
        let hook_message = if state.prepared.is_none() {
            Some(super::commit_policy::prepare_message(
                &repo, message, false,
            )?)
        } else {
            None
        };
        let message = hook_message.as_deref().unwrap_or(message);
        ensure!(
            fs::read(repo.path().join(SQUASH_STATE))? == hook_state,
            "Squash state changed while running hooks"
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
            let id = super::commit_policy::create_object(
                &repo,
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
        drop(_index_lock);
        let mut result = format!("Created single-parent squash commit {id}");
        if hook_message.is_some()
            && let Err(error) = super::commit_policy::post_commit(&repo)
        {
            result.push_str(&format!(
                "; post-commit hook failed after commit: {error:#}"
            ));
        }
        Ok(result)
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
        let original = repo.find_commit(oid(&state.original)?)?;
        guard_native_history_write(&repo, Some(original.id()))?;
        with_native_lfs(&repo, || {
            repo.checkout_tree(original.as_object(), Some(CheckoutBuilder::new().force()))?;
            if repo.state() == RepositoryState::Merge {
                repo.cleanup_state()?;
            }
            clear_squash(&repo)?;
            Ok("Squash aborted; original index and tracked files restored".into())
        })
    }
}
