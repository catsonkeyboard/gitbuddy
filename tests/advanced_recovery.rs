use gitbuddy::git::{NetworkControl, Operation, RebaseAction, Repository, autosquash};
use std::{fs, sync::Arc};
fn init(path: &std::path::Path) -> Repository {
    let repo = Repository::init(path).unwrap();
    repo.execute(Operation::SetIdentity(
        "Tester".into(),
        "test@example.invalid".into(),
    ))
    .unwrap();
    repo
}
fn write(repo: &Repository, path: &str, text: &str) {
    fs::write(repo.root.join(path), text).unwrap();
}
fn commit(repo: &Repository, message: &str) -> String {
    repo.execute(Operation::StageAll).unwrap();
    repo.execute(Operation::Commit(message.into())).unwrap();
    repo.snapshot(30).unwrap().head_id.unwrap()
}
fn head(repo: &Repository) -> String {
    repo.snapshot(30).unwrap().head_id.unwrap()
}
fn raw(repo: &Repository) -> git2::Repository {
    git2::Repository::open(&repo.root).unwrap()
}
fn conflict() -> (tempfile::TempDir, Repository, String, String) {
    let dir = tempfile::tempdir().unwrap();
    let repo = init(&dir.path().join("repo"));
    write(&repo, "file", "base\n");
    commit(&repo, "base");
    repo.execute(Operation::CreateBranch("feature".into()))
        .unwrap();
    write(&repo, "file", "feature\n");
    let original = commit(&repo, "feature");
    repo.execute(Operation::Checkout("main".into())).unwrap();
    write(&repo, "file", "main\n");
    let onto = commit(&repo, "main");
    repo.execute(Operation::Checkout("feature".into())).unwrap();
    let preview = repo.rebase_preview("main").unwrap();
    assert!(
        repo.execute(Operation::Rebase {
            steps: preview.steps.clone(),
            context: Arc::new(preview)
        })
        .is_err()
    );
    (dir, repo, original, onto)
}
#[test]
fn onto_replays_only_the_explicit_range_boundary() {
    let dir = tempfile::tempdir().unwrap();
    let repo = init(dir.path());
    write(&repo, "base", "base");
    let base = commit(&repo, "base");
    write(&repo, "one", "one");
    let boundary = commit(&repo, "one");
    write(&repo, "two", "two");
    commit(&repo, "two");
    let preview = repo.rebase_preview_onto(&boundary, Some(&base)).unwrap();
    assert_eq!(preview.steps.len(), 1);
    assert_eq!(preview.steps[0].message.trim(), "two");
    repo.execute(Operation::Rebase {
        steps: preview.steps.clone(),
        context: Arc::new(preview),
    })
    .unwrap();
    assert!(!repo.root.join("one").exists());
    assert!(repo.root.join("two").exists());
}
#[test]
fn autosquash_reorders_fixups_and_preserves_final_content() {
    let dir = tempfile::tempdir().unwrap();
    let repo = init(dir.path());
    write(&repo, "base", "base");
    let base = commit(&repo, "base");
    write(&repo, "a", "first");
    commit(&repo, "First change");
    write(&repo, "b", "other");
    commit(&repo, "Other change");
    write(&repo, "a", "fixed");
    commit(&repo, "fixup! First change");
    let preview = repo.rebase_preview(&base).unwrap();
    let steps = autosquash(&preview.steps).unwrap();
    assert_eq!(steps[1].action, RebaseAction::Fixup);
    repo.execute(Operation::Rebase {
        context: Arc::new(preview),
        steps,
    })
    .unwrap();
    assert_eq!(fs::read_to_string(repo.root.join("a")).unwrap(), "fixed");
    assert_eq!(repo.snapshot(20).unwrap().commits.len(), 3);
}
#[test]
fn autosquash_missing_or_ambiguous_targets_never_apply_a_partial_plan() {
    let dir = tempfile::tempdir().unwrap();
    let repo = init(dir.path());
    write(&repo, "base", "base");
    let base = commit(&repo, "base");
    for (i, message) in ["same", "same", "fixup! same"].iter().enumerate() {
        write(&repo, &format!("f{i}"), message);
        commit(&repo, message);
    }
    let before = head(&repo);
    let mut steps = repo.rebase_preview(&base).unwrap().steps;
    assert!(autosquash(&steps).is_err());
    steps[2].message = "fixup! missing".into();
    assert!(autosquash(&steps).is_err());
    assert_eq!(head(&repo), before);
}
#[test]
fn break_survives_reopen_then_continues_without_duplicate_commit() {
    let dir = tempfile::tempdir().unwrap();
    let repo = init(dir.path());
    write(&repo, "base", "base");
    let base = commit(&repo, "base");
    write(&repo, "one", "one");
    commit(&repo, "one");
    write(&repo, "two", "two");
    commit(&repo, "two");
    let preview = repo.rebase_preview(&base).unwrap();
    let mut steps = preview.steps.clone();
    steps[0].action = RebaseAction::Break;
    repo.execute(Operation::Rebase {
        context: Arc::new(preview),
        steps,
    })
    .unwrap();
    assert_eq!(repo.rebase_status().unwrap().unwrap().completed, 1);
    assert!(!repo.root.join("two").exists());
    let repo = Repository::open(&repo.root).unwrap();
    assert!(repo.rebase_status().unwrap().unwrap().paused);
    write(&repo, "one", "dirty");
    assert!(repo.execute(Operation::ContinueRebase).is_err());
    repo.execute(Operation::Discard("one".into())).unwrap();
    repo.execute(Operation::ContinueRebase).unwrap();
    assert!(repo.rebase_status().unwrap().is_none());
    assert_eq!(repo.snapshot(20).unwrap().commits.len(), 3);
}
#[test]
fn retry_unacknowledged_apply_preserves_resolution_index_and_journal() {
    let (_dir, repo, original, _onto) = conflict();
    let r = raw(&repo);
    write(&repo, "file", "my resolution\n");
    repo.execute(Operation::Stage(vec!["file".into()])).unwrap();
    let index = fs::read(r.path().join("index")).unwrap();
    let mut journal: serde_json::Value =
        serde_json::from_slice(&fs::read(r.path().join("gitbuddy-rebase.json")).unwrap()).unwrap();
    journal["applied"] = false.into();
    fs::write(
        r.path().join("gitbuddy-rebase.json"),
        serde_json::to_vec(&journal).unwrap(),
    )
    .unwrap();
    assert!(repo.rebase_status().unwrap().unwrap().interrupted);
    assert!(repo.execute(Operation::ContinueRebase).is_err());
    let error = repo
        .execute(Operation::RecoverRebaseStep {
            expected: original.clone(),
            skip: false,
        })
        .unwrap_err();
    assert!(format!("{error:#}").contains("Recovery files saved"));
    let backup = fs::read_dir(r.path().join("gitbuddy-rebase-recovery"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    assert_eq!(
        fs::read_to_string(backup.join("worktree/file")).unwrap(),
        "my resolution\n"
    );
    assert_eq!(fs::read(backup.join("index")).unwrap(), index);
    assert!(raw(&repo).index().unwrap().has_conflicts());
    assert!(!repo.rebase_status().unwrap().unwrap().interrupted);
    repo.execute(Operation::AbortRebase).unwrap();
    assert_eq!(head(&repo), original);
}
#[test]
fn skip_conflicting_step_keeps_original_reference_and_onto() {
    let (_dir, repo, original, onto) = conflict();
    let backup = repo.rebase_status().unwrap().unwrap().backup;
    repo.execute(Operation::RecoverRebaseStep {
        expected: original.clone(),
        skip: true,
    })
    .unwrap();
    assert_eq!(head(&repo), onto);
    assert_eq!(
        raw(&repo)
            .find_reference(&backup)
            .unwrap()
            .target()
            .unwrap()
            .to_string(),
        original
    );
    assert_eq!(
        fs::read_to_string(repo.root.join("file")).unwrap(),
        "main\n"
    );
}
#[test]
fn recovery_refuses_stale_step_index_lock_and_external_branch_change() {
    let (_dir, repo, original, _) = conflict();
    let r = raw(&repo);
    let before = fs::read(r.path().join("index")).unwrap();
    assert!(
        repo.execute(Operation::RecoverRebaseStep {
            expected: "1".repeat(40),
            skip: true
        })
        .is_err()
    );
    fs::write(r.path().join("index.lock"), "external").unwrap();
    assert!(
        repo.execute(Operation::RecoverRebaseStep {
            expected: original.clone(),
            skip: false
        })
        .is_err()
    );
    assert_eq!(fs::read(r.path().join("index")).unwrap(), before);
    fs::remove_file(r.path().join("index.lock")).unwrap();
    r.find_reference("refs/heads/feature")
        .unwrap()
        .set_target(r.head().unwrap().target().unwrap(), "external")
        .unwrap();
    assert!(
        repo.execute(Operation::RecoverRebaseStep {
            expected: original,
            skip: true
        })
        .is_err()
    );
    assert!(!r.path().join("gitbuddy-rebase-recovery").exists());
}

fn modules_fixture() -> (tempfile::TempDir, Repository) {
    let dir = tempfile::tempdir().unwrap();
    let leaf = init(&dir.path().join("leaf"));
    write(&leaf, "file", "leaf");
    commit(&leaf, "leaf");
    let middle = init(&dir.path().join("middle"));
    write(&middle, "file", "middle");
    commit(&middle, "middle");
    middle
        .execute(Operation::AddSubmodule {
            url: leaf.root.display().to_string(),
            path: "nested".into(),
        })
        .unwrap();
    commit(&middle, "nested");
    let repo = init(&dir.path().join("repo"));
    write(&repo, "file", "root");
    commit(&repo, "root");
    repo.execute(Operation::AddSubmodule {
        url: middle.root.display().to_string(),
        path: "child".into(),
    })
    .unwrap();
    commit(&repo, "child");
    repo.execute(Operation::UpdateSubmodules {
        names: vec![],
        recursive: true,
    })
    .unwrap();
    (dir, repo)
}
#[test]
fn recursive_list_and_sync_and_nested_stage_use_the_actual_parent() {
    let (_dir, repo) = modules_fixture();
    let modules = repo.submodules_recursive().unwrap();
    assert_eq!(modules.len(), 2);
    let nested = modules.iter().find(|m| m.depth == 1).unwrap();
    assert_eq!(nested.path, std::path::PathBuf::from("child/nested"));
    let leaf = Repository::open(repo.root.join("child/nested")).unwrap();
    leaf.execute(Operation::SetIdentity(
        "Tester".into(),
        "test@example.invalid".into(),
    ))
    .unwrap();
    write(&leaf, "new", "new");
    let id = commit(&leaf, "nested edit");
    let root_index = fs::read(raw(&repo).path().join("index")).unwrap();
    repo.execute(Operation::StageSubmoduleAt(nested.clone()))
        .unwrap();
    assert_eq!(
        fs::read(raw(&repo).path().join("index")).unwrap(),
        root_index
    );
    let nested = repo
        .submodules_recursive()
        .unwrap()
        .into_iter()
        .find(|m| m.depth == 1)
        .unwrap();
    assert_eq!(nested.index, Some(id));
    let parent = raw(&Repository::open(repo.root.join("child")).unwrap());
    parent
        .config()
        .unwrap()
        .set_str("submodule.nested.url", "wrong")
        .unwrap();
    repo.execute(Operation::SyncSubmodules).unwrap();
    assert_ne!(
        raw(&Repository::open(repo.root.join("child")).unwrap())
            .config()
            .unwrap()
            .get_string("submodule.nested.url")
            .unwrap(),
        "wrong"
    );
}
#[test]
fn dirty_ignored_descendant_blocks_before_parent_checkout_or_lfs_dehydrate() {
    let (_dir, repo) = modules_fixture();
    let child = Repository::open(repo.root.join("child")).unwrap();
    raw(&child)
        .config()
        .unwrap()
        .set_str("submodule.nested.ignore", "all")
        .unwrap();
    let parent_head = head(&child);
    let leaf = Repository::open(repo.root.join("child/nested")).unwrap();
    write(&leaf, "file", "unsaved");
    assert!(
        repo.execute(Operation::UpdateSubmodules {
            names: vec![],
            recursive: true
        })
        .is_err()
    );
    assert_eq!(head(&child), parent_head);
    assert_eq!(
        fs::read_to_string(leaf.root.join("file")).unwrap(),
        "unsaved"
    );
}
#[test]
fn linked_worktree_prepares_independent_recursive_submodule_checkouts() {
    let (dir, repo) = modules_fixture();
    let path = dir.path().join("linked");
    repo.execute(Operation::CreateWorktree {
        name: "linked".into(),
        path: path.clone(),
    })
    .unwrap();
    let tree = repo.worktrees().unwrap().remove(0);
    repo.execute(Operation::PrepareWorktree(tree)).unwrap();
    let linked = Repository::open(&path).unwrap();
    assert_eq!(linked.submodules_recursive().unwrap().len(), 2);
    let main_child = raw(&Repository::open(repo.root.join("child")).unwrap());
    let linked_child = raw(&Repository::open(path.join("child")).unwrap());
    assert_ne!(main_child.path(), linked_child.path());
    assert_eq!(
        linked_child.workdir().unwrap().canonicalize().unwrap(),
        path.join("child").canonicalize().unwrap()
    );
    write(
        &Repository::open(path.join("child/nested")).unwrap(),
        "file",
        "linked only",
    );
    assert_eq!(
        fs::read_to_string(repo.root.join("child/nested/file")).unwrap(),
        "leaf"
    );
}
#[test]
fn worktree_creation_expands_lfs_from_shared_cache() {
    let dir = tempfile::tempdir().unwrap();
    let repo = init(&dir.path().join("repo"));
    repo.execute(Operation::LfsTrack {
        pattern: "*.bin".into(),
        track: true,
    })
    .unwrap();
    write(&repo, "data.bin", "large data");
    commit(&repo, "lfs");
    let linked = dir.path().join("linked");
    repo.execute(Operation::CreateWorktree {
        name: "linked".into(),
        path: linked.clone(),
    })
    .unwrap();
    assert_eq!(
        fs::read_to_string(linked.join("data.bin")).unwrap(),
        "large data"
    );
    assert!(
        Repository::open(linked)
            .unwrap()
            .snapshot(10)
            .unwrap()
            .files
            .is_empty()
    );
}
#[test]
fn shared_writer_lock_blocks_linked_tasks_and_releases_explicitly() {
    let dir = tempfile::tempdir().unwrap();
    let repo = init(&dir.path().join("repo"));
    write(&repo, "f", "f");
    commit(&repo, "base");
    let path = dir.path().join("linked");
    repo.execute(Operation::CreateWorktree {
        name: "linked".into(),
        path: path.clone(),
    })
    .unwrap();
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(raw(&repo).commondir().join("gitbuddy-writer.lock"))
        .unwrap();
    lock.lock().unwrap();
    let linked = Repository::open(path).unwrap();
    assert!(linked.execute(Operation::StageAll).is_err());
    assert!(linked.lfs_maintenance(NetworkControl::default()).is_err());
    lock.unlock().unwrap();
    linked.execute(Operation::StageAll).unwrap();
}
#[test]
fn skip_promotes_combined_follower_without_rewriting_the_onto_commit() {
    let dir = tempfile::tempdir().unwrap();
    let repo = init(dir.path());
    write(&repo, "file", "base\n");
    commit(&repo, "base");
    repo.execute(Operation::CreateBranch("feature".into()))
        .unwrap();
    write(&repo, "file", "feature\n");
    let skipped = commit(&repo, "feature");
    write(&repo, "new", "new");
    commit(&repo, "fixup! feature");
    repo.execute(Operation::Checkout("main".into())).unwrap();
    write(&repo, "file", "main\n");
    let onto = commit(&repo, "main");
    repo.execute(Operation::Checkout("feature".into())).unwrap();
    let preview = repo.rebase_preview("main").unwrap();
    let steps = autosquash(&preview.steps).unwrap();
    assert!(
        repo.execute(Operation::Rebase {
            context: Arc::new(preview),
            steps
        })
        .is_err()
    );
    repo.execute(Operation::RecoverRebaseStep {
        expected: skipped,
        skip: true,
    })
    .unwrap();
    let r = raw(&repo);
    let tip = r.head().unwrap().peel_to_commit().unwrap();
    assert_eq!(tip.parent_id(0).unwrap().to_string(), onto);
    assert_eq!(
        fs::read_to_string(repo.root.join("file")).unwrap(),
        "main\n"
    );
    assert!(repo.root.join("new").exists());
}
#[test]
fn interrupted_recovery_reset_remains_explicit_and_recoverable() {
    let (_dir, repo, original, onto) = conflict();
    let r = raw(&repo);
    let path = r.path().join("gitbuddy-rebase.json");
    let mut journal: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    journal["restarting"] = true.into();
    fs::write(&path, serde_json::to_vec(&journal).unwrap()).unwrap();
    write(&repo, "file", "preserve my resolution");
    let before = fs::read(repo.root.join("file")).unwrap();
    let reopened = Repository::open(&repo.root).unwrap();
    assert!(reopened.execute(Operation::ContinueRebase).is_err());
    assert_eq!(fs::read(repo.root.join("file")).unwrap(), before);
    reopened
        .execute(Operation::RecoverRebaseStep {
            expected: original,
            skip: true,
        })
        .unwrap();
    assert_eq!(head(&repo), onto);
    let backup = fs::read_dir(r.path().join("gitbuddy-rebase-recovery"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    assert_eq!(fs::read(backup.join("worktree/file")).unwrap(), before);
}
#[test]
fn submodule_updates_expand_local_lfs_objects_and_report_missing_cache() {
    let dir = tempfile::tempdir().unwrap();
    let source = init(&dir.path().join("source"));
    source
        .execute(Operation::LfsTrack {
            pattern: "*.bin".into(),
            track: true,
        })
        .unwrap();
    write(&source, "data.bin", "source data");
    commit(&source, "lfs");
    let repo = init(&dir.path().join("repo"));
    write(&repo, "base", "base");
    commit(&repo, "base");
    let notice = repo
        .execute(Operation::AddSubmodule {
            url: source.root.display().to_string(),
            path: "child".into(),
        })
        .unwrap();
    assert!(notice.contains("1 LFS objects"));
    commit(&repo, "child");
    let child = Repository::open(repo.root.join("child")).unwrap();
    let oid = source.lfs_status().unwrap().files[0].oid.clone();
    let suffix = format!("lfs/objects/{}/{}/{oid}", &oid[..2], &oid[2..4]);
    let destination = raw(&child).commondir().join(&suffix);
    fs::create_dir_all(destination.parent().unwrap()).unwrap();
    fs::copy(raw(&source).commondir().join(suffix), destination).unwrap();
    repo.execute(Operation::UpdateSubmodules {
        names: vec![],
        recursive: true,
    })
    .unwrap();
    assert_eq!(
        fs::read_to_string(child.root.join("data.bin")).unwrap(),
        "source data"
    );
    assert!(child.snapshot(10).unwrap().files.is_empty());
}
