use gitbuddy::git::{
    CancellationToken, ConflictOperation, ConflictResolution, MergeMode, NetworkControl, Operation,
    ProgressSink, PullStrategy, Repository, is_cancelled,
};
use std::{
    fs,
    path::PathBuf,
    sync::{Arc, Mutex},
};

fn raw(repo: &Repository) -> git2::Repository {
    git2::Repository::open(&repo.root).unwrap()
}
fn head(repo: &Repository) -> String {
    raw(repo).head().unwrap().target().unwrap().to_string()
}
fn commit(repo: &Repository, path: &str, text: &str, message: &str) -> String {
    fs::write(repo.root.join(path), text).unwrap();
    repo.execute(Operation::StageAll).unwrap();
    repo.execute(Operation::Commit(message.into())).unwrap();
    head(repo)
}
fn identity(repo: &Repository, name: &str) {
    repo.execute(Operation::SetIdentity(
        name.into(),
        format!("{name}@example.invalid"),
    ))
    .unwrap();
}
fn merge(repo: &Repository, mode: MergeMode) -> anyhow::Result<String> {
    repo.execute(Operation::MergeWith {
        context: Arc::new(repo.prepare_merge("refs/heads/topic")?),
        mode,
    })
}
fn pull(repo: &Repository, strategy: PullStrategy) -> anyhow::Result<String> {
    repo.execute(Operation::PullWith {
        context: Arc::new(repo.prepare_pull()?),
        strategy,
    })
}
fn parents(repo: &Repository) -> Vec<String> {
    raw(repo)
        .head()
        .unwrap()
        .peel_to_commit()
        .unwrap()
        .parent_ids()
        .map(|id| id.to_string())
        .collect()
}
struct MergeFixture {
    _dir: tempfile::TempDir,
    repo: Repository,
    original: String,
    target: String,
}
impl MergeFixture {
    fn new(diverged: bool, conflict: bool) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(&dir.path().join("repo")).unwrap();
        identity(&repo, "Merger");
        commit(&repo, "shared", "base\n", "base");
        repo.execute(Operation::CreateBranch("topic".into()))
            .unwrap();
        let target = commit(
            &repo,
            if conflict { "shared" } else { "topic" },
            "topic\n",
            "topic change",
        );
        repo.execute(Operation::Checkout("main".into())).unwrap();
        if diverged {
            commit(
                &repo,
                if conflict { "shared" } else { "local" },
                "local\n",
                "local change",
            );
        }
        let original = head(&repo);
        Self {
            _dir: dir,
            repo,
            original,
            target,
        }
    }
}
#[test]
fn auto_fast_forwards_but_no_ff_creates_two_parents() {
    for mode in [MergeMode::Auto, MergeMode::NoFastForward] {
        let f = MergeFixture::new(false, false);
        merge(&f.repo, mode).unwrap();
        if mode == MergeMode::Auto {
            assert_eq!(head(&f.repo), f.target);
        } else {
            assert_eq!(parents(&f.repo), [f.original.clone(), f.target.clone()]);
            assert_ne!(head(&f.repo), f.target);
        }
        assert!(f.repo.snapshot(30).unwrap().files.is_empty());
        assert_eq!(raw(&f.repo).state(), git2::RepositoryState::Clean);
    }
}
#[test]
fn diverged_merge_policies_preserve_both_parent_trees() {
    for mode in [MergeMode::Auto, MergeMode::NoFastForward] {
        let f = MergeFixture::new(true, false);
        merge(&f.repo, mode).unwrap();
        assert_eq!(parents(&f.repo), [f.original, f.target]);
        assert_eq!(
            fs::read_to_string(f.repo.root.join("topic")).unwrap(),
            "topic\n"
        );
        assert_eq!(
            fs::read_to_string(f.repo.root.join("local")).unwrap(),
            "local\n"
        );
    }
}
#[test]
fn squash_stages_without_moving_head_then_creates_a_single_parent() {
    for diverged in [false, true] {
        let f = MergeFixture::new(diverged, false);
        merge(&f.repo, MergeMode::Squash).unwrap();
        assert_eq!(head(&f.repo), f.original);
        let snapshot = f.repo.snapshot(30).unwrap();
        assert_eq!(snapshot.conflict_operation, ConflictOperation::Squash);
        assert!(snapshot.can_commit());
        assert!(!snapshot.squash_pending);
        assert!(snapshot.squash_message.unwrap().contains("Squash merge"));
        assert!(!raw(&f.repo).path().join("MERGE_HEAD").exists());
        assert_eq!(raw(&f.repo).state(), git2::RepositoryState::Clean);
        f.repo
            .execute(Operation::Commit("squashed result".into()))
            .unwrap();
        assert_eq!(parents(&f.repo), [f.original]);
        assert_ne!(head(&f.repo), f.target);
        assert_eq!(
            f.repo.snapshot(30).unwrap().conflict_operation,
            ConflictOperation::None
        );
        assert!(!raw(&f.repo).path().join("gitbuddy-squash.json").exists());
        assert!(!raw(&f.repo).path().join("SQUASH_MSG").exists());
    }
}
#[test]
fn no_ff_conflict_resolves_to_two_parent_commit_after_reopen() {
    let f = MergeFixture::new(true, true);
    assert!(merge(&f.repo, MergeMode::NoFastForward).is_err());
    let repo = Repository::open(&f.repo.root).unwrap();
    assert_eq!(
        repo.conflict_session().unwrap().operation,
        ConflictOperation::Merge
    );
    let context = Arc::new(
        repo.conflict_context(std::path::Path::new("shared"))
            .unwrap(),
    );
    repo.execute(Operation::ResolveConflict {
        context,
        resolution: ConflictResolution::Theirs,
    })
    .unwrap();
    repo.execute(Operation::ContinueConflict(Arc::new(
        repo.conflict_session().unwrap(),
    )))
    .unwrap();
    assert_eq!(parents(&repo), [f.original, f.target]);
    assert_eq!(
        fs::read_to_string(repo.root.join("shared")).unwrap(),
        "topic\n"
    );
}
#[test]
fn squash_conflict_reopens_and_continue_creates_only_one_parent() {
    let f = MergeFixture::new(true, true);
    assert!(merge(&f.repo, MergeMode::Squash).is_err());
    let repo = Repository::open(&f.repo.root).unwrap();
    assert_eq!(
        repo.conflict_session().unwrap().operation,
        ConflictOperation::Squash
    );
    assert_eq!(head(&repo), f.original);
    let context = Arc::new(
        repo.conflict_context(std::path::Path::new("shared"))
            .unwrap(),
    );
    repo.execute(Operation::ResolveConflict {
        context,
        resolution: ConflictResolution::Theirs,
    })
    .unwrap();
    repo.execute(Operation::ContinueConflict(Arc::new(
        repo.conflict_session().unwrap(),
    )))
    .unwrap();
    assert_eq!(parents(&repo), [f.original]);
    assert_eq!(
        repo.snapshot(30).unwrap().conflict_operation,
        ConflictOperation::None
    );
}
#[test]
fn squash_abort_restores_original_index_files_and_preserves_untracked() {
    for conflict in [false, true] {
        let f = MergeFixture::new(true, conflict);
        let index = raw(&f.repo).index().unwrap().write_tree().unwrap();
        let _ = merge(&f.repo, MergeMode::Squash);
        fs::write(f.repo.root.join("untracked"), "keep").unwrap();
        let reopened = Repository::open(&f.repo.root).unwrap();
        reopened.execute(Operation::AbortMerge).unwrap();
        assert_eq!(head(&reopened), f.original);
        assert_eq!(raw(&reopened).index().unwrap().write_tree().unwrap(), index);
        assert_eq!(
            fs::read_to_string(reopened.root.join("untracked")).unwrap(),
            "keep"
        );
        assert_eq!(
            reopened.snapshot(30).unwrap().conflict_operation,
            ConflictOperation::None
        );
        assert!(!raw(&reopened).path().join("MERGE_HEAD").exists());
    }
}
#[test]
fn squash_blocks_unrelated_history_and_stale_continue_requests() {
    let f = MergeFixture::new(true, false);
    merge(&f.repo, MergeMode::Squash).unwrap();
    assert!(f.repo.execute(Operation::Checkout("topic".into())).is_err());
    assert!(f.repo.prepare_branch_rename("main").is_err());
    assert!(f.repo.prepare_pull().is_err());
    assert!(f.repo.prepare_merge("topic").is_err());
    let session = Arc::new(f.repo.conflict_session().unwrap());
    fs::write(f.repo.root.join("topic"), "changed after review").unwrap();
    f.repo.execute(Operation::StageAll).unwrap();
    assert!(
        f.repo
            .execute(Operation::ContinueConflict(session))
            .is_err()
    );
    assert_eq!(head(&f.repo), f.original);
}
#[test]
fn interrupted_squash_application_never_commits_an_unconfirmed_index() {
    let f = MergeFixture::new(false, false);
    merge(&f.repo, MergeMode::Squash).unwrap();
    let path = raw(&f.repo).path().join("gitbuddy-squash.json");
    let mut state: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    state["applied"] = false.into();
    fs::write(&path, serde_json::to_vec(&state).unwrap()).unwrap();
    let repo = Repository::open(&f.repo.root).unwrap();
    assert!(repo.snapshot(30).unwrap().squash_pending);
    assert!(!repo.snapshot(30).unwrap().can_commit());
    assert!(
        repo.execute(Operation::Commit("must not commit".into()))
            .is_err()
    );
    assert_eq!(head(&repo), f.original);
    repo.execute(Operation::AbortMerge).unwrap();
    assert!(!repo.root.join("topic").exists());
}
#[test]
fn prepared_squash_recovers_once_before_or_after_reference_publication() {
    for publish in [false, true] {
        let f = MergeFixture::new(false, false);
        merge(&f.repo, MergeMode::Squash).unwrap();
        let r = raw(&f.repo);
        let tree = r
            .find_tree(r.index().unwrap().write_tree().unwrap())
            .unwrap();
        let parent = r.find_commit(f.original.parse().unwrap()).unwrap();
        let signature = r.signature().unwrap();
        let prepared = r
            .commit(
                None,
                &signature,
                &signature,
                "prepared squash",
                &tree,
                &[&parent],
            )
            .unwrap();
        let path = r.path().join("gitbuddy-squash.json");
        let mut state: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        state["prepared"] = prepared.to_string().into();
        fs::write(path, serde_json::to_vec(&state).unwrap()).unwrap();
        if publish {
            r.reference("refs/heads/main", prepared, true, "simulate publication")
                .unwrap();
        }
        let reopened = Repository::open(&f.repo.root).unwrap();
        reopened
            .execute(Operation::Commit(
                "retry must not create another commit".into(),
            ))
            .unwrap();
        assert_eq!(head(&reopened), prepared.to_string());
        assert_eq!(parents(&reopened), [f.original]);
        assert!(!r.path().join("gitbuddy-squash.json").exists());
    }
}
#[test]
fn merge_preview_rejects_stale_head_target_worktree_config_and_cross_repository() {
    for mutation in 0..4 {
        let f = MergeFixture::new(false, false);
        let context = Arc::new(f.repo.prepare_merge("topic").unwrap());
        match mutation {
            0 => {
                fs::write(f.repo.root.join("shared"), "dirty").unwrap();
            }
            1 => {
                raw(&f.repo)
                    .reference(
                        "refs/heads/topic",
                        f.original.parse().unwrap(),
                        true,
                        "moved",
                    )
                    .unwrap();
            }
            2 => {
                raw(&f.repo)
                    .config()
                    .unwrap()
                    .set_str("merge.ff", "false")
                    .unwrap();
            }
            _ => {
                let other = MergeFixture::new(false, false);
                assert!(
                    other
                        .repo
                        .execute(Operation::MergeWith {
                            context,
                            mode: MergeMode::Auto
                        })
                        .is_err()
                );
                continue;
            }
        }
        assert!(
            f.repo
                .execute(Operation::MergeWith {
                    context,
                    mode: MergeMode::NoFastForward
                })
                .is_err()
        );
        assert_eq!(head(&f.repo), f.original);
        assert!(!raw(&f.repo).path().join("MERGE_HEAD").exists());
    }
}
#[test]
fn merge_locks_and_dirty_index_do_not_start_an_operation() {
    for lock in ["HEAD.lock", "refs/heads/main.lock", "index.lock"] {
        let f = MergeFixture::new(false, false);
        let context = Arc::new(f.repo.prepare_merge("topic").unwrap());
        let path = raw(&f.repo).path().join(lock);
        fs::write(&path, "other process").unwrap();
        assert!(
            f.repo
                .execute(Operation::MergeWith {
                    context,
                    mode: MergeMode::NoFastForward
                })
                .is_err()
        );
        assert_eq!(head(&f.repo), f.original);
        assert_eq!(fs::read_to_string(path).unwrap(), "other process");
        assert_eq!(
            fs::read_to_string(f.repo.root.join("shared")).unwrap(),
            "base\n"
        );
    }
    let f = MergeFixture::new(false, false);
    commit(&f.repo, "unrelated", "local", "new local commit");
    fs::write(f.repo.root.join("shared"), "staged").unwrap();
    f.repo.execute(Operation::StageAll).unwrap();
    assert!(f.repo.prepare_merge("topic").is_err());
}

struct PullFixture {
    _dir: tempfile::TempDir,
    source: Repository,
    local: Repository,
    _remote: PathBuf,
}
impl PullFixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let source = Repository::init(&dir.path().join("source")).unwrap();
        identity(&source, "RemoteAuthor");
        commit(&source, "shared", "base\n", "base");
        let remote = dir.path().join("server.git");
        git2::Repository::init_bare(&remote)
            .unwrap()
            .set_head("refs/heads/main")
            .unwrap();
        source
            .execute(Operation::AddRemote(
                "origin".into(),
                remote.to_str().unwrap().into(),
            ))
            .unwrap();
        source.execute(Operation::Push).unwrap();
        let local =
            Repository::clone_repo(remote.to_str().unwrap(), &dir.path().join("local")).unwrap();
        identity(&local, "LocalAuthor");
        Self {
            _dir: dir,
            source,
            local,
            _remote: remote,
        }
    }
    fn advance(&self, conflict: bool) -> String {
        let id = commit(
            &self.source,
            if conflict { "shared" } else { "remote" },
            "remote\n",
            "remote change",
        );
        self.source.execute(Operation::Push).unwrap();
        id
    }
}
#[test]
fn all_pull_strategies_fast_forward_when_no_local_commits_exist() {
    for strategy in [
        PullStrategy::FastForwardOnly,
        PullStrategy::Merge,
        PullStrategy::Rebase,
    ] {
        let f = PullFixture::new();
        let remote = f.advance(false);
        pull(&f.local, strategy).unwrap();
        assert_eq!(head(&f.local), remote);
        assert!(f.local.snapshot(30).unwrap().files.is_empty());
        assert!(f.local.rebase_status().unwrap().is_none());
    }
}
#[test]
fn ff_only_refuses_divergence_while_merge_creates_two_parents() {
    for strategy in [PullStrategy::FastForwardOnly, PullStrategy::Merge] {
        let f = PullFixture::new();
        let local = commit(&f.local, "local", "local\n", "local change");
        let remote = f.advance(false);
        let result = pull(&f.local, strategy);
        if strategy == PullStrategy::FastForwardOnly {
            assert!(result.is_err());
            assert_eq!(head(&f.local), local);
        } else {
            result.unwrap();
            assert_eq!(parents(&f.local), [local, remote]);
        }
    }
}
#[test]
fn pull_rebase_replays_local_commits_and_preserves_original_author() {
    let f = PullFixture::new();
    let old = commit(&f.local, "local", "local\n", "local change");
    let remote = f.advance(false);
    pull(&f.local, PullStrategy::Rebase).unwrap();
    let r = raw(&f.local);
    let new = r.head().unwrap().peel_to_commit().unwrap();
    assert_ne!(new.id().to_string(), old);
    assert_eq!(new.parent_id(0).unwrap().to_string(), remote);
    assert_eq!(new.author().name().unwrap(), "LocalAuthor");
    assert_eq!(new.message().unwrap(), "local change");
    assert_eq!(r.head().unwrap().name().unwrap(), "refs/heads/main");
    assert!(f.local.rebase_status().unwrap().is_none());
    assert!(f.local.snapshot(30).unwrap().files.is_empty());
}
#[test]
fn pull_merge_conflict_continues_with_two_parents() {
    let f = PullFixture::new();
    let local = commit(&f.local, "shared", "local\n", "local conflict");
    let remote = f.advance(true);
    assert!(pull(&f.local, PullStrategy::Merge).is_err());
    let repo = Repository::open(&f.local.root).unwrap();
    let context = Arc::new(
        repo.conflict_context(std::path::Path::new("shared"))
            .unwrap(),
    );
    repo.execute(Operation::ResolveConflict {
        context,
        resolution: ConflictResolution::Theirs,
    })
    .unwrap();
    repo.execute(Operation::ContinueConflict(Arc::new(
        repo.conflict_session().unwrap(),
    )))
    .unwrap();
    assert_eq!(parents(&repo), [local, remote]);
}
#[test]
fn pull_rebase_conflict_survives_reopen_then_continues_or_aborts() {
    for abort in [false, true] {
        let f = PullFixture::new();
        let local = commit(&f.local, "shared", "local\n", "local conflict");
        let remote = f.advance(true);
        assert!(pull(&f.local, PullStrategy::Rebase).is_err());
        let repo = Repository::open(&f.local.root).unwrap();
        assert!(repo.rebase_status().unwrap().is_some());
        if abort {
            repo.execute(Operation::AbortRebase).unwrap();
            assert_eq!(head(&repo), local);
            assert_eq!(
                fs::read_to_string(repo.root.join("shared")).unwrap(),
                "local\n"
            );
        } else {
            let context = Arc::new(
                repo.conflict_context(std::path::Path::new("shared"))
                    .unwrap(),
            );
            repo.execute(Operation::ResolveConflict {
                context,
                resolution: ConflictResolution::Theirs,
            })
            .unwrap();
            repo.execute(Operation::ContinueConflict(Arc::new(
                repo.conflict_session().unwrap(),
            )))
            .unwrap();
            assert_eq!(parents(&repo), [remote]);
        }
        assert!(repo.rebase_status().unwrap().is_none());
        assert_eq!(
            raw(&repo).head().unwrap().name().unwrap(),
            "refs/heads/main"
        );
    }
}
#[test]
fn pull_up_to_date_local_history_is_not_rewritten_by_any_strategy() {
    for strategy in [
        PullStrategy::FastForwardOnly,
        PullStrategy::Merge,
        PullStrategy::Rebase,
    ] {
        let f = PullFixture::new();
        let local = commit(&f.local, "local", "local\n", "local change");
        pull(&f.local, strategy).unwrap();
        assert_eq!(head(&f.local), local);
    }
}
#[test]
fn pull_rejects_stale_previews_dirty_work_and_missing_upstream() {
    let f = PullFixture::new();
    let preview = Arc::new(f.local.prepare_pull().unwrap());
    fs::write(f.local.root.join("shared"), "dirty").unwrap();
    assert!(
        f.local
            .execute(Operation::PullWith {
                context: preview,
                strategy: PullStrategy::Merge
            })
            .is_err()
    );
    assert!(f.local.prepare_pull().is_err());
    fs::write(f.local.root.join("shared"), "base\n").unwrap();
    raw(&f.local)
        .config()
        .unwrap()
        .remove("branch.main.merge")
        .unwrap();
    assert!(f.local.prepare_pull().is_err());
}
#[test]
fn pull_revalidates_config_and_worktree_after_fetch_before_local_writes() {
    for change_config in [false, true] {
        let f = PullFixture::new();
        let before = head(&f.local);
        f.advance(false);
        let root = f.local.root.clone();
        let progress: ProgressSink = Arc::new(Mutex::new(move |text: &str| {
            if text.starts_with("Pull: reviewing") {
                if change_config {
                    git2::Repository::open(&root)
                        .unwrap()
                        .config()
                        .unwrap()
                        .set_str("branch.main.merge", "refs/heads/different")
                        .unwrap();
                } else {
                    fs::write(root.join("shared"), "external edit").unwrap();
                }
            }
        }));
        let context = Arc::new(f.local.prepare_pull().unwrap());
        assert!(
            f.local
                .execute_with_control(
                    Operation::PullWith {
                        context,
                        strategy: PullStrategy::Merge
                    },
                    NetworkControl::new(Some(progress), CancellationToken::default())
                )
                .is_err()
        );
        assert_eq!(head(&f.local), before);
        assert!(!f.local.root.join("remote").exists());
        if !change_config {
            assert_eq!(
                fs::read_to_string(f.local.root.join("shared")).unwrap(),
                "external edit"
            );
        }
    }
}
#[test]
fn pull_cancels_after_fetch_and_seals_cancellation_before_each_strategy_applies() {
    for strategy in [
        PullStrategy::FastForwardOnly,
        PullStrategy::Merge,
        PullStrategy::Rebase,
    ] {
        for cancel_before in [false, true] {
            let f = PullFixture::new();
            let before = head(&f.local);
            let remote = f.advance(false);
            let token = CancellationToken::default();
            let cancel = token.clone();
            let progress: ProgressSink = Arc::new(Mutex::new(move |text: &str| {
                if cancel_before && text.starts_with("Pull: reviewing") {
                    assert!(cancel.cancel());
                }
                if text.starts_with("Pull: applying") {
                    assert!(cancel.is_finishing());
                    assert!(!cancel.cancel());
                }
            }));
            let context = Arc::new(f.local.prepare_pull().unwrap());
            let result = f.local.execute_with_control(
                Operation::PullWith { context, strategy },
                NetworkControl::new(Some(progress), token),
            );
            if cancel_before {
                assert!(is_cancelled(&result.unwrap_err()));
                assert_eq!(head(&f.local), before);
            } else {
                result.unwrap();
                assert_eq!(head(&f.local), remote);
            }
        }
    }
}
#[test]
fn local_upstream_and_custom_fetch_mapping_use_the_configured_target() {
    let f = MergeFixture::new(false, false);
    f.repo
        .execute(Operation::SetUpstream {
            branch: "main".into(),
            upstream: Some("refs/heads/topic".into()),
        })
        .unwrap();
    pull(&f.repo, PullStrategy::Merge).unwrap();
    assert_eq!(head(&f.repo), f.target);
    let f = PullFixture::new();
    let r = raw(&f.local);
    r.config()
        .unwrap()
        .set_str("remote.origin.fetch", "+refs/heads/*:refs/remotes/custom/*")
        .unwrap();
    let target = f.advance(false);
    pull(&f.local, PullStrategy::Rebase).unwrap();
    assert_eq!(head(&f.local), target);
}
#[test]
fn integration_accepts_implicit_boolean_config_without_panicking() {
    use std::io::Write;
    let f = MergeFixture::new(false, false);
    fs::OpenOptions::new()
        .append(true)
        .open(raw(&f.repo).path().join("config"))
        .unwrap()
        .write_all(b"\n[custom]\n implicit\n")
        .unwrap();
    merge(&f.repo, MergeMode::NoFastForward).unwrap();
    assert_eq!(parents(&f.repo), [f.original, f.target]);
}

#[test]
fn squash_with_no_net_changes_does_not_leave_a_pending_operation() {
    let f = MergeFixture::new(true, false);
    let original = commit(&f.repo, "topic", "topic\n", "independently applied topic");
    let result = merge(&f.repo, MergeMode::Squash).unwrap();
    assert!(result.contains("no changes"));
    assert_eq!(head(&f.repo), original);
    assert_eq!(
        f.repo.snapshot(30).unwrap().conflict_operation,
        ConflictOperation::None
    );
    assert!(f.repo.prepare_merge("topic").is_ok());
    assert!(!raw(&f.repo).path().join("SQUASH_MSG").exists());
}

#[test]
fn pull_rebase_refuses_local_merge_history_without_detaching_or_rewriting() {
    let f = PullFixture::new();
    f.local
        .execute(Operation::CreateBranch("topic".into()))
        .unwrap();
    commit(&f.local, "topic", "topic\n", "topic change");
    f.local.execute(Operation::Checkout("main".into())).unwrap();
    merge(&f.local, MergeMode::NoFastForward).unwrap();
    let original = head(&f.local);
    f.advance(false);
    let error = pull(&f.local, PullStrategy::Rebase).unwrap_err();
    assert!(format!("{error:#}").contains("merge commit"));
    assert_eq!(head(&f.local), original);
    assert!(!raw(&f.local).head_detached().unwrap());
    assert!(f.local.rebase_status().unwrap().is_none());
    assert!(f.local.snapshot(30).unwrap().files.is_empty());
    pull(&f.local, PullStrategy::Merge).unwrap();
    assert_eq!(parents(&f.local).len(), 2);
}

#[test]
fn squash_state_changes_invalidate_conflict_reviews_and_corruption_is_preserved() {
    let f = MergeFixture::new(false, false);
    merge(&f.repo, MergeMode::Squash).unwrap();
    let before = f.repo.fingerprint().unwrap();
    let reviewed = Arc::new(f.repo.conflict_session().unwrap());
    let path = raw(&f.repo).path().join("gitbuddy-squash.json");
    let bytes = fs::read(&path).unwrap();
    let mut state: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    state["message"] = "Changed recovery message".into();
    fs::write(&path, serde_json::to_vec(&state).unwrap()).unwrap();
    assert_ne!(f.repo.fingerprint().unwrap(), before);
    assert!(
        f.repo
            .execute(Operation::ContinueConflict(reviewed))
            .is_err()
    );
    assert_eq!(head(&f.repo), f.original);
    fs::write(&path, "broken journal").unwrap();
    assert!(
        f.repo
            .execute(Operation::Commit("must refuse".into()))
            .is_err()
    );
    assert!(f.repo.execute(Operation::AbortMerge).is_err());
    assert!(f.repo.snapshot(30).is_err());
    assert_eq!(fs::read_to_string(&path).unwrap(), "broken journal");
    fs::write(&path, bytes).unwrap();
    f.repo.execute(Operation::AbortMerge).unwrap();
}

#[test]
fn merge_squash_and_pull_policies_keep_lfs_pointers_and_materialized_content() {
    for case in 0..6 {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(&dir.path().join("repo")).unwrap();
        identity(&repo, "LfsAuthor");
        repo.execute(Operation::LfsTrack {
            pattern: "*.bin".into(),
            track: true,
        })
        .unwrap();
        commit(&repo, "data.bin", "base binary body", "base LFS");
        repo.execute(Operation::CreateBranch("topic".into()))
            .unwrap();
        commit(&repo, "data.bin", "topic binary body", "topic LFS");
        repo.execute(Operation::Checkout("main".into())).unwrap();
        if case != 3 {
            commit(&repo, "local", "local", "local change");
        }
        let original = head(&repo);
        if case < 3 {
            merge(
                &repo,
                if case == 0 {
                    MergeMode::NoFastForward
                } else {
                    MergeMode::Squash
                },
            )
            .unwrap();
        } else {
            repo.execute(Operation::SetUpstream {
                branch: "main".into(),
                upstream: Some("refs/heads/topic".into()),
            })
            .unwrap();
            pull(
                &repo,
                [
                    PullStrategy::FastForwardOnly,
                    PullStrategy::Merge,
                    PullStrategy::Rebase,
                ][case - 3],
            )
            .unwrap();
        }
        assert_eq!(
            fs::read(repo.root.join("data.bin")).unwrap(),
            b"topic binary body"
        );
        if case == 2 {
            repo.execute(Operation::AbortMerge).unwrap();
            assert_eq!(head(&repo), original);
            assert_eq!(
                fs::read(repo.root.join("data.bin")).unwrap(),
                b"base binary body"
            );
        } else {
            if case == 1 {
                repo.execute(Operation::Commit("squashed LFS".into()))
                    .unwrap();
            }
            let r = raw(&repo);
            let tree = r.head().unwrap().peel_to_commit().unwrap().tree().unwrap();
            let blob = r
                .find_blob(tree.get_name("data.bin").unwrap().id())
                .unwrap();
            assert!(
                blob.content()
                    .starts_with(b"version https://git-lfs.github.com/spec/v1\n")
            );
        }
        assert!(repo.snapshot(30).unwrap().files.is_empty());
    }
}

#[test]
fn lfs_conflicts_can_be_resolved_or_aborted_for_merge_and_squash() {
    for mode in [MergeMode::NoFastForward, MergeMode::Squash] {
        for abort in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let repo = Repository::init(&dir.path().join("repo")).unwrap();
            identity(&repo, "LfsAuthor");
            repo.execute(Operation::LfsTrack {
                pattern: "*.bin".into(),
                track: true,
            })
            .unwrap();
            commit(&repo, "data.bin", "base binary body", "base LFS");
            repo.execute(Operation::CreateBranch("topic".into()))
                .unwrap();
            commit(&repo, "data.bin", "topic binary body", "topic LFS");
            repo.execute(Operation::Checkout("main".into())).unwrap();
            let original = commit(&repo, "data.bin", "local binary body", "local LFS");
            assert!(merge(&repo, mode).is_err());
            assert!(raw(&repo).index().unwrap().has_conflicts());
            if abort {
                repo.execute(Operation::AbortMerge).unwrap();
                assert_eq!(head(&repo), original);
                assert_eq!(
                    fs::read(repo.root.join("data.bin")).unwrap(),
                    b"local binary body"
                );
            } else {
                repo.execute(Operation::ResolveConflict {
                    context: Arc::new(
                        repo.conflict_context(std::path::Path::new("data.bin"))
                            .unwrap(),
                    ),
                    resolution: ConflictResolution::Theirs,
                })
                .unwrap();
                repo.execute(Operation::ContinueConflict(Arc::new(
                    repo.conflict_session().unwrap(),
                )))
                .unwrap();
                assert_eq!(
                    parents(&repo).len(),
                    if mode == MergeMode::Squash { 1 } else { 2 }
                );
                assert_eq!(
                    fs::read(repo.root.join("data.bin")).unwrap(),
                    b"topic binary body"
                );
            }
            assert!(repo.snapshot(30).unwrap().files.is_empty());
        }
    }
}
