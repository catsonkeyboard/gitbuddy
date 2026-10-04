use gitbuddy::git::{ConflictOperation, ConflictResolution, Operation, RebaseAction, Repository};
use std::{path::Path, sync::Arc};

fn setup() -> (tempfile::TempDir, Repository) {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::init(&dir.path().join("repo")).unwrap();
    repo.execute(Operation::SetIdentity(
        "Author".into(),
        "author@example.invalid".into(),
    ))
    .unwrap();
    (dir, repo)
}
fn write(repo: &Repository, path: &str, text: &str) {
    std::fs::write(repo.root.join(path), text).unwrap();
}
fn commit(repo: &Repository, message: &str) -> String {
    repo.execute(Operation::StageAll).unwrap();
    repo.execute(Operation::Commit(message.into())).unwrap();
    repo.snapshot(100).unwrap().head_id.unwrap()
}
fn head(repo: &Repository) -> String {
    repo.snapshot(100).unwrap().head_id.unwrap()
}
fn raw(repo: &Repository) -> git2::Repository {
    git2::Repository::open(&repo.root).unwrap()
}
fn history(repo: &Repository) -> (String, String, String) {
    write(repo, "base", "base\n");
    let base = commit(repo, "base");
    write(repo, "one", "one\n");
    let one = commit(repo, "one");
    write(repo, "two", "two\n");
    let two = commit(repo, "two");
    (base, one, two)
}

#[test]
fn unsupported_lfs_pointer_is_not_staged_as_another_lfs_object() {
    let (_dir, repo) = setup();
    write(&repo, "base", "base");
    commit(&repo, "base");
    repo.execute(Operation::LfsTrack {
        pattern: "*.bin".into(),
        track: true,
    })
    .unwrap();
    repo.execute(Operation::StageAll).unwrap();
    let before = std::fs::read(raw(&repo).path().join("index")).unwrap();
    let extension = format!(
        "version https://git-lfs.github.com/spec/v1\next-0-test sha256:{}\noid sha256:{}\nsize 12\n",
        "a".repeat(64),
        "b".repeat(64)
    );
    write(&repo, "unsupported.bin", &extension);
    let error = repo
        .execute(Operation::Stage(vec!["unsupported.bin".into()]))
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("Unsupported or malformed LFS pointer")
    );
    assert_eq!(
        std::fs::read(raw(&repo).path().join("index")).unwrap(),
        before
    );
    assert_eq!(
        std::fs::read_to_string(repo.root.join("unsupported.bin")).unwrap(),
        extension
    );
    assert!(!raw(&repo).commondir().join("lfs/objects").exists());
}

#[test]
fn squash_keeps_final_tree_author_and_recovery_ref() {
    let (_dir, repo) = setup();
    let (base, _, old) = history(&repo);
    let old_tree = raw(&repo)
        .find_commit(old.parse().unwrap())
        .unwrap()
        .tree_id();
    let preview = repo.rebase_preview(&base).unwrap();
    let mut steps = preview.steps.clone();
    steps[1].action = RebaseAction::Squash;
    repo.execute(Operation::Rebase {
        context: Arc::new(preview),
        steps,
    })
    .unwrap();
    let r = raw(&repo);
    let h = r.head().unwrap().peel_to_commit().unwrap();
    assert_eq!(h.parent_id(0).unwrap().to_string(), base);
    assert_eq!(h.tree_id(), old_tree);
    assert_eq!(h.author().name().unwrap(), "Author");
    assert_eq!(h.message(), Ok("one\n\ntwo"));
    assert!(
        r.references_glob("refs/gitbuddy/rewrites/*")
            .unwrap()
            .any(|item| item.unwrap().target() == Some(old.parse().unwrap()))
    );
    assert!(repo.rebase_status().unwrap().is_none());
    let snapshot = repo.snapshot(100).unwrap();
    assert!(snapshot.files.is_empty());
    assert!(snapshot.commits.iter().all(|commit| commit.id != old));
    assert!(
        snapshot
            .commits
            .iter()
            .all(|commit| !commit.refs.contains("gitbuddy/rewrites"))
    );
}
#[test]
fn reorder_reword_fixup_and_drop_use_exact_plan() {
    let (_dir, repo) = setup();
    let (base, one, _) = history(&repo);
    write(&repo, "three", "three");
    commit(&repo, "three");
    let preview = repo.rebase_preview(&base).unwrap();
    let mut steps = preview.steps.clone();
    steps.swap(0, 1);
    steps[0].action = RebaseAction::Reword;
    steps[0].message = "new title\n\nbody".into();
    steps[1].action = RebaseAction::Fixup;
    steps[2].action = RebaseAction::Drop;
    repo.execute(Operation::Rebase {
        context: Arc::new(preview),
        steps,
    })
    .unwrap();
    let r = raw(&repo);
    let h = r.head().unwrap().peel_to_commit().unwrap();
    assert_eq!(h.message(), Ok("new title\n\nbody"));
    assert_eq!(h.parent_id(0).unwrap().to_string(), base);
    assert!(h.tree().unwrap().get_path(Path::new("one")).is_ok());
    assert!(h.tree().unwrap().get_path(Path::new("two")).is_ok());
    assert!(!repo.root.join("three").exists());
    assert_ne!(head(&repo), one);
}
#[test]
fn rebase_conflict_survives_reopen_and_continues_through_conflict_ui_adapter() {
    let (_dir, repo) = setup();
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
    let repo = Repository::open(&repo.root).unwrap();
    assert_eq!(repo.rebase_status().unwrap().unwrap().original, original);
    assert_eq!(
        repo.snapshot(100).unwrap().conflict_operation,
        ConflictOperation::Rebase
    );
    assert!(repo.execute(Operation::Commit("wrong".into())).is_err());
    let context = repo.conflict_context(Path::new("file")).unwrap();
    repo.execute(Operation::ResolveConflict {
        context: Arc::new(context),
        resolution: ConflictResolution::Edited("resolved\n".into()),
    })
    .unwrap();
    let session = repo.conflict_session().unwrap();
    assert_eq!(session.operation, ConflictOperation::Rebase);
    repo.execute(Operation::ContinueConflict(Arc::new(session)))
        .unwrap();
    let r = raw(&repo);
    let h = r.head().unwrap().peel_to_commit().unwrap();
    assert_eq!(h.parent_id(0).unwrap().to_string(), onto);
    assert_eq!(r.head().unwrap().name().unwrap(), "refs/heads/feature");
    assert_eq!(
        std::fs::read_to_string(repo.root.join("file")).unwrap(),
        "resolved\n"
    );
}
#[test]
fn abort_rebase_restores_original_branch_tree_and_keeps_untracked_files() {
    let (_dir, repo) = setup();
    write(&repo, "file", "base\n");
    commit(&repo, "base");
    repo.execute(Operation::CreateBranch("feature".into()))
        .unwrap();
    write(&repo, "file", "feature\n");
    let original = commit(&repo, "feature");
    repo.execute(Operation::Checkout("main".into())).unwrap();
    write(&repo, "file", "main\n");
    commit(&repo, "main");
    repo.execute(Operation::Checkout("feature".into())).unwrap();
    let preview = repo.rebase_preview("main").unwrap();
    assert!(
        repo.execute(Operation::Rebase {
            steps: preview.steps.clone(),
            context: Arc::new(preview)
        })
        .is_err()
    );
    write(&repo, "keep", "untracked");
    repo.execute(Operation::AbortRebase).unwrap();
    assert_eq!(head(&repo), original);
    assert_eq!(
        raw(&repo).head().unwrap().name().unwrap(),
        "refs/heads/feature"
    );
    assert_eq!(
        std::fs::read_to_string(repo.root.join("file")).unwrap(),
        "feature\n"
    );
    assert!(repo.root.join("keep").exists());
}
#[test]
fn rebase_rejects_stale_duplicate_squash_first_dirty_and_merge_plans() {
    let (_dir, repo) = setup();
    let (base, _, old) = history(&repo);
    let preview = repo.rebase_preview(&base).unwrap();
    let mut steps = preview.steps.clone();
    steps[0].action = RebaseAction::Squash;
    assert!(
        repo.execute(Operation::Rebase {
            context: Arc::new(preview.clone()),
            steps
        })
        .is_err()
    );
    assert_eq!(head(&repo), old);
    let steps = vec![preview.steps[0].clone(); 2];
    assert!(
        repo.execute(Operation::Rebase {
            context: Arc::new(preview.clone()),
            steps
        })
        .is_err()
    );
    write(&repo, "untracked", "keep");
    assert!(repo.rebase_preview(&base).is_err());
    std::fs::remove_file(repo.root.join("untracked")).unwrap();
    write(&repo, "new", "new");
    commit(&repo, "new");
    assert!(
        repo.execute(Operation::Rebase {
            steps: preview.steps.clone(),
            context: Arc::new(preview)
        })
        .is_err()
    );
    repo.execute(Operation::CreateBranch("side".into()))
        .unwrap();
    write(&repo, "side", "side");
    commit(&repo, "side");
    repo.execute(Operation::Checkout("main".into())).unwrap();
    write(&repo, "main", "main");
    commit(&repo, "main");
    repo.execute(Operation::Merge("side".into())).unwrap();
    assert!(
        repo.rebase_preview(&base)
            .unwrap_err()
            .to_string()
            .contains("merge")
    );
}
#[test]
fn worktree_create_lock_open_and_guarded_remove() {
    let (dir, repo) = setup();
    history(&repo);
    let destination = dir.path().join("linked");
    repo.execute(Operation::CreateWorktree {
        name: "topic".into(),
        path: destination.clone(),
    })
    .unwrap();
    let linked = Repository::open(&destination).unwrap();
    assert_eq!(linked.snapshot(100).unwrap().branch, "topic");
    let info = repo.worktrees().unwrap().remove(0);
    assert_eq!(
        info.path.canonicalize().unwrap(),
        destination.canonicalize().unwrap()
    );
    assert!(!info.dirty);
    repo.execute(Operation::LockWorktree {
        name: "topic".into(),
        locked: true,
    })
    .unwrap();
    assert!(
        repo.execute(Operation::RemoveWorktree(info.clone()))
            .is_err()
    );
    repo.execute(Operation::LockWorktree {
        name: "topic".into(),
        locked: false,
    })
    .unwrap();
    write(&linked, "untracked", "keep");
    assert!(
        repo.execute(Operation::RemoveWorktree(info.clone()))
            .is_err()
    );
    std::fs::remove_file(linked.root.join("untracked")).unwrap();
    assert!(
        linked
            .execute(Operation::RemoveWorktree(info.clone()))
            .is_err()
    );
    repo.execute(Operation::RemoveWorktree(info)).unwrap();
    assert!(repo.worktrees().unwrap().is_empty());
    assert!(!destination.exists());
    assert!(
        raw(&repo)
            .find_branch("topic", git2::BranchType::Local)
            .is_ok()
    );
}
#[test]
fn worktree_rejects_existing_destination_and_stale_head() {
    let (dir, repo) = setup();
    history(&repo);
    let destination = dir.path().join("linked");
    repo.execute(Operation::CreateWorktree {
        name: "topic".into(),
        path: destination.clone(),
    })
    .unwrap();
    assert!(
        repo.execute(Operation::CreateWorktree {
            name: "other".into(),
            path: destination.clone()
        })
        .is_err()
    );
    let info = repo.worktrees().unwrap().remove(0);
    let linked = Repository::open(destination).unwrap();
    write(&linked, "new", "new");
    commit(&linked, "new");
    assert!(repo.execute(Operation::RemoveWorktree(info)).is_err());
}
#[test]
fn submodule_add_update_stage_and_dirty_protection() {
    let (dir, repo) = setup();
    history(&repo);
    let source = Repository::init(&dir.path().join("source")).unwrap();
    source
        .execute(Operation::SetIdentity(
            "Author".into(),
            "author@example.invalid".into(),
        ))
        .unwrap();
    write(&source, "file", "one");
    let one = commit(&source, "one");
    repo.execute(Operation::AddSubmodule {
        url: source.root.display().to_string(),
        path: "child".into(),
    })
    .unwrap();
    let module = repo.submodules().unwrap().remove(0);
    assert!(module.initialized);
    assert_eq!(module.index, Some(one));
    commit(&repo, "add child");
    let child = Repository::open(repo.root.join("child")).unwrap();
    write(&child, "file", "dirty");
    assert!(
        repo.execute(Operation::UpdateSubmodules {
            names: vec![],
            recursive: true
        })
        .is_err()
    );
    child.execute(Operation::Discard("file".into())).unwrap();
    child
        .execute(Operation::SetIdentity(
            "Author".into(),
            "author@example.invalid".into(),
        ))
        .unwrap();
    write(&child, "file", "two");
    let two = commit(&child, "two");
    repo.execute(Operation::StageSubmodule("child".into()))
        .unwrap();
    assert_eq!(repo.submodules().unwrap()[0].index, Some(two.clone()));
    repo.execute(Operation::UpdateSubmodules {
        names: vec![],
        recursive: true,
    })
    .unwrap();
    assert_eq!(head(&child), two);
    repo.execute(Operation::SyncSubmodules).unwrap();
}
#[test]
fn lfs_native_stage_pointer_cache_status_checkout_and_partial_guard() {
    let (_dir, repo) = setup();
    write(&repo, "base", "base");
    commit(&repo, "base");
    repo.execute(Operation::LfsTrack {
        pattern: "*.bin".into(),
        track: true,
    })
    .unwrap();
    let content = vec![42u8; 128 * 1024];
    std::fs::write(repo.root.join("large.bin"), &content).unwrap();
    repo.execute(Operation::StageAll).unwrap();
    let r = raw(&repo);
    let entry = r
        .index()
        .unwrap()
        .get_path(Path::new("large.bin"), 0)
        .unwrap();
    let blob = r.find_blob(entry.id).unwrap();
    assert!(
        blob.content()
            .starts_with(b"version https://git-lfs.github.com/spec/v1\n")
    );
    assert!(blob.size() < 200);
    let status = repo.lfs_status().unwrap();
    assert_eq!(status.files.len(), 1);
    assert!(status.files[0].available && status.files[0].hydrated);
    let file = repo
        .snapshot(100)
        .unwrap()
        .files
        .into_iter()
        .find(|f| f.path == Path::new("large.bin"))
        .unwrap();
    assert_eq!(file.worktree, ' ');
    assert!(repo.worktree_patch(&file, true).unwrap().partial.is_none());
    commit(&repo, "large");
    assert!(repo.snapshot(100).unwrap().files.is_empty());
    std::fs::write(repo.root.join("large.bin"), blob.content()).unwrap();
    repo.execute(Operation::LfsCheckout).unwrap();
    assert_eq!(std::fs::read(repo.root.join("large.bin")).unwrap(), content);
}
#[test]
fn lfs_switch_preserves_materialized_content_and_dirty_edits() {
    let (_dir, repo) = setup();
    write(&repo, "base", "base");
    commit(&repo, "base");
    repo.execute(Operation::LfsTrack {
        pattern: "*.bin".into(),
        track: true,
    })
    .unwrap();
    write(&repo, "data.bin", "one");
    commit(&repo, "one");
    repo.execute(Operation::CreateBranch("side".into()))
        .unwrap();
    write(&repo, "data.bin", "two");
    commit(&repo, "two");
    repo.execute(Operation::Checkout("main".into())).unwrap();
    assert_eq!(
        std::fs::read_to_string(repo.root.join("data.bin")).unwrap(),
        "one"
    );
    assert!(repo.snapshot(100).unwrap().files.is_empty());
    write(&repo, "data.bin", "dirty");
    assert!(repo.execute(Operation::Checkout("side".into())).is_err());
    assert_eq!(
        std::fs::read_to_string(repo.root.join("data.bin")).unwrap(),
        "dirty"
    );
}
#[test]
fn lfs_globs_match_spaced_paths_and_untrack_without_touching_history() {
    let (_dir, repo) = setup();
    write(&repo, "base", "base");
    commit(&repo, "base");
    repo.execute(Operation::LfsTrack {
        pattern: "my*.bin".into(),
        track: true,
    })
    .unwrap();
    write(&repo, "my file.bin", "content");
    commit(&repo, "tracked");
    assert_eq!(repo.lfs_status().unwrap().files.len(), 1);
    let before = head(&repo);
    repo.execute(Operation::LfsTrack {
        pattern: "my*.bin".into(),
        track: false,
    })
    .unwrap();
    assert_eq!(head(&repo), before);
    assert!(
        !std::fs::read_to_string(repo.root.join(".gitattributes"))
            .unwrap()
            .contains("filter=lfs")
    );
    assert!(
        repo.execute(Operation::LfsTrack {
            pattern: "*.bin\n*.txt".into(),
            track: true
        })
        .is_err()
    );
    assert!(
        repo.execute(Operation::LfsTrack {
            pattern: "my file.bin".into(),
            track: true
        })
        .is_err()
    );
}
#[test]
fn lfs_corrupt_object_never_overwrites_pointer() {
    let (_dir, repo) = setup();
    write(&repo, "base", "base");
    commit(&repo, "base");
    repo.execute(Operation::LfsTrack {
        pattern: "*.bin".into(),
        track: true,
    })
    .unwrap();
    write(&repo, "data.bin", "content");
    commit(&repo, "tracked");
    let status = repo.lfs_status().unwrap();
    let oid = &status.files[0].oid;
    let r = raw(&repo);
    let path = r
        .commondir()
        .join("lfs/objects")
        .join(&oid[..2])
        .join(&oid[2..4])
        .join(oid);
    std::fs::write(path, "wrong").unwrap();
    let entry = r
        .index()
        .unwrap()
        .get_path(Path::new("data.bin"), 0)
        .unwrap();
    let pointer = r.find_blob(entry.id).unwrap();
    std::fs::write(repo.root.join("data.bin"), pointer.content()).unwrap();
    assert!(repo.execute(Operation::LfsCheckout).is_err());
    assert_eq!(
        std::fs::read(repo.root.join("data.bin")).unwrap(),
        pointer.content()
    );
}
#[cfg(unix)]
#[test]
fn ordinary_symlinks_can_still_be_staged() {
    let (_dir, repo) = setup();
    write(&repo, "base", "base");
    std::os::unix::fs::symlink("base", repo.root.join("link")).unwrap();
    commit(&repo, "symlink");
    assert_eq!(
        raw(&repo)
            .index()
            .unwrap()
            .get_path(Path::new("link"), 0)
            .unwrap()
            .mode,
        0o120000
    );
}

#[test]
fn branches_checked_out_in_other_worktrees_cannot_be_checked_out_or_deleted() {
    let (dir, repo) = setup();
    history(&repo);
    repo.execute(Operation::CreateWorktree {
        name: "topic".into(),
        path: dir.path().join("topic"),
    })
    .unwrap();
    assert!(
        repo.execute(Operation::Checkout("topic".into()))
            .unwrap_err()
            .to_string()
            .contains("another worktree")
    );
    assert!(
        repo.execute(Operation::DeleteBranch("topic".into()))
            .unwrap_err()
            .to_string()
            .contains("another worktree")
    );
    let linked = Repository::open(dir.path().join("topic")).unwrap();
    assert!(linked.execute(Operation::Checkout("main".into())).is_err());
}
#[test]
fn missing_worktrees_can_be_pruned_without_deleting_their_branch() {
    let (dir, repo) = setup();
    history(&repo);
    let path = dir.path().join("topic");
    repo.execute(Operation::CreateWorktree {
        name: "topic".into(),
        path: path.clone(),
    })
    .unwrap();
    std::fs::remove_dir_all(path).unwrap();
    let info = repo.worktrees().unwrap().remove(0);
    assert!(!info.valid);
    repo.execute(Operation::RemoveWorktree(info)).unwrap();
    assert!(repo.worktrees().unwrap().is_empty());
    assert!(
        raw(&repo)
            .find_branch("topic", git2::BranchType::Local)
            .is_ok()
    );
}
#[test]
fn lfs_rebase_and_discard_keep_files_materialized() {
    let (_dir, repo) = setup();
    write(&repo, "base", "base");
    let base = commit(&repo, "base");
    repo.execute(Operation::LfsTrack {
        pattern: "*.bin".into(),
        track: true,
    })
    .unwrap();
    write(&repo, "data.bin", "one");
    commit(&repo, "one");
    let plan = repo.rebase_preview(&base).unwrap();
    let mut steps = plan.steps.clone();
    steps[0].action = RebaseAction::Reword;
    steps[0].message = "reworded".into();
    repo.execute(Operation::Rebase {
        context: Arc::new(plan),
        steps,
    })
    .unwrap();
    assert_eq!(
        std::fs::read_to_string(repo.root.join("data.bin")).unwrap(),
        "one"
    );
    assert!(repo.snapshot(100).unwrap().files.is_empty());
    write(&repo, "data.bin", "changed");
    repo.execute(Operation::Discard("data.bin".into())).unwrap();
    assert_eq!(
        std::fs::read_to_string(repo.root.join("data.bin")).unwrap(),
        "one"
    );
}
#[test]
fn quoted_lfs_rules_refuse_staging_without_modifying_index() {
    let (_dir, repo) = setup();
    write(&repo, "base", "base");
    commit(&repo, "base");
    let r = raw(&repo);
    let before = std::fs::read(r.index().unwrap().path().unwrap()).unwrap();
    write(
        &repo,
        ".gitattributes",
        "\"my file.bin\" filter=lfs diff=lfs merge=lfs -text\n",
    );
    write(&repo, "my file.bin", "raw content");
    assert!(
        repo.execute(Operation::Stage(vec!["my file.bin".into()]))
            .unwrap_err()
            .to_string()
            .contains("Quoted")
    );
    assert_eq!(
        std::fs::read(r.index().unwrap().path().unwrap()).unwrap(),
        before
    );
}
#[test]
fn missing_lfs_object_blocks_git_push_before_remote_refs_change() {
    let (dir, repo) = setup();
    write(&repo, "base", "base");
    commit(&repo, "base");
    repo.execute(Operation::LfsTrack {
        pattern: "*.bin".into(),
        track: true,
    })
    .unwrap();
    write(
        &repo,
        "missing.bin",
        &format!(
            "version https://git-lfs.github.com/spec/v1\noid sha256:{}\nsize 99\n",
            "f".repeat(64)
        ),
    );
    commit(&repo, "missing object");
    let remote = git2::Repository::init_bare(dir.path().join("remote.git")).unwrap();
    repo.execute(Operation::AddRemote(
        "origin".into(),
        remote.path().display().to_string(),
    ))
    .unwrap();
    assert!(repo.execute(Operation::Push).is_err());
    assert!(remote.find_reference("refs/heads/main").is_err());
}

#[test]
fn lfs_stash_requires_staging_and_hydrates_restored_working_files() {
    let (_dir, repo) = setup();
    write(&repo, "base", "base");
    commit(&repo, "base");
    repo.execute(Operation::LfsTrack {
        pattern: "*.bin".into(),
        track: true,
    })
    .unwrap();
    write(&repo, "data.bin", "one");
    commit(&repo, "one");
    write(&repo, "data.bin", "changed");
    assert!(repo.execute(Operation::Stash("must stage".into())).is_err());
    assert_eq!(
        std::fs::read_to_string(repo.root.join("data.bin")).unwrap(),
        "changed"
    );
    write(&repo, "new.bin", "new");
    repo.execute(Operation::StageAll).unwrap();
    repo.execute(Operation::Stash("staged lfs".into())).unwrap();
    assert_eq!(
        std::fs::read_to_string(repo.root.join("data.bin")).unwrap(),
        "one"
    );
    assert!(!repo.root.join("new.bin").exists());
    repo.execute(Operation::ApplyStash("stash@{0}".into()))
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(repo.root.join("data.bin")).unwrap(),
        "changed"
    );
    assert_eq!(
        std::fs::read_to_string(repo.root.join("new.bin")).unwrap(),
        "new"
    );
}
