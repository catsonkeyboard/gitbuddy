use gitbuddy::git::{Operation, ReflogTarget, Repository};
use std::{path::Path, sync::Arc};

fn setup() -> (tempfile::TempDir, Repository) {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();
    repo.execute(Operation::SetIdentity(
        "Original Author".into(),
        "original@example.invalid".into(),
    ))
    .unwrap();
    (dir, repo)
}
fn write(repo: &Repository, name: &str, text: &str) {
    std::fs::write(repo.root.join(name), text).unwrap();
}
fn commit(repo: &Repository, message: &str) -> String {
    repo.execute(Operation::StageAll).unwrap();
    repo.execute(Operation::Commit(message.into())).unwrap();
    repo.snapshot(20).unwrap().head_id.unwrap()
}
fn index_bytes(repo: &Repository) -> Vec<u8> {
    let raw = git2::Repository::open(&repo.root).unwrap();
    std::fs::read(raw.index().unwrap().path().unwrap()).unwrap()
}
fn target(repo: &Repository, id: &str) -> ReflogTarget {
    repo.reflog(100)
        .unwrap()
        .entries
        .into_iter()
        .flat_map(|entry| [entry.before, entry.after])
        .flatten()
        .find(|t| t.id == id)
        .expect("commit in reflog")
}

#[test]
fn amend_uses_staged_tree_preserves_author_parents_worktree_and_index() {
    let (_dir, repo) = setup();
    write(&repo, "file", "base\n");
    let parent = commit(&repo, "base");
    write(&repo, "file", "old\n");
    let old = commit(&repo, "old message\n\nbody\n");
    repo.execute(Operation::SetIdentity(
        "New Committer".into(),
        "committer@example.invalid".into(),
    ))
    .unwrap();
    write(&repo, "file", "staged\n");
    repo.execute(Operation::StageAll).unwrap();
    write(&repo, "file", "unstaged\n");
    write(&repo, "untracked", "keep\n");
    let context = repo.commit_edit().unwrap();
    assert_eq!(context.message, "old message\n\nbody\n");
    let bytes = index_bytes(&repo);
    repo.execute(Operation::Amend {
        context: Arc::new(context),
        message: "new message\n\nnew body\n".into(),
    })
    .unwrap();
    assert_eq!(index_bytes(&repo), bytes);
    assert_eq!(
        std::fs::read_to_string(repo.root.join("file")).unwrap(),
        "unstaged\n"
    );
    assert_eq!(
        std::fs::read_to_string(repo.root.join("untracked")).unwrap(),
        "keep\n"
    );
    let raw = git2::Repository::open(&repo.root).unwrap();
    let head = raw.head().unwrap().peel_to_commit().unwrap();
    assert_ne!(head.id().to_string(), old);
    assert_eq!(head.parent_id(0).unwrap().to_string(), parent);
    assert_eq!(head.parent_count(), 1);
    assert_eq!(head.author().name().unwrap(), "Original Author");
    assert_eq!(head.committer().name().unwrap(), "New Committer");
    assert_eq!(head.message().unwrap(), "new message\n\nnew body\n");
    let tree = head.tree().unwrap();
    let entry = tree.get_path(Path::new("file")).unwrap();
    assert_eq!(raw.find_blob(entry.id()).unwrap().content(), b"staged\n");
    let log = repo.reflog(1).unwrap();
    assert_eq!(log.entries[0].before.as_ref().unwrap().id, old);
    assert_eq!(
        log.entries[0].after.as_ref().unwrap().id,
        head.id().to_string()
    );
}

#[test]
fn message_only_amend_handles_initial_and_merge_commits() {
    let (_dir, repo) = setup();
    write(&repo, "base", "base");
    commit(&repo, "initial");
    let edit = repo.commit_edit().unwrap();
    repo.execute(Operation::Amend {
        context: Arc::new(edit),
        message: "initial amended".into(),
    })
    .unwrap();
    let raw = git2::Repository::open(&repo.root).unwrap();
    assert_eq!(
        raw.head().unwrap().peel_to_commit().unwrap().parent_count(),
        0
    );
    repo.execute(Operation::CreateBranch("feature".into()))
        .unwrap();
    write(&repo, "feature", "feature");
    let feature = commit(&repo, "feature");
    repo.execute(Operation::Checkout("main".into())).unwrap();
    write(&repo, "main", "main");
    let main = commit(&repo, "main");
    repo.execute(Operation::Merge("feature".into())).unwrap();
    let edit = repo.commit_edit().unwrap();
    assert_eq!(edit.parents, [main.clone(), feature.clone()]);
    repo.execute(Operation::Amend {
        context: Arc::new(edit),
        message: "merge amended".into(),
    })
    .unwrap();
    assert_eq!(repo.commit_edit().unwrap().parents, [main.clone(), feature]);
    repo.execute(Operation::UndoLast(repo.commit_edit().unwrap().head))
        .unwrap();
    assert_eq!(repo.snapshot(10).unwrap().head_id, Some(main));
}

#[test]
fn undo_and_restore_keep_both_staged_and_unstaged_changes() {
    let (_dir, repo) = setup();
    write(&repo, "file", "first");
    let first = commit(&repo, "first");
    write(&repo, "file", "second");
    let second = commit(&repo, "second");
    write(&repo, "file", "staged");
    repo.execute(Operation::StageAll).unwrap();
    write(&repo, "file", "working");
    let bytes = index_bytes(&repo);
    repo.execute(Operation::UndoLast(repo.commit_edit().unwrap().head))
        .unwrap();
    assert_eq!(repo.snapshot(10).unwrap().head_id, Some(first));
    assert_eq!(index_bytes(&repo), bytes);
    let page = repo.reflog(1).unwrap();
    assert_eq!(page.entries[0].before.as_ref().unwrap().id, second);
    repo.execute(Operation::RestoreReflog {
        expected: page.head,
        target: page.entries[0].before.clone().unwrap(),
    })
    .unwrap();
    assert_eq!(repo.snapshot(10).unwrap().head_id, Some(second));
    assert_eq!(index_bytes(&repo), bytes);
    assert_eq!(
        std::fs::read_to_string(repo.root.join("file")).unwrap(),
        "working"
    );
}

#[test]
fn initial_commit_undo_and_reflog_restore_work_with_logging_disabled() {
    let (_dir, repo) = setup();
    let raw = git2::Repository::open(&repo.root).unwrap();
    raw.config()
        .unwrap()
        .set_bool("core.logallrefupdates", false)
        .unwrap();
    write(&repo, "file", "initial");
    let first = commit(&repo, "initial");
    let bytes = index_bytes(&repo);
    repo.execute(Operation::UndoLast(repo.commit_edit().unwrap().head))
        .unwrap();
    let snapshot = repo.snapshot(10).unwrap();
    assert_eq!(snapshot.head_id, None);
    assert_eq!(snapshot.branch, "main");
    assert!(snapshot.files[0].staged());
    assert_eq!(index_bytes(&repo), bytes);
    let page = repo.reflog(10).unwrap();
    assert!(page.entries[0].after.is_none());
    let target = page.entries[0].before.clone().unwrap();
    assert_eq!(target.id, first);
    repo.execute(Operation::RestoreReflog {
        expected: page.head,
        target,
    })
    .unwrap();
    assert_eq!(repo.snapshot(10).unwrap().head_id, Some(first));
    assert_eq!(index_bytes(&repo), bytes);
}

#[test]
fn reflog_branch_recovery_never_switches_or_overwrites_existing_branch() {
    let (_dir, repo) = setup();
    write(&repo, "file", "first");
    let first = commit(&repo, "first");
    write(&repo, "file", "second");
    let second = commit(&repo, "second");
    write(&repo, "file", "dirty");
    let bytes = index_bytes(&repo);
    let selected = target(&repo, &first);
    repo.execute(Operation::RecoverBranch {
        target: selected.clone(),
        name: "recovered/first".into(),
    })
    .unwrap();
    assert_eq!(repo.snapshot(10).unwrap().head_id, Some(second.clone()));
    assert_eq!(repo.snapshot(10).unwrap().branch, "main");
    assert_eq!(index_bytes(&repo), bytes);
    let raw = git2::Repository::open(&repo.root).unwrap();
    assert_eq!(
        raw.refname_to_id("refs/heads/recovered/first")
            .unwrap()
            .to_string(),
        first
    );
    assert!(
        repo.execute(Operation::RecoverBranch {
            target: target(&repo, &second),
            name: "recovered/first".into()
        })
        .is_err()
    );
    assert!(
        repo.execute(Operation::RecoverBranch {
            target: selected,
            name: "bad\0branch".into()
        })
        .is_err()
    );
    assert_eq!(
        std::fs::read_to_string(repo.root.join("file")).unwrap(),
        "dirty"
    );
}

#[test]
fn history_actions_reject_stale_head_branch_and_index() {
    let (_dir, repo) = setup();
    write(&repo, "file", "first");
    commit(&repo, "first");
    let edit = repo.commit_edit().unwrap();
    write(&repo, "file", "second");
    repo.execute(Operation::StageAll).unwrap();
    assert!(
        repo.execute(Operation::Amend {
            context: Arc::new(edit.clone()),
            message: "no".into()
        })
        .unwrap_err()
        .to_string()
        .contains("Staged content changed")
    );
    let bytes = index_bytes(&repo);
    let second = commit(&repo, "second");
    assert!(repo.execute(Operation::UndoLast(edit.head)).is_err());
    assert_eq!(repo.snapshot(10).unwrap().head_id, Some(second.clone()));
    let edit = repo.commit_edit().unwrap();
    repo.execute(Operation::CreateBranch("same-commit".into()))
        .unwrap();
    assert!(
        repo.execute(Operation::Amend {
            context: Arc::new(edit.clone()),
            message: "no".into()
        })
        .is_err()
    );
    assert!(repo.execute(Operation::UndoLast(edit.head)).is_err());
    assert_eq!(index_bytes(&repo), bytes);
    assert_eq!(repo.snapshot(10).unwrap().head_id, Some(second));
}

#[test]
fn unavailable_reflog_objects_and_cross_repository_actions_fail_without_writes() {
    let (_dir, repo) = setup();
    write(&repo, "file", "first");
    let first = commit(&repo, "first");
    let raw = git2::Repository::open(&repo.root).unwrap();
    let mut log = raw.reflog("HEAD").unwrap();
    log.append(
        git2::Oid::from_str("1111111111111111111111111111111111111111").unwrap(),
        &raw.signature().unwrap(),
        Some("expired"),
    )
    .unwrap();
    log.write().unwrap();
    let page = repo.reflog(1).unwrap();
    assert_eq!(page.entries.len(), 1);
    assert!(page.total > 1);
    let missing = page.entries[0].after.clone().unwrap();
    assert!(!missing.available);
    assert!(
        repo.execute(Operation::RecoverBranch {
            target: missing,
            name: "missing".into()
        })
        .is_err()
    );
    let (_other, other) = setup();
    assert!(
        other
            .execute(Operation::RecoverBranch {
                target: target(&repo, &first),
                name: "cross".into()
            })
            .is_err()
    );
    assert!(
        other
            .execute(Operation::UndoLast(repo.commit_edit().unwrap().head))
            .is_err()
    );
    assert_eq!(repo.snapshot(10).unwrap().head_id, Some(first));
}

#[test]
fn conflicted_and_in_progress_repositories_refuse_history_rewrites() {
    let (_dir, repo) = setup();
    write(&repo, "file", "base");
    commit(&repo, "base");
    repo.execute(Operation::CreateBranch("feature".into()))
        .unwrap();
    write(&repo, "file", "feature");
    commit(&repo, "feature");
    repo.execute(Operation::Checkout("main".into())).unwrap();
    write(&repo, "file", "main");
    commit(&repo, "main");
    let edit = repo.commit_edit().unwrap();
    let _ = repo.execute(Operation::Merge("feature".into()));
    assert!(repo.commit_edit().is_err());
    assert!(
        repo.execute(Operation::UndoLast(edit.head.clone()))
            .is_err()
    );
    assert!(
        repo.execute(Operation::Amend {
            context: Arc::new(edit),
            message: "no".into()
        })
        .is_err()
    );
    // Recovery to a separate branch remains available during conflicts.
    let page = repo.reflog(1).unwrap();
    repo.execute(Operation::RecoverBranch {
        target: page.entries[0].after.clone().unwrap(),
        name: "rescue".into(),
    })
    .unwrap();
    assert!(
        repo.snapshot(10)
            .unwrap()
            .files
            .iter()
            .any(|f| f.conflict())
    );
}

#[test]
fn detached_head_is_preserved_and_initial_detached_undo_is_rejected() {
    let (_dir, repo) = setup();
    write(&repo, "file", "first");
    let first = commit(&repo, "first");
    write(&repo, "file", "second");
    let second = commit(&repo, "second");
    let raw = git2::Repository::open(&repo.root).unwrap();
    raw.set_head_detached(git2::Oid::from_str(&second).unwrap())
        .unwrap();
    repo.execute(Operation::Amend {
        context: Arc::new(repo.commit_edit().unwrap()),
        message: "detached amended".into(),
    })
    .unwrap();
    assert!(raw.head_detached().unwrap());
    assert_eq!(
        raw.refname_to_id("refs/heads/main").unwrap().to_string(),
        second
    );
    repo.execute(Operation::UndoLast(repo.commit_edit().unwrap().head))
        .unwrap();
    assert_eq!(repo.snapshot(10).unwrap().head_id, Some(first));
    assert!(
        repo.execute(Operation::UndoLast(repo.commit_edit().unwrap().head))
            .is_err()
    );
}
