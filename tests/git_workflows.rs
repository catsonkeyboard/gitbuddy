use gitbuddy::git::{
    DIFF_LIMIT_LINES, Operation, Repository, diff_lines, diff_preview, parse_status,
};
use std::{path::Path, process::Command};
use tempfile::TempDir;

fn command(path: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().into()
}
fn setup() -> (TempDir, Repository) {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();
    command(dir.path(), &["config", "user.name", "GitBuddy Test"]);
    command(
        dir.path(),
        &["config", "user.email", "test@example.invalid"],
    );
    command(dir.path(), &["config", "commit.gpgsign", "false"]);
    command(dir.path(), &["config", "tag.gpgsign", "false"]);
    command(dir.path(), &["config", "core.hooksPath", "/dev/null"]);
    (dir, repo)
}
fn write(repo: &Repository, name: &str, value: &str) {
    std::fs::write(repo.root.join(name), value).unwrap();
}
fn commit(repo: &Repository, message: &str) {
    repo.execute(Operation::StageAll).unwrap();
    repo.execute(Operation::Commit(message.into())).unwrap();
}

#[test]
fn unborn_stage_unstage_and_first_commit() {
    let (_dir, repo) = setup();
    assert!(repo.snapshot(20).unwrap().commits.is_empty());
    write(&repo, "a file.txt", "hello\n");
    repo.execute(Operation::StageAll).unwrap();
    assert!(repo.snapshot(20).unwrap().files[0].staged());
    repo.execute(Operation::UnstageAll).unwrap();
    assert!(repo.root.join("a file.txt").exists());
    assert_eq!(repo.snapshot(20).unwrap().files[0].index, '?');
    commit(&repo, "Initial commit");
    let s = repo.snapshot(20).unwrap();
    assert!(s.files.is_empty());
    assert_eq!(s.commits[0].subject, "Initial commit");
}
#[test]
fn staged_and_unstaged_diff_are_independent() {
    let (_dir, repo) = setup();
    write(&repo, "file.txt", "base\n");
    commit(&repo, "base");
    write(&repo, "file.txt", "staged\n");
    repo.execute(Operation::StageAll).unwrap();
    write(&repo, "file.txt", "unstaged\n");
    let s = repo.snapshot(20).unwrap();
    let file = &s.files[0];
    assert!(file.staged() && file.unstaged());
    assert!(repo.diff(file, true).unwrap().contains("+staged"));
    assert!(repo.diff(file, false).unwrap().contains("+unstaged"));
    repo.execute(Operation::Discard(file.path.clone())).unwrap();
    assert_eq!(
        std::fs::read_to_string(repo.root.join("file.txt")).unwrap(),
        "staged\n"
    );
    repo.execute(Operation::Unstage(vec![file.path.clone()]))
        .unwrap();
    assert!(!repo.snapshot(20).unwrap().files[0].staged());
}
#[test]
fn literal_pathspec_never_stages_other_files() {
    let (_dir, repo) = setup();
    write(&repo, "[a].txt", "one");
    write(&repo, "a.txt", "two");
    repo.execute(Operation::Stage(vec!["[a].txt".into()]))
        .unwrap();
    let s = repo.snapshot(20).unwrap();
    assert_eq!(s.files.iter().filter(|f| f.staged()).count(), 1);
    assert!(
        s.files
            .iter()
            .find(|f| f.path == Path::new("[a].txt"))
            .unwrap()
            .staged()
    );
}
#[test]
fn discard_uses_a_literal_path_and_preserves_other_changes() {
    let (_dir, repo) = setup();
    write(&repo, "[a].txt", "one\n");
    write(&repo, "a.txt", "two\n");
    commit(&repo, "base");
    write(&repo, "[a].txt", "modified one\n");
    write(&repo, "a.txt", "modified two\n");
    repo.execute(Operation::Discard("[a].txt".into())).unwrap();
    assert_eq!(
        std::fs::read_to_string(repo.root.join("[a].txt")).unwrap(),
        "one\n"
    );
    assert_eq!(
        std::fs::read_to_string(repo.root.join("a.txt")).unwrap(),
        "modified two\n"
    );
}
#[test]
fn rename_and_newline_paths_are_lossless() {
    let entries = parse_status(b"R  new name\n.txt\0old name.txt\0?? other\0");
    assert_eq!(entries[0].path, Path::new("new name\n.txt"));
    assert_eq!(
        entries[0].original.as_deref(),
        Some(Path::new("old name.txt"))
    );
    assert_eq!(entries.len(), 2);
    let (_dir, repo) = setup();
    write(&repo, "old name.txt", "original\n");
    commit(&repo, "base");
    std::fs::rename(
        repo.root.join("old name.txt"),
        repo.root.join("new name\n.txt"),
    )
    .unwrap();
    repo.execute(Operation::StageAll).unwrap();
    let s = repo.snapshot(20).unwrap();
    assert_eq!(s.files[0].index, 'R');
    assert!(repo.diff(&s.files[0], true).unwrap().contains("rename"));
    repo.execute(Operation::Unstage(vec![
        s.files[0].path.clone(),
        s.files[0].original.clone().unwrap(),
    ]))
    .unwrap();
    assert!(repo.snapshot(20).unwrap().files.iter().all(|f| !f.staged()));
}
#[test]
fn branches_merge_tags_and_safe_delete() {
    let (_dir, repo) = setup();
    write(&repo, "base", "base");
    commit(&repo, "base");
    repo.execute(Operation::CreateBranch("feature/test".into()))
        .unwrap();
    write(&repo, "feature", "done");
    commit(&repo, "feature");
    repo.execute(Operation::Checkout("main".into())).unwrap();
    assert!(
        repo.execute(Operation::DeleteBranch("feature/test".into()))
            .is_err()
    );
    repo.execute(Operation::Merge("feature/test".into()))
        .unwrap();
    assert!(repo.root.join("feature").exists());
    repo.execute(Operation::DeleteBranch("feature/test".into()))
        .unwrap();
    repo.execute(Operation::Tag("v0.1.0".into())).unwrap();
    assert_eq!(repo.snapshot(20).unwrap().tags, vec!["v0.1.0"]);
    repo.execute(Operation::DeleteTag("v0.1.0".into())).unwrap();
    assert!(repo.snapshot(20).unwrap().tags.is_empty());
}
#[test]
fn stash_includes_untracked_and_apply_keeps_entry() {
    let (_dir, repo) = setup();
    write(&repo, "base", "base");
    commit(&repo, "base");
    write(&repo, "draft", "draft");
    let output = repo.execute(Operation::Stash("WIP".into())).unwrap();
    assert!(
        !repo.root.join("draft").exists(),
        "{output}; status: {:?}",
        repo.snapshot(20).unwrap()
    );
    let id = repo.snapshot(20).unwrap().stashes[0].0.clone();
    repo.execute(Operation::ApplyStash(id.clone())).unwrap();
    assert!(repo.root.join("draft").exists());
    assert_eq!(repo.snapshot(20).unwrap().stashes.len(), 1);
    repo.execute(Operation::DropStash(id)).unwrap();
    assert!(repo.snapshot(20).unwrap().stashes.is_empty());
}
#[test]
fn revert_and_cherry_pick() {
    let (_dir, repo) = setup();
    write(&repo, "base", "base");
    commit(&repo, "base");
    repo.execute(Operation::CreateBranch("feature".into()))
        .unwrap();
    write(&repo, "added", "feature");
    commit(&repo, "add feature");
    let id = repo.snapshot(20).unwrap().commits[0].id.clone();
    repo.execute(Operation::Checkout("main".into())).unwrap();
    repo.execute(Operation::CherryPick(id)).unwrap();
    assert!(repo.root.join("added").exists());
    let id = command(&repo.root, &["rev-parse", "HEAD"]);
    repo.execute(Operation::Revert(id)).unwrap();
    assert!(!repo.root.join("added").exists());
}
#[test]
fn merge_conflicts_are_reported_and_abort_restores_state() {
    let (_dir, repo) = setup();
    write(&repo, "conflict", "base\n");
    commit(&repo, "base");
    repo.execute(Operation::CreateBranch("feature".into()))
        .unwrap();
    write(&repo, "conflict", "feature\n");
    commit(&repo, "feature");
    repo.execute(Operation::Checkout("main".into())).unwrap();
    write(&repo, "conflict", "main\n");
    commit(&repo, "main");
    assert!(repo.execute(Operation::Merge("feature".into())).is_err());
    let s = repo.snapshot(20).unwrap();
    assert!(s.merging);
    assert!(s.files[0].conflict());
    repo.execute(Operation::AbortMerge).unwrap();
    assert!(repo.snapshot(20).unwrap().files.is_empty());
    assert_eq!(
        std::fs::read_to_string(repo.root.join("conflict")).unwrap(),
        "main\n"
    );
}
#[test]
fn resolved_merge_commit_retains_both_parents() {
    let (_dir, repo) = setup();
    write(&repo, "conflict", "base\n");
    commit(&repo, "base");
    repo.execute(Operation::CreateBranch("feature".into()))
        .unwrap();
    write(&repo, "conflict", "feature\n");
    commit(&repo, "feature");
    let feature_id = repo.snapshot(20).unwrap().commits[0].id.clone();
    repo.execute(Operation::Checkout("main".into())).unwrap();
    write(&repo, "conflict", "main\n");
    commit(&repo, "main");
    let main_id = repo.snapshot(20).unwrap().commits[0].id.clone();

    assert!(repo.execute(Operation::Merge("feature".into())).is_err());
    write(&repo, "conflict", "resolved\n");
    repo.execute(Operation::Stage(vec!["conflict".into()]))
        .unwrap();
    repo.execute(Operation::Commit("Resolve merge".into()))
        .unwrap();
    let snapshot = repo.snapshot(20).unwrap();
    assert!(!snapshot.merging);
    assert_eq!(snapshot.commits[0].parents, vec![main_id, feature_id]);
    assert_eq!(
        std::fs::read_to_string(repo.root.join("conflict")).unwrap(),
        "resolved\n"
    );
}
#[test]
fn merge_refuses_dirty_worktree_before_any_resettable_operation() {
    let (_dir, repo) = setup();
    write(&repo, "base", "base\n");
    commit(&repo, "base");
    repo.execute(Operation::CreateBranch("feature".into()))
        .unwrap();
    write(&repo, "feature", "feature\n");
    commit(&repo, "feature");
    repo.execute(Operation::Checkout("main".into())).unwrap();
    write(&repo, "base", "keep this edit\n");
    assert!(repo.execute(Operation::Merge("feature".into())).is_err());
    assert_eq!(
        std::fs::read_to_string(repo.root.join("base")).unwrap(),
        "keep this edit\n"
    );
}
#[test]
fn local_remote_clone_fetch_pull_and_push() {
    let (dir, repo) = setup();
    write(&repo, "base", "base");
    commit(&repo, "base");
    let bare = dir.path().join("remote.git");
    command(dir.path(), &["init", "--bare", bare.to_str().unwrap()]);
    repo.execute(Operation::AddRemote(
        "origin".into(),
        bare.to_str().unwrap().into(),
    ))
    .unwrap();
    repo.execute(Operation::Push).unwrap();
    command(&bare, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    let clone_dir = tempfile::tempdir().unwrap();
    let second =
        Repository::clone_repo(bare.to_str().unwrap(), &clone_dir.path().join("clone")).unwrap();
    write(&repo, "base", "updated");
    repo.execute(Operation::Stage(vec!["base".into()])).unwrap();
    repo.execute(Operation::Commit("update".into())).unwrap();
    repo.execute(Operation::Push).unwrap();
    second.execute(Operation::Fetch).unwrap();
    assert_eq!(second.snapshot(20).unwrap().behind, 1);
    second.execute(Operation::Pull).unwrap();
    assert_eq!(
        std::fs::read_to_string(second.root.join("base")).unwrap(),
        "updated"
    );
}
#[test]
fn invalid_inputs_and_untracked_discard_are_rejected() {
    let (_dir, repo) = setup();
    write(&repo, "untracked", "preserved");
    assert!(
        repo.execute(Operation::Discard("untracked".into()))
            .is_err()
    );
    assert!(repo.root.join("untracked").exists());
    assert!(
        repo.execute(Operation::CreateBranch("--help".into()))
            .is_err()
    );
    assert!(repo.execute(Operation::Commit("  ".into())).is_err());
    assert!(repo.show("--help").is_err());
    assert!(
        repo.execute(Operation::ApplyStash("--help".into()))
            .is_err()
    );
}

#[test]
fn push_updates_the_configured_upstream_without_touching_the_same_named_branch() {
    for remote in ["origin", "team/upstream"] {
        let (_dir, repo) = setup();
        write(&repo, "base", "base\n");
        commit(&repo, "base");
        let original = command(&repo.root, &["rev-parse", "HEAD"]);
        let remote_dir = tempfile::tempdir().unwrap();
        command(remote_dir.path(), &["init", "--bare"]);
        repo.execute(Operation::AddRemote(
            remote.into(),
            remote_dir.path().to_str().unwrap().into(),
        ))
        .unwrap();
        command(
            &repo.root,
            &["push", remote, "main:main", "main:release/stable"],
        );
        command(
            &repo.root,
            &[
                "branch",
                "--set-upstream-to",
                &format!("{remote}/release/stable"),
            ],
        );

        for (message, remove_tracking_ref) in [("first update", false), ("second update", true)] {
            if remove_tracking_ref {
                command(
                    &repo.root,
                    &[
                        "update-ref",
                        "-d",
                        &format!("refs/remotes/{remote}/release/stable"),
                    ],
                );
            }
            write(&repo, "base", message);
            commit(&repo, message);
            repo.execute(Operation::Push).unwrap();
            assert_eq!(
                command(
                    remote_dir.path(),
                    &["rev-parse", "refs/heads/release/stable"]
                ),
                command(&repo.root, &["rev-parse", "HEAD"]),
            );
            assert_eq!(
                command(remote_dir.path(), &["rev-parse", "refs/heads/main"]),
                original
            );
            assert_eq!(
                command(&repo.root, &["config", "branch.main.merge"]),
                "refs/heads/release/stable"
            );
        }
    }
}

#[test]
fn push_rejects_incomplete_upstream_configuration() {
    let (_dir, repo) = setup();
    write(&repo, "base", "base\n");
    commit(&repo, "base");
    let remote_dir = tempfile::tempdir().unwrap();
    command(remote_dir.path(), &["init", "--bare"]);
    repo.execute(Operation::AddRemote(
        "origin".into(),
        remote_dir.path().to_str().unwrap().into(),
    ))
    .unwrap();
    command(&repo.root, &["config", "branch.main.remote", "origin"]);
    assert!(repo.execute(Operation::Push).is_err());
    assert!(command(remote_dir.path(), &["for-each-ref", "--format=%(refname)"]).is_empty());
}
#[test]
fn diff_numbers_reset_per_hunk() {
    let lines = diff_lines(
        "--- a/file\n+++ b/file\n@@ -10,2 +20,2 @@\n same\n-old\n+new\n@@ -40 +50 @@\n-x\n+y",
    );
    assert_eq!(lines[3].old, "10");
    assert_eq!(lines[3].new, "20");
    assert_eq!(lines[4].old, "11");
    assert_eq!(lines[5].new, "21");
    assert_eq!(lines[7].old, "40");
    assert_eq!(lines[8].new, "50");
}

#[test]
fn diff_preview_marks_truncation_and_matches_snapshot_cap() {
    let small = "diff --git a/x b/x\n@@ -1 +1 @@\n-a\n+b\n";
    let lines = diff_preview(small);
    assert_eq!(lines.len(), 4);
    assert!(lines.iter().all(|l| !l.text.contains("Preview limited")));

    let huge = "diff --git a/x b/x\n".to_string() + &"+padding\n".repeat(DIFF_LIMIT_LINES + 500);
    let lines = diff_preview(&huge);
    assert_eq!(lines.len(), DIFF_LIMIT_LINES + 1);
    assert_eq!(
        lines.last().unwrap().text,
        "Preview limited to 20,000 lines."
    );
}

#[test]
fn fingerprint_is_stable_until_the_repository_changes() {
    let (_dir, repo) = setup();
    write(&repo, "base", "base\n");
    commit(&repo, "base");
    let before = repo.fingerprint().unwrap();
    // Repeated reads must agree; unrelated mtime churn is not a change.
    assert_eq!(repo.fingerprint().unwrap(), before);
    assert_eq!(repo.snapshot(20).unwrap().fingerprint, before);
    write(&repo, "base", "modified\n");
    assert_ne!(repo.fingerprint().unwrap(), before);
    repo.execute(Operation::StageAll).unwrap();
    let staged = repo.fingerprint().unwrap();
    assert_ne!(staged, before);
    repo.execute(Operation::Commit("change".into())).unwrap();
    assert_ne!(repo.fingerprint().unwrap(), staged);
    // Reference moves (branch creation) are also visible to the timer.
    repo.execute(Operation::CreateBranch("feature".into()))
        .unwrap();
    assert_ne!(repo.fingerprint().unwrap(), staged);
}

#[test]
fn fingerprint_detects_repeated_edits_with_unchanged_status() {
    let (_dir, repo) = setup();
    write(&repo, "base", "base\n");
    commit(&repo, "base");
    write(&repo, "base", "edit one\n");
    let first = repo.snapshot(20).unwrap();
    write(&repo, "base", "edit two\n");
    let second = repo.snapshot(20).unwrap();
    assert_eq!(first.files[0].index, second.files[0].index);
    assert_eq!(first.files[0].worktree, second.files[0].worktree);
    assert_ne!(first.fingerprint, second.fingerprint);
    assert_eq!(second.fingerprint, repo.fingerprint().unwrap());
    assert!(
        repo.diff(&second.files[0], false)
            .unwrap()
            .contains("+edit two")
    );

    write(&repo, "untracked", "one\n");
    let first = repo.fingerprint().unwrap();
    write(&repo, "untracked", "two\n");
    assert_ne!(first, repo.fingerprint().unwrap());
}

#[test]
fn fingerprint_detects_staged_blob_changes_without_worktree_changes() {
    let (_dir, repo) = setup();
    write(&repo, "base", "base\n");
    commit(&repo, "base");
    write(&repo, "base", "staged one\n");
    repo.execute(Operation::StageAll).unwrap();
    write(&repo, "base", "worktree stays the same\n");
    let before = repo.snapshot(20).unwrap();

    // Change only the staged blob, preserving the worktree and MM status.
    let raw = git2::Repository::open(&repo.root).unwrap();
    let mut index = raw.index().unwrap();
    let mut entry = index.get_path(Path::new("base"), 0).unwrap();
    entry.id = raw.blob(b"staged two\n").unwrap();
    index.add(&entry).unwrap();
    index.write().unwrap();

    let after = repo.snapshot(20).unwrap();
    assert_eq!(before.files[0].index, after.files[0].index);
    assert_eq!(before.files[0].worktree, after.files[0].worktree);
    assert_ne!(before.fingerprint, after.fingerprint);
    assert!(
        repo.diff(&after.files[0], true)
            .unwrap()
            .contains("+staged two")
    );
}
#[test]
fn binary_untracked_preview_is_safe() {
    let (_dir, repo) = setup();
    std::fs::write(repo.root.join("binary"), [0, 1, 2, 255]).unwrap();
    let s = repo.snapshot(20).unwrap();
    assert!(repo.diff(&s.files[0], false).unwrap().contains("Binary"));
}

#[test]
fn commit_details_separate_metadata_and_each_file_patch() {
    let (_dir, repo) = setup();
    write(&repo, "[one]\nfile.txt", "unique-first-file\n");
    write(&repo, "second file.txt", "unique-second-file\n");
    std::fs::write(repo.root.join("binary.dat"), [0, 1, 255]).unwrap();
    commit(
        &repo,
        "Root subject\n\nA long description belongs outside the diff.",
    );
    let id = command(&repo.root, &["rev-parse", "HEAD"]);
    let detail = repo.commit_detail(&id).unwrap();
    assert_eq!(detail.subject, "Root subject");
    assert_eq!(detail.body, "A long description belongs outside the diff.");
    assert_eq!(detail.files.len(), 3);
    let first = detail
        .files
        .iter()
        .find(|file| file.path == Path::new("[one]\nfile.txt"))
        .unwrap();
    assert_eq!(first.status, 'A');
    let patch = repo.commit_file_diff(&id, first).unwrap();
    assert!(patch.contains("+unique-first-file"));
    assert!(!patch.contains("unique-second-file"));
    assert!(!patch.contains("Root subject"));
    let binary = detail
        .files
        .iter()
        .find(|file| file.path == Path::new("binary.dat"))
        .unwrap();
    assert!(
        repo.commit_file_diff(&id, binary)
            .unwrap()
            .contains("Binary files")
    );
}

#[test]
fn commit_file_list_preserves_renames_and_deletions() {
    let (_dir, repo) = setup();
    write(&repo, "before name.txt", "rename-me\n");
    write(&repo, "delete.txt", "delete-me\n");
    commit(&repo, "base");
    std::fs::rename(
        repo.root.join("before name.txt"),
        repo.root.join("after\nname.txt"),
    )
    .unwrap();
    std::fs::remove_file(repo.root.join("delete.txt")).unwrap();
    commit(&repo, "rename and remove");
    let id = command(&repo.root, &["rev-parse", "HEAD"]);
    let detail = repo.commit_detail(&id).unwrap();
    assert_eq!(detail.files.len(), 2);
    let renamed = detail.files.iter().find(|file| file.status == 'R').unwrap();
    assert_eq!(renamed.path, Path::new("after\nname.txt"));
    assert_eq!(
        renamed.original.as_deref(),
        Some(Path::new("before name.txt"))
    );
    let patch = repo.commit_file_diff(&id, renamed).unwrap();
    assert!(patch.contains("rename from"));
    assert!(!patch.contains("delete-me"));
    let deleted = detail.files.iter().find(|file| file.status == 'D').unwrap();
    assert!(
        repo.commit_file_diff(&id, deleted)
            .unwrap()
            .contains("-delete-me")
    );
}

#[test]
fn merge_commit_files_and_patches_use_the_same_first_parent() {
    let (_dir, repo) = setup();
    write(&repo, "base", "base\n");
    commit(&repo, "base");
    repo.execute(Operation::CreateBranch("feature".into()))
        .unwrap();
    write(&repo, "feature.txt", "from-feature\n");
    commit(&repo, "feature change");
    repo.execute(Operation::Checkout("main".into())).unwrap();
    write(&repo, "main.txt", "from-main\n");
    commit(&repo, "main change");
    repo.execute(Operation::Merge("feature".into())).unwrap();
    let id = command(&repo.root, &["rev-parse", "HEAD"]);
    let detail = repo.commit_detail(&id).unwrap();
    assert_eq!(detail.files.len(), 1);
    assert_eq!(detail.files[0].path, Path::new("feature.txt"));
    let patch = repo.commit_file_diff(&id, &detail.files[0]).unwrap();
    assert!(patch.contains("+from-feature"));
    assert!(!patch.contains("from-main"));
}
