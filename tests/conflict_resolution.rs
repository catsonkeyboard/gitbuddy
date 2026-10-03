use gitbuddy::git::{ConflictOperation, ConflictResolution as Resolution, Operation, Repository};
use std::{path::Path, sync::Arc};

fn write(repo: &Repository, path: &str, content: Option<&[u8]>) {
    match content {
        Some(bytes) => std::fs::write(repo.root.join(path), bytes).unwrap(),
        None => {
            let _ = std::fs::remove_file(repo.root.join(path));
        }
    }
}
fn commit(repo: &Repository, message: &str) -> String {
    repo.execute(Operation::StageAll).unwrap();
    repo.execute(Operation::Commit(message.into())).unwrap();
    repo.snapshot(20).unwrap().head_id.unwrap()
}
fn setup(
    base: Option<&[u8]>,
    ours: Option<&[u8]>,
    theirs: Option<&[u8]>,
    name: &str,
) -> (tempfile::TempDir, Repository, String, String) {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();
    repo.execute(Operation::SetIdentity(
        "Conflict Tester".into(),
        "test@example.invalid".into(),
    ))
    .unwrap();
    write(&repo, "unrelated", Some(b"base\n"));
    write(&repo, name, base);
    commit(&repo, "base");
    repo.execute(Operation::CreateBranch("feature".into()))
        .unwrap();
    write(&repo, name, theirs);
    let incoming = commit(&repo, "incoming version");
    repo.execute(Operation::Checkout("main".into())).unwrap();
    write(&repo, name, ours);
    let head = commit(&repo, "current version");
    assert!(repo.execute(Operation::Merge("feature".into())).is_err());
    assert!(!repo.conflict_session().unwrap().files.is_empty());
    (dir, repo, head, incoming)
}
fn index_bytes(repo: &Repository) -> Vec<u8> {
    std::fs::read(
        git2::Repository::open(&repo.root)
            .unwrap()
            .index()
            .unwrap()
            .path()
            .unwrap(),
    )
    .unwrap()
}
fn staged(repo: &Repository, path: &str) -> Option<Vec<u8>> {
    let raw = git2::Repository::open(&repo.root).unwrap();
    raw.index()
        .unwrap()
        .get_path(Path::new(path), 0)
        .map(|e| raw.find_blob(e.id).unwrap().content().to_vec())
}
fn resolve(repo: &Repository, name: &str, resolution: Resolution) {
    let context = Arc::new(repo.conflict_context(Path::new(name)).unwrap());
    repo.execute(Operation::ResolveConflict {
        context,
        resolution,
    })
    .unwrap();
}

#[test]
fn three_sides_edited_resolution_preserves_other_staged_files_and_merge_parents() {
    let (_dir, repo, head, incoming) = setup(
        Some(b"base\n"),
        Some(b"ours\n"),
        Some(b"theirs\n"),
        "conflict",
    );
    let before = index_bytes(&repo);
    let context = repo.conflict_context(Path::new("conflict")).unwrap();
    assert_eq!(context.base.unwrap().text.unwrap(), "base\n");
    assert_eq!(context.ours.unwrap().text.unwrap(), "ours\n");
    assert_eq!(context.theirs.unwrap().text.unwrap(), "theirs\n");
    assert_eq!(index_bytes(&repo), before);
    write(&repo, "unrelated", Some(b"keep staged\n"));
    repo.execute(Operation::Stage(vec!["unrelated".into()]))
        .unwrap();
    write(&repo, "unrelated", Some(b"keep unstaged\n"));
    resolve(&repo, "conflict", Resolution::Edited("combined\n".into()));
    assert_eq!(staged(&repo, "conflict").unwrap(), b"combined\n");
    assert_eq!(staged(&repo, "unrelated").unwrap(), b"keep staged\n");
    assert_eq!(
        std::fs::read(repo.root.join("unrelated")).unwrap(),
        b"keep unstaged\n"
    );
    let session = repo.conflict_session().unwrap();
    assert_eq!(session.operation, ConflictOperation::Merge);
    assert!(session.files.is_empty());
    repo.execute(Operation::ContinueConflict(Arc::new(session)))
        .unwrap();
    let raw = git2::Repository::open(&repo.root).unwrap();
    let c = raw.head().unwrap().peel_to_commit().unwrap();
    assert_eq!(c.parent_count(), 2);
    assert_eq!(c.parent_id(0).unwrap().to_string(), head);
    assert_eq!(c.parent_id(1).unwrap().to_string(), incoming);
    assert_eq!(raw.state(), git2::RepositoryState::Clean);
    let log = raw.reflog("HEAD").unwrap();
    assert_eq!(log.get(0).unwrap().id_old().to_string(), head);
    assert_eq!(log.get(0).unwrap().id_new(), c.id());
}

#[test]
fn selecting_sides_preserves_binary_crlf_non_utf8_and_no_final_newline() {
    for (ours, theirs) in [
        (b"ours\0\xff".as_slice(), b"theirs\0\xfe".as_slice()),
        (b"ours\r\nlast".as_slice(), b"theirs\r\nlast".as_slice()),
    ] {
        for choose_ours in [true, false] {
            let (_dir, repo, _, _) = setup(Some(b"base"), Some(ours), Some(theirs), "file");
            resolve(
                &repo,
                "file",
                if choose_ours {
                    Resolution::Ours
                } else {
                    Resolution::Theirs
                },
            );
            let expected = if choose_ours { ours } else { theirs };
            assert_eq!(staged(&repo, "file").unwrap(), expected);
            assert_eq!(std::fs::read(repo.root.join("file")).unwrap(), expected);
            assert!(repo.conflict_session().unwrap().files.is_empty());
        }
    }
}

#[test]
fn modify_delete_and_add_add_conflicts_keep_missing_sides_distinct_from_empty_files() {
    for delete_ours in [true, false] {
        let (_dir, repo, _, _) = setup(
            Some(b"base\n"),
            if delete_ours { None } else { Some(b"ours\n") },
            if delete_ours { Some(b"theirs\n") } else { None },
            "file",
        );
        let context = repo.conflict_context(Path::new("file")).unwrap();
        assert_eq!(context.ours.is_none(), delete_ours);
        assert_eq!(context.theirs.is_none(), !delete_ours);
        resolve(
            &repo,
            "file",
            if delete_ours {
                Resolution::Ours
            } else {
                Resolution::Theirs
            },
        );
        assert!(staged(&repo, "file").is_none());
        assert!(!repo.root.join("file").exists());
        assert!(repo.conflict_session().unwrap().files.is_empty());
    }
    let (_dir, repo, _, _) = setup(None, Some(b"ours\n"), Some(b"theirs\n"), "file");
    assert!(
        repo.conflict_context(Path::new("file"))
            .unwrap()
            .base
            .is_none()
    );
    resolve(&repo, "file", Resolution::Edited(String::new()));
    assert_eq!(staged(&repo, "file").unwrap(), b"");
    assert!(repo.root.join("file").exists());
}

#[test]
fn existing_working_resolution_is_staged_without_rewriting_the_file() {
    let (_dir, repo, _, _) = setup(Some(b"base"), Some(b"ours"), Some(b"theirs"), "file");
    write(&repo, "file", Some(b"external\r\nresolved\xff"));
    let modified = std::fs::metadata(repo.root.join("file"))
        .unwrap()
        .modified()
        .unwrap();
    resolve(&repo, "file", Resolution::Worktree);
    assert_eq!(staged(&repo, "file").unwrap(), b"external\r\nresolved\xff");
    assert_eq!(
        std::fs::metadata(repo.root.join("file"))
            .unwrap()
            .modified()
            .unwrap(),
        modified
    );
}

#[test]
fn markers_and_stale_worktree_or_conflict_are_rejected_without_side_effects() {
    let (_dir, repo, _, _) = setup(Some(b"base"), Some(b"ours"), Some(b"theirs"), "file");
    let context = Arc::new(repo.conflict_context(Path::new("file")).unwrap());
    let before = index_bytes(&repo);
    let work = std::fs::read(repo.root.join("file")).unwrap();
    for resolution in [
        Resolution::Edited("<<<<<<< HEAD\nours\n=======\ntheirs\n>>>>>>> feature\n".into()),
        Resolution::Worktree,
    ] {
        assert!(
            repo.execute(Operation::ResolveConflict {
                context: context.clone(),
                resolution
            })
            .is_err()
        );
        assert_eq!(index_bytes(&repo), before);
        assert_eq!(std::fs::read(repo.root.join("file")).unwrap(), work);
    }
    write(&repo, "file", Some(b"external edit"));
    assert!(
        repo.execute(Operation::ResolveConflict {
            context: context.clone(),
            resolution: Resolution::Ours
        })
        .is_err()
    );
    assert_eq!(index_bytes(&repo), before);
    assert_eq!(
        std::fs::read(repo.root.join("file")).unwrap(),
        b"external edit"
    );
    resolve(&repo, "file", Resolution::Theirs);
    let resolved = index_bytes(&repo);
    assert!(
        repo.execute(Operation::ResolveConflict {
            context,
            resolution: Resolution::Delete
        })
        .is_err()
    );
    assert_eq!(index_bytes(&repo), resolved);
    assert_eq!(std::fs::read(repo.root.join("file")).unwrap(), b"theirs");
}

#[test]
fn stale_operation_cross_repo_and_index_lock_are_rejected() {
    let (_dir, repo, _, _) = setup(Some(b"base"), Some(b"ours"), Some(b"theirs"), "file");
    let context = Arc::new(repo.conflict_context(Path::new("file")).unwrap());
    let before = index_bytes(&repo);
    let work = std::fs::read(repo.root.join("file")).unwrap();
    let lock = repo.root.join(".git/index.lock");
    std::fs::write(&lock, b"owned by other process").unwrap();
    assert!(
        repo.execute(Operation::ResolveConflict {
            context: context.clone(),
            resolution: Resolution::Ours
        })
        .is_err()
    );
    assert_eq!(std::fs::read(&lock).unwrap(), b"owned by other process");
    assert_eq!(index_bytes(&repo), before);
    assert_eq!(std::fs::read(repo.root.join("file")).unwrap(), work);
    std::fs::remove_file(lock).unwrap();
    let (_other, other, _, _) = setup(Some(b"base"), Some(b"ours"), Some(b"theirs"), "file");
    let other_bytes = index_bytes(&other);
    assert!(
        other
            .execute(Operation::ResolveConflict {
                context: context.clone(),
                resolution: Resolution::Theirs
            })
            .is_err()
    );
    assert_eq!(index_bytes(&other), other_bytes);
    repo.execute(Operation::AbortMerge).unwrap();
    assert!(
        repo.execute(Operation::ResolveConflict {
            context,
            resolution: Resolution::Ours
        })
        .is_err()
    );
    assert_eq!(std::fs::read(repo.root.join("file")).unwrap(), b"ours");
}

#[test]
fn continue_requires_all_resolutions_and_current_reviewed_index() {
    let (_dir, repo, _, _) = setup(Some(b"base"), Some(b"ours"), Some(b"theirs"), "file");
    let unresolved = Arc::new(repo.conflict_session().unwrap());
    assert!(
        repo.execute(Operation::ContinueConflict(unresolved.clone()))
            .is_err()
    );
    resolve(&repo, "file", Resolution::Ours);
    assert!(
        repo.execute(Operation::ContinueConflict(unresolved))
            .is_err()
    );
    let old_review = Arc::new(repo.conflict_session().unwrap());
    write(&repo, "unrelated", Some(b"new staged"));
    repo.execute(Operation::Stage(vec!["unrelated".into()]))
        .unwrap();
    assert!(
        repo.execute(Operation::ContinueConflict(old_review))
            .is_err()
    );
    repo.execute(Operation::ContinueConflict(Arc::new(
        repo.conflict_session().unwrap(),
    )))
    .unwrap();
}

#[test]
fn cherry_pick_and_revert_can_be_resolved_and_continued() {
    for revert in [false, true] {
        let (_dir, repo, head, incoming) =
            setup(Some(b"base\n"), Some(b"ours\n"), Some(b"theirs\n"), "file");
        repo.execute(Operation::AbortMerge).unwrap();
        repo.execute(Operation::SetIdentity(
            "Current Committer".into(),
            "committer@example.invalid".into(),
        ))
        .unwrap();
        assert!(
            repo.execute(if revert {
                Operation::Revert(incoming)
            } else {
                Operation::CherryPick(incoming)
            })
            .is_err()
        );
        assert_eq!(
            repo.conflict_session().unwrap().operation,
            if revert {
                ConflictOperation::Revert
            } else {
                ConflictOperation::CherryPick
            }
        );
        resolve(&repo, "file", Resolution::Edited("resolved\n".into()));
        repo.execute(Operation::ContinueConflict(Arc::new(
            repo.conflict_session().unwrap(),
        )))
        .unwrap();
        let raw = git2::Repository::open(&repo.root).unwrap();
        let c = raw.head().unwrap().peel_to_commit().unwrap();
        assert_eq!(c.parent_count(), 1);
        assert_eq!(c.parent_id(0).unwrap().to_string(), head);
        assert_eq!(c.committer().name().unwrap(), "Current Committer");
        assert_eq!(
            c.author().name().unwrap(),
            if revert {
                "Current Committer"
            } else {
                "Conflict Tester"
            }
        );
        assert_eq!(raw.state(), git2::RepositoryState::Clean);
    }
}

#[test]
fn literal_paths_deletion_and_large_preview_restrictions() {
    let (_dir, repo, _, _) = setup(Some(b"base"), Some(b"ours"), Some(b"theirs"), "a[1]*?.txt");
    resolve(&repo, "a[1]*?.txt", Resolution::Delete);
    assert!(repo.conflict_session().unwrap().files.is_empty());
    let big_ours = vec![b'a'; 2 * 1024 * 1024 + 1];
    let big_theirs = vec![b'b'; big_ours.len()];
    let (_dir, repo, _, _) = setup(Some(b"base"), Some(&big_ours), Some(&big_theirs), "big");
    let c = repo.conflict_context(Path::new("big")).unwrap();
    assert!(!c.editable);
    assert!(c.ours.unwrap().text.is_none());
    assert!(
        repo.execute(Operation::ResolveConflict {
            context: Arc::new(repo.conflict_context(Path::new("big")).unwrap()),
            resolution: Resolution::Edited("small".into())
        })
        .is_err()
    );
    resolve(&repo, "big", Resolution::Theirs);
    assert_eq!(staged(&repo, "big").unwrap(), big_theirs);
}

#[cfg(unix)]
#[test]
fn permission_and_symlink_changes_are_checked() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let (_dir, repo, _, _) = setup(Some(b"base"), Some(b"ours"), Some(b"theirs"), "file");
    let context = Arc::new(repo.conflict_context(Path::new("file")).unwrap());
    std::fs::set_permissions(
        repo.root.join("file"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    assert!(
        repo.execute(Operation::ResolveConflict {
            context,
            resolution: Resolution::Ours
        })
        .is_err()
    );
    resolve(&repo, "file", Resolution::Edited("resolved\n".into()));
    assert_eq!(
        git2::Repository::open(&repo.root)
            .unwrap()
            .index()
            .unwrap()
            .get_path(Path::new("file"), 0)
            .unwrap()
            .mode,
        0o100755
    );
    let (_dir, repo, _, _) = setup(Some(b"base"), Some(b"ours"), Some(b"theirs"), "file");
    let outside = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(outside.path(), b"keep outside").unwrap();
    std::fs::remove_file(repo.root.join("file")).unwrap();
    symlink(outside.path(), repo.root.join("file")).unwrap();
    assert!(repo.conflict_context(Path::new("file")).is_err());
    assert_eq!(std::fs::read(outside.path()).unwrap(), b"keep outside");
}

#[test]
fn resolving_one_file_keeps_other_conflicts_and_their_open_contexts_valid() {
    let (_dir, repo, _, _) = setup(Some(b"base"), Some(b"ours"), Some(b"theirs"), "file");
    let raw = git2::Repository::open(&repo.root).unwrap();
    let mut index = raw.index().unwrap();
    for stage in 1..=3 {
        let mut entry = index.get_path(Path::new("file"), stage).unwrap();
        entry.path = b"second".to_vec();
        index.add(&entry).unwrap();
    }
    index.write().unwrap();
    write(&repo, "second", Some(b"unresolved second"));
    let second = Arc::new(repo.conflict_context(Path::new("second")).unwrap());
    resolve(&repo, "file", Resolution::Ours);
    assert_eq!(repo.conflict_session().unwrap().files.len(), 1);
    repo.execute(Operation::ResolveConflict {
        context: second,
        resolution: Resolution::Theirs,
    })
    .unwrap();
    assert_eq!(staged(&repo, "file").unwrap(), b"ours");
    assert_eq!(staged(&repo, "second").unwrap(), b"theirs");
    assert!(repo.conflict_session().unwrap().files.is_empty());
}

#[test]
fn unsupported_index_types_and_changed_branch_are_rejected() {
    let (_dir, repo, _, _) = setup(Some(b"base"), Some(b"ours"), Some(b"theirs"), "file");
    let raw = git2::Repository::open(&repo.root).unwrap();
    let mut index = raw.index().unwrap();
    let mut entry = index.get_path(Path::new("file"), 2).unwrap();
    entry.mode = 0o120000;
    index.add(&entry).unwrap();
    index.write().unwrap();
    let context = Arc::new(repo.conflict_context(Path::new("file")).unwrap());
    assert!(!context.file.supported);
    let before = index_bytes(&repo);
    let bytes = std::fs::read(repo.root.join("file")).unwrap();
    assert!(
        repo.execute(Operation::ResolveConflict {
            context,
            resolution: Resolution::Ours
        })
        .is_err()
    );
    assert_eq!(index_bytes(&repo), before);
    assert_eq!(std::fs::read(repo.root.join("file")).unwrap(), bytes);
    let (_dir, repo, _, _) = setup(Some(b"base"), Some(b"ours"), Some(b"theirs"), "file");
    let context = Arc::new(repo.conflict_context(Path::new("file")).unwrap());
    let raw = git2::Repository::open(&repo.root).unwrap();
    let commit = raw.head().unwrap().peel_to_commit().unwrap();
    raw.branch("same-head", &commit, false).unwrap();
    raw.set_head("refs/heads/same-head").unwrap();
    let before = index_bytes(&repo);
    assert!(
        repo.execute(Operation::ResolveConflict {
            context,
            resolution: Resolution::Ours
        })
        .is_err()
    );
    assert_eq!(index_bytes(&repo), before);
}
