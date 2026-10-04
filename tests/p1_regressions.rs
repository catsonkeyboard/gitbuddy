//! Regression coverage for the nine P1 findings in AUDIT-2026-10-04.
use gitbuddy::git::{ConflictResolution, Operation, Repository};
use std::{fs, path::Path, process::Command, sync::Arc};

fn setup() -> (tempfile::TempDir, Repository) {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::init(dir.path().join("repo").as_path()).unwrap();
    repo.execute(Operation::SetIdentity(
        "Tester".into(),
        "test@example.invalid".into(),
    ))
    .unwrap();
    let raw = raw(&repo);
    let mut config = raw.config().unwrap();
    config.set_bool("commit.gpgsign", false).unwrap();
    config.set_bool("tag.gpgsign", false).unwrap();
    config.set_str("core.hooksPath", "/dev/null").unwrap();
    (dir, repo)
}
fn raw(repo: &Repository) -> git2::Repository {
    git2::Repository::open(&repo.root).unwrap()
}
fn write(repo: &Repository, name: &str, text: &str) {
    fs::write(repo.root.join(name), text).unwrap();
}
fn commit(repo: &Repository, message: &str) -> git2::Oid {
    repo.execute(Operation::StageAll).unwrap();
    repo.execute(Operation::Commit(message.into())).unwrap();
    raw(repo).head().unwrap().target().unwrap()
}
fn head(repo: &Repository) -> git2::Oid {
    raw(repo).head().unwrap().target().unwrap()
}
fn blob(repo: &Repository, revision: &str, path: &str) -> Vec<u8> {
    let raw = raw(repo);
    let tree = raw
        .revparse_single(revision)
        .unwrap()
        .peel_to_tree()
        .unwrap();
    raw.find_blob(tree.get_path(Path::new(path)).unwrap().id())
        .unwrap()
        .content()
        .to_vec()
}
fn git(repo: &Repository, args: &[&str]) -> std::process::Output {
    Command::new("git")
        .arg("-C")
        .arg(&repo.root)
        .args(args)
        .env("GIT_EDITOR", "true")
        .env("GIT_SEQUENCE_EDITOR", "true")
        .output()
        .unwrap()
}

// B01: preserve HEAD->index and index->worktree paths independently.
#[test]
fn chained_rename_diffs_and_stage_all_keep_final_file() {
    let (_dir, repo) = setup();
    write(&repo, "a", "original\n");
    commit(&repo, "base");
    fs::rename(repo.root.join("a"), repo.root.join("b")).unwrap();
    repo.execute(Operation::StageAll).unwrap();
    fs::rename(repo.root.join("b"), repo.root.join("c")).unwrap();
    let snapshot = repo.snapshot(100).unwrap();
    let staged = snapshot.files.iter().find(|f| f.staged()).unwrap();
    assert_eq!(staged.path, Path::new("b"));
    assert_eq!(staged.original.as_deref(), Some(Path::new("a")));
    assert!(!staged.unstaged());
    let unstaged = snapshot.files.iter().find(|f| f.unstaged()).unwrap();
    assert_eq!(unstaged.path, Path::new("c"));
    assert_eq!(unstaged.original.as_deref(), Some(Path::new("b")));
    assert!(!unstaged.staged());
    assert!(repo.diff(staged, true).unwrap().contains("b/b"));
    assert!(repo.diff(unstaged, false).unwrap().contains("b/c"));
    commit(&repo, "final rename");
    assert_eq!(blob(&repo, "HEAD", "c"), b"original\n");
    let r = raw(&repo);
    let tree = r.head().unwrap().peel_to_tree().unwrap();
    assert!(tree.get_path(Path::new("a")).is_err());
    assert!(tree.get_path(Path::new("b")).is_err());
    assert!(repo.snapshot(100).unwrap().files.is_empty());
}

#[test]
fn chained_rename_can_stage_worktree_delta_or_unstage_index_delta() {
    for stage in [true, false] {
        let (_dir, repo) = setup();
        write(&repo, "a", "original\n");
        commit(&repo, "base");
        fs::rename(repo.root.join("a"), repo.root.join("b")).unwrap();
        repo.execute(Operation::StageAll).unwrap();
        fs::rename(repo.root.join("b"), repo.root.join("c")).unwrap();
        let snapshot = repo.snapshot(100).unwrap();
        let file = snapshot
            .files
            .iter()
            .find(|f| if stage { f.unstaged() } else { f.staged() })
            .unwrap();
        let paths = vec![file.path.clone(), file.original.clone().unwrap()];
        repo.execute(if stage {
            Operation::Stage(paths)
        } else {
            Operation::Unstage(paths)
        })
        .unwrap();
        let r = raw(&repo);
        let index = r.index().unwrap();
        assert!(
            index
                .get_path(Path::new(if stage { "c" } else { "a" }), 0)
                .is_some()
        );
        assert!(index.get_path(Path::new("b"), 0).is_none());
        assert_eq!(fs::read(repo.root.join("c")).unwrap(), b"original\n");
    }
}

// B02: never delete a reused rename source after adding its new contents.
#[test]
fn rename_source_reused_as_new_file_is_committed_together_with_target() {
    let (_dir, repo) = setup();
    write(&repo, "a", "original\n");
    commit(&repo, "base");
    fs::rename(repo.root.join("a"), repo.root.join("b")).unwrap();
    repo.execute(Operation::StageAll).unwrap();
    write(&repo, "a", "new source\n");
    commit(&repo, "both files");
    assert_eq!(blob(&repo, "HEAD", "a"), b"new source\n");
    assert_eq!(blob(&repo, "HEAD", "b"), b"original\n");
    assert!(repo.snapshot(100).unwrap().files.is_empty());
}

// B03: UI sends full reference names; ambiguous legacy shorthand fails closed.
#[test]
fn merge_distinguishes_local_and_remote_refs_with_identical_shorthand() {
    for remote in [false, true] {
        let (_dir, repo) = setup();
        write(&repo, "base", "base");
        let base = commit(&repo, "base");
        repo.execute(Operation::CreateBranch("origin/main".into()))
            .unwrap();
        write(&repo, "local", "local");
        let local_id = commit(&repo, "local");
        repo.execute(Operation::Checkout("main".into())).unwrap();
        repo.execute(Operation::CreateBranch("incoming".into()))
            .unwrap();
        write(&repo, "remote", "remote");
        let remote_id = commit(&repo, "remote");
        let r = raw(&repo);
        r.reference("refs/remotes/origin/main", remote_id, false, "fixture")
            .unwrap();
        repo.execute(Operation::Checkout("main".into())).unwrap();
        assert!(
            repo.execute(Operation::Merge("origin/main".into()))
                .unwrap_err()
                .to_string()
                .contains("Ambiguous")
        );
        assert_eq!(head(&repo), base);
        let reference = if remote {
            "refs/remotes/origin/main"
        } else {
            "refs/heads/origin/main"
        };
        repo.execute(Operation::Merge(reference.into())).unwrap();
        assert_eq!(head(&repo), if remote { remote_id } else { local_id });
        assert!(
            repo.root
                .join(if remote { "remote" } else { "local" })
                .is_file()
        );
        assert!(
            !repo
                .root
                .join(if remote { "local" } else { "remote" })
                .exists()
        );
    }
}

// B04: an externally owned rebase can still finish all its remaining commits.
#[test]
fn ordinary_commit_preserves_external_merge_and_apply_rebase_state() {
    for mode in ["--merge", "--apply"] {
        let (_dir, repo) = setup();
        write(&repo, "a", "base\n");
        commit(&repo, "base");
        repo.execute(Operation::CreateBranch("feature".into()))
            .unwrap();
        write(&repo, "a", "feature\n");
        commit(&repo, "first");
        write(&repo, "b", "second step\n");
        let original = commit(&repo, "second");
        repo.execute(Operation::Checkout("main".into())).unwrap();
        write(&repo, "a", "main\n");
        commit(&repo, "main");
        repo.execute(Operation::Checkout("feature".into())).unwrap();
        let out = git(&repo, &["rebase", mode, "main"]);
        assert!(!out.status.success(), "Expected conflict: {:?}", out);
        write(&repo, "a", "resolved\n");
        repo.execute(Operation::Stage(vec!["a".into()])).unwrap();
        let before = head(&repo);
        let r = raw(&repo);
        let state = r.state();
        let directory = r.path().join(if mode == "--merge" {
            "rebase-merge"
        } else {
            "rebase-apply"
        });
        let names: Vec<_> = fs::read_dir(&directory)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        let index = fs::read(r.path().join("index")).unwrap();
        assert!(
            repo.execute(Operation::Commit("unsafe ordinary commit".into()))
                .is_err()
        );
        assert_eq!(head(&repo), before);
        assert_eq!(raw(&repo).state(), state);
        assert_eq!(
            raw(&repo)
                .find_reference("refs/heads/feature")
                .unwrap()
                .target(),
            Some(original)
        );
        assert_eq!(fs::read(r.path().join("index")).unwrap(), index);
        for name in names {
            assert!(directory.join(name).exists());
        }
        let out = git(&repo, &["rebase", "--continue"]);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(blob(&repo, "HEAD", "b"), b"second step\n");
        assert_eq!(raw(&repo).state(), git2::RepositoryState::Clean);
        assert!(!directory.exists());
    }
}

// B05: a marker alone never authorizes committing the old index.
#[test]
fn interrupted_apply_or_legacy_journal_never_skips_required_patch() {
    for (acknowledged_field, marker, stale_lock) in [
        (true, false, false),
        (true, true, false),
        (true, true, true),
        (false, true, false),
    ] {
        let (_dir, repo) = setup();
        write(&repo, "base", "base");
        let base = commit(&repo, "base");
        write(&repo, "required", "important change");
        let original = commit(&repo, "feature");
        let preview = repo.rebase_preview(&base.to_string()).unwrap();
        let r = raw(&repo);
        r.reference(
            "refs/gitbuddy/crash-backup",
            original,
            false,
            "fixture backup",
        )
        .unwrap();
        let onto = r.find_commit(base).unwrap();
        r.checkout_tree(
            onto.as_object(),
            Some(git2::build::CheckoutBuilder::new().force()),
        )
        .unwrap();
        r.set_head_detached(base).unwrap();
        let mut state = serde_json::json!({
            "version": 1, "branch": preview.head.reference, "original": original.to_string(),
            "onto": base.to_string(), "current": base.to_string(), "steps": preview.steps,
            "cursor": 0, "pending": true, "backup": "refs/gitbuddy/crash-backup",
            "prepared": null, "finishing": false
        });
        if acknowledged_field {
            state["applied"] = serde_json::json!(false);
        }
        let journal = r.path().join("gitbuddy-rebase.json");
        let bytes = serde_json::to_vec(&state).unwrap();
        fs::write(&journal, &bytes).unwrap();
        if marker {
            fs::write(r.path().join("CHERRY_PICK_HEAD"), format!("{}\n", original)).unwrap();
            fs::write(r.path().join("MERGE_MSG"), "feature\n").unwrap();
        }
        if stale_lock {
            fs::write(r.path().join("index.lock"), []).unwrap();
        }
        let index_before = fs::read(r.path().join("index")).unwrap();
        let error = repo.execute(Operation::ContinueRebase).unwrap_err();
        assert!(
            error.to_string().contains("application cannot be verified"),
            "{error:#}"
        );
        assert_eq!(head(&repo), base);
        assert_eq!(
            r.find_reference("refs/heads/main").unwrap().target(),
            Some(original)
        );
        assert_eq!(fs::read(&journal).unwrap(), bytes);
        assert_eq!(fs::read(r.path().join("index")).unwrap(), index_before);
        assert!(repo.rebase_status().unwrap().is_some());
        if stale_lock {
            fs::remove_file(r.path().join("index.lock")).unwrap();
        }
        repo.execute(Operation::AbortRebase).unwrap();
        assert_eq!(head(&repo), original);
        assert_eq!(blob(&repo, "HEAD", "required"), b"important change");
        assert!(repo.rebase_status().unwrap().is_none());
    }
}

// B06: all resolutions use clean, retaining the worktree body and cache object.
#[test]
fn lfs_conflict_resolutions_commit_pointers_in_merge_and_rebase() {
    for rebase in [false, true] {
        for choice in ["edited", "worktree", "ours", "theirs", "delete"] {
            let (_dir, repo) = setup();
            repo.execute(Operation::LfsTrack {
                pattern: "*.bin".into(),
                track: true,
            })
            .unwrap();
            write(&repo, "data.bin", "base body\n");
            commit(&repo, "base");
            repo.execute(Operation::CreateBranch("feature".into()))
                .unwrap();
            write(&repo, "data.bin", "feature body\n");
            let feature = commit(&repo, "feature");
            repo.execute(Operation::Checkout("main".into())).unwrap();
            write(&repo, "data.bin", "main body\n");
            let main = commit(&repo, "main");
            let result = if rebase {
                repo.execute(Operation::Checkout("feature".into())).unwrap();
                let plan = repo.rebase_preview("main").unwrap();
                repo.execute(Operation::Rebase {
                    steps: plan.steps.clone(),
                    context: Arc::new(plan),
                })
            } else {
                repo.execute(Operation::Merge("refs/heads/feature".into()))
            };
            assert!(result.is_err());
            if choice == "worktree" {
                write(&repo, "data.bin", "resolved body\n");
            }
            let context = repo.conflict_context(Path::new("data.bin")).unwrap();
            let resolution = match choice {
                "edited" => ConflictResolution::Edited("resolved body\n".into()),
                "worktree" => ConflictResolution::Worktree,
                "ours" => ConflictResolution::Ours,
                "theirs" => ConflictResolution::Theirs,
                _ => ConflictResolution::Delete,
            };
            repo.execute(Operation::ResolveConflict {
                context: Arc::new(context),
                resolution,
            })
            .unwrap();
            // Preserve an unrelated staged edit while the conflict result is saved.
            write(&repo, "unrelated", "keep me");
            repo.execute(Operation::Stage(vec!["unrelated".into()]))
                .unwrap();
            let session = repo.conflict_session().unwrap();
            assert!(session.files.is_empty());
            repo.execute(Operation::ContinueConflict(Arc::new(session)))
                .unwrap();
            assert_eq!(blob(&repo, "HEAD", "unrelated"), b"keep me");
            if choice == "delete" {
                assert!(!repo.root.join("data.bin").exists());
                assert!(
                    raw(&repo)
                        .head()
                        .unwrap()
                        .peel_to_tree()
                        .unwrap()
                        .get_path(Path::new("data.bin"))
                        .is_err()
                );
                continue;
            }
            let expected = match choice {
                "ours" => b"main body\n".as_slice(),
                "theirs" => b"feature body\n".as_slice(),
                _ => b"resolved body\n".as_slice(),
            };
            let pointer = blob(&repo, "HEAD", "data.bin");
            assert!(pointer.starts_with(b"version https://git-lfs.github.com/spec/v1\n"));
            assert_ne!(pointer.as_slice(), expected);
            let status = repo.lfs_status().unwrap();
            assert_eq!(status.files.len(), 1);
            let file = &status.files[0];
            assert!(file.available && file.hydrated);
            assert_eq!(file.size, expected.len() as u64);
            let object = raw(&repo)
                .commondir()
                .join("lfs/objects")
                .join(&file.oid[..2])
                .join(&file.oid[2..4])
                .join(&file.oid);
            assert_eq!(fs::read(object).unwrap(), expected);
            assert_eq!(fs::read(repo.root.join("data.bin")).unwrap(), expected);
            let r = raw(&repo);
            let c = r.head().unwrap().peel_to_commit().unwrap();
            if !rebase {
                assert_eq!(c.parent_ids().collect::<Vec<_>>(), vec![main, feature]);
            }
            assert!(repo.snapshot(100).unwrap().files.is_empty());
        }
    }
}

#[test]
fn malformed_lfs_conflict_pointer_does_not_modify_index_or_worktree() {
    let (_dir, repo) = setup();
    repo.execute(Operation::LfsTrack {
        pattern: "*.bin".into(),
        track: true,
    })
    .unwrap();
    write(&repo, "data.bin", "base");
    commit(&repo, "base");
    repo.execute(Operation::CreateBranch("feature".into()))
        .unwrap();
    write(&repo, "data.bin", "feature");
    commit(&repo, "feature");
    repo.execute(Operation::Checkout("main".into())).unwrap();
    write(&repo, "data.bin", "main");
    commit(&repo, "main");
    assert!(repo.execute(Operation::Merge("feature".into())).is_err());
    let context = repo.conflict_context(Path::new("data.bin")).unwrap();
    let before = fs::read(raw(&repo).path().join("index")).unwrap();
    let work = fs::read(repo.root.join("data.bin")).unwrap();
    let result = repo.execute(Operation::ResolveConflict {
        context: Arc::new(context),
        resolution: ConflictResolution::Edited(
            "version https://git-lfs.github.com/spec/v1\ninvalid pointer\n".into(),
        ),
    });
    assert!(result.is_err());
    assert_eq!(fs::read(raw(&repo).path().join("index")).unwrap(), before);
    assert_eq!(fs::read(repo.root.join("data.bin")).unwrap(), work);
    assert!(raw(&repo).index().unwrap().has_conflicts());
}

// B07: quoted patterns cannot sneak through an indirect macro assignment.
#[test]
fn quoted_lfs_macros_fail_before_any_index_change() {
    for all in [false, true] {
        let (_dir, repo) = setup();
        write(&repo, "base", "base");
        commit(&repo, "base");
        write(
            &repo,
            ".gitattributes",
            "[attr]bigfile filter=lfs diff=lfs merge=lfs -text\n\"my file.bin\" bigfile\n",
        );
        write(&repo, "my file.bin", "must not enter Git");
        let before = fs::read(raw(&repo).path().join("index")).unwrap();
        let op = if all {
            Operation::StageAll
        } else {
            Operation::Stage(vec!["my file.bin".into()])
        };
        assert!(repo.execute(op).unwrap_err().to_string().contains("Quoted"));
        assert_eq!(fs::read(raw(&repo).path().join("index")).unwrap(), before);
        assert_eq!(
            fs::read(repo.root.join("my file.bin")).unwrap(),
            b"must not enter Git"
        );
    }
}

#[test]
fn unquoted_lfs_macro_still_stages_a_pointer() {
    let (_dir, repo) = setup();
    write(
        &repo,
        ".gitattributes",
        "[attr]bigfile filter=lfs diff=lfs merge=lfs -text\n*.bin bigfile\n",
    );
    write(&repo, "my file.bin", "body");
    commit(&repo, "macro");
    assert!(
        blob(&repo, "HEAD", "my file.bin")
            .starts_with(b"version https://git-lfs.github.com/spec/v1\n")
    );
}

#[test]
fn quoted_macros_in_index_global_and_info_attributes_cannot_bypass_guard() {
    for source in ["index", "global", "info", "non_utf8"] {
        let (dir, repo) = setup();
        write(&repo, "base", "base");
        commit(&repo, "base");
        let rules = b"[attr]bigfile filter=lfs diff=lfs merge=lfs -text\n\"my file.bin\" bigfile\n";
        let r = raw(&repo);
        match source {
            "index" => {
                fs::write(repo.root.join(".gitattributes"), rules).unwrap();
                // External Git may have added attribute syntax we cannot interpret.
                let mut index = r.index().unwrap();
                index.add_path(Path::new(".gitattributes")).unwrap();
                index.write().unwrap();
                fs::remove_file(repo.root.join(".gitattributes")).unwrap();
            }
            "global" => {
                let path = dir.path().join("global-attributes");
                fs::write(&path, rules).unwrap();
                r.config()
                    .unwrap()
                    .set_str("core.attributesfile", path.to_str().unwrap())
                    .unwrap();
            }
            "info" => {
                fs::create_dir_all(r.commondir().join("info")).unwrap();
                fs::write(r.commondir().join("info/attributes"), rules).unwrap();
            }
            _ => {
                let mut bytes = rules.to_vec();
                bytes.extend_from_slice(b"# non UTF-8 comment \xff\n");
                fs::write(repo.root.join(".gitattributes"), bytes).unwrap();
            }
        }
        write(&repo, "my file.bin", "raw body");
        let before = fs::read(r.path().join("index")).unwrap();
        assert!(
            repo.execute(Operation::Stage(vec!["my file.bin".into()]))
                .unwrap_err()
                .to_string()
                .contains("Quoted")
        );
        assert_eq!(fs::read(r.path().join("index")).unwrap(), before);
    }
}

// B08: ignore=all is a display setting, not permission to delete child edits.
#[test]
fn worktree_removal_preserves_ignored_submodule_changes() {
    for kind in ["modified", "staged", "untracked", "head", "clean"] {
        let (source_dir, source) = setup();
        write(&source, "file", "committed\n");
        commit(&source, "source");
        let (dir, repo) = setup();
        write(&repo, "base", "base");
        commit(&repo, "base");
        repo.execute(Operation::AddSubmodule {
            url: source.root.display().to_string(),
            path: "child".into(),
        })
        .unwrap();
        git2::Config::open(&repo.root.join(".gitmodules"))
            .unwrap()
            .set_str("submodule.child.ignore", "all")
            .unwrap();
        commit(&repo, "submodule");
        let path = dir.path().join("topic");
        repo.execute(Operation::CreateWorktree {
            name: "topic".into(),
            path: path.clone(),
        })
        .unwrap();
        fs::remove_dir_all(path.join("child")).unwrap();
        git2::Repository::clone(source.root.to_str().unwrap(), path.join("child")).unwrap();
        let child = Repository::open(path.join("child")).unwrap();
        child
            .execute(Operation::SetIdentity(
                "Tester".into(),
                "test@example.invalid".into(),
            ))
            .unwrap();
        match kind {
            "modified" => write(&child, "file", "important edit"),
            "staged" => {
                write(&child, "file", "important edit");
                child.execute(Operation::StageAll).unwrap();
            }
            "untracked" => write(&child, "draft", "important draft"),
            "head" => {
                write(&child, "file", "important commit");
                commit(&child, "local change");
            }
            _ => {}
        }
        let linked = Repository::open(&path).unwrap();
        // Normal parent statuses honor ignore=all and intentionally hide this.
        assert!(linked.snapshot(100).unwrap().files.is_empty());
        let info = repo.worktrees().unwrap().remove(0);
        assert_eq!(info.dirty, kind != "clean");
        let result = repo.execute(Operation::RemoveWorktree(info));
        if kind == "clean" {
            result.unwrap();
            assert!(!path.exists());
        } else {
            assert!(result.is_err());
            assert!(path.join("child/file").is_file());
            if kind == "untracked" {
                assert_eq!(
                    fs::read(path.join("child/draft")).unwrap(),
                    b"important draft"
                );
            } else {
                assert!(
                    fs::read(path.join("child/file"))
                        .unwrap()
                        .starts_with(b"important")
                );
            }
            assert_eq!(repo.worktrees().unwrap().len(), 1);
        }
        drop(source_dir);
    }
}

#[test]
fn worktree_removal_checks_nested_submodules_even_when_each_parent_ignores_them() {
    let (_leaf_dir, leaf) = setup();
    write(&leaf, "file", "leaf");
    commit(&leaf, "leaf");
    let (_middle_dir, middle) = setup();
    write(&middle, "file", "middle");
    commit(&middle, "middle");
    middle
        .execute(Operation::AddSubmodule {
            url: leaf.root.display().to_string(),
            path: "grandchild".into(),
        })
        .unwrap();
    git2::Config::open(&middle.root.join(".gitmodules"))
        .unwrap()
        .set_str("submodule.grandchild.ignore", "all")
        .unwrap();
    commit(&middle, "nested submodule");
    let (dir, repo) = setup();
    write(&repo, "base", "base");
    commit(&repo, "base");
    repo.execute(Operation::AddSubmodule {
        url: middle.root.display().to_string(),
        path: "child".into(),
    })
    .unwrap();
    git2::Config::open(&repo.root.join(".gitmodules"))
        .unwrap()
        .set_str("submodule.child.ignore", "all")
        .unwrap();
    commit(&repo, "child");
    let path = dir.path().join("topic");
    repo.execute(Operation::CreateWorktree {
        name: "topic".into(),
        path: path.clone(),
    })
    .unwrap();
    fs::remove_dir_all(path.join("child")).unwrap();
    git2::Repository::clone(middle.root.to_str().unwrap(), path.join("child")).unwrap();
    fs::remove_dir_all(path.join("child/grandchild")).unwrap();
    git2::Repository::clone(leaf.root.to_str().unwrap(), path.join("child/grandchild")).unwrap();
    let important = path.join("child/grandchild/draft");
    fs::write(&important, "nested draft").unwrap();
    assert!(
        Repository::open(&path)
            .unwrap()
            .snapshot(100)
            .unwrap()
            .files
            .is_empty()
    );
    let info = repo.worktrees().unwrap().remove(0);
    assert!(info.dirty);
    assert!(repo.execute(Operation::RemoveWorktree(info)).is_err());
    assert_eq!(fs::read(&important).unwrap(), b"nested draft");
    fs::remove_file(important).unwrap();
    let info = repo.worktrees().unwrap().remove(0);
    assert!(!info.dirty);
    repo.execute(Operation::RemoveWorktree(info)).unwrap();
    assert!(!path.exists());
}

// B09: deletion and application follow the selected OID after reflog shifts.
#[test]
fn stash_drop_uses_selected_object_after_external_push() {
    let (_dir, repo) = setup();
    write(&repo, "base", "base");
    commit(&repo, "base");
    write(&repo, "old", "old draft");
    repo.execute(Operation::Stash("old".into())).unwrap();
    let selected = repo.snapshot(100).unwrap().stashes[0].0.clone();
    write(&repo, "new", "new draft");
    let mut r = raw(&repo);
    let signature = r.signature().unwrap();
    let new_id = r
        .stash_save(
            &signature,
            "external new stash",
            Some(git2::StashFlags::INCLUDE_UNTRACKED),
        )
        .unwrap();
    repo.execute(Operation::DropStash(selected)).unwrap();
    let snapshot = repo.snapshot(100).unwrap();
    assert_eq!(snapshot.stashes.len(), 1);
    assert_eq!(snapshot.stashes[0].0, new_id.to_string());
    repo.execute(Operation::ApplyStash(new_id.to_string()))
        .unwrap();
    assert_eq!(fs::read(repo.root.join("new")).unwrap(), b"new draft");
    assert!(!repo.root.join("old").exists());
    repo.execute(Operation::DropStash(new_id.to_string()))
        .unwrap();
    assert!(repo.snapshot(100).unwrap().stashes.is_empty());
}

#[test]
fn stash_apply_uses_selected_object_after_external_push() {
    let (_dir, repo) = setup();
    write(&repo, "base", "base");
    commit(&repo, "base");
    write(&repo, "old", "old draft");
    repo.execute(Operation::Stash("old".into())).unwrap();
    let selected = repo.snapshot(100).unwrap().stashes[0].0.clone();
    write(&repo, "new", "new draft");
    let mut r = raw(&repo);
    let signature = r.signature().unwrap();
    r.stash_save(
        &signature,
        "external new stash",
        Some(git2::StashFlags::INCLUDE_UNTRACKED),
    )
    .unwrap();
    repo.execute(Operation::ApplyStash(selected)).unwrap();
    assert_eq!(fs::read(repo.root.join("old")).unwrap(), b"old draft");
    assert!(!repo.root.join("new").exists());
    assert_eq!(repo.snapshot(100).unwrap().stashes.len(), 2);
}

#[test]
fn disappeared_stash_or_held_ref_lock_cannot_apply_or_delete_another_stash() {
    for apply in [false, true] {
        let (_dir, repo) = setup();
        write(&repo, "base", "base");
        commit(&repo, "base");
        write(&repo, "old", "old");
        repo.execute(Operation::Stash("old".into())).unwrap();
        let selected = repo.snapshot(100).unwrap().stashes[0].0.clone();
        write(&repo, "new", "new");
        repo.execute(Operation::Stash("new".into())).unwrap();
        let r = raw(&repo);
        let before = repo.snapshot(100).unwrap().stashes;
        let mut lock = r.transaction().unwrap();
        lock.lock_ref("refs/stash").unwrap();
        let operation = || {
            if apply {
                Operation::ApplyStash(selected.clone())
            } else {
                Operation::DropStash(selected.clone())
            }
        };
        assert!(repo.execute(operation()).is_err());
        assert_eq!(repo.snapshot(100).unwrap().stashes, before);
        assert!(!repo.root.join("old").exists());
        drop(lock);
        repo.execute(Operation::DropStash(selected.clone()))
            .unwrap();
        let before = repo.snapshot(100).unwrap().stashes;
        assert!(repo.execute(operation()).is_err());
        assert_eq!(repo.snapshot(100).unwrap().stashes, before);
        assert!(repo.snapshot(100).unwrap().files.is_empty());
    }
}
