use gitbuddy::git::{Operation, Repository, StashAction};
use std::{path::Path, sync::Arc};

fn setup() -> (tempfile::TempDir, Repository) {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::init(&dir.path().join("repo")).unwrap();
    repo.execute(Operation::SetIdentity(
        "Tester".into(),
        "tester@example.invalid".into(),
    ))
    .unwrap();
    write(&repo, "a", "base a\n");
    write(&repo, "b", "base b\n");
    commit(&repo, "base");
    (dir, repo)
}
fn raw(repo: &Repository) -> git2::Repository {
    git2::Repository::open(&repo.root).unwrap()
}
fn write(repo: &Repository, path: &str, text: &str) {
    std::fs::write(repo.root.join(path), text).unwrap();
}
fn text(repo: &Repository, path: &str) -> String {
    std::fs::read_to_string(repo.root.join(path)).unwrap()
}
fn commit(repo: &Repository, message: &str) -> String {
    repo.execute(Operation::StageAll).unwrap();
    repo.execute(Operation::Commit(message.into())).unwrap();
    repo.snapshot(1).unwrap().head_id.unwrap()
}
fn index_text(repo: &Repository, path: &str) -> Option<String> {
    let raw = raw(repo);
    let entry = raw.index().unwrap().get_path(Path::new(path), 0)?;
    Some(String::from_utf8(raw.find_blob(entry.id).unwrap().content().to_vec()).unwrap())
}
fn save(repo: &Repository, paths: &[&str]) -> String {
    let context = Arc::new(repo.stash_context().unwrap());
    repo.execute(Operation::SaveStash {
        context,
        paths: paths.iter().map(|p| (*p).into()).collect(),
        message: "selected stash".into(),
    })
    .unwrap();
    repo.snapshot(1).unwrap().stashes[0].0.clone()
}
fn restore(
    repo: &Repository,
    id: &str,
    action: StashAction,
    reinstate_index: bool,
) -> anyhow::Result<String> {
    repo.execute(Operation::RestoreStash {
        context: Arc::new(repo.stash_context()?),
        id: id.into(),
        action,
        reinstate_index,
    })
}

#[test]
fn selected_stash_preserves_unselected_staged_and_unstaged_contents() {
    let (_dir, repo) = setup();
    write(&repo, "a", "staged a\n");
    write(&repo, "b", "staged b\n");
    repo.execute(Operation::StageAll).unwrap();
    write(&repo, "a", "working a\n");
    write(&repo, "b", "working b\n");
    write(&repo, "new-a", "new a\n");
    write(&repo, "new-b", "new b\n");
    let b_entry = raw(&repo)
        .index()
        .unwrap()
        .get_path(Path::new("b"), 0)
        .unwrap();
    let id = save(&repo, &["a", "new-a"]);
    assert_eq!(text(&repo, "a"), "base a\n");
    assert_eq!(index_text(&repo, "a").unwrap(), "base a\n");
    assert!(!repo.root.join("new-a").exists());
    assert_eq!(text(&repo, "new-b"), "new b\n");
    assert_eq!(text(&repo, "b"), "working b\n");
    assert_eq!(index_text(&repo, "b").unwrap(), "staged b\n");
    let after = raw(&repo)
        .index()
        .unwrap()
        .get_path(Path::new("b"), 0)
        .unwrap();
    assert_eq!(after.id, b_entry.id);
    assert_eq!(after.flags, b_entry.flags);
    assert_eq!(after.mtime, b_entry.mtime);
    let preview = repo.stash_preview(&id).unwrap();
    assert_eq!(
        preview
            .sections
            .iter()
            .map(|s| s.files.len())
            .collect::<Vec<_>>(),
        vec![1, 1, 1]
    );
    restore(&repo, &id, StashAction::Apply, true).unwrap();
    assert_eq!(text(&repo, "a"), "working a\n");
    assert_eq!(index_text(&repo, "a").unwrap(), "staged a\n");
    assert_eq!(text(&repo, "b"), "working b\n");
    assert_eq!(index_text(&repo, "b").unwrap(), "staged b\n");
    assert_eq!(text(&repo, "new-a"), "new a\n");
    assert_eq!(repo.snapshot(1).unwrap().stashes[0].0, id);
}
#[test]
fn pop_deletes_only_the_selected_stable_id_and_restores_index() {
    let (_dir, repo) = setup();
    write(&repo, "a", "staged a\n");
    repo.execute(Operation::StageAll).unwrap();
    write(&repo, "a", "working a\n");
    let first = save(&repo, &["a"]);
    let context = Arc::new(repo.stash_context().unwrap());
    write(&repo, "b", "other stash\n");
    let second = save(&repo, &["b"]);
    // A changed stash reflog requires reopening the action; stale contexts fail.
    assert!(
        repo.execute(Operation::RestoreStash {
            context,
            id: first.clone(),
            action: StashAction::Pop,
            reinstate_index: true
        })
        .is_err()
    );
    restore(&repo, &first, StashAction::Pop, true).unwrap();
    assert_eq!(text(&repo, "a"), "working a\n");
    assert_eq!(index_text(&repo, "a").unwrap(), "staged a\n");
    assert_eq!(
        repo.snapshot(1).unwrap().stashes,
        vec![(
            second.clone(),
            repo.snapshot(1).unwrap().stashes[0].1.clone()
        )]
    );
    assert_eq!(
        raw(&repo)
            .find_reference("refs/stash")
            .unwrap()
            .target()
            .unwrap()
            .to_string(),
        second
    );
}
#[test]
fn failed_pop_keeps_stash_and_local_edits() {
    let (_dir, repo) = setup();
    write(&repo, "a", "saved a\n");
    let id = save(&repo, &["a"]);
    write(&repo, "a", "local a\n");
    let before = std::fs::read(raw(&repo).path().join("index")).unwrap();
    assert!(restore(&repo, &id, StashAction::Pop, true).is_err());
    assert_eq!(text(&repo, "a"), "local a\n");
    assert_eq!(
        std::fs::read(raw(&repo).path().join("index")).unwrap(),
        before
    );
    assert_eq!(repo.snapshot(1).unwrap().stashes[0].0, id);
}
#[test]
fn apply_without_index_leaves_saved_staged_changes_unstaged() {
    let (_dir, repo) = setup();
    write(&repo, "a", "staged a\n");
    repo.execute(Operation::StageAll).unwrap();
    write(&repo, "a", "working a\n");
    let id = save(&repo, &["a"]);
    restore(&repo, &id, StashAction::Apply, false).unwrap();
    assert_eq!(index_text(&repo, "a").unwrap(), "base a\n");
    assert_eq!(text(&repo, "a"), "working a\n");
}
#[test]
fn branch_from_stash_uses_original_base_and_saved_index() {
    let (_dir, repo) = setup();
    let base = repo.snapshot(1).unwrap().head_id.unwrap();
    write(&repo, "a", "staged a\n");
    repo.execute(Operation::StageAll).unwrap();
    write(&repo, "a", "working a\n");
    write(&repo, "new", "new file\n");
    let id = save(&repo, &["a", "new"]);
    write(&repo, "a", "later a\n");
    let later = commit(&repo, "later");
    let old_branch = raw(&repo).head().unwrap().name().unwrap().to_owned();
    restore(&repo, &id, StashAction::Branch("saved-work".into()), false).unwrap();
    assert_eq!(repo.snapshot(1).unwrap().head_id.unwrap(), base);
    assert_eq!(repo.snapshot(1).unwrap().branch, "saved-work");
    assert_eq!(
        raw(&repo)
            .find_reference(&old_branch)
            .unwrap()
            .target()
            .unwrap()
            .to_string(),
        later
    );
    assert_eq!(text(&repo, "a"), "working a\n");
    assert_eq!(index_text(&repo, "a").unwrap(), "staged a\n");
    assert_eq!(text(&repo, "new"), "new file\n");
    assert!(repo.snapshot(1).unwrap().stashes.is_empty());
}
#[test]
fn branch_rejects_dirty_tree_and_existing_branch_without_modifying_files() {
    let (_dir, repo) = setup();
    write(&repo, "a", "saved\n");
    let id = save(&repo, &["a"]);
    let name = repo.snapshot(1).unwrap().branch;
    assert!(restore(&repo, &id, StashAction::Branch(name), true).is_err());
    write(&repo, "local", "local\n");
    assert!(restore(&repo, &id, StashAction::Branch("new".into()), true).is_err());
    assert_eq!(text(&repo, "local"), "local\n");
    assert_eq!(text(&repo, "a"), "base a\n");
    assert!(raw(&repo).find_reference("refs/heads/new").is_err());
    assert_eq!(repo.snapshot(1).unwrap().stashes[0].0, id);
}
#[test]
fn stale_save_context_and_index_lock_fail_before_mutation() {
    let (_dir, repo) = setup();
    write(&repo, "a", "first\n");
    let context = Arc::new(repo.stash_context().unwrap());
    write(&repo, "a", "changed after review\n");
    assert!(
        repo.execute(Operation::SaveStash {
            context,
            paths: vec!["a".into()],
            message: String::new()
        })
        .is_err()
    );
    let lock = raw(&repo).path().join("index.lock");
    std::fs::write(&lock, "external lock").unwrap();
    assert!(
        repo.execute(Operation::SaveStash {
            context: Arc::new(repo.stash_context().unwrap()),
            paths: vec!["a".into()],
            message: String::new()
        })
        .is_err()
    );
    assert_eq!(std::fs::read_to_string(lock).unwrap(), "external lock");
    assert_eq!(text(&repo, "a"), "changed after review\n");
    assert!(repo.snapshot(1).unwrap().stashes.is_empty());
}
#[test]
fn preview_separates_worktree_index_and_untracked_and_supports_full_context() {
    let (_dir, repo) = setup();
    write(&repo, "a", "staged\n");
    repo.execute(Operation::StageAll).unwrap();
    write(&repo, "a", "working\n");
    write(&repo, "new", "untracked\n");
    let id = save(&repo, &["a", "new"]);
    let preview = repo.stash_preview(&id).unwrap();
    for (s, expected) in ["working", "staged", "untracked"].into_iter().enumerate() {
        let section = &preview.sections[s];
        let patch = repo
            .stash_file_diff_context(section, &section.files[0], true)
            .unwrap();
        assert!(patch.contains(&format!("+{expected}")));
        assert!(patch.contains("@@"));
    }
    assert!(repo.snapshot(1).unwrap().files.is_empty());
}
#[test]
fn selected_add_delete_and_literal_glob_paths_round_trip() {
    let (_dir, repo) = setup();
    write(&repo, "[abc]*.txt", "base literal\n");
    commit(&repo, "literal");
    std::fs::remove_file(repo.root.join("a")).unwrap();
    repo.execute(Operation::Stage(vec!["a".into()])).unwrap();
    write(&repo, "[abc]*.txt", "saved literal\n");
    write(&repo, "c.txt", "keep c\n");
    write(&repo, "added", "saved addition\n");
    repo.execute(Operation::Stage(vec!["added".into()]))
        .unwrap();
    let id = save(&repo, &["a", "[abc]*.txt", "added"]);
    assert_eq!(text(&repo, "a"), "base a\n");
    assert_eq!(text(&repo, "[abc]*.txt"), "base literal\n");
    assert_eq!(text(&repo, "c.txt"), "keep c\n");
    assert!(!repo.root.join("added").exists());
    restore(&repo, &id, StashAction::Pop, true).unwrap();
    assert!(!repo.root.join("a").exists());
    assert!(index_text(&repo, "a").is_none());
    assert_eq!(text(&repo, "[abc]*.txt"), "saved literal\n");
    assert_eq!(index_text(&repo, "added").unwrap(), "saved addition\n");
    assert_eq!(text(&repo, "c.txt"), "keep c\n");
}
#[test]
fn selected_lfs_stash_captures_unstaged_data_as_pointers_and_round_trips() {
    let (_dir, repo) = setup();
    repo.execute(Operation::LfsTrack {
        pattern: "*.bin".into(),
        track: true,
    })
    .unwrap();
    write(&repo, "saved.bin", "original large file\n");
    commit(&repo, "LFS base");
    write(&repo, "saved.bin", "staged large file\n");
    repo.execute(Operation::Stage(vec!["saved.bin".into()]))
        .unwrap();
    write(&repo, "saved.bin", "working large file\n");
    write(&repo, "b", "keep local\n");
    let id = save(&repo, &["saved.bin"]);
    assert_eq!(text(&repo, "saved.bin"), "original large file\n");
    assert_eq!(text(&repo, "b"), "keep local\n");
    let stash = raw(&repo);
    let tree = stash
        .find_commit(git2::Oid::from_str(&id).unwrap())
        .unwrap()
        .tree()
        .unwrap();
    let entry = tree.get_path(Path::new("saved.bin")).unwrap();
    assert!(
        std::str::from_utf8(stash.find_blob(entry.id()).unwrap().content())
            .unwrap()
            .starts_with("version https://git-lfs.github.com/spec/v1\n")
    );
    restore(&repo, &id, StashAction::Pop, true).unwrap();
    assert_eq!(text(&repo, "saved.bin"), "working large file\n");
    assert!(
        index_text(&repo, "saved.bin")
            .unwrap()
            .contains("oid sha256:")
    );
    assert_eq!(text(&repo, "b"), "keep local\n");
}

#[test]
fn unrelated_staged_additions_and_deletions_survive_pop() {
    let (_dir, repo) = setup();
    write(&repo, "a", "saved a\n");
    let id = save(&repo, &["a"]);
    std::fs::remove_file(repo.root.join("b")).unwrap();
    write(&repo, "new-local", "local staged\n");
    repo.execute(Operation::StageAll).unwrap();
    write(&repo, "new-local", "local unstaged\n");
    restore(&repo, &id, StashAction::Pop, true).unwrap();
    assert!(index_text(&repo, "b").is_none());
    assert!(!repo.root.join("b").exists());
    assert_eq!(index_text(&repo, "new-local").unwrap(), "local staged\n");
    assert_eq!(text(&repo, "new-local"), "local unstaged\n");
    assert!(raw(&repo).find_reference("refs/stash").is_err());
}
#[test]
fn overlapping_staged_changes_are_rejected_before_modification() {
    let (_dir, repo) = setup();
    write(&repo, "a", "saved a\n");
    let id = save(&repo, &["a"]);
    write(&repo, "a", "local staged\n");
    repo.execute(Operation::StageAll).unwrap();
    write(&repo, "a", "local unstaged\n");
    let index = std::fs::read(raw(&repo).path().join("index")).unwrap();
    for reinstate in [false, true] {
        assert!(
            restore(&repo, &id, StashAction::Pop, reinstate)
                .unwrap_err()
                .to_string()
                .contains("overlaps staged changes")
        );
        assert_eq!(
            std::fs::read(raw(&repo).path().join("index")).unwrap(),
            index
        );
        assert_eq!(text(&repo, "a"), "local unstaged\n");
    }
    assert_eq!(repo.snapshot(1).unwrap().stashes[0].0, id);
}
#[test]
fn pop_with_conflicting_new_committed_changes_retains_stash() {
    let (_dir, repo) = setup();
    write(&repo, "a", "saved a\n");
    let id = save(&repo, &["a"]);
    write(&repo, "a", "committed later\n");
    commit(&repo, "later");
    assert!(restore(&repo, &id, StashAction::Pop, true).is_err());
    assert_eq!(repo.snapshot(1).unwrap().stashes[0].0, id);
    assert!(text(&repo, "a").contains("committed later"));
}
#[test]
fn custom_stash_is_compatible_with_native_libgit2_pop() {
    let (_dir, repo) = setup();
    write(&repo, "a", "staged a\n");
    repo.execute(Operation::StageAll).unwrap();
    write(&repo, "a", "working a\n");
    write(&repo, "new", "untracked\n");
    save(&repo, &["a", "new"]);
    let mut options = git2::StashApplyOptions::new();
    options.reinstantiate_index();
    raw(&repo).stash_pop(0, Some(&mut options)).unwrap();
    assert_eq!(text(&repo, "a"), "working a\n");
    assert_eq!(index_text(&repo, "a").unwrap(), "staged a\n");
    assert_eq!(text(&repo, "new"), "untracked\n");
    assert!(repo.snapshot(1).unwrap().stashes.is_empty());
}
#[test]
fn native_stash_can_be_previewed_and_restored_with_saved_index() {
    let (_dir, repo) = setup();
    write(&repo, "a", "native staged\n");
    repo.execute(Operation::StageAll).unwrap();
    write(&repo, "a", "native working\n");
    write(&repo, "new", "native new\n");
    repo.execute(Operation::Stash("native stash".into()))
        .unwrap();
    let id = repo.snapshot(1).unwrap().stashes[0].0.clone();
    assert_eq!(repo.stash_preview(&id).unwrap().sections.len(), 3);
    restore(&repo, &id, StashAction::Pop, true).unwrap();
    assert_eq!(index_text(&repo, "a").unwrap(), "native staged\n");
    assert_eq!(text(&repo, "a"), "native working\n");
    assert_eq!(text(&repo, "new"), "native new\n");
}
#[test]
fn partial_stash_in_linked_worktree_preserves_main_index() {
    let (_dir, repo) = setup();
    let target = repo.root.parent().unwrap().join("linked");
    repo.execute(Operation::CreateWorktree {
        name: "linked".into(),
        path: target.clone(),
    })
    .unwrap();
    let linked = Repository::open(&target).unwrap();
    let index = std::fs::read(raw(&repo).path().join("index")).unwrap();
    write(&linked, "a", "linked saved\n");
    write(&linked, "b", "linked local\n");
    let id = save(&linked, &["a"]);
    assert_eq!(text(&linked, "b"), "linked local\n");
    assert_eq!(text(&repo, "a"), "base a\n");
    restore(&linked, &id, StashAction::Pop, true).unwrap();
    assert_eq!(text(&linked, "a"), "linked saved\n");
    assert_eq!(
        std::fs::read(raw(&repo).path().join("index")).unwrap(),
        index
    );
}
#[cfg(unix)]
#[test]
fn selected_rename_symlink_and_executable_round_trip() {
    use std::os::unix::{fs::PermissionsExt, fs::symlink};
    let (_dir, repo) = setup();
    write(&repo, "script", "original script\n");
    symlink("a", repo.root.join("link")).unwrap();
    commit(&repo, "links");
    std::fs::rename(repo.root.join("a"), repo.root.join("renamed")).unwrap();
    repo.execute(Operation::StageAll).unwrap();
    std::fs::remove_file(repo.root.join("link")).unwrap();
    symlink("b", repo.root.join("link")).unwrap();
    std::fs::set_permissions(
        repo.root.join("script"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    let id = save(&repo, &["renamed", "link", "script"]);
    assert!(repo.root.join("a").exists());
    assert!(!repo.root.join("renamed").exists());
    assert_eq!(
        std::fs::read_link(repo.root.join("link")).unwrap(),
        Path::new("a")
    );
    restore(&repo, &id, StashAction::Pop, true).unwrap();
    assert!(!repo.root.join("a").exists());
    assert_eq!(text(&repo, "renamed"), "base a\n");
    assert_eq!(
        std::fs::read_link(repo.root.join("link")).unwrap(),
        Path::new("b")
    );
    assert_ne!(
        std::fs::metadata(repo.root.join("script"))
            .unwrap()
            .permissions()
            .mode()
            & 0o111,
        0
    );
}
#[test]
fn stash_inspection_and_expanded_paths_survive_session_serialization() {
    use gitbuddy::session::{Inspection, PatchKey, Selection, Session, Tab};
    let (_dir, repo) = setup();
    write(&repo, "a", "saved\n");
    let id = save(&repo, &["a"]);
    let session = Session {
        tabs: vec![Tab {
            path: repo.root.clone(),
            selection: Selection::Inspect,
            inspection: Some(Inspection::Stash(id.clone())),
            expanded: vec![PatchKey::Stash(id, "Working tree".into(), "a".into())],
            ..Tab::default()
        }],
        ..Session::default()
    };
    assert_eq!(
        serde_json::from_str::<Session>(&serde_json::to_string(&session).unwrap()).unwrap(),
        session
    );
}

#[test]
fn pop_with_missing_lfs_object_keeps_original_stash() {
    let (_dir, repo) = setup();
    repo.execute(Operation::LfsTrack {
        pattern: "*.bin".into(),
        track: true,
    })
    .unwrap();
    write(&repo, "saved.bin", "base binary\n");
    commit(&repo, "LFS base");
    write(&repo, "saved.bin", "saved binary\n");
    let id = save(&repo, &["saved.bin"]);
    let repo_raw = raw(&repo);
    let tree = repo_raw
        .find_commit(git2::Oid::from_str(&id).unwrap())
        .unwrap()
        .tree()
        .unwrap();
    let entry = tree.get_path(Path::new("saved.bin")).unwrap();
    let blob = repo_raw.find_blob(entry.id()).unwrap();
    let pointer = std::str::from_utf8(blob.content()).unwrap();
    let hash = pointer
        .lines()
        .find_map(|line| line.strip_prefix("oid sha256:"))
        .unwrap();
    std::fs::remove_file(
        repo_raw
            .commondir()
            .join("lfs/objects")
            .join(&hash[..2])
            .join(&hash[2..4])
            .join(hash),
    )
    .unwrap();
    assert!(
        restore(&repo, &id, StashAction::Pop, true)
            .unwrap_err()
            .to_string()
            .contains("LFS objects are missing")
    );
    assert_eq!(repo.snapshot(1).unwrap().stashes[0].0, id);
    assert!(text(&repo, "saved.bin").contains(hash));
}
#[test]
fn pop_does_not_overwrite_colliding_untracked_file() {
    let (_dir, repo) = setup();
    write(&repo, "new", "saved new\n");
    let id = save(&repo, &["new"]);
    write(&repo, "new", "local new\n");
    assert!(restore(&repo, &id, StashAction::Pop, false).is_err());
    assert_eq!(text(&repo, "new"), "local new\n");
    assert_eq!(repo.snapshot(1).unwrap().stashes[0].0, id);
    assert!(index_text(&repo, "new").is_none());
}

#[test]
fn saved_index_change_with_worktree_back_at_head_round_trips() {
    let (_dir, repo) = setup();
    write(&repo, "a", "staged change\n");
    repo.execute(Operation::StageAll).unwrap();
    write(&repo, "a", "base a\n");
    let id = save(&repo, &["a"]);
    let preview = repo.stash_preview(&id).unwrap();
    assert!(preview.sections[0].files.is_empty());
    assert_eq!(preview.sections[1].files.len(), 1);
    restore(&repo, &id, StashAction::Pop, true).unwrap();
    assert_eq!(text(&repo, "a"), "base a\n");
    assert_eq!(index_text(&repo, "a").unwrap(), "staged change\n");
}
