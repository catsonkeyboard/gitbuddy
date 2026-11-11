use gitbuddy::git::{Operation, Repository, ResetMode};
use std::{fs, path::Path, process::Command, sync::Arc};

const BASE: &str = "base 中文 😀\n";
const TARGET: &str = "target 日本語 😀\n";

fn encoded(text: &str, little: bool) -> Vec<u8> {
    text.encode_utf16()
        .flat_map(|unit| {
            if little {
                unit.to_le_bytes()
            } else {
                unit.to_be_bytes()
            }
        })
        .collect()
}
fn git(root: &Path, args: &[&str]) -> String {
    let output = git_output(root, args);
    assert!(
        output.status.success(),
        "Git fixture {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}
fn git_output(root: &Path, args: &[&str]) -> std::process::Output {
    Command::new("git")
        .args(args)
        .current_dir(root)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env(
            "GIT_CONFIG_GLOBAL",
            root.parent().unwrap().join("empty-config"),
        )
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .unwrap()
}
fn setup() -> (tempfile::TempDir, Repository, String) {
    let directory = tempfile::tempdir().unwrap();
    fs::write(directory.path().join("empty-config"), "").unwrap();
    let root = directory.path().join("repo");
    fs::create_dir(&root).unwrap();
    git(&root, &["init", "-q", "-b", "main"]);
    for (key, value) in [
        ("user.name", "Encoding fixture"),
        ("user.email", "encoding@example.invalid"),
        ("commit.gpgsign", "false"),
        ("core.hooksPath", "/dev/null"),
        ("core.attributesfile", ""),
        ("core.autocrlf", "false"),
        ("core.safecrlf", "false"),
        ("core.ignorecase", "false"),
    ] {
        git(&root, &["config", key, value]);
    }
    fs::write(
        root.join(".gitattributes"),
        "file.txt text working-tree-encoding=UTF-16LE eol=lf\n",
    )
    .unwrap();
    fs::write(root.join("file.txt"), encoded(BASE, true)).unwrap();
    git(&root, &["add", "."]);
    git(&root, &["commit", "-qm", "base encoded"]);
    git(&root, &["checkout", "-qb", "target"]);
    fs::write(
        root.join(".gitattributes"),
        "file.txt text working-tree-encoding=UTF-16BE eol=lf\n",
    )
    .unwrap();
    fs::write(root.join("file.txt"), encoded(TARGET, false)).unwrap();
    git(&root, &["add", "."]);
    git(&root, &["commit", "-qm", "target encoded"]);
    let target = git(&root, &["rev-parse", "HEAD"]).trim().to_owned();
    git(&root, &["checkout", "-q", "main"]);
    (directory, Repository::open(&root).unwrap(), target)
}

#[test]
fn branch_checkout_changes_worktree_encoding_without_misclassifying_clean_source() {
    let (_directory, repo, target) = setup();
    assert_eq!(
        fs::read(repo.root.join("file.txt")).unwrap(),
        encoded(BASE, true)
    );
    repo.execute(Operation::Checkout("target".into())).unwrap();
    assert_eq!(repo.resolve_revision("HEAD").unwrap().id, target);
    assert_eq!(
        fs::read(repo.root.join("file.txt")).unwrap(),
        encoded(TARGET, false)
    );
    assert!(repo.snapshot(10).unwrap().files.is_empty());
    let status = git(&repo.root, &["status", "--porcelain"]);
    assert!(
        status.is_empty(),
        "Unexpected native Git status: {status}; staged diff: {}; working diff: {}",
        git(&repo.root, &["diff", "--cached", "--stat"]),
        git(&repo.root, &["diff", "--stat"])
    );
}

#[test]
fn discard_encoded_file_restores_worktree_bytes_and_preserves_other_edits() {
    let (_directory, repo, _) = setup();
    fs::write(repo.root.join("file.txt"), encoded("user edit\n", true)).unwrap();
    fs::write(repo.root.join("untracked"), b"keep").unwrap();
    repo.execute(Operation::Discard("file.txt".into())).unwrap();
    assert_eq!(
        fs::read(repo.root.join("file.txt")).unwrap(),
        encoded(BASE, true)
    );
    assert_eq!(fs::read(repo.root.join("untracked")).unwrap(), b"keep");
    assert_eq!(git(&repo.root, &["diff", "--", "file.txt"]), "");
}

#[test]
fn hard_reset_uses_the_reviewed_target_encoding_and_keeps_untracked_files() {
    let (_directory, repo, target) = setup();
    fs::write(repo.root.join("file.txt"), encoded("user edit\n", true)).unwrap();
    fs::write(repo.root.join("untracked"), b"keep").unwrap();
    let context = repo.prepare_reset(&target).unwrap();
    repo.execute(Operation::Reset {
        context: Arc::new(context),
        mode: ResetMode::Hard,
    })
    .unwrap();
    assert_eq!(repo.resolve_revision("HEAD").unwrap().id, target);
    assert_eq!(
        fs::read(repo.root.join("file.txt")).unwrap(),
        encoded(TARGET, false)
    );
    assert_eq!(fs::read(repo.root.join("untracked")).unwrap(), b"keep");
    assert_eq!(git(&repo.root, &["diff", "--", "file.txt"]), "");
}

#[test]
fn interrupted_checkout_blocks_writes_and_recovers_through_normal_operation_dispatch() {
    for abort in [false, true] {
        let (_directory, repo, target) = setup();
        let original = repo.resolve_revision("HEAD").unwrap().id;
        let _receipt = repo
            .checkout_worktree(
                "target",
                gitbuddy::git::CheckoutOptions {
                    publication: Some(gitbuddy::git::CheckoutHead {
                        reference: Some("refs/heads/target".into()),
                        id: target.parse().unwrap(),
                    }),
                    ..Default::default()
                },
                Default::default(),
            )
            .unwrap();
        let snapshot = repo.snapshot(10).unwrap();
        assert!(snapshot.worktree_recovery);
        assert!(snapshot.files.iter().any(|file| file.staged()));
        assert!(!snapshot.can_commit());
        assert!(repo.execute(Operation::StageAll).is_err());
        let status = repo.worktree_recovery_status().unwrap().unwrap();
        assert!(status.original_head.contains(&original));
        assert!(status.target_head.contains(&target));
        let operation = if abort {
            Operation::AbortWorktreeCheckout {
                expected: status.expected,
            }
        } else {
            Operation::ContinueWorktreeCheckout {
                expected: status.expected,
            }
        };
        repo.execute(operation).unwrap();
        assert!(repo.worktree_recovery_status().unwrap().is_none());
        assert_eq!(
            repo.resolve_revision("HEAD").unwrap().id,
            if abort { original } else { target }
        );
        assert_eq!(
            fs::read(repo.root.join("file.txt")).unwrap(),
            if abort {
                encoded(BASE, true)
            } else {
                encoded(TARGET, false)
            }
        );
    }
}

#[test]
fn discard_can_replace_invalid_source_encoding_without_staging_it() {
    let (_directory, repo, _) = setup();
    fs::write(repo.root.join("file.txt"), [0x00]).unwrap();
    repo.execute(Operation::Discard("file.txt".into())).unwrap();
    assert_eq!(
        fs::read(repo.root.join("file.txt")).unwrap(),
        encoded(BASE, true)
    );
    assert_eq!(git(&repo.root, &["diff", "--cached"]), "");
}

fn encoded_conflict(repo: &Repository) {
    git(&repo.root, &["checkout", "-qb", "incoming"]);
    fs::write(repo.root.join("file.txt"), encoded("incoming 中文\n", true)).unwrap();
    git(&repo.root, &["commit", "-qam", "incoming"]);
    git(&repo.root, &["checkout", "-q", "main"]);
    fs::write(repo.root.join("file.txt"), encoded("ours 日本語\n", true)).unwrap();
    git(&repo.root, &["commit", "-qam", "ours"]);
    let merge = git_output(&repo.root, &["merge", "incoming"]);
    assert_eq!(
        merge.status.code(),
        Some(1),
        "{}",
        String::from_utf8_lossy(&merge.stderr)
    );
    assert!(!git(&repo.root, &["ls-files", "-u"]).is_empty());
}

#[test]
fn encoded_conflict_resolution_preserves_working_encoding_and_canonical_index() {
    use gitbuddy::git::ConflictResolution;
    for (choice, text) in [
        (ConflictResolution::Ours, "ours 日本語\n"),
        (
            ConflictResolution::Edited("resolved 😀\n".into()),
            "resolved 😀\n",
        ),
        (ConflictResolution::Worktree, "external 中文\n"),
    ] {
        let (_directory, repo, _) = setup();
        encoded_conflict(&repo);
        if matches!(choice, ConflictResolution::Worktree) {
            fs::write(repo.root.join("file.txt"), encoded(text, true)).unwrap();
        }
        let context = repo.conflict_context(Path::new("file.txt")).unwrap();
        if matches!(choice, ConflictResolution::Edited(_)) {
            assert!(context.editable);
            assert!(context.result.as_deref().unwrap().contains("<<<<<<<"));
        }
        repo.execute(Operation::ResolveConflict {
            context: Arc::new(context),
            resolution: choice,
        })
        .unwrap();
        assert_eq!(
            fs::read(repo.root.join("file.txt")).unwrap(),
            encoded(text, true)
        );
        assert_eq!(git(&repo.root, &["show", ":file.txt"]), text);
        assert!(git(&repo.root, &["diff", "--", "file.txt"]).is_empty());
    }
}

#[test]
fn clone_installs_encoded_working_bytes_with_a_clean_canonical_index() {
    let (directory, source, _) = setup();
    let destination = directory.path().join("clone");
    let cloned = Repository::clone_repo(source.root.to_str().unwrap(), &destination).unwrap();
    assert_eq!(
        fs::read(cloned.root.join("file.txt")).unwrap(),
        encoded(BASE, true)
    );
    assert_eq!(git(&cloned.root, &["show", ":file.txt"]), BASE);
    assert!(git(&cloned.root, &["status", "--porcelain"]).is_empty());
}

#[test]
fn clone_leaves_existing_destination_empty_when_target_cannot_be_encoded() {
    let (directory, source, _) = setup();
    // The canonical object is valid UTF-8, but the new checkout rule cannot
    // represent its existing non-ASCII contents.
    fs::write(
        source.root.join(".gitattributes"),
        "file.txt working-tree-encoding=ASCII\n",
    )
    .unwrap();
    git(&source.root, &["add", ".gitattributes"]);
    git(&source.root, &["commit", "-qm", "unrepresentable checkout"]);
    let destination = directory.path().join("empty");
    fs::create_dir(&destination).unwrap();
    assert!(Repository::clone_repo(source.root.to_str().unwrap(), &destination).is_err());
    assert!(fs::read_dir(destination).unwrap().next().is_none());
}

#[test]
fn encoded_submodule_remote_update_leaves_parent_gitlink_unchanged() {
    let (directory, source, target) = setup();
    git(&source.root, &["checkout", "-qb", "plain"]);
    fs::remove_file(source.root.join(".gitattributes")).unwrap();
    fs::write(source.root.join("file.txt"), BASE).unwrap();
    git(&source.root, &["add", "."]);
    git(&source.root, &["commit", "-qm", "plain"]);
    let parent = Repository::init(&directory.path().join("parent")).unwrap();
    let raw = git2::Repository::open(&parent.root).unwrap();
    let mut config = raw.config().unwrap();
    config.set_bool("commit.gpgsign", false).unwrap();
    config.set_str("core.hooksPath", "/dev/null").unwrap();
    config.set_str("core.attributesfile", "").unwrap();
    parent
        .execute(Operation::SetIdentity(
            "Fixture".into(),
            "fixture@example.invalid".into(),
        ))
        .unwrap();
    fs::write(parent.root.join("README"), "root\n").unwrap();
    parent.execute(Operation::StageAll).unwrap();
    parent.execute(Operation::Commit("root".into())).unwrap();
    parent
        .execute(Operation::AddSubmodule {
            url: source.root.to_str().unwrap().to_owned(),
            path: "module".into(),
        })
        .unwrap();
    parent
        .execute(Operation::Commit("add module".into()))
        .unwrap();
    let previous = parent.submodules().unwrap().remove(0);
    parent
        .execute(Operation::UpdateSubmoduleRemote {
            expected: previous.clone(),
            branch: "target".into(),
        })
        .unwrap();
    let after = parent.submodules().unwrap().remove(0);
    assert_eq!(after.index, previous.index);
    assert_eq!(after.workdir.as_deref(), Some(target.as_str()));
    let child = Repository::open(parent.root.join("module")).unwrap();
    assert_eq!(
        fs::read(child.root.join("file.txt")).unwrap(),
        encoded(TARGET, false)
    );
    assert!(child.snapshot(5).unwrap().files.is_empty());
}

#[test]
fn linked_worktree_initialization_uses_its_selected_branch_encoding() {
    let (directory, repo, target) = setup();
    let path = directory.path().join("linked");
    repo.execute(Operation::CreateWorktreeAt {
        source: "target".into(),
        path: path.clone(),
        detached: false,
    })
    .unwrap();
    let linked = Repository::open(&path).unwrap();
    assert_eq!(linked.resolve_revision("HEAD").unwrap().id, target);
    assert_eq!(
        fs::read(path.join("file.txt")).unwrap(),
        encoded(TARGET, false)
    );
    assert_eq!(
        fs::read(repo.root.join("file.txt")).unwrap(),
        encoded(BASE, true)
    );
    assert!(git(&path, &["status", "--porcelain"]).is_empty());
}

#[test]
fn encoded_submodule_initialization_and_recorded_gitlink_update_are_canonical() {
    let (directory, source, target) = setup();
    let parent = Repository::init(&directory.path().join("parent")).unwrap();
    let raw = git2::Repository::open(&parent.root).unwrap();
    let mut config = raw.config().unwrap();
    config.set_bool("commit.gpgsign", false).unwrap();
    config.set_str("core.hooksPath", "/dev/null").unwrap();
    config.set_str("core.attributesfile", "").unwrap();
    parent
        .execute(Operation::SetIdentity(
            "Fixture".into(),
            "fixture@example.invalid".into(),
        ))
        .unwrap();
    fs::write(parent.root.join("README"), "root\n").unwrap();
    parent.execute(Operation::StageAll).unwrap();
    parent.execute(Operation::Commit("root".into())).unwrap();
    parent
        .execute(Operation::AddSubmodule {
            url: source.root.to_str().unwrap().to_owned(),
            path: "module".into(),
        })
        .unwrap();
    parent
        .execute(Operation::Commit("add module".into()))
        .unwrap();
    let child_path = parent.root.join("module");
    assert_eq!(
        fs::read(child_path.join("file.txt")).unwrap(),
        encoded(BASE, true)
    );
    git(
        &parent.root,
        &[
            "update-index",
            "--cacheinfo",
            &format!("160000,{target},module"),
        ],
    );
    parent
        .execute(Operation::UpdateSubmodules {
            names: vec!["module".into()],
            recursive: false,
        })
        .unwrap();
    assert_eq!(
        fs::read(child_path.join("file.txt")).unwrap(),
        encoded(TARGET, false)
    );
    assert_eq!(git(&child_path, &["show", ":file.txt"]), TARGET);
    assert!(git(&child_path, &["status", "--porcelain"]).is_empty());
    assert_eq!(
        parent.submodules().unwrap()[0].workdir.as_deref(),
        Some(target.as_str())
    );
}

#[test]
fn encoded_conflict_rejects_stale_policy_and_unresolved_external_markers() {
    use gitbuddy::git::ConflictResolution;
    let (_directory, repo, _) = setup();
    encoded_conflict(&repo);
    let context = Arc::new(repo.conflict_context(Path::new("file.txt")).unwrap());
    let before = fs::read(repo.root.join("file.txt")).unwrap();
    let raw = git2::Repository::open(&repo.root).unwrap();
    let index_path = raw.path().join("index");
    let index = fs::read(&index_path).unwrap();
    assert!(
        repo.execute(Operation::ResolveConflict {
            context: context.clone(),
            resolution: ConflictResolution::Worktree
        })
        .is_err()
    );
    fs::write(
        repo.root.join(".gitattributes"),
        "file.txt working-tree-encoding=UTF-16BE\n",
    )
    .unwrap();
    assert!(
        repo.execute(Operation::ResolveConflict {
            context,
            resolution: ConflictResolution::Ours
        })
        .is_err()
    );
    assert_eq!(fs::read(repo.root.join("file.txt")).unwrap(), before);
    assert_eq!(fs::read(index_path).unwrap(), index);
}

#[test]
fn encoded_checkout_handles_implicit_git_config_booleans() {
    let (_directory, repo, target) = setup();
    git(&repo.root, &["config", "--unset-all", "core.ignorecase"]);
    let raw = git2::Repository::open(&repo.root).unwrap();
    let config_path = raw.path().join("config");
    let mut config = fs::read_to_string(&config_path).unwrap();
    config.push_str("\n[core]\n\tignorecase\n");
    fs::write(config_path, config).unwrap();
    repo.execute(Operation::Checkout("target".into())).unwrap();
    assert_eq!(repo.resolve_revision("HEAD").unwrap().id, target);
    assert_eq!(
        fs::read(repo.root.join("file.txt")).unwrap(),
        encoded(TARGET, false)
    );
    assert!(git(&repo.root, &["status", "--porcelain"]).is_empty());
}

#[test]
fn lfs_hydration_error_does_not_leave_native_checkout_at_the_wrong_head() {
    use sha2::{Digest, Sha256};
    let (_directory, repo, _) = setup();
    // Convert the fixture to a regular LFS-only branch pair; no external
    // filter is required to put pointer bytes in the test commits.
    git(&repo.root, &["checkout", "-qb", "lfs-base"]);
    fs::write(
        repo.root.join(".gitattributes"),
        "file.txt filter=lfs -text\n",
    )
    .unwrap();
    let raw = git2::Repository::open(&repo.root).unwrap();
    let object_dir = raw.commondir().join("lfs/objects");
    let pointer = |body: &[u8], cache: &[u8]| {
        let oid = format!("{:x}", Sha256::digest(body));
        let cache_path = object_dir.join(&oid[..2]).join(&oid[2..4]).join(&oid);
        fs::create_dir_all(cache_path.parent().unwrap()).unwrap();
        fs::write(cache_path, cache).unwrap();
        format!(
            "version https://git-lfs.github.com/spec/v1\noid sha256:{oid}\nsize {}\n",
            body.len()
        )
    };
    let old = pointer(b"valid source", b"valid source");
    let new = pointer(b"target bytes", b"broken bytes");
    fs::write(repo.root.join("file.txt"), &old).unwrap();
    git(&repo.root, &["add", "."]);
    git(&repo.root, &["commit", "-qm", "source pointer"]);
    git(&repo.root, &["checkout", "-qb", "lfs-target"]);
    fs::write(repo.root.join("file.txt"), &new).unwrap();
    git(&repo.root, &["commit", "-qam", "target pointer"]);
    let target = git(&repo.root, &["rev-parse", "HEAD"]).trim().to_owned();
    git(&repo.root, &["checkout", "-q", "lfs-base"]);
    repo.execute(Operation::LfsCheckout).unwrap();
    assert_eq!(
        fs::read(repo.root.join("file.txt")).unwrap(),
        b"valid source"
    );
    let error = repo
        .execute(Operation::Checkout("lfs-target".into()))
        .unwrap_err();
    assert!(format!("{error:#}").contains("LFS"));
    assert_eq!(repo.resolve_revision("HEAD").unwrap().id, target);
    assert_eq!(git(&repo.root, &["show", ":file.txt"]), new);
    assert_eq!(fs::read_to_string(repo.root.join("file.txt")).unwrap(), new);
    assert!(!repo.snapshot(5).unwrap().worktree_recovery);
}

#[test]
fn discarding_a_plain_file_does_not_hydrate_an_unrelated_lfs_pointer() {
    use sha2::{Digest, Sha256};
    let (_directory, repo, _) = setup();
    fs::write(
        repo.root.join(".gitattributes"),
        "file.txt filter=lfs -text\n",
    )
    .unwrap();
    let body = b"cached unrelated contents";
    let oid = format!("{:x}", Sha256::digest(body));
    let pointer = format!(
        "version https://git-lfs.github.com/spec/v1\noid sha256:{oid}\nsize {}\n",
        body.len()
    );
    fs::write(repo.root.join("file.txt"), &pointer).unwrap();
    fs::write(repo.root.join("plain"), "base\n").unwrap();
    git(&repo.root, &["add", "."]);
    git(&repo.root, &["commit", "-qm", "plain and LFS"]);
    let raw = git2::Repository::open(&repo.root).unwrap();
    let object = raw
        .commondir()
        .join("lfs/objects")
        .join(&oid[..2])
        .join(&oid[2..4])
        .join(&oid);
    fs::create_dir_all(object.parent().unwrap()).unwrap();
    fs::write(object, body).unwrap();
    fs::write(repo.root.join("plain"), "discard this\n").unwrap();
    repo.execute(Operation::Discard("plain".into())).unwrap();
    assert_eq!(
        fs::read_to_string(repo.root.join("plain")).unwrap(),
        "base\n"
    );
    assert_eq!(
        fs::read_to_string(repo.root.join("file.txt")).unwrap(),
        pointer
    );
}
