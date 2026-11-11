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

fn case_rename_fixture(content_change: bool) -> (tempfile::TempDir, Repository, Vec<u8>) {
    rename_fixture("Readme.txt", "README.txt", content_change)
}

fn rename_fixture(
    old: &str,
    new: &str,
    content_change: bool,
) -> (tempfile::TempDir, Repository, Vec<u8>) {
    use std::process::Command;
    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();
    let raw = git2::Repository::open(&repo.root).unwrap();
    raw.config()
        .unwrap()
        .set_bool("commit.gpgsign", false)
        .unwrap();
    raw.config()
        .unwrap()
        .set_str("core.hooksPath", "/dev/null")
        .unwrap();
    repo.execute(Operation::SetIdentity(
        "Test".into(),
        "test@example.invalid".into(),
    ))
    .unwrap();
    let original = "one\ntwo\nthree\nfour\nfive\nsix\n";
    std::fs::create_dir_all(repo.root.join(old).parent().unwrap()).unwrap();
    std::fs::write(repo.root.join(old), original).unwrap();
    std::fs::write(repo.root.join("unrelated.txt"), "before\n").unwrap();
    repo.execute(Operation::StageAll).unwrap();
    repo.execute(Operation::Commit("base".into())).unwrap();
    let git = |args: &[&str]| {
        let output = Command::new("git")
            .arg("-C")
            .arg(&repo.root)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    };
    // Two actual Git moves also force the spelling change on case-insensitive volumes.
    git(&["mv", old, "rename-intermediate"]);
    std::fs::create_dir_all(repo.root.join(new).parent().unwrap()).unwrap();
    git(&["mv", "rename-intermediate", new]);
    if content_change {
        std::fs::write(repo.root.join(new), "one\ntwo\nTHREE\nfour\nfive\nsix\n").unwrap();
        std::fs::write(repo.root.join("unrelated.txt"), "after\n").unwrap();
    }
    git(&["add", "-A"]);
    let patch = git(&["diff", "--cached", "--binary", "-M"]);
    assert!(
        git2::Diff::from_buffer(&patch)
            .unwrap()
            .deltas()
            .any(|d| d.status() == git2::Delta::Renamed)
    );
    std::fs::rename(repo.root.join(new), repo.root.join("rename-intermediate")).unwrap();
    std::fs::rename(repo.root.join("rename-intermediate"), repo.root.join(old)).unwrap();
    std::fs::write(repo.root.join(old), original).unwrap();
    std::fs::write(repo.root.join("unrelated.txt"), "before\n").unwrap();
    git(&["reset", "--mixed", "HEAD"]);
    (dir, repo, patch)
}

#[test]
fn case_only_basename_rename_applies_with_unchanged_content_and_index() {
    let (_dir, repo, patch) = case_rename_fixture(false);
    let raw = git2::Repository::open(&repo.root).unwrap();
    let original_index = std::fs::read(raw.path().join("index")).unwrap();
    let plan = repo.prepare_patch_file(patch).unwrap();
    repo.execute(Operation::ApplyPatch(Arc::new(plan))).unwrap();
    let names = std::fs::read_dir(&repo.root)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect::<Vec<_>>();
    assert!(names.contains(&"README.txt".into()));
    assert!(!names.contains(&"Readme.txt".into()));
    assert_eq!(
        std::fs::read_to_string(repo.root.join("README.txt")).unwrap(),
        "one\ntwo\nthree\nfour\nfive\nsix\n"
    );
    assert_eq!(
        std::fs::read(raw.path().join("index")).unwrap(),
        original_index
    );
    assert!(!repo.has_patch_recovery().unwrap());
}

#[test]
fn case_only_rename_with_content_changes_can_share_an_ordinary_batch() {
    let (_dir, repo, patch) = case_rename_fixture(true);
    let plan = repo.prepare_patch_file(patch).unwrap();
    repo.execute(Operation::ApplyPatch(Arc::new(plan))).unwrap();
    assert_eq!(
        std::fs::read_to_string(repo.root.join("README.txt")).unwrap(),
        "one\ntwo\nTHREE\nfour\nfive\nsix\n"
    );
    assert_eq!(
        std::fs::read_to_string(repo.root.join("unrelated.txt")).unwrap(),
        "after\n"
    );
    let raw = git2::Repository::open(&repo.root).unwrap();
    let index = raw.index().unwrap();
    assert!(index.iter().any(|entry| entry.path == b"Readme.txt"));
    assert!(!index.iter().any(|entry| entry.path == b"README.txt"));
}

#[test]
fn case_only_rename_refuses_a_distinct_destination_or_stale_source_spelling() {
    let (_dir, repo, patch) = case_rename_fixture(false);
    let source = repo.root.join("Readme.txt");
    let destination = repo.root.join("README.txt");
    // create_new detects the actual filesystem semantics without assuming the host OS.
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&destination)
    {
        Ok(_) => {
            std::fs::write(&destination, "existing destination\n").unwrap();
            assert!(repo.prepare_patch_file(patch.clone()).is_err());
            assert_eq!(
                std::fs::read_to_string(&destination).unwrap(),
                "existing destination\n"
            );
            std::fs::remove_file(&destination).unwrap();
        }
        Err(error) => assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists),
    }
    // The same test still checks stale casing on case-insensitive filesystems.
    let plan = repo.prepare_patch_file(patch).unwrap();
    std::fs::rename(&source, repo.root.join("rename-intermediate")).unwrap();
    std::fs::rename(
        repo.root.join("rename-intermediate"),
        repo.root.join("readme.txt"),
    )
    .unwrap();
    assert!(repo.execute(Operation::ApplyPatch(Arc::new(plan))).is_err());
    assert!(
        std::fs::read_dir(&repo.root)
            .unwrap()
            .any(|e| e.unwrap().file_name() == "readme.txt")
    );
}

#[test]
fn case_only_rename_refuses_a_staged_destination_even_with_an_ignorecase_index() {
    let (_dir, repo, patch) = case_rename_fixture(false);
    git2::Repository::open(&repo.root)
        .unwrap()
        .config()
        .unwrap()
        .set_bool("core.ignorecase", false)
        .unwrap();
    let raw = git2::Repository::open(&repo.root).unwrap();
    let mut index = raw.index().unwrap();
    let mut entry = index.get_path(Path::new("Readme.txt"), 0).unwrap();
    entry.path = b"README.txt".to_vec();
    index.add(&entry).unwrap();
    index.write().unwrap();
    assert!(index.iter().any(|entry| entry.path == b"Readme.txt"));
    assert!(index.iter().any(|entry| entry.path == b"README.txt"));
    raw.config()
        .unwrap()
        .set_bool("core.ignorecase", true)
        .unwrap();
    let original_index = std::fs::read(raw.path().join("index")).unwrap();
    assert!(repo.prepare_patch_file(patch).is_err());
    assert_eq!(
        std::fs::read(raw.path().join("index")).unwrap(),
        original_index
    );
}

#[test]
fn case_only_rename_refuses_directory_case_changes_and_unicode_basename_folding() {
    for (old, new) in [
        ("docs/Readme.txt", "Docs/Readme.txt"),
        ("docs/Readme.txt", "Docs/Renamed.txt"),
        ("Réadme.txt", "RÉADME.txt"),
    ] {
        let (_dir, repo, patch) = rename_fixture(old, new, false);
        assert!(repo.prepare_patch_file(patch).is_err());
        assert_eq!(
            std::fs::read_to_string(repo.root.join(old)).unwrap(),
            "one\ntwo\nthree\nfour\nfive\nsix\n"
        );
        assert!(!repo.has_patch_recovery().unwrap());
    }
}

#[cfg(target_os = "macos")]
#[test]
fn case_only_rename_preserves_working_file_attributes() {
    let (_dir, repo, patch) = case_rename_fixture(true);
    assert!(
        std::process::Command::new("xattr")
            .args(["-w", "org.gitbuddy.case-rename", "preserve"])
            .arg(repo.root.join("Readme.txt"))
            .status()
            .unwrap()
            .success()
    );
    let plan = repo.prepare_patch_file(patch).unwrap();
    repo.execute(Operation::ApplyPatch(Arc::new(plan))).unwrap();
    let attributes = std::process::Command::new("xattr")
        .args(["-p", "org.gitbuddy.case-rename"])
        .arg(repo.root.join("README.txt"))
        .output()
        .unwrap();
    assert!(attributes.status.success());
    assert_eq!(
        String::from_utf8_lossy(&attributes.stdout).trim(),
        "preserve"
    );
}

fn conversion_fixture(
    attributes: &str,
    autocrlf: &str,
) -> (tempfile::TempDir, Repository, Vec<u8>, Vec<u8>, Vec<u8>) {
    use std::process::Command;
    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();
    let raw = git2::Repository::open(&repo.root).unwrap();
    let mut config = raw.config().unwrap();
    config.set_bool("commit.gpgsign", false).unwrap();
    config.set_str("core.hooksPath", "/dev/null").unwrap();
    config.set_str("core.attributesfile", "").unwrap();
    config.set_str("core.autocrlf", autocrlf).unwrap();
    config.set_bool("core.safecrlf", false).unwrap();
    repo.execute(Operation::SetIdentity(
        "Test".into(),
        "test@example.invalid".into(),
    ))
    .unwrap();
    std::fs::write(
        repo.root.join(".gitattributes"),
        format!("* !filter !working-tree-encoding\n{attributes}"),
    )
    .unwrap();
    std::fs::write(repo.root.join("file.txt"), "before\n$Id$\n$Id$\n").unwrap();
    let git = |args: &[&str]| {
        let output = Command::new("git")
            .arg("-C")
            .arg(&repo.root)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    };
    git(&["add", "-A"]);
    git(&["commit", "-qm", "base"]);
    std::fs::remove_file(repo.root.join("file.txt")).unwrap();
    git(&["checkout-index", "-f", "file.txt"]);
    let before = std::fs::read(repo.root.join("file.txt")).unwrap();
    let after = String::from_utf8(before.clone())
        .unwrap()
        .replace("before", "after");
    std::fs::write(repo.root.join("file.txt"), after).unwrap();
    let patch = git(&[
        "diff",
        "--binary",
        "--no-ext-diff",
        "--no-textconv",
        "--",
        "file.txt",
    ]);
    std::fs::write(repo.root.join("file.txt"), &before).unwrap();
    let patch_path = dir.path().join("reference.patch");
    std::fs::write(&patch_path, &patch).unwrap();
    git(&["apply", patch_path.to_str().unwrap()]);
    let expected = std::fs::read(repo.root.join("file.txt")).unwrap();
    std::fs::write(repo.root.join("file.txt"), &before).unwrap();
    std::fs::remove_file(patch_path).unwrap();
    (dir, repo, patch, before, expected)
}

#[test]
fn builtin_patch_conversions_match_git_for_crlf_ident_autocrlf_and_quoted_rules() {
    for (attributes, autocrlf) in [
        ("file.txt text eol=crlf\n", "false"),
        ("file.txt text=auto eol=crlf\n", "false"),
        ("file.txt -text\n", "true"),
        ("file.txt text eol=lf\n", "true"),
        ("file.txt ident\n", "false"),
        ("\"file.txt\" text eol=crlf ident\n", "false"),
        ("", "true"),
        ("", "input"),
    ] {
        let (_dir, repo, patch, _before, expected) = conversion_fixture(attributes, autocrlf);
        let raw = git2::Repository::open(&repo.root).unwrap();
        let index = std::fs::read(raw.path().join("index")).unwrap();
        let plan = repo.prepare_patch_file(patch).unwrap();
        repo.execute(Operation::ApplyPatch(Arc::new(plan))).unwrap();
        assert_eq!(
            std::fs::read(repo.root.join("file.txt")).unwrap(),
            expected,
            "{attributes} / {autocrlf}"
        );
        assert_eq!(std::fs::read(raw.path().join("index")).unwrap(), index);
    }
}

#[test]
fn builtin_patch_conversion_matches_git_apply_safecrlf_exception() {
    let (_dir, repo, patch, before, expected) =
        conversion_fixture("file.txt text eol=crlf\n", "false");
    let lf = String::from_utf8(before).unwrap().replace("\r\n", "\n");
    std::fs::write(repo.root.join("file.txt"), lf).unwrap();
    git2::Repository::open(&repo.root)
        .unwrap()
        .config()
        .unwrap()
        .set_bool("core.safecrlf", true)
        .unwrap();
    let plan = repo.prepare_patch_file(patch).unwrap();
    repo.execute(Operation::ApplyPatch(Arc::new(plan))).unwrap();
    assert_eq!(std::fs::read(repo.root.join("file.txt")).unwrap(), expected);
}

#[test]
fn builtin_conversion_does_not_treat_unknown_core_eol_spelling_as_crlf() {
    let (_dir, repo, patch, _, expected) = conversion_fixture("file.txt text\n", "false");
    git2::Repository::open(&repo.root)
        .unwrap()
        .config()
        .unwrap()
        .set_str("core.eol", "CRLF")
        .unwrap();
    let plan = repo.prepare_patch_file(patch).unwrap();
    repo.execute(Operation::ApplyPatch(Arc::new(plan))).unwrap();
    // Git treats unknown/case-mismatched core.eol values as its native default.
    assert_eq!(std::fs::read(repo.root.join("file.txt")).unwrap(), expected);
}

#[test]
fn builtin_conversion_rejects_stale_attributes_and_configuration() {
    for attributes_changed in [false, true] {
        let (_dir, repo, patch, before, _) =
            conversion_fixture("file.txt text eol=crlf\n", "false");
        let plan = repo.prepare_patch_file(patch).unwrap();
        if attributes_changed {
            std::fs::write(repo.root.join(".gitattributes"), "file.txt text eol=lf\n").unwrap();
        } else {
            git2::Repository::open(&repo.root)
                .unwrap()
                .config()
                .unwrap()
                .set_str("core.autocrlf", "input")
                .unwrap();
        }
        assert!(repo.execute(Operation::ApplyPatch(Arc::new(plan))).is_err());
        assert_eq!(std::fs::read(repo.root.join("file.txt")).unwrap(), before);
        assert!(!repo.has_patch_recovery().unwrap());
    }
}

#[test]
fn builtin_conversion_rejects_attribute_provenance_changes_with_identical_effective_values() {
    for source in ["global", "info", "worktree"] {
        let (dir, repo, patch, before, _) = conversion_fixture("file.txt text eol=crlf\n", "false");
        let external = tempfile::tempdir().unwrap();
        let raw = git2::Repository::open(&repo.root).unwrap();
        let path = match source {
            "global" => {
                let path = external.path().join("attributes");
                std::fs::write(&path, "# initial external source\n").unwrap();
                raw.config()
                    .unwrap()
                    .set_str("core.attributesfile", path.to_str().unwrap())
                    .unwrap();
                path
            }
            "info" => raw.path().join("info/attributes"),
            _ => repo.root.join(".gitattributes"),
        };
        let plan = repo.prepare_patch_file(patch).unwrap();
        let mut contents = std::fs::read(&path).unwrap_or_default();
        contents.extend_from_slice(b"# changed provenance, same selected attribute outcomes\n");
        std::fs::write(&path, contents).unwrap();
        assert!(
            repo.execute(Operation::ApplyPatch(Arc::new(plan))).is_err(),
            "{source}"
        );
        assert_eq!(std::fs::read(repo.root.join("file.txt")).unwrap(), before);
        assert!(!repo.has_patch_recovery().unwrap());
        drop(dir);
    }
}

#[test]
fn patch_conversion_explicitly_refuses_custom_drivers_and_unknown_worktree_encoding() {
    for rule in ["filter=custom", "working-tree-encoding=GITBUDDY-UNKNOWN"] {
        let (_dir, repo, patch) = fixture();
        let marker = repo.root.join("filter-ran");
        let raw = git2::Repository::open(&repo.root).unwrap();
        let command = format!(
            "touch '{}'",
            marker.to_string_lossy().replace('\'', "'\\''")
        );
        for operation in ["clean", "smudge", "process"] {
            raw.config()
                .unwrap()
                .set_str(&format!("filter.custom.{operation}"), &command)
                .unwrap();
        }
        std::fs::write(
            repo.root.join(".gitattributes"),
            format!("file.txt {rule}\n"),
        )
        .unwrap();
        let error = repo.prepare_patch_file(patch).unwrap_err().to_string();
        assert!(
            error.contains("filter") || error.contains("encoding"),
            "{error}"
        );
        assert!(
            !marker.exists(),
            "External filter program ran during preview"
        );
        assert_eq!(
            std::fs::read_to_string(repo.root.join("file.txt")).unwrap(),
            "before\n"
        );
    }
}

#[test]
fn renamed_file_uses_destination_conversion_rules_and_preserves_index() {
    let (_dir, repo, _patch, before, _) = conversion_fixture(
        "file.txt text eol=crlf\nrenamed.txt text eol=lf ident\n",
        "false",
    );
    let git = |args: &[&str]| {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(&repo.root)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    };
    git(&["mv", "file.txt", "renamed.txt"]);
    std::fs::write(
        repo.root.join("renamed.txt"),
        String::from_utf8(before.clone())
            .unwrap()
            .replace("before", "after"),
    )
    .unwrap();
    git(&["add", "renamed.txt"]);
    let patch = git(&["diff", "--cached", "--binary", "-M"]);
    assert!(String::from_utf8_lossy(&patch).contains("rename from file.txt"));
    git(&["reset", "--mixed", "HEAD"]);
    std::fs::rename(repo.root.join("renamed.txt"), repo.root.join("file.txt")).unwrap();
    std::fs::write(repo.root.join("file.txt"), &before).unwrap();
    let raw = git2::Repository::open(&repo.root).unwrap();
    let index = std::fs::read(raw.path().join("index")).unwrap();
    let plan = repo.prepare_patch_file(patch).unwrap();
    repo.execute(Operation::ApplyPatch(Arc::new(plan))).unwrap();
    assert!(!repo.root.join("file.txt").exists());
    let after = std::fs::read(repo.root.join("renamed.txt")).unwrap();
    assert!(!after.contains(&b'\r'));
    let canonical = b"after\n$Id$\n$Id$\n";
    let oid = git2::Oid::hash_object(git2::ObjectType::Blob, canonical).unwrap();
    assert_eq!(
        after,
        format!("after\n$Id: {oid} $\n$Id: {oid} $\n").as_bytes()
    );
    assert_eq!(std::fs::read(raw.path().join("index")).unwrap(), index);
}

#[test]
fn builtin_conversion_refuses_controlling_attribute_changes_in_the_same_batch() {
    let (_dir, repo, mut patch) = fixture();
    patch.extend_from_slice(b"diff --git a/.gitattributes b/.gitattributes\nnew file mode 100644\n--- /dev/null\n+++ b/.gitattributes\n@@ -0,0 +1 @@\n+file.txt text eol=crlf\n");
    let error = repo.prepare_patch_file(patch).unwrap_err().to_string();
    assert!(error.contains("controlling .gitattributes"), "{error}");
    assert_eq!(
        std::fs::read(repo.root.join("file.txt")).unwrap(),
        b"before\n"
    );
}

#[test]
fn builtin_conversion_bounds_ident_expansion_and_total_actual_payload() {
    for (files, tokens) in [(1, 400_000), (5, 300_000)] {
        let (_dir, repo, _patch) = fixture();
        std::fs::write(repo.root.join(".gitattributes"), "*.bin -text ident\n").unwrap();
        let git = |args: &[&str]| {
            let output = std::process::Command::new("git")
                .arg("-C")
                .arg(&repo.root)
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            output.stdout
        };
        let mut bytes = vec![0];
        bytes.extend_from_slice(&b"$Id$".repeat(tokens));
        for i in 0..files {
            let name = format!("{i}.bin");
            std::fs::write(repo.root.join(&name), &bytes).unwrap();
            git(&["add", &name]);
        }
        let patch = git(&["diff", "--cached", "--binary"]);
        assert!(patch.len() < 2 * 1024 * 1024);
        git(&["reset", "--mixed", "HEAD"]);
        for i in 0..files {
            std::fs::remove_file(repo.root.join(format!("{i}.bin"))).unwrap();
        }
        let error = repo.prepare_patch_file(patch).unwrap_err().to_string();
        assert!(
            error.contains(if files == 1 { "16 MiB" } else { "64 MiB" }),
            "{error}"
        );
        assert!(!repo.has_patch_recovery().unwrap());
        for i in 0..files {
            assert!(!repo.root.join(format!("{i}.bin")).exists());
        }
    }
}

#[cfg(unix)]
#[test]
fn symbolic_link_patch_content_bypasses_all_content_conversions() {
    use std::os::unix::fs::symlink;
    let (_dir, repo, _) = fixture();
    symlink("old-$Id$", repo.root.join("link")).unwrap();
    repo.execute(Operation::Stage(vec!["link".into()])).unwrap();
    repo.execute(Operation::Commit("link".into())).unwrap();
    std::fs::write(
        repo.root.join(".gitattributes"),
        "link text eol=crlf ident filter=custom working-tree-encoding=UTF-16LE\n",
    )
    .unwrap();
    std::fs::remove_file(repo.root.join("link")).unwrap();
    symlink("new-$Id$", repo.root.join("link")).unwrap();
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(&repo.root)
        .args(["diff", "--binary", "--", "link"])
        .output()
        .unwrap();
    assert!(output.status.success());
    std::fs::remove_file(repo.root.join("link")).unwrap();
    symlink("old-$Id$", repo.root.join("link")).unwrap();
    let plan = repo.prepare_patch_file(output.stdout).unwrap();
    repo.execute(Operation::ApplyPatch(Arc::new(plan))).unwrap();
    assert_eq!(
        std::fs::read_link(repo.root.join("link")).unwrap(),
        Path::new("new-$Id$")
    );
}

fn standard_lfs_pointer(bytes: &[u8]) -> Vec<u8> {
    use sha2::{Digest, Sha256};
    format!(
        "version https://git-lfs.github.com/spec/v1\noid sha256:{:x}\nsize {}\n",
        Sha256::digest(bytes),
        bytes.len()
    )
    .into_bytes()
}

struct LfsPatchFixture {
    _directory: tempfile::TempDir,
    repo: Repository,
    patch: Vec<u8>,
    before: Vec<u8>,
    after: Vec<u8>,
    new_object: std::path::PathBuf,
}

fn lfs_patch_fixture() -> LfsPatchFixture {
    use sha2::{Digest, Sha256};
    let (directory, repo, _) = fixture();
    let raw = git2::Repository::open(&repo.root).unwrap();
    raw.config()
        .unwrap()
        .set_str("core.attributesfile", "")
        .unwrap();
    raw.config().unwrap().set_str("lfs.storage", "lfs").unwrap();
    std::fs::write(
        repo.root.join(".gitattributes"),
        "*.bin filter=lfs diff=lfs merge=lfs -text\n",
    )
    .unwrap();
    let before = b"before LFS bytes\0\n".to_vec();
    let after = b"after! LFS bytes\0\n".to_vec();
    std::fs::write(repo.root.join("data.bin"), &before).unwrap();
    let mut index = raw.index().unwrap();
    index.add_path(Path::new(".gitattributes")).unwrap();
    let mut entry = index.get_path(Path::new("file.txt"), 0).unwrap();
    entry.path = b"data.bin".to_vec();
    entry.flags = 0;
    index
        .add_frombuffer(&entry, &standard_lfs_pointer(&before))
        .unwrap();
    index.write().unwrap();
    repo.execute(Operation::Commit("LFS base".into())).unwrap();
    let mut objects = Vec::new();
    for bytes in [&before, &after] {
        let oid = format!("{:x}", Sha256::digest(bytes));
        let object = raw
            .commondir()
            .join("lfs/objects")
            .join(&oid[..2])
            .join(&oid[2..4])
            .join(&oid);
        std::fs::create_dir_all(object.parent().unwrap()).unwrap();
        std::fs::write(&object, bytes).unwrap();
        objects.push(object);
    }
    let mut index = raw.index().unwrap();
    let entry = index.get_path(Path::new("data.bin"), 0).unwrap();
    index
        .add_frombuffer(&entry, &standard_lfs_pointer(&after))
        .unwrap();
    index.write().unwrap();
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(&repo.root)
        .args(["diff", "--cached", "--binary", "--", "data.bin"])
        .output()
        .unwrap();
    assert!(output.status.success());
    index
        .read_tree(&raw.head().unwrap().peel_to_tree().unwrap())
        .unwrap();
    index.write().unwrap();
    LfsPatchFixture {
        _directory: directory,
        repo,
        patch: output.stdout,
        before,
        after,
        new_object: objects.pop().unwrap(),
    }
}

#[test]
fn lfs_pointer_patch_materializes_only_cached_content_without_changing_index() {
    let f = lfs_patch_fixture();
    let raw = git2::Repository::open(&f.repo.root).unwrap();
    let index = std::fs::read(raw.path().join("index")).unwrap();
    let plan = f.repo.prepare_patch_file(f.patch).unwrap();
    f.repo
        .execute(Operation::ApplyPatch(Arc::new(plan)))
        .unwrap();
    assert_eq!(
        std::fs::read(f.repo.root.join("data.bin")).unwrap(),
        f.after
    );
    assert_eq!(std::fs::read(raw.path().join("index")).unwrap(), index);
    assert!(!f.repo.has_patch_recovery().unwrap());
}

#[test]
fn lfs_pointer_patch_refuses_missing_cached_object_without_publishing_a_pointer() {
    let f = lfs_patch_fixture();
    std::fs::remove_file(&f.new_object).unwrap();
    let raw = git2::Repository::open(&f.repo.root).unwrap();
    let index = std::fs::read(raw.path().join("index")).unwrap();
    let error = f.repo.prepare_patch_file(f.patch).unwrap_err().to_string();
    assert!(error.contains("LFS"), "{error}");
    assert_eq!(
        std::fs::read(f.repo.root.join("data.bin")).unwrap(),
        f.before
    );
    assert_eq!(std::fs::read(raw.path().join("index")).unwrap(), index);
    assert!(!f.repo.has_patch_recovery().unwrap());
}

#[test]
fn lfs_additions_and_deletions_keep_the_index_and_materialization_contract() {
    for addition in [false, true] {
        let f = lfs_patch_fixture();
        let raw = git2::Repository::open(&f.repo.root).unwrap();
        let mut index = raw.index().unwrap();
        let affected = if addition {
            let mut entry = index.get_path(Path::new("data.bin"), 0).unwrap();
            entry.path = b"fresh.bin".to_vec();
            index
                .add_frombuffer(&entry, &standard_lfs_pointer(&f.after))
                .unwrap();
            "fresh.bin"
        } else {
            index.remove_path(Path::new("data.bin")).unwrap();
            std::fs::remove_dir_all(raw.commondir().join("lfs")).unwrap();
            "data.bin"
        };
        index.write().unwrap();
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(&f.repo.root)
            .args(["diff", "--cached", "--binary", "--", affected])
            .output()
            .unwrap();
        assert!(output.status.success());
        index
            .read_tree(&raw.head().unwrap().peel_to_tree().unwrap())
            .unwrap();
        index.write().unwrap();
        let original_index = std::fs::read(raw.path().join("index")).unwrap();
        let plan = f.repo.prepare_patch_file(output.stdout).unwrap();
        f.repo
            .execute(Operation::ApplyPatch(Arc::new(plan)))
            .unwrap();
        if addition {
            assert_eq!(std::fs::read(f.repo.root.join(affected)).unwrap(), f.after);
            assert_eq!(
                std::fs::read(f.repo.root.join("data.bin")).unwrap(),
                f.before
            );
        } else {
            assert!(!f.repo.root.join(affected).exists());
        }
        assert_eq!(
            std::fs::read(raw.path().join("index")).unwrap(),
            original_index
        );
        assert!(!f.repo.has_patch_recovery().unwrap());
    }
}

#[test]
fn lfs_patch_rejects_stale_attributes_and_storage_configuration() {
    for attributes in [false, true] {
        let f = lfs_patch_fixture();
        let plan = f.repo.prepare_patch_file(f.patch).unwrap();
        if attributes {
            std::fs::write(f.repo.root.join(".gitattributes"), "*.bin -filter -text\n").unwrap();
        } else {
            git2::Repository::open(&f.repo.root)
                .unwrap()
                .config()
                .unwrap()
                .set_str("lfs.storage", "changed-cache")
                .unwrap();
        }
        assert!(
            f.repo
                .execute(Operation::ApplyPatch(Arc::new(plan)))
                .is_err()
        );
        assert_eq!(
            std::fs::read(f.repo.root.join("data.bin")).unwrap(),
            f.before
        );
        assert!(!f.repo.has_patch_recovery().unwrap());
    }
}

#[test]
fn renamed_patch_uses_lfs_attributes_at_each_end_of_the_move() {
    for (destination, plain_source) in [
        ("renamed.bin", false),
        ("renamed.txt", false),
        ("renamed.bin", true),
    ] {
        let f = lfs_patch_fixture();
        let raw = git2::Repository::open(&f.repo.root).unwrap();
        if plain_source {
            std::fs::write(
                f.repo.root.join(".gitattributes"),
                "renamed.bin filter=lfs -text\n",
            )
            .unwrap();
            std::fs::write(
                f.repo.root.join("data.bin"),
                standard_lfs_pointer(&f.before),
            )
            .unwrap();
        }
        if destination.ends_with(".txt") {
            std::fs::remove_file(&f.new_object).unwrap();
        }
        let mut index = raw.index().unwrap();
        let mut entry = index.get_path(Path::new("data.bin"), 0).unwrap();
        index.remove_path(Path::new("data.bin")).unwrap();
        entry.path = destination.as_bytes().to_vec();
        index
            .add_frombuffer(&entry, &standard_lfs_pointer(&f.after))
            .unwrap();
        index.write().unwrap();
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(&f.repo.root)
            .args(["diff", "--cached", "--binary", "--find-renames=1%"])
            .output()
            .unwrap();
        assert!(output.status.success());
        assert!(String::from_utf8_lossy(&output.stdout).contains("rename from data.bin"));
        index
            .read_tree(&raw.head().unwrap().peel_to_tree().unwrap())
            .unwrap();
        index.write().unwrap();
        let original_index = std::fs::read(raw.path().join("index")).unwrap();
        let plan = f.repo.prepare_patch_file(output.stdout).unwrap();
        f.repo
            .execute(Operation::ApplyPatch(Arc::new(plan)))
            .unwrap();
        assert!(!f.repo.root.join("data.bin").exists());
        let expected = if destination.ends_with(".bin") {
            f.after.clone()
        } else {
            standard_lfs_pointer(&f.after)
        };
        assert_eq!(
            std::fs::read(f.repo.root.join(destination)).unwrap(),
            expected
        );
        assert_eq!(
            std::fs::read(raw.path().join("index")).unwrap(),
            original_index
        );
    }
}

#[test]
fn lfs_patch_cannot_publish_non_pointer_canonical_text_as_ordinary_content() {
    let f = lfs_patch_fixture();
    let raw = git2::Repository::open(&f.repo.root).unwrap();
    let mut index = raw.index().unwrap();
    let entry = index.get_path(Path::new("data.bin"), 0).unwrap();
    index
        .add_frombuffer(&entry, b"not an LFS pointer\n")
        .unwrap();
    index.write().unwrap();
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(&f.repo.root)
        .args(["diff", "--cached", "--binary", "--", "data.bin"])
        .output()
        .unwrap();
    assert!(output.status.success());
    index
        .read_tree(&raw.head().unwrap().peel_to_tree().unwrap())
        .unwrap();
    index.write().unwrap();
    let error = f
        .repo
        .prepare_patch_file(output.stdout)
        .unwrap_err()
        .to_string();
    assert!(error.contains("LFS"), "{error}");
    assert_eq!(
        std::fs::read(f.repo.root.join("data.bin")).unwrap(),
        f.before
    );
    assert!(!f.repo.has_patch_recovery().unwrap());
}

#[test]
fn lfs_patch_rejects_oversized_declared_objects_before_reading_or_downloading_them() {
    for size in [17 * 1024 * 1024u64, u64::MAX] {
        let f = lfs_patch_fixture();
        let raw = git2::Repository::open(&f.repo.root).unwrap();
        let mut index = raw.index().unwrap();
        let entry = index.get_path(Path::new("data.bin"), 0).unwrap();
        let pointer = format!(
            "version https://git-lfs.github.com/spec/v1\noid sha256:{}\nsize {size}\n",
            "a".repeat(64)
        );
        index.add_frombuffer(&entry, pointer.as_bytes()).unwrap();
        index.write().unwrap();
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(&f.repo.root)
            .args(["diff", "--cached", "--binary", "--", "data.bin"])
            .output()
            .unwrap();
        assert!(output.status.success());
        index
            .read_tree(&raw.head().unwrap().peel_to_tree().unwrap())
            .unwrap();
        index.write().unwrap();
        let error = f
            .repo
            .prepare_patch_file(output.stdout)
            .unwrap_err()
            .to_string();
        assert!(error.contains("limit"), "{error}");
        assert_eq!(
            std::fs::read(f.repo.root.join("data.bin")).unwrap(),
            f.before
        );
        assert!(!f.repo.has_patch_recovery().unwrap());
    }
}

#[cfg(unix)]
#[test]
fn official_lfs_extension_patch_pins_materialized_bytes_and_never_refilters_on_apply() {
    use sha2::{Digest, Sha256};
    let downloaded = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target/tooling-lfs-v3.8.1/git-lfs-3.8.1/git-lfs");
    let tool = std::env::var_os("GITBUDDY_TEST_LFS_PROGRAM")
        .map(std::path::PathBuf::from)
        .or_else(|| downloaded.is_file().then_some(downloaded))
        .or_else(|| {
            std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
                .map(|path| path.join("git-lfs"))
                .find(|path| path.is_file())
        });
    let Some(tool) = tool else {
        eprintln!("Official git-lfs unavailable; extension-specific fixture not run");
        return;
    };
    let mut f = lfs_patch_fixture();
    let processor = f._directory.path().join("extension.py");
    let calls = f._directory.path().join("extension-calls");
    std::fs::write(&processor, "import sys\nopen(sys.argv[2], 'ab').write(b'x')\ndata=sys.stdin.buffer.read()\nif sys.argv[1]=='clean':\n sys.stdout.buffer.write(b'EXT:'+data[::-1])\nelse:\n assert data.startswith(b'EXT:')\n sys.stdout.buffer.write(data[4:][::-1])\n").unwrap();
    let raw = git2::Repository::open(&f.repo.root).unwrap();
    let mut config = raw.config().unwrap();
    config
        .set_str("gitbuddy.lfsProgram", tool.to_str().unwrap())
        .unwrap();
    config.set_i64("lfs.extension.reverse.priority", 0).unwrap();
    for operation in ["clean", "smudge"] {
        config
            .set_str(
                &format!("lfs.extension.reverse.{operation}"),
                &format!("python3 extension.py {operation} extension-calls"),
            )
            .unwrap();
    }
    let extended = |original: &[u8]| {
        let canonical = [
            b"EXT:".as_slice(),
            &original.iter().rev().copied().collect::<Vec<_>>(),
        ]
        .concat();
        let oid = format!("{:x}", Sha256::digest(&canonical));
        let object = raw
            .commondir()
            .join("lfs/objects")
            .join(&oid[..2])
            .join(&oid[2..4])
            .join(&oid);
        std::fs::create_dir_all(object.parent().unwrap()).unwrap();
        std::fs::write(&object, &canonical).unwrap();
        let pointer = format!("version https://git-lfs.github.com/spec/v1\next-0-reverse sha256:{:x}\noid sha256:{oid}\nsize {}\n", Sha256::digest(original), canonical.len()).into_bytes();
        (pointer, object)
    };
    let (before_pointer, _) = extended(&f.before);
    let (after_pointer, after_object) = extended(&f.after);
    let mut index = raw.index().unwrap();
    let entry = index.get_path(Path::new("data.bin"), 0).unwrap();
    index.add_frombuffer(&entry, &before_pointer).unwrap();
    index.write().unwrap();
    f.repo
        .execute(Operation::Commit("extension base".into()))
        .unwrap();
    let mut index = raw.index().unwrap();
    let entry = index.get_path(Path::new("data.bin"), 0).unwrap();
    index.add_frombuffer(&entry, &after_pointer).unwrap();
    index.write().unwrap();
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(&f.repo.root)
        .args(["diff", "--cached", "--binary", "--", "data.bin"])
        .output()
        .unwrap();
    assert!(output.status.success());
    index
        .read_tree(&raw.head().unwrap().peel_to_tree().unwrap())
        .unwrap();
    index.write().unwrap();
    f.patch = output.stdout;
    let original_index = std::fs::read(raw.path().join("index")).unwrap();
    let original_config = std::fs::read(raw.path().join("config")).unwrap();
    let plan = f.repo.prepare_patch_file(f.patch).unwrap();
    assert_eq!(
        std::fs::read(raw.path().join("config")).unwrap(),
        original_config
    );
    let prepared_calls = std::fs::read(&calls).unwrap();
    assert!(!prepared_calls.is_empty());
    std::fs::remove_file(after_object).unwrap();
    f.repo
        .execute(Operation::ApplyPatch(Arc::new(plan)))
        .unwrap();
    assert_eq!(
        std::fs::read(f.repo.root.join("data.bin")).unwrap(),
        f.after
    );
    assert_eq!(std::fs::read(calls).unwrap(), prepared_calls);
    assert_eq!(
        std::fs::read(raw.path().join("index")).unwrap(),
        original_index
    );
    assert_eq!(
        std::fs::read(raw.path().join("config")).unwrap(),
        original_config
    );
    assert!(!f.repo.has_patch_recovery().unwrap());
}

struct EncodingPatchFixture {
    _directory: tempfile::TempDir,
    repo: Repository,
    patch: Vec<u8>,
    before: Vec<u8>,
    expected: Vec<u8>,
}

fn encoding_patch_fixture(label: &str, text: &str) -> EncodingPatchFixture {
    let (directory, repo, _) = fixture();
    let raw = git2::Repository::open(&repo.root).unwrap();
    raw.config()
        .unwrap()
        .set_str("core.attributesfile", "")
        .unwrap();
    raw.config()
        .unwrap()
        .set_bool("core.autocrlf", false)
        .unwrap();
    std::fs::write(repo.root.join(".gitattributes"), format!("* !filter !working-tree-encoding\nencoded.txt text eol=crlf ident working-tree-encoding={label}\n")).unwrap();
    let mut index = raw.index().unwrap();
    index.add_path(Path::new(".gitattributes")).unwrap();
    let mut entry = index.get_path(Path::new("file.txt"), 0).unwrap();
    entry.path = b"encoded.txt".to_vec();
    entry.flags = 0;
    index.add_frombuffer(&entry, text.as_bytes()).unwrap();
    index.write().unwrap();
    repo.execute(Operation::Commit("encoded base".into()))
        .unwrap();
    let git = |args: &[&str]| {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(&repo.root)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            output.stderr.is_empty(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    };
    let id = raw
        .head()
        .unwrap()
        .peel_to_tree()
        .unwrap()
        .get_path(Path::new("encoded.txt"))
        .unwrap()
        .id();
    let before = git(&[
        "cat-file",
        "--filters",
        "--path=encoded.txt",
        &id.to_string(),
    ]);
    std::fs::write(repo.root.join("encoded.txt"), &before).unwrap();
    let after = text.replace("before", "after");
    let mut index = raw.index().unwrap();
    let entry = index.get_path(Path::new("encoded.txt"), 0).unwrap();
    index.add_frombuffer(&entry, after.as_bytes()).unwrap();
    index.write().unwrap();
    let patch = git(&["diff", "--cached", "--binary", "--", "encoded.txt"]);
    index
        .read_tree(&raw.head().unwrap().peel_to_tree().unwrap())
        .unwrap();
    index.write().unwrap();
    let input = directory.path().join("reference-encoding.patch");
    std::fs::write(&input, &patch).unwrap();
    git(&["apply", input.to_str().unwrap()]);
    let expected = std::fs::read(repo.root.join("encoded.txt")).unwrap();
    std::fs::write(repo.root.join("encoded.txt"), &before).unwrap();
    std::fs::remove_file(input).unwrap();
    EncodingPatchFixture {
        _directory: directory,
        repo,
        patch,
        before,
        expected,
    }
}

#[test]
fn working_tree_unicode_encodings_match_git_bom_endianness_eol_and_ident() {
    for label in [
        "UTF-8",
        "UTF-16",
        "UTF-16LE",
        "UTF-16BE",
        "UTF-16LE-BOM",
        "UTF-32",
        "UTF-32LE",
        "UTF-32BE",
    ] {
        let f = encoding_patch_fixture(label, "before 日本 😀\n$Id$\n$Id$\n");
        let raw = git2::Repository::open(&f.repo.root).unwrap();
        let index = std::fs::read(raw.path().join("index")).unwrap();
        let plan = f.repo.prepare_patch_file(f.patch).unwrap();
        f.repo
            .execute(Operation::ApplyPatch(Arc::new(plan)))
            .unwrap();
        assert_eq!(
            std::fs::read(f.repo.root.join("encoded.txt")).unwrap(),
            f.expected,
            "{label}"
        );
        assert_eq!(std::fs::read(raw.path().join("index")).unwrap(), index);
        assert!(!f.repo.has_patch_recovery().unwrap());
    }
}

#[test]
fn supported_legacy_working_encodings_match_native_git_without_whatwg_substitution() {
    for (label, text) in [
        ("SHIFT-JIS", "before 日本語\n$Id$\n"),
        ("WINDOWS-1252", "before café €\n$Id$\n"),
        ("ISO-8859-1", "before café \u{0080}\n$Id$\n"),
        ("ASCII", "before plain\n$Id$\n"),
    ] {
        let f = encoding_patch_fixture(label, text);
        let plan = f.repo.prepare_patch_file(f.patch).unwrap();
        f.repo
            .execute(Operation::ApplyPatch(Arc::new(plan)))
            .unwrap();
        assert_eq!(
            std::fs::read(f.repo.root.join("encoded.txt")).unwrap(),
            f.expected,
            "{label}"
        );
    }
}

#[test]
fn working_encoding_rejects_stale_roundtrip_configuration_before_mutation() {
    let f = encoding_patch_fixture("UTF-16LE", "before text\n");
    let plan = f.repo.prepare_patch_file(f.patch).unwrap();
    git2::Repository::open(&f.repo.root)
        .unwrap()
        .config()
        .unwrap()
        .set_str("core.checkRoundtripEncoding", "UTF-16LE, SHIFT-JIS")
        .unwrap();
    assert!(
        f.repo
            .execute(Operation::ApplyPatch(Arc::new(plan)))
            .is_err()
    );
    assert_eq!(
        std::fs::read(f.repo.root.join("encoded.txt")).unwrap(),
        f.before
    );
    assert!(!f.repo.has_patch_recovery().unwrap());
}

#[test]
fn working_encoding_matches_git_apply_roundtrip_exception_and_bom_normalization() {
    let f = encoding_patch_fixture("UTF-16", "before text\n$Id$\n");
    let alternate: Vec<u8> = f
        .before
        .as_chunks::<2>()
        .0
        .iter()
        .flat_map(|unit| [unit[1], unit[0]])
        .collect();
    let raw = git2::Repository::open(&f.repo.root).unwrap();
    raw.config()
        .unwrap()
        .set_str("core.checkRoundtripEncoding", "UTF-16")
        .unwrap();
    std::fs::write(f.repo.root.join("encoded.txt"), &alternate).unwrap();
    let patch = f._directory.path().join("roundtrip-reference.patch");
    std::fs::write(&patch, &f.patch).unwrap();
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(&f.repo.root)
        .args(["apply", patch.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let expected = std::fs::read(f.repo.root.join("encoded.txt")).unwrap();
    std::fs::write(f.repo.root.join("encoded.txt"), &alternate).unwrap();
    std::fs::remove_file(patch).unwrap();
    let index = std::fs::read(raw.path().join("index")).unwrap();
    let plan = f.repo.prepare_patch_file(f.patch).unwrap();
    f.repo
        .execute(Operation::ApplyPatch(Arc::new(plan)))
        .unwrap();
    assert_eq!(
        std::fs::read(f.repo.root.join("encoded.txt")).unwrap(),
        expected
    );
    assert_eq!(std::fs::read(raw.path().join("index")).unwrap(), index);
}

#[test]
fn implicit_autocrlf_boolean_matches_git_apply_without_panicking_or_rewriting_config() {
    use std::io::Write;
    let (directory, repo, patch, before, _) = conversion_fixture("", "false");
    let raw = git2::Repository::open(&repo.root).unwrap();
    let path = raw.path().join("config");
    std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(b"\n[core]\n\tautocrlf\n")
        .unwrap();
    assert!(
        git2::Repository::open(&repo.root)
            .unwrap()
            .config()
            .unwrap()
            .get_bool("core.autocrlf")
            .unwrap()
    );
    let input = directory.path().join("implicit-reference.patch");
    std::fs::write(&input, &patch).unwrap();
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(&repo.root)
        .args(["apply", input.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let expected = std::fs::read(repo.root.join("file.txt")).unwrap();
    assert!(expected.windows(2).any(|window| window == b"\r\n"));
    std::fs::write(repo.root.join("file.txt"), &before).unwrap();
    std::fs::remove_file(input).unwrap();
    let original_config = std::fs::read(&path).unwrap();
    let index = std::fs::read(raw.path().join("index")).unwrap();
    let plan = repo.prepare_patch_file(patch).unwrap();
    repo.execute(Operation::ApplyPatch(Arc::new(plan))).unwrap();
    assert_eq!(std::fs::read(repo.root.join("file.txt")).unwrap(), expected);
    assert_eq!(std::fs::read(path).unwrap(), original_config);
    assert_eq!(std::fs::read(raw.path().join("index")).unwrap(), index);
}

#[test]
fn implicit_string_conversion_settings_return_errors_instead_of_panicking() {
    use std::io::Write;
    for setting in ["eol", "checkRoundtripEncoding"] {
        let (_dir, repo, patch) = fixture();
        let raw = git2::Repository::open(&repo.root).unwrap();
        std::fs::OpenOptions::new()
            .append(true)
            .open(raw.path().join("config"))
            .unwrap()
            .write_all(format!("\n[core]\n\t{setting}\n").as_bytes())
            .unwrap();
        let error = repo.prepare_patch_file(patch).unwrap_err().to_string();
        assert!(
            error
                .to_ascii_lowercase()
                .contains(&setting.to_ascii_lowercase()),
            "{error}"
        );
        assert_eq!(
            std::fs::read(repo.root.join("file.txt")).unwrap(),
            b"before\n"
        );
        assert!(!repo.has_patch_recovery().unwrap());
    }
}

#[test]
fn imported_encoded_worktree_stages_utf8_canonical_bytes_without_rewriting_the_file() {
    let f = encoding_patch_fixture("UTF-16LE", "before 日本 😀\n$Id$\n$Id$\n");
    let plan = f.repo.prepare_patch_file(f.patch).unwrap();
    f.repo
        .execute(Operation::ApplyPatch(Arc::new(plan)))
        .unwrap();
    let raw_before = std::fs::read(f.repo.root.join("encoded.txt")).unwrap();
    f.repo
        .execute(Operation::Stage(vec!["encoded.txt".into()]))
        .unwrap();
    let raw = git2::Repository::open(&f.repo.root).unwrap();
    let entry = raw
        .index()
        .unwrap()
        .get_path(Path::new("encoded.txt"), 0)
        .unwrap();
    assert_eq!(
        raw.find_blob(entry.id).unwrap().content(),
        "after 日本 😀\n$Id$\n$Id$\n".as_bytes()
    );
    assert_eq!(
        std::fs::read(f.repo.root.join("encoded.txt")).unwrap(),
        raw_before
    );
}

#[test]
fn encoded_status_and_working_diff_use_canonical_git_content() {
    let f = encoding_patch_fixture("UTF-16LE", "before 日本 😀\n$Id$\n");
    assert!(
        !f.repo
            .snapshot(10)
            .unwrap()
            .files
            .iter()
            .any(|file| file.path == Path::new("encoded.txt")),
        "clean encoded bytes must not look modified"
    );
    let plan = f.repo.prepare_patch_file(f.patch).unwrap();
    f.repo
        .execute(Operation::ApplyPatch(Arc::new(plan)))
        .unwrap();
    let file = f
        .repo
        .snapshot(10)
        .unwrap()
        .files
        .into_iter()
        .find(|file| file.path == Path::new("encoded.txt"))
        .unwrap();
    assert_eq!(file.worktree, 'M');
    let actual = f.repo.diff(&file, false).unwrap();
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(&f.repo.root)
        .args([
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--unified=3",
            "--",
            "encoded.txt",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    let hunks = |text: &str| {
        text.lines()
            .skip_while(|line| !line.starts_with("@@"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    assert!(!hunks(&actual).is_empty());
    assert_eq!(
        hunks(&actual),
        hunks(&String::from_utf8(output.stdout).unwrap())
    );
    let exported = f.repo.export_patch(false).unwrap();
    let diff = git2::Diff::from_buffer(&exported).unwrap();
    let raw = git2::Repository::open(&f.repo.root).unwrap();
    let result = raw
        .apply_to_tree(&raw.head().unwrap().peel_to_tree().unwrap(), &diff, None)
        .unwrap();
    let entry = result.get_path(Path::new("encoded.txt"), 0).unwrap();
    assert_eq!(
        raw.find_blob(entry.id).unwrap().content(),
        "after 日本 😀\n$Id$\n".as_bytes()
    );
}

#[test]
fn invalid_encoded_staging_preserves_index_worktree_and_modified_status() {
    let f = encoding_patch_fixture("UTF-16LE", "before text\n");
    let invalid = &f.before[..f.before.len() - 1];
    std::fs::write(f.repo.root.join("encoded.txt"), invalid).unwrap();
    let raw = git2::Repository::open(&f.repo.root).unwrap();
    let index = std::fs::read(raw.path().join("index")).unwrap();
    assert!(
        f.repo
            .execute(Operation::Stage(vec!["encoded.txt".into()]))
            .is_err()
    );
    assert_eq!(std::fs::read(raw.path().join("index")).unwrap(), index);
    assert_eq!(
        std::fs::read(f.repo.root.join("encoded.txt")).unwrap(),
        invalid
    );
    let file = f
        .repo
        .snapshot(10)
        .unwrap()
        .files
        .into_iter()
        .find(|file| file.path == Path::new("encoded.txt"))
        .unwrap();
    assert_eq!(file.worktree, 'M');
    assert!(f.repo.diff(&file, false).is_err());
}

#[test]
fn configured_custom_filter_cannot_stage_raw_or_appear_verified_clean() {
    let (_dir, repo, _) = fixture();
    std::fs::write(
        repo.root.join(".gitattributes"),
        "file.txt filter=unknown-driver\n",
    )
    .unwrap();
    let raw = git2::Repository::open(&repo.root).unwrap();
    raw.config()
        .unwrap()
        .set_str("filter.unknown-driver.clean", "touch filter-ran && cat")
        .unwrap();
    let index = std::fs::read(raw.path().join("index")).unwrap();
    assert!(
        repo.execute(Operation::Stage(vec!["file.txt".into()]))
            .is_err()
    );
    assert_eq!(std::fs::read(raw.path().join("index")).unwrap(), index);
    assert_eq!(
        std::fs::read(repo.root.join("file.txt")).unwrap(),
        b"before\n"
    );
    let file = repo
        .snapshot(10)
        .unwrap()
        .files
        .into_iter()
        .find(|file| file.path == Path::new("file.txt"))
        .expect("unsupported conversion must not be reported verified clean");
    assert_eq!(file.worktree, 'M');
    assert!(repo.diff(&file, false).is_err());
    assert!(!repo.root.join("filter-ran").exists());
}

#[test]
fn ordinary_unencoded_large_file_keeps_native_staging_without_the_patch_limit() {
    let (_dir, repo, _) = fixture();
    std::fs::write(
        repo.root.join(".gitattributes"),
        "* !filter !working-tree-encoding\n",
    )
    .unwrap();
    let bytes = vec![b'x'; 17 * 1024 * 1024];
    std::fs::write(repo.root.join("large.dat"), &bytes).unwrap();
    repo.execute(Operation::Stage(vec!["large.dat".into()]))
        .unwrap();
    let raw = git2::Repository::open(&repo.root).unwrap();
    let entry = raw
        .index()
        .unwrap()
        .get_path(Path::new("large.dat"), 0)
        .unwrap();
    assert_eq!(raw.find_blob(entry.id).unwrap().content(), bytes);
}

#[test]
fn encoded_staging_uses_add_safety_instead_of_patch_apply_policy() {
    let f = encoding_patch_fixture("UTF-16", "before text\n");
    let alternate: Vec<u8> = f
        .before
        .as_chunks::<2>()
        .0
        .iter()
        .flat_map(|unit| [unit[1], unit[0]])
        .collect();
    std::fs::write(f.repo.root.join("encoded.txt"), &alternate).unwrap();
    let raw = git2::Repository::open(&f.repo.root).unwrap();
    raw.config()
        .unwrap()
        .set_str("core.checkRoundtripEncoding", "UTF-16")
        .unwrap();
    let index = std::fs::read(raw.path().join("index")).unwrap();
    assert!(
        f.repo
            .execute(Operation::Stage(vec!["encoded.txt".into()]))
            .is_err()
    );
    assert_eq!(std::fs::read(raw.path().join("index")).unwrap(), index);
    assert_eq!(
        std::fs::read(f.repo.root.join("encoded.txt")).unwrap(),
        alternate
    );
    let f = encoding_patch_fixture("UTF-16LE", "before text\n");
    let mut lf = Vec::new();
    let units = f.before.as_chunks::<2>().0;
    let mut at = 0;
    while at < units.len() {
        if units[at] == [13, 0] && units.get(at + 1) == Some(&[10, 0]) {
            at += 1;
        }
        lf.extend_from_slice(&units[at]);
        at += 1;
    }
    std::fs::write(f.repo.root.join("encoded.txt"), &lf).unwrap();
    let raw = git2::Repository::open(&f.repo.root).unwrap();
    raw.config()
        .unwrap()
        .set_bool("core.safecrlf", true)
        .unwrap();
    let index = std::fs::read(raw.path().join("index")).unwrap();
    assert!(
        f.repo
            .execute(Operation::Stage(vec!["encoded.txt".into()]))
            .is_err()
    );
    assert_eq!(std::fs::read(raw.path().join("index")).unwrap(), index);
    assert_eq!(std::fs::read(f.repo.root.join("encoded.txt")).unwrap(), lf);
}

#[test]
fn mixed_encoded_and_plain_export_contains_only_canonical_git_blobs() {
    let f = encoding_patch_fixture("UTF-16LE", "before 日本\n$Id$\n");
    let plan = f.repo.prepare_patch_file(f.patch).unwrap();
    f.repo
        .execute(Operation::ApplyPatch(Arc::new(plan)))
        .unwrap();
    std::fs::write(f.repo.root.join("file.txt"), b"plain after\n").unwrap();
    let exported = f.repo.export_patch(false).unwrap();
    let diff = git2::Diff::from_buffer(&exported).unwrap();
    let raw = git2::Repository::open(&f.repo.root).unwrap();
    let result = raw
        .apply_to_tree(&raw.head().unwrap().peel_to_tree().unwrap(), &diff, None)
        .unwrap();
    assert_eq!(
        raw.find_blob(result.get_path(Path::new("encoded.txt"), 0).unwrap().id)
            .unwrap()
            .content(),
        "after 日本\n$Id$\n".as_bytes()
    );
    assert_eq!(
        raw.find_blob(result.get_path(Path::new("file.txt"), 0).unwrap().id)
            .unwrap()
            .content(),
        b"plain after\n"
    );
}

#[test]
fn lfs_unstaged_diff_and_export_use_pointer_canonical_content() {
    let f = lfs_patch_fixture();
    std::fs::write(f.repo.root.join("data.bin"), &f.after).unwrap();
    let file = f
        .repo
        .snapshot(10)
        .unwrap()
        .files
        .into_iter()
        .find(|file| file.path == Path::new("data.bin"))
        .unwrap();
    let diff = f.repo.diff(&file, false).unwrap();
    assert!(diff.contains("oid sha256:"));
    assert!(!diff.contains("Binary files"));
    let exported = f.repo.export_patch(false).unwrap();
    let diff = git2::Diff::from_buffer(&exported).unwrap();
    let raw = git2::Repository::open(&f.repo.root).unwrap();
    let result = raw
        .apply_to_tree(&raw.head().unwrap().peel_to_tree().unwrap(), &diff, None)
        .unwrap();
    assert_eq!(
        raw.find_blob(result.get_path(Path::new("data.bin"), 0).unwrap().id)
            .unwrap()
            .content(),
        standard_lfs_pointer(&f.after)
    );
    assert_eq!(
        std::fs::read(f.repo.root.join("data.bin")).unwrap(),
        f.after
    );
}

#[test]
fn encoded_partial_discard_keeps_the_unselected_raw_ident_expansion() {
    let f = encoding_patch_fixture("UTF-16LE", "before 日本\n$Id$\n");
    let plan = f.repo.prepare_patch_file(f.patch).unwrap();
    f.repo
        .execute(Operation::ApplyPatch(Arc::new(plan)))
        .unwrap();
    let file = f
        .repo
        .snapshot(10)
        .unwrap()
        .files
        .into_iter()
        .find(|file| file.path == Path::new("encoded.txt"))
        .unwrap();
    let preview = f.repo.worktree_patch(&file, false).unwrap();
    assert!(preview.partial.is_some());
    assert!(preview.unavailable.is_none());
    assert!(
        preview
            .lines
            .iter()
            .any(|line| line.text.contains("+after 日本"))
    );
    assert_eq!(
        std::fs::read(f.repo.root.join("encoded.txt")).unwrap(),
        f.expected
    );
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        let row = preview
            .lines
            .iter()
            .position(|line| line.kind == '@')
            .unwrap();
        f.repo
            .execute(Operation::DiscardPartial {
                patch: preview.partial.unwrap(),
                selection: gitbuddy::git::PatchSelection::Hunk(row),
            })
            .unwrap();
        let after_first_line = |bytes: &[u8]| {
            2 * (1 + bytes
                .as_chunks::<2>()
                .0
                .iter()
                .position(|unit| unit == b"\n\0")
                .unwrap())
        };
        let expected = [
            &f.before[..after_first_line(&f.before)],
            &f.expected[after_first_line(&f.expected)..],
        ]
        .concat();
        assert_eq!(
            std::fs::read(f.repo.root.join("encoded.txt")).unwrap(),
            expected
        );
    }
}

#[test]
fn working_unicode_bom_violations_preserve_the_exact_rejected_working_file() {
    for (label, bom, remove) in [
        ("UTF-16", vec![0xfe, 0xff], true),
        ("UTF-32", vec![0, 0, 0xfe, 0xff], true),
        ("UTF-16LE", vec![0xff, 0xfe], false),
        ("UTF-32LE", vec![0xff, 0xfe, 0, 0], false),
    ] {
        let f = encoding_patch_fixture(label, "before text\n");
        let invalid = if remove {
            f.before[bom.len()..].to_vec()
        } else {
            [bom, f.before.clone()].concat()
        };
        std::fs::write(f.repo.root.join("encoded.txt"), &invalid).unwrap();
        let raw = git2::Repository::open(&f.repo.root).unwrap();
        let index = std::fs::read(raw.path().join("index")).unwrap();
        assert!(f.repo.prepare_patch_file(f.patch).is_err(), "{label}");
        assert_eq!(
            std::fs::read(f.repo.root.join("encoded.txt")).unwrap(),
            invalid
        );
        assert_eq!(std::fs::read(raw.path().join("index")).unwrap(), index);
        assert!(!f.repo.has_patch_recovery().unwrap());
    }
}

#[test]
fn malformed_unicode_is_not_lossily_replaced_to_match_a_canonical_replacement_character() {
    for (label, replacement, invalid) in [
        ("UTF-16LE", vec![0xfd, 0xff], vec![0, 0xd8]),
        ("UTF-32LE", vec![0xfd, 0xff, 0, 0], vec![0, 0, 0x11, 0]),
    ] {
        let f = encoding_patch_fixture(label, "before \u{fffd}\n");
        let mut bytes = f.before;
        let at = bytes
            .windows(replacement.len())
            .position(|window| window == replacement)
            .unwrap();
        bytes[at..at + invalid.len()].copy_from_slice(&invalid);
        std::fs::write(f.repo.root.join("encoded.txt"), &bytes).unwrap();
        assert!(f.repo.prepare_patch_file(f.patch).is_err(), "{label}");
        assert_eq!(
            std::fs::read(f.repo.root.join("encoded.txt")).unwrap(),
            bytes
        );
        assert!(!f.repo.has_patch_recovery().unwrap());
    }
}

#[test]
fn encoded_patch_bounds_expansion_before_allocating_a_utf32_working_payload() {
    let (_dir, repo, _) = fixture();
    std::fs::write(
        repo.root.join(".gitattributes"),
        "* !filter !working-tree-encoding\nlarge.txt -diff -text working-tree-encoding=UTF-32LE\n",
    )
    .unwrap();
    let raw = git2::Repository::open(&repo.root).unwrap();
    let mut index = raw.index().unwrap();
    let mut entry = index.get_path(Path::new("file.txt"), 0).unwrap();
    entry.path = b"large.txt".to_vec();
    entry.flags = 0;
    index
        .add_frombuffer(&entry, &vec![b'A'; 5 * 1024 * 1024])
        .unwrap();
    index.write().unwrap();
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(&repo.root)
        .args(["diff", "--cached", "--binary", "--", "large.txt"])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(output.stdout.len() < 2 * 1024 * 1024);
    assert!(String::from_utf8_lossy(&output.stdout).contains("GIT binary patch"));
    index
        .read_tree(&raw.head().unwrap().peel_to_tree().unwrap())
        .unwrap();
    index.write().unwrap();
    let original_index = std::fs::read(raw.path().join("index")).unwrap();
    let error = repo
        .prepare_patch_file(output.stdout)
        .unwrap_err()
        .to_string();
    assert!(error.contains("byte limit"), "{error}");
    assert!(!repo.root.join("large.txt").exists());
    assert!(!repo.has_patch_recovery().unwrap());
    assert_eq!(
        std::fs::read(raw.path().join("index")).unwrap(),
        original_index
    );
}

#[cfg(unix)]
#[test]
fn import_rejects_symlink_substitution() {
    use std::os::unix::fs::symlink;
    let (dir, repo, patch) = fixture();
    let target = repo.root.join("file.txt");
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

#[cfg(unix)]
#[test]
fn patch_import_applies_mode_only_changes_and_rejects_stale_permissions() {
    use std::os::unix::fs::PermissionsExt;
    let (_dir, repo, _) = fixture();
    let patch = b"diff --git a/file.txt b/file.txt\nold mode 100644\nnew mode 100755\n".to_vec();
    let plan = repo.prepare_patch_file(patch.clone()).unwrap();
    repo.execute(Operation::ApplyPatch(Arc::new(plan))).unwrap();
    assert_ne!(
        std::fs::metadata(repo.root.join("file.txt"))
            .unwrap()
            .permissions()
            .mode()
            & 0o111,
        0
    );
    assert_eq!(
        std::fs::read_to_string(repo.root.join("file.txt")).unwrap(),
        "before\n"
    );
    std::fs::set_permissions(
        repo.root.join("file.txt"),
        std::fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    let plan = repo.prepare_patch_file(patch).unwrap();
    std::fs::set_permissions(
        repo.root.join("file.txt"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    assert!(repo.execute(Operation::ApplyPatch(Arc::new(plan))).is_err());
}

#[cfg(unix)]
#[test]
fn hardlinked_target_is_replaced_without_modifying_other_links() {
    let (dir, repo, patch) = fixture();
    let other = dir.path().join("other-link");
    std::fs::hard_link(repo.root.join("file.txt"), &other).unwrap();
    let plan = repo.prepare_patch_file(patch).unwrap();
    repo.execute(Operation::ApplyPatch(Arc::new(plan))).unwrap();
    assert_eq!(
        std::fs::read_to_string(repo.root.join("file.txt")).unwrap(),
        "after\n"
    );
    assert_eq!(std::fs::read_to_string(other).unwrap(), "before\n");
}

#[cfg(unix)]
#[test]
fn symlink_patch_changes_link_text_without_touching_target() {
    use std::os::unix::fs::symlink;
    let (_dir, repo, _) = fixture();
    symlink("outside-old", repo.root.join("link")).unwrap();
    repo.execute(Operation::Stage(vec!["link".into()])).unwrap();
    repo.execute(Operation::Commit("add link".into())).unwrap();
    std::fs::write(repo.root.join("outside-old"), "preserve old target").unwrap();
    std::fs::write(repo.root.join("outside-new"), "preserve new target").unwrap();
    std::fs::remove_file(repo.root.join("link")).unwrap();
    symlink("outside-new", repo.root.join("link")).unwrap();
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(&repo.root)
        .args(["diff", "--", "link"])
        .output()
        .unwrap();
    assert!(output.status.success());
    std::fs::remove_file(repo.root.join("link")).unwrap();
    symlink("outside-old", repo.root.join("link")).unwrap();
    let plan = repo.prepare_patch_file(output.stdout).unwrap();
    repo.execute(Operation::ApplyPatch(Arc::new(plan))).unwrap();
    assert_eq!(
        std::fs::read_link(repo.root.join("link")).unwrap(),
        Path::new("outside-new")
    );
    assert_eq!(
        std::fs::read_to_string(repo.root.join("outside-old")).unwrap(),
        "preserve old target"
    );
    assert_eq!(
        std::fs::read_to_string(repo.root.join("outside-new")).unwrap(),
        "preserve new target"
    );
}

#[test]
fn addition_patch_can_be_applied_to_an_unborn_repository_without_touching_index() {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();
    let raw = git2::Repository::open(&repo.root).unwrap();
    assert!(raw.head().is_err());
    let bytes=b"diff --git a/new.txt b/new.txt\nnew file mode 100644\n--- /dev/null\n+++ b/new.txt\n@@ -0,0 +1 @@\n+new repository content\n".to_vec();
    let plan = repo.prepare_patch_file(bytes).unwrap();
    repo.execute(Operation::ApplyPatch(Arc::new(plan))).unwrap();
    assert_eq!(
        std::fs::read_to_string(repo.root.join("new.txt")).unwrap(),
        "new repository content\n"
    );
    assert!(raw.head().is_err());
    assert!(raw.index().unwrap().is_empty());
    assert!(!repo.has_patch_recovery().unwrap());
}
