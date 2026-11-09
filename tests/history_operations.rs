use gitbuddy::git::{Operation, PatchSelection, RebaseAction, Repository, ResetMode};
use std::{fs, sync::Arc};

fn setup() -> (tempfile::TempDir, Repository, String, String, String) {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::init(&dir.path().join("repo")).unwrap();
    repo.execute(Operation::SetIdentity(
        "History Author".into(),
        "history@example.invalid".into(),
    ))
    .unwrap();
    fs::write(
        repo.root.join("file"),
        (0..20).map(|n| format!("line {n}\n")).collect::<String>(),
    )
    .unwrap();
    let base = commit(&repo, "base");
    let text = fs::read_to_string(repo.root.join("file"))
        .unwrap()
        .replace("line 1\n", "first change\n")
        .replace("line 18\n", "second change\n");
    fs::write(repo.root.join("file"), text).unwrap();
    fs::write(repo.root.join("added"), "new file\n").unwrap();
    let old = commit(&repo, "old commit");
    fs::write(repo.root.join("later"), "later change\n").unwrap();
    let tip = commit(&repo, "descendant");
    (dir, repo, base, old, tip)
}
fn raw(repo: &Repository) -> git2::Repository {
    git2::Repository::open(&repo.root).unwrap()
}
fn commit(repo: &Repository, message: &str) -> String {
    repo.execute(Operation::StageAll).unwrap();
    repo.execute(Operation::Commit(message.into())).unwrap();
    repo.snapshot(20).unwrap().head_id.unwrap()
}
fn start(repo: &Repository, ids: &[String], action: RebaseAction) {
    let (context, steps) = repo.history_rewrite_preview(ids, action).unwrap();
    repo.execute(Operation::Rebase {
        context: Arc::new(context),
        steps,
    })
    .unwrap();
}
fn replacement(repo: &Repository, message: &str) -> String {
    repo.execute(Operation::CommitRebase(message.into()))
        .unwrap();
    repo.snapshot(20).unwrap().head_id.unwrap()
}

#[test]
fn branch_from_any_commit_does_not_checkout_or_overwrite() {
    let (_dir, repo, base, _, tip) = setup();
    fs::write(repo.root.join("file"), "dirty").unwrap();
    repo.execute(Operation::CreateBranchAt {
        name: "from-old".into(),
        target: base.clone(),
    })
    .unwrap();
    let r = raw(&repo);
    assert_eq!(
        r.find_branch("from-old", git2::BranchType::Local)
            .unwrap()
            .get()
            .target()
            .unwrap()
            .to_string(),
        base
    );
    assert_eq!(r.head().unwrap().target().unwrap().to_string(), tip);
    assert_eq!(fs::read_to_string(repo.root.join("file")).unwrap(), "dirty");
    assert!(
        repo.execute(Operation::CreateBranchAt {
            name: "from-old".into(),
            target: tip.clone()
        })
        .is_err()
    );
    assert!(
        repo.execute(Operation::CreateBranchAt {
            name: "bad..name".into(),
            target: tip
        })
        .is_err()
    );
}

#[test]
fn reset_modes_have_distinct_index_and_worktree_effects_and_backups() {
    for mode in [ResetMode::Soft, ResetMode::Mixed, ResetMode::Hard] {
        let (_dir, repo, base, _, tip) = setup();
        fs::write(repo.root.join("file"), "staged text\n").unwrap();
        repo.execute(Operation::Stage(vec!["file".into()])).unwrap();
        let staged = raw(&repo).index().unwrap().write_tree().unwrap();
        fs::write(repo.root.join("file"), "working text\n").unwrap();
        fs::write(repo.root.join("untracked"), "keep\n").unwrap();
        let context = repo.prepare_reset(&base).unwrap();
        repo.execute(Operation::Reset {
            context: Arc::new(context),
            mode,
        })
        .unwrap();
        let r = raw(&repo);
        assert_eq!(r.head().unwrap().target().unwrap().to_string(), base);
        assert_eq!(
            r.find_reference("ORIG_HEAD")
                .unwrap()
                .target()
                .unwrap()
                .to_string(),
            tip
        );
        assert!(
            r.references_glob("refs/gitbuddy/rewrites/reset-*")
                .unwrap()
                .any(|r| r.unwrap().target().unwrap().to_string() == tip)
        );
        let tree = r.find_commit(base.parse().unwrap()).unwrap().tree_id();
        assert_eq!(
            r.index().unwrap().write_tree().unwrap(),
            if mode == ResetMode::Soft {
                staged
            } else {
                tree
            }
        );
        assert_eq!(
            fs::read_to_string(repo.root.join("file")).unwrap(),
            if mode == ResetMode::Hard {
                (0..20).map(|n| format!("line {n}\n")).collect::<String>()
            } else {
                "working text\n".into()
            }
        );
        assert_eq!(
            fs::read_to_string(repo.root.join("untracked")).unwrap(),
            "keep\n"
        );
        if mode == ResetMode::Hard {
            assert!(!repo.root.join("added").exists());
            assert!(!repo.root.join("later").exists());
        }
    }
}

#[test]
fn reset_stale_head_index_worktree_and_other_repository_are_rejected() {
    for mutation in 0..4 {
        let (_dir, repo, base, _, tip) = setup();
        let context = repo.prepare_reset(&base).unwrap();
        match mutation {
            0 => {
                fs::write(repo.root.join("new"), "new").unwrap();
            }
            1 => {
                repo.execute(Operation::UnstageAll).unwrap();
                fs::write(repo.root.join("file"), "different").unwrap();
                repo.execute(Operation::StageAll).unwrap();
            }
            2 => {
                repo.execute(Operation::CreateBranch("switched".into()))
                    .unwrap();
            }
            _ => {
                let (_d, other, _, _, _) = setup();
                assert!(
                    other
                        .execute(Operation::Reset {
                            context: Arc::new(context),
                            mode: ResetMode::Hard
                        })
                        .is_err()
                );
                continue;
            }
        }
        assert!(
            repo.execute(Operation::Reset {
                context: Arc::new(context),
                mode: ResetMode::Hard
            })
            .is_err()
        );
        assert_eq!(repo.snapshot(1).unwrap().head_id, Some(tip));
        assert!(
            !raw(&repo)
                .references_glob("refs/gitbuddy/rewrites/reset-*")
                .unwrap()
                .any(|_| true)
        );
    }
}

#[test]
fn reset_detached_head_is_supported_and_locked_refs_leave_files_untouched() {
    let (_dir, repo, base, _, tip) = setup();
    let r = raw(&repo);
    r.set_head_detached(tip.parse().unwrap()).unwrap();
    let context = Arc::new(repo.prepare_reset(&base).unwrap());
    let mut lock = r.transaction().unwrap();
    lock.lock_ref("HEAD").unwrap();
    let content = fs::read(repo.root.join("file")).unwrap();
    assert!(
        repo.execute(Operation::Reset {
            context: context.clone(),
            mode: ResetMode::Hard
        })
        .is_err()
    );
    assert_eq!(fs::read(repo.root.join("file")).unwrap(), content);
    drop(lock);
    repo.execute(Operation::Reset {
        context,
        mode: ResetMode::Soft,
    })
    .unwrap();
    assert!(r.head_detached().unwrap());
    assert_eq!(repo.snapshot(1).unwrap().head_id, Some(base));
}

#[test]
fn split_old_commit_by_hunk_survives_reopen_and_replays_descendants() {
    let (_dir, repo, base, old, tip) = setup();
    let final_tree = raw(&repo)
        .find_commit(tip.parse().unwrap())
        .unwrap()
        .tree_id();
    start(&repo, std::slice::from_ref(&old), RebaseAction::Split);
    let status = repo.rebase_status().unwrap().unwrap();
    assert!(status.editing);
    assert_eq!(status.completed, 0);
    assert_eq!(repo.snapshot(20).unwrap().head_id, Some(base.clone()));
    assert!(!repo.snapshot(20).unwrap().can_commit());
    assert!(repo.execute(Operation::ContinueRebase).is_err());
    assert!(repo.execute(Operation::Commit("bypass".into())).is_err());
    let file = repo
        .snapshot(20)
        .unwrap()
        .files
        .into_iter()
        .find(|f| f.path.to_str() == Some("file"))
        .unwrap();
    let patch = repo.worktree_patch(&file, false).unwrap();
    let hunks: Vec<_> = patch
        .lines
        .iter()
        .enumerate()
        .filter(|(_, l)| l.kind == '@')
        .map(|(i, _)| i)
        .collect();
    assert_eq!(hunks.len(), 2);
    repo.execute(Operation::ApplyPartial {
        patch: patch.partial.unwrap(),
        selection: PatchSelection::Hunk(hunks[0]),
    })
    .unwrap();
    assert!(repo.snapshot(20).unwrap().can_commit());
    let first = replacement(&repo, "split first hunk");
    assert!(repo.execute(Operation::ContinueRebase).is_err());
    let repo = Repository::open(&repo.root).unwrap();
    assert_eq!(
        repo.rebase_status().unwrap().unwrap().replacement_commits,
        1
    );
    repo.execute(Operation::StageAll).unwrap();
    let second = replacement(&repo, "split remainder");
    repo.execute(Operation::ContinueRebase).unwrap();
    let r = raw(&repo);
    let head = r.head().unwrap().peel_to_commit().unwrap();
    assert_eq!(head.tree_id(), final_tree);
    assert_eq!(head.message(), Ok("descendant"));
    assert_eq!(head.parent_id(0).unwrap().to_string(), second);
    assert_eq!(
        head.parent(0).unwrap().parent_id(0).unwrap().to_string(),
        first
    );
    assert_eq!(
        r.find_commit(first.parse().unwrap())
            .unwrap()
            .parent_id(0)
            .unwrap()
            .to_string(),
        base
    );
    assert!(repo.rebase_status().unwrap().is_none());
    assert!(repo.snapshot(20).unwrap().files.is_empty());
    assert_eq!(
        r.find_reference(&status.backup)
            .unwrap()
            .target()
            .unwrap()
            .to_string(),
        tip
    );
    assert_eq!(
        r.find_commit(second.parse().unwrap())
            .unwrap()
            .author()
            .name(),
        Ok("History Author")
    );
}

#[test]
fn edit_old_commit_modifies_content_message_and_keeps_later_commit() {
    let (_dir, repo, _, old, _) = setup();
    start(&repo, std::slice::from_ref(&old), RebaseAction::Edit);
    assert!(repo.snapshot(20).unwrap().can_commit());
    fs::write(repo.root.join("file"), "edited old content\n").unwrap();
    repo.execute(Operation::StageAll).unwrap();
    let edited = replacement(&repo, "new old message");
    repo.execute(Operation::ContinueRebase).unwrap();
    let r = raw(&repo);
    let head = r.head().unwrap().peel_to_commit().unwrap();
    assert_eq!(head.message(), Ok("descendant"));
    assert_eq!(head.parent_id(0).unwrap().to_string(), edited);
    assert_eq!(
        fs::read_to_string(repo.root.join("file")).unwrap(),
        "edited old content\n"
    );
    assert!(repo.root.join("later").exists());
}

#[test]
fn editing_multiple_commits_pauses_separately_and_abort_restores_original() {
    let (_dir, repo, _, old, tip) = setup();
    start(&repo, &[old, tip.clone()], RebaseAction::Edit);
    replacement(&repo, "edit first");
    repo.execute(Operation::ContinueRebase).unwrap();
    assert_eq!(repo.rebase_status().unwrap().unwrap().completed, 1);
    replacement(&repo, "edit second");
    repo.execute(Operation::AbortRebase).unwrap();
    assert_eq!(repo.snapshot(20).unwrap().head_id, Some(tip));
    assert!(repo.snapshot(20).unwrap().files.is_empty());
    assert!(!raw(&repo).head_detached().unwrap());
}

#[test]
fn history_selection_validates_ancestry_contiguity_and_clean_state() {
    let (_dir, repo, base, old, tip) = setup();
    assert!(
        repo.history_rewrite_preview(std::slice::from_ref(&base), RebaseAction::Edit)
            .is_err()
    );
    assert!(
        repo.history_rewrite_preview(&[old.clone(), old.clone()], RebaseAction::Split)
            .is_err()
    );
    assert!(
        repo.history_rewrite_preview(std::slice::from_ref(&old), RebaseAction::Squash)
            .is_err()
    );
    fs::write(repo.root.join("third"), "third").unwrap();
    let third = commit(&repo, "third");
    assert!(
        repo.history_rewrite_preview(&[old.clone(), third], RebaseAction::Squash)
            .is_err()
    );
    let (preview, steps) = repo
        .history_rewrite_preview(&[tip, old.clone()], RebaseAction::Squash)
        .unwrap();
    assert_eq!(preview.base, base);
    assert_eq!(steps[0].action, RebaseAction::Pick);
    assert_eq!(steps[1].action, RebaseAction::Squash);
    assert_eq!(steps[2].action, RebaseAction::Pick);
    fs::write(repo.root.join("dirty"), "dirty").unwrap();
    assert!(
        repo.history_rewrite_preview(&[old], RebaseAction::Edit)
            .is_err()
    );
}

#[test]
fn edit_rejects_external_head_change_and_ordinary_commit_outside_pause() {
    let (_dir, repo, base, old, _) = setup();
    assert!(
        repo.execute(Operation::CommitRebase("outside".into()))
            .is_err()
    );
    start(&repo, &[old], RebaseAction::Edit);
    let r = raw(&repo);
    r.set_head_detached(base.parse().unwrap()).unwrap();
    // Same base is allowed, but another commit or branch is not.
    r.set_head("refs/heads/main").unwrap();
    assert!(
        repo.execute(Operation::CommitRebase("wrong head".into()))
            .is_err()
    );
    assert!(repo.execute(Operation::ContinueRebase).is_err());
}

#[test]
fn split_reset_modes_are_blocked_while_sequencer_active() {
    let (_dir, repo, base, old, _) = setup();
    let context = repo.prepare_reset(&base).unwrap();
    start(&repo, &[old], RebaseAction::Split);
    for mode in [ResetMode::Soft, ResetMode::Mixed, ResetMode::Hard] {
        assert!(
            repo.execute(Operation::Reset {
                context: Arc::new(context.clone()),
                mode
            })
            .is_err()
        );
    }
    repo.execute(Operation::AbortRebase).unwrap();
}

#[test]
fn lfs_split_and_hard_reset_keep_pointer_commits_and_hydrated_workfiles() {
    let (_dir, repo, _, _, _) = setup();
    repo.execute(Operation::LfsTrack {
        pattern: "*.bin".into(),
        track: true,
    })
    .unwrap();
    fs::write(repo.root.join("asset.bin"), "original asset").unwrap();
    let base = commit(&repo, "asset base");
    fs::write(repo.root.join("asset.bin"), "changed asset").unwrap();
    let old = commit(&repo, "asset edit");
    start(&repo, &[old], RebaseAction::Split);
    assert_eq!(
        fs::read(repo.root.join("asset.bin")).unwrap(),
        b"changed asset"
    );
    repo.execute(Operation::StageAll).unwrap();
    let edited = replacement(&repo, "split asset");
    repo.execute(Operation::ContinueRebase).unwrap();
    let r = raw(&repo);
    let tree = r
        .find_commit(edited.parse().unwrap())
        .unwrap()
        .tree()
        .unwrap();
    let blob = r
        .find_blob(
            tree.get_path(std::path::Path::new("asset.bin"))
                .unwrap()
                .id(),
        )
        .unwrap();
    assert!(
        blob.content()
            .starts_with(b"version https://git-lfs.github.com/spec/v1\n")
    );
    let context = repo.prepare_reset(&base).unwrap();
    repo.execute(Operation::Reset {
        context: Arc::new(context),
        mode: ResetMode::Hard,
    })
    .unwrap();
    assert_eq!(
        fs::read(repo.root.join("asset.bin")).unwrap(),
        b"original asset"
    );
    assert!(repo.snapshot(10).unwrap().files.is_empty());
}

#[test]
fn edit_conflicts_resolve_before_user_pause_and_keep_the_resolution() {
    let (_dir, repo, base, old, _) = setup();
    let r = raw(&repo);
    r.branch(
        "onto",
        &r.find_commit(base.parse().unwrap()).unwrap(),
        false,
    )
    .unwrap();
    repo.execute(Operation::Checkout("onto".into())).unwrap();
    fs::write(repo.root.join("file"), "conflicting onto\n").unwrap();
    commit(&repo, "onto");
    repo.execute(Operation::Checkout("main".into())).unwrap();
    let preview = repo.rebase_preview("onto").unwrap();
    let mut steps = preview.steps.clone();
    assert_eq!(steps[0].id, old);
    steps[0].action = RebaseAction::Edit;
    assert!(
        repo.execute(Operation::Rebase {
            context: Arc::new(preview),
            steps
        })
        .is_err()
    );
    assert!(!repo.snapshot(10).unwrap().can_commit());
    assert!(
        repo.execute(Operation::CommitRebase("too early".into()))
            .is_err()
    );
    let context = repo.conflict_context(std::path::Path::new("file")).unwrap();
    repo.execute(Operation::ResolveConflict {
        context: Arc::new(context),
        resolution: gitbuddy::git::ConflictResolution::Edited("resolved edit\n".into()),
    })
    .unwrap();
    repo.execute(Operation::ContinueRebase).unwrap();
    assert!(repo.rebase_status().unwrap().unwrap().editing);
    replacement(&repo, "edited resolution");
    repo.execute(Operation::ContinueRebase).unwrap();
    assert_eq!(
        fs::read(repo.root.join("file")).unwrap(),
        b"resolved edit\n"
    );
    assert!(repo.root.join("later").exists());
}

#[test]
fn nonancestor_and_merge_ranges_are_rejected_without_mutation() {
    let (_dir, repo, base, old, tip) = setup();
    let r = raw(&repo);
    r.branch(
        "side",
        &r.find_commit(base.parse().unwrap()).unwrap(),
        false,
    )
    .unwrap();
    repo.execute(Operation::Checkout("side".into())).unwrap();
    fs::write(repo.root.join("side"), "side").unwrap();
    let side = commit(&repo, "side");
    repo.execute(Operation::Checkout("main".into())).unwrap();
    assert!(
        repo.history_rewrite_preview(&[side], RebaseAction::Edit)
            .is_err()
    );
    repo.execute(Operation::Merge("side".into())).unwrap();
    let merged = repo.snapshot(1).unwrap().head_id.unwrap();
    assert!(
        repo.history_rewrite_preview(&[old], RebaseAction::Split)
            .is_err()
    );
    assert!(
        repo.history_rewrite_preview(std::slice::from_ref(&merged), RebaseAction::Edit)
            .is_err()
    );
    assert_ne!(merged, tip);
    assert_eq!(repo.snapshot(1).unwrap().head_id, Some(merged));
    assert!(repo.rebase_status().unwrap().is_none());
}

#[test]
fn empty_commit_edit_is_rejected_before_rewrite_and_can_be_reworded() {
    let (_dir, repo, _, _, _) = setup();
    let r = raw(&repo);
    let parent = r.head().unwrap().peel_to_commit().unwrap();
    let signature = r.signature().unwrap();
    let empty = r
        .commit(
            Some("HEAD"),
            &signature,
            &signature,
            "empty message",
            &parent.tree().unwrap(),
            &[&parent],
        )
        .unwrap()
        .to_string();
    let (preview, steps) = repo
        .history_rewrite_preview(std::slice::from_ref(&empty), RebaseAction::Edit)
        .unwrap();
    assert!(
        repo.execute(Operation::Rebase {
            context: Arc::new(preview.clone()),
            steps
        })
        .unwrap_err()
        .to_string()
        .contains("Empty commits")
    );
    assert!(repo.rebase_status().unwrap().is_none());
    assert_eq!(repo.snapshot(1).unwrap().head_id, Some(empty));
    let mut steps = preview.steps.clone();
    steps[0].action = RebaseAction::Reword;
    steps[0].message = "reworded empty".into();
    repo.execute(Operation::Rebase {
        context: Arc::new(preview),
        steps,
    })
    .unwrap();
    let new = r.head().unwrap().peel_to_commit().unwrap();
    assert_eq!(new.message(), Ok("reworded empty"));
    assert_eq!(new.tree_id(), parent.tree_id());
}

#[test]
fn merge_commit_actions_use_the_selected_parent_and_reject_invalid_parent() {
    for revert in [false, true] {
        let (_dir, repo, base, old, tip) = setup();
        let r = raw(&repo);
        let base_commit = r.find_commit(git2::Oid::from_str(&base).unwrap()).unwrap();
        let old_commit = r.find_commit(git2::Oid::from_str(&old).unwrap()).unwrap();
        let tip_commit = r.find_commit(git2::Oid::from_str(&tip).unwrap()).unwrap();
        let sig = r.signature().unwrap();
        let merge = r
            .commit(
                None,
                &sig,
                &sig,
                "test merge",
                &tip_commit.tree().unwrap(),
                &[&base_commit, &old_commit],
            )
            .unwrap()
            .to_string();
        let invalid = if revert {
            Operation::RevertMerge {
                id: merge.clone(),
                mainline: 3,
            }
        } else {
            Operation::CherryPickMerge {
                id: merge.clone(),
                mainline: 3,
            }
        };
        assert!(repo.execute(invalid).is_err());
        assert_eq!(repo.snapshot(20).unwrap().head_id.unwrap(), tip);
        if !revert {
            let raw = raw(&repo);
            let object = raw
                .find_object(git2::Oid::from_str(&base).unwrap(), None)
                .unwrap();
            raw.reset(&object, git2::ResetType::Hard, None).unwrap();
        }
        let operation = if revert {
            Operation::RevertMerge {
                id: merge,
                mainline: 2,
            }
        } else {
            Operation::CherryPickMerge {
                id: merge,
                mainline: 2,
            }
        };
        repo.execute(operation).unwrap();
        assert_eq!(repo.root.join("later").exists(), !revert);
        if !revert {
            assert!(
                fs::read_to_string(repo.root.join("file"))
                    .unwrap()
                    .contains("line 1\n")
            );
        }
    }
}

#[test]
fn history_search_reaches_beyond_loaded_page_and_can_be_cancelled() {
    let (_dir, repo, base, _, _) = setup();
    assert!(
        repo.snapshot(1)
            .unwrap()
            .commits
            .iter()
            .all(|c| c.id != base)
    );
    let token = gitbuddy::git::CancellationToken::default();
    let result = repo.search_history("base", &token).unwrap();
    assert!(result.commits.iter().any(|c| c.id == base));
    assert!(result.scanned >= 3);
    assert!(!result.truncated);
    assert!(token.cancel());
    assert!(repo.search_history("base", &token).is_err());
    assert!(
        repo.search_history("   ", &gitbuddy::git::CancellationToken::default())
            .is_err()
    );
}

#[test]
fn history_search_filters_by_author_path_date_and_revision() {
    let (_dir, repo, base, old, tip) = setup();
    let token = gitbuddy::git::CancellationToken::default();
    let find = |query: &str| {
        repo.search_history(query, &token)
            .unwrap()
            .commits
            .into_iter()
            .map(|c| c.id)
            .collect::<Vec<_>>()
    };
    assert_eq!(
        find("author:history@example.invalid path:later"),
        vec![tip.clone()]
    );
    assert_eq!(find(&format!("path:added ref:{old}")), vec![old.clone()]);
    assert_eq!(find(&format!("path:later ref:{old}")), Vec::<String>::new());
    assert!(find("after:2100-01-01").is_empty());
    assert!(find("before:2100-01-01").contains(&base));
    assert!(repo.search_history("after:bad-date", &token).is_err());
    assert!(
        repo.search_history("ref:no-such-reference", &token)
            .is_err()
    );
    assert!(
        repo.search_history("after:2100-01-01 before:2000-01-01", &token)
            .is_err()
    );
    fs::write(repo.root.join("space name.txt"), "text").unwrap();
    let spaced = commit(&repo, "path with spaces");
    assert_eq!(find("path:\"space name.txt\""), vec![spaced]);
    assert!(repo.search_history("path:\"unterminated", &token).is_err());
}

#[test]
fn batch_cherry_pick_replays_in_order_and_rejects_stale_plan() {
    let (_dir, repo, base, old, tip) = setup();
    let raw = raw(&repo);
    let object = raw
        .find_object(git2::Oid::from_str(&base).unwrap(), None)
        .unwrap();
    raw.reset(&object, git2::ResetType::Hard, None).unwrap();
    let plan = repo
        .prepare_cherry_batch(&[old.clone(), tip.clone()])
        .unwrap();
    repo.execute(Operation::CherryPickBatch(Arc::new(plan)))
        .unwrap();
    assert!(
        fs::read_to_string(repo.root.join("file"))
            .unwrap()
            .contains("first change")
    );
    assert!(repo.root.join("later").exists());
    assert!(repo.rebase_status().unwrap().is_none());
    let history = repo.snapshot(5).unwrap().commits;
    assert_eq!(history[0].subject, "descendant");
    assert_eq!(history[1].subject, "old commit");
    let stale = repo
        .prepare_cherry_batch(std::slice::from_ref(&old))
        .unwrap();
    fs::write(repo.root.join("new"), "new work").unwrap();
    assert!(
        repo.execute(Operation::CherryPickBatch(Arc::new(stale)))
            .is_err()
    );
    assert_eq!(
        fs::read_to_string(repo.root.join("new")).unwrap(),
        "new work"
    );
}

#[test]
fn batch_cherry_pick_conflict_can_continue_and_abort() {
    for abort in [false, true] {
        let (_dir, repo, base, old, _) = setup();
        let raw = raw(&repo);
        let object = raw
            .find_object(git2::Oid::from_str(&base).unwrap(), None)
            .unwrap();
        raw.reset(&object, git2::ResetType::Hard, None).unwrap();
        let text = fs::read_to_string(repo.root.join("file"))
            .unwrap()
            .replace("line 1\n", "other change\n");
        fs::write(repo.root.join("file"), text).unwrap();
        let original = commit(&repo, "different change");
        let plan = repo
            .prepare_cherry_batch(std::slice::from_ref(&old))
            .unwrap();
        let _ = repo.execute(Operation::CherryPickBatch(Arc::new(plan)));
        assert!(repo.rebase_status().unwrap().is_some());
        assert!(!repo.conflict_session().unwrap().files.is_empty());
        if abort {
            repo.execute(Operation::AbortRebase).unwrap();
            assert_eq!(repo.snapshot(5).unwrap().head_id.unwrap(), original);
            assert!(
                fs::read_to_string(repo.root.join("file"))
                    .unwrap()
                    .contains("other change")
            );
        } else {
            let context = repo.conflict_context(std::path::Path::new("file")).unwrap();
            repo.execute(Operation::ResolveConflict {
                context: Arc::new(context),
                resolution: gitbuddy::git::ConflictResolution::Theirs,
            })
            .unwrap();
            repo.execute(Operation::ContinueRebase).unwrap();
            assert!(repo.rebase_status().unwrap().is_none());
            assert!(
                fs::read_to_string(repo.root.join("file"))
                    .unwrap()
                    .contains("first change")
            );
        }
    }
}

#[test]
fn batch_revert_replays_newest_first_and_can_recover_conflicts() {
    let (_dir, repo, base, old, tip) = setup();
    let base_tree = raw(&repo)
        .find_commit(git2::Oid::from_str(&base).unwrap())
        .unwrap()
        .tree_id();
    let plan = repo.prepare_revert_batch(&[tip, old.clone()]).unwrap();
    repo.execute(Operation::RevertBatch(Arc::new(plan)))
        .unwrap();
    let r = raw(&repo);
    assert_eq!(
        r.head().unwrap().peel_to_commit().unwrap().tree_id(),
        base_tree
    );
    assert!(repo.rebase_status().unwrap().is_none());
    let history = repo.snapshot(4).unwrap().commits;
    assert!(history[0].subject.starts_with("Revert \"old commit\""));

    let (_dir2, repo2, _, old2, _) = setup();
    let text = fs::read_to_string(repo2.root.join("file"))
        .unwrap()
        .replace("first change\n", "later conflict\n");
    fs::write(repo2.root.join("file"), text).unwrap();
    let original = commit(&repo2, "conflicting edit");
    let plan = repo2
        .prepare_revert_batch(std::slice::from_ref(&old2))
        .unwrap();
    let _ = repo2.execute(Operation::RevertBatch(Arc::new(plan)));
    assert!(repo2.rebase_status().unwrap().is_some());
    assert!(!repo2.conflict_session().unwrap().files.is_empty());
    repo2.execute(Operation::AbortRebase).unwrap();
    assert_eq!(repo2.snapshot(5).unwrap().head_id.unwrap(), original);
    assert!(
        fs::read_to_string(repo2.root.join("file"))
            .unwrap()
            .contains("later conflict")
    );
}

#[test]
fn batch_revert_conflict_can_resume_after_resolving() {
    let (_dir, repo, _, old, _) = setup();
    let text = fs::read_to_string(repo.root.join("file"))
        .unwrap()
        .replace("first change\n", "conflicting change\n");
    fs::write(repo.root.join("file"), text).unwrap();
    commit(&repo, "conflicting edit");
    let plan = repo.prepare_revert_batch(&[old]).unwrap();
    let _ = repo.execute(Operation::RevertBatch(Arc::new(plan)));
    assert!(!repo.rebase_status().unwrap().unwrap().interrupted);
    let ctx = repo.conflict_context(std::path::Path::new("file")).unwrap();
    repo.execute(Operation::ResolveConflict {
        context: Arc::new(ctx),
        resolution: gitbuddy::git::ConflictResolution::Theirs,
    })
    .unwrap();
    repo.execute(Operation::ContinueRebase).unwrap();
    assert!(repo.rebase_status().unwrap().is_none());
    assert!(
        repo.snapshot(1).unwrap().commits[0]
            .subject
            .starts_with("Revert")
    );
}

#[test]
fn root_rebase_onto_unrelated_target_replays_root_and_descendants() {
    let (_dir, repo, base, _, tip) = setup();
    let raw = raw(&repo);
    let tree_id = raw.treebuilder(None).unwrap().write().unwrap();
    let tree = raw.find_tree(tree_id).unwrap();
    let sig = raw.signature().unwrap();
    let target = raw
        .commit(None, &sig, &sig, "new base", &tree, &[])
        .unwrap()
        .to_string();
    let plan = repo.rebase_preview_onto("--root", Some(&target)).unwrap();
    assert!(plan.root);
    assert_eq!(plan.steps.len(), 3);
    assert_eq!(plan.steps[0].id, base);
    repo.execute(Operation::Rebase {
        context: Arc::new(plan.clone()),
        steps: plan.steps,
    })
    .unwrap();
    let head = raw.head().unwrap().peel_to_commit().unwrap();
    let first = head.parent(0).unwrap().parent(0).unwrap();
    assert_eq!(first.parent_id(0).unwrap().to_string(), target);
    assert_eq!(first.message().unwrap(), "base");
    assert_ne!(head.id().to_string(), tip);
    assert!(repo.rebase_status().unwrap().is_none());
}

#[test]
fn root_rebase_conflict_can_abort_and_root_edit_is_rejected() {
    let (_dir, repo, _, _, tip) = setup();
    let raw = raw(&repo);
    let blob = raw.blob(b"different root content\n").unwrap();
    let mut builder = raw.treebuilder(None).unwrap();
    builder.insert("file", blob, 0o100644).unwrap();
    let tree_id = builder.write().unwrap();
    let tree = raw.find_tree(tree_id).unwrap();
    let sig = raw.signature().unwrap();
    let target = raw
        .commit(None, &sig, &sig, "conflicting base", &tree, &[])
        .unwrap()
        .to_string();
    let plan = repo.rebase_preview_onto("--root", Some(&target)).unwrap();
    let mut invalid = plan.steps.clone();
    invalid[0].action = RebaseAction::Edit;
    assert!(
        repo.execute(Operation::Rebase {
            context: Arc::new(plan.clone()),
            steps: invalid
        })
        .is_err()
    );
    assert_eq!(repo.snapshot(5).unwrap().head_id.unwrap(), tip);
    let _ = repo.execute(Operation::Rebase {
        context: Arc::new(plan.clone()),
        steps: plan.steps,
    });
    assert!(repo.rebase_status().unwrap().is_some());
    assert!(!repo.conflict_session().unwrap().files.is_empty());
    repo.execute(Operation::AbortRebase).unwrap();
    assert_eq!(repo.snapshot(5).unwrap().head_id.unwrap(), tip);
}
