use gitbuddy::git::{Operation, Repository};
use std::{path::Path, sync::Arc};

fn fixture() -> (tempfile::TempDir, Repository, Vec<u8>) {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();
    let raw = git2::Repository::open(&repo.root).unwrap();
    let mut config = raw.config().unwrap();
    config.set_bool("commit.gpgsign", false).unwrap();
    config.set_str("core.hooksPath", "/dev/null").unwrap();
    repo.execute(Operation::SetIdentity(
        "Test".into(),
        "test@example.invalid".into(),
    ))
    .unwrap();
    std::fs::write(repo.root.join("file.txt"), "before\n").unwrap();
    repo.execute(Operation::StageAll).unwrap();
    repo.execute(Operation::Commit("base".into())).unwrap();
    std::fs::write(repo.root.join("file.txt"), "after\n").unwrap();
    let file = repo
        .snapshot(10)
        .unwrap()
        .files
        .into_iter()
        .find(|f| f.path == Path::new("file.txt"))
        .unwrap();
    let patch = repo
        .worktree_patch(&file, false)
        .unwrap()
        .partial
        .unwrap()
        .patch_bytes()
        .to_vec();
    std::fs::write(repo.root.join("file.txt"), "before\n").unwrap();
    (dir, repo, patch)
}
#[test]
fn exported_patch_applies_to_clean_file_without_changing_index() {
    let (_dir, repo, patch) = fixture();
    let plan = repo.prepare_patch_file(patch).unwrap();
    repo.execute(Operation::ApplyPatch(Arc::new(plan))).unwrap();
    assert_eq!(
        std::fs::read_to_string(repo.root.join("file.txt")).unwrap(),
        "after\n"
    );
    let raw = git2::Repository::open(&repo.root).unwrap();
    let entry = raw
        .index()
        .unwrap()
        .get_path(Path::new("file.txt"), 0)
        .unwrap();
    assert_eq!(raw.find_blob(entry.id).unwrap().content(), b"before\n");
}
#[test]
fn patch_rejects_stale_or_dirty_target_and_unsafe_input() {
    let (_dir, repo, patch) = fixture();
    let plan = repo.prepare_patch_file(patch.clone()).unwrap();
    std::fs::write(repo.root.join("file.txt"), "external\n").unwrap();
    assert!(repo.execute(Operation::ApplyPatch(Arc::new(plan))).is_err());
    assert!(repo.prepare_patch_file(patch).is_err());
    assert_eq!(
        std::fs::read_to_string(repo.root.join("file.txt")).unwrap(),
        "external\n"
    );
    assert!(repo.prepare_patch_file(b"not a patch".to_vec()).is_err());
    assert!(repo.prepare_patch_file(vec![0; 10]).is_err());
}

#[test]
fn import_rejects_invalid_duplicate_and_keeps_unrelated_staged_content() {
    let (_dir, repo, patch) = fixture();
    let mut two = patch.clone();
    two.extend_from_slice(&patch);
    assert!(repo.prepare_patch_file(two).is_err());
    std::fs::write(repo.root.join("other.txt"), "staged\n").unwrap();
    repo.execute(Operation::Stage(vec!["other.txt".into()]))
        .unwrap();
    let plan = repo.prepare_patch_file(patch).unwrap();
    repo.execute(Operation::ApplyPatch(Arc::new(plan))).unwrap();
    let raw = git2::Repository::open(&repo.root).unwrap();
    let id = raw
        .index()
        .unwrap()
        .get_path(Path::new("other.txt"), 0)
        .unwrap()
        .id;
    assert_eq!(raw.find_blob(id).unwrap().content(), b"staged\n");
}

#[test]
fn addition_does_not_overwrite_untracked_destination() {
    let (_dir, repo, _) = fixture();
    let addition = b"diff --git a/new.txt b/new.txt\nnew file mode 100644\n--- /dev/null\n+++ b/new.txt\n@@ -0,0 +1 @@\n+patch content\n";
    std::fs::write(repo.root.join("new.txt"), "existing content\n").unwrap();
    assert!(repo.prepare_patch_file(addition.to_vec()).is_err());
    assert_eq!(
        std::fs::read_to_string(repo.root.join("new.txt")).unwrap(),
        "existing content\n"
    );
}

#[cfg(unix)]
#[test]
fn import_rejects_symlink_and_hardlink_targets() {
    use std::os::unix::fs::symlink;
    let (dir, repo, patch) = fixture();
    let target = repo.root.join("file.txt");
    std::fs::hard_link(&target, dir.path().join("hardlink")).unwrap();
    assert!(repo.prepare_patch_file(patch.clone()).is_err());
    std::fs::remove_file(dir.path().join("hardlink")).unwrap();
    let external = dir.path().join("external");
    std::fs::write(&external, "before\n").unwrap();
    std::fs::remove_file(&target).unwrap();
    symlink(&external, &target).unwrap();
    assert!(repo.prepare_patch_file(patch).is_err());
    assert_eq!(std::fs::read_to_string(external).unwrap(), "before\n");
}

#[cfg(target_os = "macos")]
#[test]
fn import_preserves_working_file_attributes() {
    let (_dir, repo, patch) = fixture();
    let path = repo.root.join("file.txt");
    assert!(
        std::process::Command::new("xattr")
            .args(["-w", "org.gitbuddy.test", "preserve"])
            .arg(&path)
            .status()
            .unwrap()
            .success()
    );
    let plan = repo.prepare_patch_file(patch).unwrap();
    repo.execute(Operation::ApplyPatch(Arc::new(plan))).unwrap();
    let result = std::process::Command::new("xattr")
        .args(["-p", "org.gitbuddy.test"])
        .arg(&path)
        .output()
        .unwrap();
    assert!(result.status.success());
    assert_eq!(String::from_utf8_lossy(&result.stdout).trim(), "preserve");
}

#[test]
fn libgit2_can_parse_and_apply_git_binary_patch_to_tree() {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();
    let raw = git2::Repository::open(dir.path()).unwrap();
    let mut config = raw.config().unwrap();
    config.set_bool("commit.gpgsign", false).unwrap();
    config.set_str("core.hooksPath", "/dev/null").unwrap();
    repo.execute(Operation::SetIdentity(
        "Test".into(),
        "test@example.invalid".into(),
    ))
    .unwrap();
    std::fs::write(dir.path().join("binary.bin"), [0, 1, 2, 3, 4, 5]).unwrap();
    repo.execute(Operation::StageAll).unwrap();
    repo.execute(Operation::Commit("base".into())).unwrap();
    std::fs::write(dir.path().join("binary.bin"), [0, 1, 8, 9, 4, 5]).unwrap();
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(dir.path())
        .args(["diff", "--binary", "--", "binary.bin"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let diff = git2::Diff::from_buffer(&output.stdout).unwrap();
    let mut binary_count = 0;
    diff.foreach(
        &mut |_, _| true,
        Some(&mut |_, _| {
            binary_count += 1;
            true
        }),
        None,
        None,
    )
    .unwrap();
    assert_eq!(
        binary_count, 1,
        "binary payload must be inspected before libgit2 inflates it"
    );
    let tree = raw.head().unwrap().peel_to_tree().unwrap();
    let result = raw.apply_to_tree(&tree, &diff, None).unwrap();
    let entry = result.get_path(Path::new("binary.bin"), 0).unwrap();
    assert_eq!(
        raw.find_blob(entry.id).unwrap().content(),
        &[0, 1, 8, 9, 4, 5]
    );
    std::fs::write(dir.path().join("binary.bin"), [0, 1, 2, 3, 4, 5]).unwrap();
    let no_payload = b"diff --git a/binary.bin b/binary.bin\nindex 1234567..7654321 100644\nBinary files a/binary.bin and b/binary.bin differ\n";
    assert!(repo.prepare_patch_file(no_payload.to_vec()).is_err());
    let plan = repo.prepare_patch_file(output.stdout).unwrap();
    repo.execute(Operation::ApplyPatch(Arc::new(plan))).unwrap();
    assert_eq!(
        std::fs::read(dir.path().join("binary.bin")).unwrap(),
        &[0, 1, 8, 9, 4, 5]
    );
}

#[test]
fn batch_patch_applies_add_delete_rename_and_binary_preserving_index() {
    use std::process::Command;
    let dir = tempfile::tempdir().unwrap();
    let source = Repository::init(&dir.path().join("source")).unwrap();
    let raw = git2::Repository::open(&source.root).unwrap();
    let mut config = raw.config().unwrap();
    config.set_bool("commit.gpgsign", false).unwrap();
    config.set_str("core.hooksPath", "/dev/null").unwrap();
    source
        .execute(Operation::SetIdentity(
            "Test".into(),
            "test@example.invalid".into(),
        ))
        .unwrap();
    std::fs::write(source.root.join("delete.txt"), "delete me\n").unwrap();
    std::fs::write(source.root.join("old.txt"), "rename content\n").unwrap();
    std::fs::write(source.root.join("binary.bin"), [0, 1, 2, 3, 4]).unwrap();
    std::fs::write(source.root.join("other.txt"), "base\n").unwrap();
    source.execute(Operation::StageAll).unwrap();
    source.execute(Operation::Commit("base".into())).unwrap();
    let target_path = dir.path().join("target");
    assert!(
        Command::new("git")
            .args(["clone", "--quiet", "--no-hardlinks"])
            .arg(&source.root)
            .arg(&target_path)
            .status()
            .unwrap()
            .success()
    );
    let target = Repository::open(&target_path).unwrap();
    std::fs::remove_file(source.root.join("delete.txt")).unwrap();
    std::fs::rename(source.root.join("old.txt"), source.root.join("renamed.txt")).unwrap();
    std::fs::create_dir(source.root.join("nested")).unwrap();
    std::fs::write(source.root.join("nested/new.txt"), "new file\n").unwrap();
    std::fs::write(source.root.join("binary.bin"), [0, 7, 8, 3, 4]).unwrap();
    assert!(
        Command::new("git")
            .arg("-C")
            .arg(&source.root)
            .args(["add", "-A"])
            .status()
            .unwrap()
            .success()
    );
    let output = Command::new("git")
        .arg("-C")
        .arg(&source.root)
        .args(["diff", "--cached", "--binary", "-M"])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("rename from old.txt"));
    let exported = source.export_patch(true).unwrap();
    assert!(String::from_utf8_lossy(&exported).contains("GIT binary patch"));
    let exported_diff = git2::Diff::from_buffer(&exported).unwrap();
    let target_raw = git2::Repository::open(&target.root).unwrap();
    let target_tree = target_raw.head().unwrap().peel_to_tree().unwrap();
    let exported_index = target_raw
        .apply_to_tree(&target_tree, &exported_diff, None)
        .unwrap();
    let binary_entry = exported_index.get_path(Path::new("binary.bin"), 0).unwrap();
    assert_eq!(
        target_raw.find_blob(binary_entry.id).unwrap().content(),
        &[0, 7, 8, 3, 4]
    );
    std::fs::write(target.root.join("other.txt"), "staged elsewhere\n").unwrap();
    target
        .execute(Operation::Stage(vec!["other.txt".into()]))
        .unwrap();
    let plan = target.prepare_patch_file(output.stdout).unwrap();
    target
        .execute(Operation::ApplyPatch(Arc::new(plan)))
        .unwrap();
    assert!(!target.root.join("delete.txt").exists());
    assert!(!target.root.join("old.txt").exists());
    assert_eq!(
        std::fs::read_to_string(target.root.join("renamed.txt")).unwrap(),
        "rename content\n"
    );
    assert_eq!(
        std::fs::read_to_string(target.root.join("nested/new.txt")).unwrap(),
        "new file\n"
    );
    assert_eq!(
        std::fs::read(target.root.join("binary.bin")).unwrap(),
        [0, 7, 8, 3, 4]
    );
    let raw = git2::Repository::open(&target.root).unwrap();
    let index = raw.index().unwrap();
    assert!(index.get_path(Path::new("old.txt"), 0).is_some());
    assert!(index.get_path(Path::new("renamed.txt"), 0).is_none());
    assert_eq!(
        raw.find_blob(index.get_path(Path::new("other.txt"), 0).unwrap().id)
            .unwrap()
            .content(),
        b"staged elsewhere\n"
    );
    assert!(!target.has_patch_recovery().unwrap());
}
