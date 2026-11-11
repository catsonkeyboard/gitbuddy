use gitbuddy::git::{NetworkControl, Operation, Repository};
use std::{fs, path::PathBuf, sync::Arc};

fn setup() -> (tempfile::TempDir, Repository) {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::init(&dir.path().join("repo")).unwrap();
    repo.execute(Operation::SetIdentity(
        "Migration".into(),
        "migration@example.invalid".into(),
    ))
    .unwrap();
    let tool = std::env::var_os("GITBUDDY_TEST_GIT_LFS")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("target/tooling-lfs-v3.8.1/git-lfs-3.8.1/git-lfs")
        });
    assert!(
        tool.is_file(),
        "Set GITBUDDY_TEST_GIT_LFS to the official git-lfs binary"
    );
    git2::Repository::open(&repo.root)
        .unwrap()
        .config()
        .unwrap()
        .set_str("gitbuddy.lfsProgram", tool.to_str().unwrap())
        .unwrap();
    (dir, repo)
}
fn commit(repo: &Repository, text: &str) -> String {
    repo.execute(Operation::StageAll).unwrap();
    repo.execute(Operation::Commit(text.into())).unwrap();
    repo.resolve_revision("HEAD").unwrap().id
}
fn prepare(repo: &Repository) -> gitbuddy::git::LfsMigrationPlan {
    repo.prepare_lfs_migration("*.bin", false, NetworkControl::default())
        .unwrap()
}

#[test]
fn preview_is_source_invariant_and_apply_rewrites_only_current_branch_with_custom_storage() {
    let (dir, repo) = setup();
    fs::write(repo.root.join("asset.bin"), b"first\0binary\xff").unwrap();
    let old = commit(&repo, "binary");
    let raw = git2::Repository::open(&repo.root).unwrap();
    raw.branch(
        "other",
        &raw.find_commit(old.parse().unwrap()).unwrap(),
        false,
    )
    .unwrap();
    raw.tag_lightweight(
        "saved",
        raw.find_commit(old.parse().unwrap()).unwrap().as_object(),
        false,
    )
    .unwrap();
    let storage = dir.path().join("shared-lfs");
    raw.config()
        .unwrap()
        .set_str("lfs.storage", storage.to_str().unwrap())
        .unwrap();
    let index = fs::read(raw.path().join("index")).unwrap();
    let config = fs::read(raw.path().join("config")).unwrap();
    let plan = prepare(&repo);
    assert_ne!(plan.original, plan.target);
    assert_eq!(repo.resolve_revision("HEAD").unwrap().id, old);
    assert_eq!(fs::read(raw.path().join("index")).unwrap(), index);
    assert_eq!(fs::read(raw.path().join("config")).unwrap(), config);
    assert_eq!(
        fs::read(repo.root.join("asset.bin")).unwrap(),
        b"first\0binary\xff"
    );
    assert!(!storage.exists());
    let target = plan.target.clone();
    repo.execute(Operation::LfsMigrate(Arc::new(plan))).unwrap();
    assert_eq!(repo.resolve_revision("HEAD").unwrap().id, target);
    assert_eq!(repo.resolve_revision("other").unwrap().id, old);
    assert_eq!(repo.resolve_revision("saved").unwrap().id, old);
    assert_eq!(
        fs::read(repo.root.join("asset.bin")).unwrap(),
        b"first\0binary\xff"
    );
    assert!(repo.snapshot(10).unwrap().files.is_empty());
    assert!(repo.lfs_migration_status().unwrap().is_none());
    let status = repo.lfs_status().unwrap();
    assert_eq!(status.files.len(), 1);
    assert!(status.files[0].available && status.files[0].hydrated);
    assert!(storage.join("objects").exists());
}

#[test]
fn migration_from_linked_worktree_preserves_main_checkout_and_uses_shared_cache() {
    let (dir, main) = setup();
    fs::write(main.root.join("asset.bin"), b"original\0asset").unwrap();
    let original = commit(&main, "original");
    let raw = git2::Repository::open(&main.root).unwrap();
    let linked_path = dir.path().join("linked");
    raw.worktree("linked", &linked_path, None).unwrap();
    let linked = Repository::open(&linked_path).unwrap();
    let other = git2::Repository::open(&linked.root).unwrap();
    let main_index = fs::read(raw.path().join("index")).unwrap();
    fs::write(main.root.join("asset.bin"), b"uncommitted main editing").unwrap();
    let plan = prepare(&linked);
    let target = plan.target.clone();
    linked
        .execute(Operation::LfsMigrate(Arc::new(plan)))
        .unwrap();
    assert_eq!(linked.resolve_revision("HEAD").unwrap().id, target);
    assert_eq!(main.resolve_revision("HEAD").unwrap().id, original);
    assert_eq!(fs::read(raw.path().join("index")).unwrap(), main_index);
    assert_eq!(
        fs::read(main.root.join("asset.bin")).unwrap(),
        b"uncommitted main editing"
    );
    assert_eq!(
        fs::read(linked.root.join("asset.bin")).unwrap(),
        b"original\0asset"
    );
    assert!(linked.snapshot(10).unwrap().files.is_empty());
    assert!(raw.commondir().join("lfs/objects").exists());
    assert!(!other.path().join("lfs/objects").exists());
}

#[test]
fn migration_rechecks_branch_ownership_if_another_worktree_checks_it_out() {
    let (dir, main) = setup();
    fs::write(main.root.join("asset.bin"), b"original\0asset").unwrap();
    let original = commit(&main, "original");
    let raw = git2::Repository::open(&main.root).unwrap();
    let linked_path = dir.path().join("linked");
    raw.worktree("linked", &linked_path, None).unwrap();
    let plan = prepare(&main);
    let other = git2::Repository::open(&linked_path).unwrap();
    // Model an external checkout --ignore-other-worktrees. The native setter
    // correctly refuses this duplicate ownership itself.
    fs::write(
        other.path().join("HEAD"),
        format!("ref: {}\n", raw.head().unwrap().name().unwrap()),
    )
    .unwrap();
    assert!(main.execute(Operation::LfsMigrate(Arc::new(plan))).is_err());
    assert_eq!(main.resolve_revision("HEAD").unwrap().id, original);
    assert_eq!(
        fs::read(main.root.join("asset.bin")).unwrap(),
        b"original\0asset"
    );
    assert!(
        main.prepare_lfs_migration("*.bin", false, NetworkControl::default())
            .is_err()
    );
    assert!(main.lfs_migration_status().unwrap().is_none());
}

#[test]
fn linked_migration_recovery_requires_owner_and_preserves_other_worktree_edits() {
    let (dir, main) = setup();
    fs::write(main.root.join("asset.bin"), b"original\0asset").unwrap();
    let original = commit(&main, "original");
    let raw = git2::Repository::open(&main.root).unwrap();
    let linked_path = dir.path().join("linked");
    raw.worktree("linked", &linked_path, None).unwrap();
    let linked = Repository::open(&linked_path).unwrap();
    let main_index = fs::read(raw.path().join("index")).unwrap();
    interrupt(&linked, "LFS migration: checkout verified");
    fs::write(main.root.join("asset.bin"), b"preserved main edit").unwrap();
    let status = main.lfs_migration_status().unwrap().unwrap();
    assert_eq!(status.owner, linked.root);
    assert!(
        main.execute(Operation::ContinueLfsMigration {
            expected: status.expected.clone()
        })
        .is_err()
    );
    assert!(
        main.execute(Operation::AbortLfsMigration {
            expected: status.expected.clone()
        })
        .is_err()
    );
    let reopened = Repository::open(&linked_path).unwrap();
    reopened
        .execute(Operation::AbortLfsMigration {
            expected: status.expected,
        })
        .unwrap();
    assert!(main.lfs_migration_status().unwrap().is_none());
    assert_eq!(linked.resolve_revision("HEAD").unwrap().id, original);
    assert_eq!(main.resolve_revision("HEAD").unwrap().id, original);
    assert_eq!(fs::read(raw.path().join("index")).unwrap(), main_index);
    assert_eq!(
        fs::read(main.root.join("asset.bin")).unwrap(),
        b"preserved main edit"
    );
    assert_eq!(
        fs::read(linked.root.join("asset.bin")).unwrap(),
        b"original\0asset"
    );
}

#[test]
fn migration_detects_same_length_config_edit_with_restored_timestamp() {
    let (_dir, repo) = setup();
    let raw = git2::Repository::open(&repo.root).unwrap();
    raw.config()
        .unwrap()
        .set_bool("commit.gpgsign", false)
        .unwrap();
    fs::write(repo.root.join("asset.bin"), "raw").unwrap();
    let old = commit(&repo, "raw");
    let plan = Arc::new(prepare(&repo));
    let path = raw.path().join("config");
    let original = fs::read_to_string(&path).unwrap();
    let modified = fs::metadata(&path).unwrap().modified().unwrap();
    let changed = original.replacen("gpgsign = false", "gpgsign = true ", 1);
    assert_ne!(original, changed);
    assert_eq!(original.len(), changed.len());
    let sink: gitbuddy::git::ProgressSink =
        Arc::new(std::sync::Mutex::new(move |message: &str| {
            if message == "LFS migration: checkout pending" {
                fs::write(&path, &changed).unwrap();
                fs::OpenOptions::new()
                    .write(true)
                    .open(&path)
                    .unwrap()
                    .set_times(fs::FileTimes::new().set_modified(modified))
                    .unwrap();
            }
        }));
    assert!(
        repo.execute_with_control(
            Operation::LfsMigrate(plan),
            NetworkControl::new(Some(sink), Default::default())
        )
        .is_err()
    );
    assert_eq!(repo.resolve_revision("HEAD").unwrap().id, old);
    assert!(
        git2::Repository::open(&repo.root)
            .unwrap()
            .config()
            .unwrap()
            .get_bool("commit.gpgsign")
            .unwrap()
    );
    assert!(repo.lfs_migration_status().unwrap().is_some());
}

#[test]
fn migration_detects_effective_signing_changes_from_duplicate_order_or_implicit_bool() {
    for (before, after) in [
        (
            "gpgsign = true\n\tgpgsign = false",
            "gpgsign = false\n\tgpgsign = true",
        ),
        ("gpgsign = ", "gpgsign   "),
    ] {
        let (_dir, repo) = setup();
        let raw = git2::Repository::open(&repo.root).unwrap();
        raw.config()
            .unwrap()
            .set_bool("commit.gpgsign", false)
            .unwrap();
        fs::write(repo.root.join("asset.bin"), "raw").unwrap();
        let old = commit(&repo, "raw");
        let path = raw.path().join("config");
        let original = fs::read_to_string(&path).unwrap();
        let configured = original.replacen("gpgsign = false", before, 1);
        let changed = original.replacen("gpgsign = false", after, 1);
        assert_ne!(configured, changed);
        assert_eq!(configured.len(), changed.len());
        fs::write(&path, configured).unwrap();
        assert!(
            !git2::Repository::open(&repo.root)
                .unwrap()
                .config()
                .unwrap()
                .get_bool("commit.gpgsign")
                .unwrap()
        );
        let plan = Arc::new(prepare(&repo));
        let modified = fs::metadata(&path).unwrap().modified().unwrap();
        let sink: gitbuddy::git::ProgressSink =
            Arc::new(std::sync::Mutex::new(move |message: &str| {
                if message == "LFS migration: checkout pending" {
                    fs::write(&path, &changed).unwrap();
                    fs::OpenOptions::new()
                        .write(true)
                        .open(&path)
                        .unwrap()
                        .set_times(fs::FileTimes::new().set_modified(modified))
                        .unwrap();
                }
            }));
        assert!(
            repo.execute_with_control(
                Operation::LfsMigrate(plan),
                NetworkControl::new(Some(sink), Default::default())
            )
            .is_err()
        );
        assert_eq!(repo.resolve_revision("HEAD").unwrap().id, old);
        assert!(
            git2::Repository::open(&repo.root)
                .unwrap()
                .config()
                .unwrap()
                .get_bool("commit.gpgsign")
                .unwrap()
        );
    }
}

#[test]
fn migration_locks_other_worktree_heads_until_publication() {
    let (dir, repo) = setup();
    fs::write(repo.root.join("asset.bin"), "raw").unwrap();
    let old = commit(&repo, "raw");
    let raw = git2::Repository::open(&repo.root).unwrap();
    let linked = dir.path().join("linked");
    raw.worktree("linked", &linked, None).unwrap();
    let plan = Arc::new(prepare(&repo));
    let target = plan.target.clone();
    let observed = linked.clone();
    let sink: gitbuddy::git::ProgressSink =
        Arc::new(std::sync::Mutex::new(move |message: &str| {
            if message == "LFS migration: checkout verified" {
                let status = std::process::Command::new("git")
                    .arg("-C")
                    .arg(&observed)
                    .args([
                        "-c",
                        "core.hooksPath=/dev/null",
                        "checkout",
                        "--ignore-other-worktrees",
                        "main",
                    ])
                    .output()
                    .unwrap();
                assert!(
                    !status.status.success(),
                    "Other worktree HEAD must stay locked through publication"
                );
            }
        }));
    repo.execute_with_control(
        Operation::LfsMigrate(plan),
        NetworkControl::new(Some(sink), Default::default()),
    )
    .unwrap();
    assert_eq!(repo.resolve_revision("HEAD").unwrap().id, target);
    let other = git2::Repository::open(&linked).unwrap();
    assert_eq!(other.head().unwrap().name().unwrap(), "refs/heads/linked");
    assert_eq!(other.head().unwrap().target().unwrap().to_string(), old);
}

#[test]
fn migration_refuses_to_wrap_malformed_extended_pointers() {
    for suffix in [String::new(), "oversized".repeat(256)] {
        let (_dir, repo) = setup();
        let malformed = format!(
            "version https://git-lfs.github.com/spec/v1\next-0-bad sha1:{}\noid sha256:{}\nsize 3\n{suffix}",
            "a".repeat(40),
            "b".repeat(64)
        );
        fs::write(repo.root.join("asset.bin"), &malformed).unwrap();
        let old = commit(&repo, "malformed pointer");
        let raw = git2::Repository::open(&repo.root).unwrap();
        let index = fs::read(raw.path().join("index")).unwrap();
        assert!(
            repo.prepare_lfs_migration("*.bin", false, NetworkControl::default())
                .is_err()
        );
        assert_eq!(repo.resolve_revision("HEAD").unwrap().id, old);
        assert_eq!(fs::read(raw.path().join("index")).unwrap(), index);
        assert_eq!(
            fs::read_to_string(repo.root.join("asset.bin")).unwrap(),
            malformed
        );
        assert!(!raw.commondir().join("lfs").exists());
    }
}

#[test]
fn migration_rechecks_worktree_ownership_when_a_writer_bypasses_head_locks() {
    let (dir, repo) = setup();
    fs::write(repo.root.join("asset.bin"), "raw").unwrap();
    let old = commit(&repo, "raw");
    let raw = git2::Repository::open(&repo.root).unwrap();
    let linked = dir.path().join("linked");
    raw.worktree("linked", &linked, None).unwrap();
    let other = git2::Repository::open(&linked).unwrap();
    let other_head = other.path().join("HEAD");
    let original = fs::read(&other_head).unwrap();
    let plan = Arc::new(prepare(&repo));
    let overwritten = other_head.clone();
    let sink: gitbuddy::git::ProgressSink =
        Arc::new(std::sync::Mutex::new(move |message: &str| {
            if message == "LFS migration: checkout verified" {
                fs::write(&overwritten, b"ref: refs/heads/main\n").unwrap();
            }
        }));
    assert!(
        repo.execute_with_control(
            Operation::LfsMigrate(plan),
            NetworkControl::new(Some(sink), Default::default())
        )
        .is_err()
    );
    assert_eq!(repo.resolve_revision("HEAD").unwrap().id, old);
    assert!(repo.lfs_migration_status().unwrap().is_some());
    fs::write(&other_head, original).unwrap();
    let status = repo.lfs_migration_status().unwrap().unwrap();
    repo.execute(Operation::AbortLfsMigration {
        expected: status.expected,
    })
    .unwrap();
    assert!(repo.lfs_migration_status().unwrap().is_none());
    assert_eq!(fs::read(repo.root.join("asset.bin")).unwrap(), b"raw");
}

#[test]
fn stale_index_or_config_refuses_apply_before_worktree_or_branch_changes() {
    let (_dir, repo) = setup();
    fs::write(repo.root.join("asset.bin"), "committed").unwrap();
    let old = commit(&repo, "initial");
    let plan = prepare(&repo);
    fs::write(repo.root.join("asset.bin"), "new staged content").unwrap();
    repo.execute(Operation::StageAll).unwrap();
    assert!(repo.execute(Operation::LfsMigrate(Arc::new(plan))).is_err());
    assert_eq!(repo.resolve_revision("HEAD").unwrap().id, old);
    assert_eq!(
        fs::read(repo.root.join("asset.bin")).unwrap(),
        b"new staged content"
    );
    assert!(repo.lfs_migration_status().unwrap().is_none());
    repo.execute(Operation::Discard("asset.bin".into()))
        .unwrap();
    repo.execute(Operation::UnstageAll).unwrap();
    repo.execute(Operation::Discard("asset.bin".into()))
        .unwrap();
    let plan = prepare(&repo);
    git2::Repository::open(&repo.root)
        .unwrap()
        .config()
        .unwrap()
        .set_bool("commit.gpgsign", true)
        .unwrap();
    assert!(repo.execute(Operation::LfsMigrate(Arc::new(plan))).is_err());
    assert_eq!(repo.resolve_revision("HEAD").unwrap().id, old);
}

#[test]
fn ignored_attributes_collision_is_preserved_and_never_overwritten() {
    let (_dir, repo) = setup();
    fs::write(repo.root.join("asset.bin"), "content").unwrap();
    fs::write(repo.root.join(".gitignore"), ".gitattributes\n").unwrap();
    let old = commit(&repo, "initial");
    fs::write(
        repo.root.join(".gitattributes"),
        "private ignored attributes\n",
    )
    .unwrap();
    assert!(
        repo.prepare_lfs_migration("*.bin", false, NetworkControl::default())
            .is_err()
    );
    assert_eq!(repo.resolve_revision("HEAD").unwrap().id, old);
    assert_eq!(
        fs::read(repo.root.join(".gitattributes")).unwrap(),
        b"private ignored attributes\n"
    );
}

#[test]
fn effective_signing_requires_explicit_unsigned_plan_and_incomplete_processors_are_refused() {
    let (_dir, repo) = setup();
    fs::write(repo.root.join("asset.bin"), "content").unwrap();
    commit(&repo, "initial");
    let raw = git2::Repository::open(&repo.root).unwrap();
    raw.config()
        .unwrap()
        .set_bool("commit.gpgsign", true)
        .unwrap();
    assert!(
        repo.prepare_lfs_migration("*.bin", false, NetworkControl::default())
            .is_err()
    );
    let plan = repo
        .prepare_lfs_migration("*.bin", true, NetworkControl::default())
        .unwrap();
    assert!(plan.unsigned);
    raw.config()
        .unwrap()
        .set_str("lfs.extension.demo.clean", "must-not-execute")
        .unwrap();
    assert!(
        repo.prepare_lfs_migration("*.bin", true, NetworkControl::default())
            .is_err()
    );
}

#[test]
fn extension_migration_round_trips_history_and_retains_existing_extended_pointers() {
    for existing_pointer in [false, true] {
        let (dir, repo) = setup();
        fs::write(repo.root.join("asset.bin"), b"historical\0asset").unwrap();
        fs::write(repo.root.join("empty.bin"), b"").unwrap();
        let historical = commit(&repo, "historical raw asset");
        let raw = git2::Repository::open(&repo.root).unwrap();
        let processor = dir.path().join("transform.py");
        fs::write(&processor, "import sys\nd=sys.stdin.buffer.read()\nif sys.argv[1]=='clean':\n sys.stdout.buffer.write(b'EXT:'+d[::-1])\nelse:\n assert d.startswith(b'EXT:')\n sys.stdout.buffer.write(d[4:][::-1])\n").unwrap();
        let mut config = raw.config().unwrap();
        config.set_i64("lfs.extension.reverse.priority", 0).unwrap();
        for op in ["clean", "smudge"] {
            config
                .set_str(
                    &format!("lfs.extension.reverse.{op}"),
                    &format!("python3 {} {op}", processor.display()),
                )
                .unwrap();
        }
        fs::write(repo.root.join("asset.bin"), b"current\0asset").unwrap();
        if existing_pointer {
            repo.execute(Operation::LfsTrack {
                pattern: "*.bin".into(),
                track: true,
            })
            .unwrap();
        }
        let old_tip = commit(&repo, "current asset");
        let old_tip_blob = raw
            .find_commit(old_tip.parse().unwrap())
            .unwrap()
            .tree()
            .unwrap()
            .get_name("asset.bin")
            .unwrap()
            .id();
        let original_index = fs::read(raw.path().join("index")).unwrap();
        if !existing_pointer {
            config.set_i64("gitbuddy.lfsFilterMaxBytes", 8).unwrap();
            assert!(
                repo.prepare_lfs_migration("*.bin", false, NetworkControl::default())
                    .is_err()
            );
            config.remove("gitbuddy.lfsFilterMaxBytes").unwrap();
            assert_eq!(fs::read(raw.path().join("index")).unwrap(), original_index);
            assert_eq!(repo.resolve_revision("HEAD").unwrap().id, old_tip);
        }
        let original_config = fs::read(raw.commondir().join("config")).unwrap();
        let plan = prepare(&repo);
        assert_ne!(plan.target, old_tip);
        assert_eq!(fs::read(raw.path().join("index")).unwrap(), original_index);
        assert_eq!(repo.resolve_revision("HEAD").unwrap().id, old_tip);
        assert_eq!(
            fs::read(repo.root.join("asset.bin")).unwrap(),
            b"current\0asset"
        );
        let result = repo.execute(Operation::LfsMigrate(Arc::new(plan)));
        assert_eq!(
            fs::read(raw.commondir().join("config")).unwrap(),
            original_config,
            "Migration must not rewrite source configuration"
        );
        result.unwrap();
        let tip = raw.head().unwrap().peel_to_commit().unwrap();
        let entry = tip.tree().unwrap().get_name("asset.bin").unwrap().id();
        let pointer = raw.find_blob(entry).unwrap();
        assert!(
            std::str::from_utf8(pointer.content())
                .unwrap()
                .contains("\next-0-reverse sha256:")
        );
        if existing_pointer {
            assert_eq!(entry, old_tip_blob);
            let old_pointer = raw.find_blob(old_tip_blob).unwrap();
            let mut double_wrapped = b"EXT:".to_vec();
            double_wrapped.extend(old_pointer.content().iter().rev());
            use sha2::{Digest, Sha256};
            let obsolete = format!("{:x}", Sha256::digest(&double_wrapped));
            assert!(
                !raw.commondir()
                    .join("lfs/objects")
                    .join(&obsolete[..2])
                    .join(&obsolete[2..4])
                    .join(obsolete)
                    .exists(),
                "Unreferenced double-wrapped content must not enter the source cache"
            );
        }
        assert_eq!(
            fs::read(repo.root.join("asset.bin")).unwrap(),
            b"current\0asset"
        );
        assert!(repo.snapshot(10).unwrap().files.is_empty());
        let parent = tip.parent(0).unwrap();
        assert_ne!(parent.id().to_string(), historical);
        assert!(
            std::str::from_utf8(
                raw.find_blob(parent.tree().unwrap().get_name("asset.bin").unwrap().id())
                    .unwrap()
                    .content()
            )
            .unwrap()
            .contains("\next-0-reverse sha256:")
        );
        let status = repo.lfs_status().unwrap();
        assert!(status.files.iter().all(|f| f.available && f.hydrated));
    }
}

#[test]
fn malformed_common_journal_blocks_unrelated_writes_and_commit_eligibility() {
    let (_dir, repo) = setup();
    fs::write(repo.root.join("asset.bin"), "content").unwrap();
    commit(&repo, "initial");
    let raw = git2::Repository::open(&repo.root).unwrap();
    fs::write(
        raw.commondir().join("gitbuddy-lfs-migration.json"),
        "malformed state",
    )
    .unwrap();
    assert!(
        repo.execute(Operation::SetIdentity(
            "changed".into(),
            "changed@example.invalid".into()
        ))
        .is_err()
    );
    assert!(repo.lfs_migration_status().is_err());
    let snapshot = repo.snapshot(10).unwrap();
    assert!(snapshot.lfs_migration);
    assert!(!snapshot.can_commit());
}

fn interrupt(repo: &Repository, phase: &'static str) -> String {
    let plan = Arc::new(prepare(repo));
    let target = plan.target.clone();
    let sink: gitbuddy::git::ProgressSink =
        Arc::new(std::sync::Mutex::new(move |message: &str| {
            assert_ne!(
                message, phase,
                "simulated process interruption at a durable checkpoint"
            );
        }));
    let control = NetworkControl::new(Some(sink), Default::default());
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            repo.execute_with_control(Operation::LfsMigrate(plan), control)
                .unwrap();
        }))
        .is_err()
    );
    target
}

#[test]
fn interrupted_checkout_refuses_continue_and_abort_preserves_partial_user_edits() {
    let (_dir, repo) = setup();
    fs::write(repo.root.join("asset.bin"), "original").unwrap();
    let old = commit(&repo, "original");
    interrupt(&repo, "LFS migration: checkout pending");
    let reopened = Repository::open(&repo.root).unwrap();
    fs::write(repo.root.join("asset.bin"), "user edit after interruption").unwrap();
    fs::write(repo.root.join(".gitattributes"), "user-created collision").unwrap();
    let status = reopened.lfs_migration_status().unwrap().unwrap();
    assert_eq!(status.phase, "CheckingOut");
    assert!(!status.can_continue);
    assert!(
        reopened
            .execute(Operation::ContinueLfsMigration {
                expected: status.expected.clone()
            })
            .is_err()
    );
    assert!(reopened.execute(Operation::StageAll).is_err());
    reopened
        .execute(Operation::AbortLfsMigration {
            expected: status.expected.clone(),
        })
        .unwrap();
    assert_eq!(reopened.resolve_revision("HEAD").unwrap().id, old);
    assert_eq!(fs::read(repo.root.join("asset.bin")).unwrap(), b"original");
    assert!(!repo.root.join(".gitattributes").exists());
    let backup = fs::read_dir(&status.recovery)
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| {
            p.file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with("aborted-")
        })
        .unwrap();
    assert_eq!(
        fs::read(backup.join("worktree/asset.bin")).unwrap(),
        b"user edit after interruption"
    );
    assert_eq!(
        fs::read(backup.join("worktree/.gitattributes")).unwrap(),
        b"user-created collision"
    );
    assert!(
        reopened
            .execute(Operation::AbortLfsMigration {
                expected: status.expected
            })
            .is_ok()
    );
}

#[test]
fn verified_checkout_resumes_before_or_after_branch_publication_idempotently() {
    for phase in [
        "LFS migration: checkout verified",
        "LFS migration: branch published",
    ] {
        let (_dir, repo) = setup();
        fs::write(repo.root.join("asset.bin"), "payload").unwrap();
        let old = commit(&repo, "original");
        let target = interrupt(&repo, phase);
        let reopened = Repository::open(&repo.root).unwrap();
        let status = reopened.lfs_migration_status().unwrap().unwrap();
        assert!(status.can_continue);
        assert_eq!(status.phase, "CheckedOut");
        assert_eq!(
            reopened.resolve_revision("HEAD").unwrap().id,
            if phase.ends_with("published") {
                target.clone()
            } else {
                old
            }
        );
        reopened
            .execute(Operation::ContinueLfsMigration {
                expected: status.expected.clone(),
            })
            .unwrap();
        assert_eq!(reopened.resolve_revision("HEAD").unwrap().id, target);
        assert!(reopened.snapshot(10).unwrap().files.is_empty());
        assert!(reopened.lfs_migration_status().unwrap().is_none());
        reopened
            .execute(Operation::ContinueLfsMigration {
                expected: status.expected,
            })
            .unwrap();
    }
}

#[test]
fn verified_checkout_changed_by_user_cannot_publish_and_abort_restores_original() {
    let (_dir, repo) = setup();
    fs::write(repo.root.join("asset.bin"), "original").unwrap();
    let old = commit(&repo, "original");
    interrupt(&repo, "LFS migration: checkout verified");
    fs::write(repo.root.join("asset.bin"), "user edit").unwrap();
    let status = repo.lfs_migration_status().unwrap().unwrap();
    assert!(
        repo.execute(Operation::ContinueLfsMigration {
            expected: status.expected.clone()
        })
        .is_err()
    );
    assert_eq!(repo.resolve_revision("HEAD").unwrap().id, old);
    repo.execute(Operation::AbortLfsMigration {
        expected: status.expected,
    })
    .unwrap();
    assert_eq!(fs::read(repo.root.join("asset.bin")).unwrap(), b"original");
}

#[test]
fn source_hooks_never_run_and_existing_commit_signatures_need_explicit_consent() {
    let (_dir, repo) = setup();
    fs::write(repo.root.join("asset.bin"), "payload").unwrap();
    let original = commit(&repo, "original");
    let raw = git2::Repository::open(&repo.root).unwrap();
    let old = raw.find_commit(original.parse().unwrap()).unwrap();
    let buffer = raw
        .commit_create_buffer(
            &old.author(),
            &old.committer(),
            "signed original",
            &old.tree().unwrap(),
            &[],
        )
        .unwrap();
    let signed = raw
        .commit_signed(
            std::str::from_utf8(&buffer).unwrap(),
            "-----BEGIN PGP SIGNATURE-----\nfixture\n-----END PGP SIGNATURE-----",
            None,
        )
        .unwrap();
    raw.find_reference("refs/heads/main")
        .unwrap()
        .set_target(signed, "fixture signed history")
        .unwrap();
    let hooks = raw.path().join("source-hooks");
    fs::create_dir(&hooks).unwrap();
    let sentinel = repo.root.parent().unwrap().join("source-hook-ran");
    fs::write(
        hooks.join("post-checkout"),
        format!("#!/bin/sh\ntouch '{}'\nexit 63\n", sentinel.display()),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(
            hooks.join("post-checkout"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
    }
    raw.config()
        .unwrap()
        .set_str("core.hooksPath", hooks.to_str().unwrap())
        .unwrap();
    assert!(
        repo.prepare_lfs_migration("*.bin", false, NetworkControl::default())
            .is_err()
    );
    let plan = repo
        .prepare_lfs_migration("*.bin", true, NetworkControl::default())
        .unwrap();
    assert_eq!(plan.signed_commits, 1);
    repo.execute(Operation::LfsMigrate(Arc::new(plan))).unwrap();
    assert!(!sentinel.exists());
    assert!(
        raw.extract_signature(&raw.head().unwrap().target().unwrap(), None)
            .is_err()
    );
}

#[test]
fn migration_preserves_merge_topology_and_never_changes_gitlinks_or_submodule_files() {
    let (_dir, repo) = setup();
    fs::write(repo.root.join("root.bin"), "root").unwrap();
    commit(&repo, "root");
    repo.execute(Operation::CreateBranch("feature".into()))
        .unwrap();
    fs::write(repo.root.join("feature.bin"), "feature").unwrap();
    commit(&repo, "feature");
    repo.execute(Operation::Checkout("main".into())).unwrap();
    fs::write(repo.root.join("main.bin"), "main").unwrap();
    commit(&repo, "main");
    repo.execute(Operation::Merge("feature".into())).unwrap();
    let raw = git2::Repository::open(&repo.root).unwrap();
    let sub = git2::Repository::init(repo.root.join("module")).unwrap();
    fs::write(repo.root.join("module/inner"), "submodule stays intact").unwrap();
    let mut sub_index = sub.index().unwrap();
    sub_index.add_path(std::path::Path::new("inner")).unwrap();
    sub_index.write().unwrap();
    let tree = sub.find_tree(sub_index.write_tree().unwrap()).unwrap();
    let signature = git2::Signature::now("Submodule", "submodule@example.invalid").unwrap();
    let sub_id = sub
        .commit(
            Some("HEAD"),
            &signature,
            &signature,
            "submodule",
            &tree,
            &[],
        )
        .unwrap();
    fs::write(
        repo.root.join(".gitmodules"),
        "[submodule \"module\"]\n path = module\n url = ../never-contacted\n",
    )
    .unwrap();
    let mut index = raw.index().unwrap();
    index.add_path(std::path::Path::new(".gitmodules")).unwrap();
    index
        .add(&git2::IndexEntry {
            ctime: git2::IndexTime::new(0, 0),
            mtime: git2::IndexTime::new(0, 0),
            dev: 0,
            ino: 0,
            mode: 0o160000,
            uid: 0,
            gid: 0,
            file_size: 0,
            id: sub_id,
            flags: 0,
            flags_extended: 0,
            path: b"module".to_vec(),
        })
        .unwrap();
    index.write().unwrap();
    repo.execute(Operation::Commit("record gitlink".into()))
        .unwrap();
    let sub_head = fs::read(sub.path().join("HEAD")).unwrap();
    let plan = prepare(&repo);
    repo.execute(Operation::LfsMigrate(Arc::new(plan))).unwrap();
    let tip = raw.head().unwrap().peel_to_commit().unwrap();
    assert_eq!(tip.tree().unwrap().get_name("module").unwrap().id(), sub_id);
    let merge = tip.parent(0).unwrap();
    assert_eq!(merge.parent_count(), 2);
    assert_eq!(merge.parent(0).unwrap().summary().unwrap(), Some("main"));
    assert_eq!(merge.parent(1).unwrap().summary().unwrap(), Some("feature"));
    assert_eq!(fs::read(sub.path().join("HEAD")).unwrap(), sub_head);
    assert_eq!(
        fs::read(repo.root.join("module/inner")).unwrap(),
        b"submodule stays intact"
    );
}

#[test]
fn unsafe_custom_cache_or_cancelled_plan_never_changes_source_checkout() {
    let (dir, repo) = setup();
    fs::write(repo.root.join("asset.bin"), "content").unwrap();
    let old = commit(&repo, "original");
    let cancellation = gitbuddy::git::CancellationToken::default();
    cancellation.cancel();
    let error = repo
        .prepare_lfs_migration("*.bin", false, NetworkControl::new(None, cancellation))
        .unwrap_err();
    assert!(gitbuddy::git::is_cancelled(&error));
    #[cfg(unix)]
    {
        let raw = git2::Repository::open(&repo.root).unwrap();
        let storage = dir.path().join("cache");
        fs::create_dir(&storage).unwrap();
        let outside = dir.path().join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("keep"), "private").unwrap();
        std::os::unix::fs::symlink(&outside, storage.join("objects")).unwrap();
        raw.config()
            .unwrap()
            .set_str("lfs.storage", storage.to_str().unwrap())
            .unwrap();
        let plan = prepare(&repo);
        assert!(repo.execute(Operation::LfsMigrate(Arc::new(plan))).is_err());
        assert_eq!(fs::read(outside.join("keep")).unwrap(), b"private");
    }
    assert_eq!(repo.resolve_revision("HEAD").unwrap().id, old);
    assert_eq!(fs::read(repo.root.join("asset.bin")).unwrap(), b"content");
    assert!(repo.lfs_migration_status().unwrap().is_none());
}

#[test]
fn ignored_collision_created_during_checkout_is_preserved_for_abort() {
    let (_dir, repo) = setup();
    fs::write(repo.root.join("asset.bin"), "original").unwrap();
    fs::write(repo.root.join(".gitignore"), ".gitattributes\n").unwrap();
    let old = commit(&repo, "original");
    let plan = Arc::new(prepare(&repo));
    let collision = repo.root.join(".gitattributes");
    let sink: gitbuddy::git::ProgressSink =
        Arc::new(std::sync::Mutex::new(move |message: &str| {
            if message == "LFS migration: checkout pending" {
                fs::write(&collision, "concurrent ignored content").unwrap();
            }
        }));
    assert!(
        repo.execute_with_control(
            Operation::LfsMigrate(plan),
            NetworkControl::new(Some(sink), Default::default())
        )
        .is_err()
    );
    assert_eq!(
        fs::read(repo.root.join(".gitattributes")).unwrap(),
        b"concurrent ignored content"
    );
    assert_eq!(repo.resolve_revision("HEAD").unwrap().id, old);
    let status = repo.lfs_migration_status().unwrap().unwrap();
    assert!(!status.can_continue);
    repo.execute(Operation::AbortLfsMigration {
        expected: status.expected,
    })
    .unwrap();
    assert!(!repo.root.join(".gitattributes").exists());
}

#[test]
fn abort_after_publication_restores_old_tip_and_leaves_unrelated_untracked_files() {
    let (_dir, repo) = setup();
    fs::write(repo.root.join("asset.bin"), b"original\0bytes").unwrap();
    let old = commit(&repo, "original");
    let new = interrupt(&repo, "LFS migration: branch published");
    assert_eq!(repo.resolve_revision("HEAD").unwrap().id, new);
    fs::write(repo.root.join("unrelated"), "leave this alone").unwrap();
    let status = repo.lfs_migration_status().unwrap().unwrap();
    repo.execute(Operation::AbortLfsMigration {
        expected: status.expected,
    })
    .unwrap();
    assert_eq!(repo.resolve_revision("HEAD").unwrap().id, old);
    assert_eq!(
        fs::read(repo.root.join("asset.bin")).unwrap(),
        b"original\0bytes"
    );
    assert_eq!(
        fs::read(repo.root.join("unrelated")).unwrap(),
        b"leave this alone"
    );
}

#[test]
fn common_migration_journal_also_blocks_linked_worktree_writers() {
    let (dir, repo) = setup();
    fs::write(repo.root.join("asset.bin"), "content").unwrap();
    commit(&repo, "original");
    let raw = git2::Repository::open(&repo.root).unwrap();
    let linked = dir.path().join("linked");
    raw.worktree("linked", &linked, None).unwrap();
    let linked = Repository::open(linked).unwrap();
    fs::write(
        raw.commondir().join("gitbuddy-lfs-migration.json"),
        "invalid journal",
    )
    .unwrap();
    assert!(linked.execute(Operation::StageAll).is_err());
    assert!(linked.snapshot(10).unwrap().lfs_migration);
    assert!(linked.lfs_migration_status().is_err());
}

#[test]
fn corrupt_existing_shared_cache_object_is_never_replaced_or_checked_out() {
    use sha2::{Digest, Sha256};
    let (dir, repo) = setup();
    fs::write(repo.root.join("asset.bin"), "payload").unwrap();
    let original = commit(&repo, "original");
    let raw = git2::Repository::open(&repo.root).unwrap();
    let storage = dir.path().join("cache");
    raw.config()
        .unwrap()
        .set_str("lfs.storage", storage.to_str().unwrap())
        .unwrap();
    let plan = prepare(&repo);
    let oid = format!("{:x}", Sha256::digest(b"payload"));
    let object = storage
        .join("objects")
        .join(&oid[..2])
        .join(&oid[2..4])
        .join(&oid);
    fs::create_dir_all(object.parent().unwrap()).unwrap();
    fs::write(&object, "corrupt but retained").unwrap();
    assert!(repo.execute(Operation::LfsMigrate(Arc::new(plan))).is_err());
    assert_eq!(fs::read(object).unwrap(), b"corrupt but retained");
    assert_eq!(repo.resolve_revision("HEAD").unwrap().id, original);
    assert_eq!(fs::read(repo.root.join("asset.bin")).unwrap(), b"payload");
    assert!(repo.lfs_migration_status().unwrap().is_none());
}

#[test]
fn abort_refuses_a_later_edit_before_replacing_its_destination_and_can_retry() {
    let (_dir, repo) = setup();
    fs::write(repo.root.join("a-first"), "original first").unwrap();
    fs::write(repo.root.join("z.bin"), "original last").unwrap();
    let original = commit(&repo, "original");
    interrupt(&repo, "LFS migration: checkout pending");
    fs::write(repo.root.join("z.bin"), "edit preserved before abort").unwrap();
    let status = repo.lfs_migration_status().unwrap().unwrap();
    let later = repo.root.join("z.bin");
    let sink: gitbuddy::git::ProgressSink =
        Arc::new(std::sync::Mutex::new(move |message: &str| {
            if message == "LFS migration: restored a-first" {
                fs::write(&later, "new edit after abort snapshot").unwrap();
            }
        }));
    let result = repo.execute_with_control(
        Operation::AbortLfsMigration {
            expected: status.expected,
        },
        NetworkControl::new(Some(sink), Default::default()),
    );
    assert!(
        result.is_err(),
        "Abort must refuse a destination changed since preservation"
    );
    assert_eq!(
        fs::read(repo.root.join("z.bin")).unwrap(),
        b"new edit after abort snapshot"
    );
    assert_eq!(repo.resolve_revision("HEAD").unwrap().id, original);
    let status = Repository::open(&repo.root)
        .unwrap()
        .lfs_migration_status()
        .unwrap()
        .unwrap();
    assert_eq!(status.phase, "Aborting");
    repo.execute(Operation::AbortLfsMigration {
        expected: status.expected,
    })
    .unwrap();
    assert_eq!(fs::read(repo.root.join("z.bin")).unwrap(), b"original last");
    let snapshots: Vec<_> = fs::read_dir(status.recovery)
        .unwrap()
        .map(|e| e.unwrap().path().join("worktree/z.bin"))
        .filter(|p| p.is_file())
        .map(|p| fs::read(p).unwrap())
        .collect();
    assert!(
        snapshots
            .iter()
            .any(|bytes| bytes == b"edit preserved before abort")
    );
    assert!(
        snapshots
            .iter()
            .any(|bytes| bytes == b"new edit after abort snapshot")
    );
}

#[test]
fn abort_holds_a_real_index_lock_through_preservation_and_restoration() {
    let (_dir, repo) = setup();
    fs::write(repo.root.join("a-first"), "first").unwrap();
    fs::write(repo.root.join("z.bin"), "last").unwrap();
    commit(&repo, "original");
    interrupt(&repo, "LFS migration: checkout pending");
    let status = repo.lfs_migration_status().unwrap().unwrap();
    let index_lock = git2::Repository::open(&repo.root)
        .unwrap()
        .path()
        .join("index.lock");
    let blocked = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let observed = blocked.clone();
    let sink: gitbuddy::git::ProgressSink =
        Arc::new(std::sync::Mutex::new(move |message: &str| {
            if message == "LFS migration: restored a-first" {
                match fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&index_lock)
                {
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                        observed.store(true, std::sync::atomic::Ordering::SeqCst);
                    }
                    Ok(file) => {
                        drop(file);
                        fs::remove_file(&index_lock).unwrap();
                    }
                    Err(e) => panic!("Unexpected competing index writer result: {e}"),
                }
            }
        }));
    repo.execute_with_control(
        Operation::AbortLfsMigration {
            expected: status.expected,
        },
        NetworkControl::new(Some(sink), Default::default()),
    )
    .unwrap();
    assert!(
        blocked.load(std::sync::atomic::Ordering::SeqCst),
        "A cooperating Git writer acquired index.lock during Abort"
    );
}

#[test]
fn abort_does_not_delete_an_attribute_file_edited_after_its_snapshot() {
    let (_dir, repo) = setup();
    fs::write(repo.root.join("z.bin"), "original").unwrap();
    commit(&repo, "original");
    interrupt(&repo, "LFS migration: checkout pending");
    fs::write(repo.root.join(".gitattributes"), "saved attribute edit").unwrap();
    let status = repo.lfs_migration_status().unwrap().unwrap();
    let later = repo.root.join(".gitattributes");
    let sink: gitbuddy::git::ProgressSink =
        Arc::new(std::sync::Mutex::new(move |message: &str| {
            if message == "LFS migration: restoring .gitattributes" {
                fs::write(&later, "newer attribute edit").unwrap();
            }
        }));
    assert!(
        repo.execute_with_control(
            Operation::AbortLfsMigration {
                expected: status.expected
            },
            NetworkControl::new(Some(sink), Default::default())
        )
        .is_err()
    );
    assert_eq!(
        fs::read(repo.root.join(".gitattributes")).unwrap(),
        b"newer attribute edit"
    );
    assert_eq!(
        repo.lfs_migration_status().unwrap().unwrap().phase,
        "Aborting"
    );
}

#[cfg(unix)]
#[test]
fn abort_keeps_a_permission_change_made_after_the_preserved_snapshot() {
    use std::os::unix::fs::PermissionsExt;
    let (_dir, repo) = setup();
    fs::write(repo.root.join("a-first"), "first").unwrap();
    fs::write(repo.root.join("z.bin"), "last").unwrap();
    fs::set_permissions(repo.root.join("z.bin"), fs::Permissions::from_mode(0o644)).unwrap();
    commit(&repo, "original");
    interrupt(&repo, "LFS migration: checkout pending");
    let status = repo.lfs_migration_status().unwrap().unwrap();
    let later = repo.root.join("z.bin");
    let sink: gitbuddy::git::ProgressSink =
        Arc::new(std::sync::Mutex::new(move |message: &str| {
            if message == "LFS migration: restored a-first" {
                fs::set_permissions(&later, fs::Permissions::from_mode(0o600)).unwrap();
            }
        }));
    assert!(
        repo.execute_with_control(
            Operation::AbortLfsMigration {
                expected: status.expected
            },
            NetworkControl::new(Some(sink), Default::default())
        )
        .is_err()
    );
    assert_eq!(
        fs::metadata(repo.root.join("z.bin"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
}

#[test]
fn abort_keeps_an_index_changed_by_a_writer_that_ignores_git_locking() {
    let (_dir, repo) = setup();
    fs::write(repo.root.join("a-first"), "first").unwrap();
    fs::write(repo.root.join("z.bin"), "last").unwrap();
    commit(&repo, "original");
    interrupt(&repo, "LFS migration: checkout pending");
    let status = repo.lfs_migration_status().unwrap().unwrap();
    let index = git2::Repository::open(&repo.root)
        .unwrap()
        .path()
        .join("index");
    let original_index = fs::read(&index).unwrap();
    let mut newer = original_index.clone();
    newer.extend_from_slice(b"uncooperative writer update");
    let later = index.clone();
    let content = newer.clone();
    let sink: gitbuddy::git::ProgressSink =
        Arc::new(std::sync::Mutex::new(move |message: &str| {
            if message == "LFS migration: restored a-first" {
                fs::write(&later, &content).unwrap();
            }
        }));
    assert!(
        repo.execute_with_control(
            Operation::AbortLfsMigration {
                expected: status.expected
            },
            NetworkControl::new(Some(sink), Default::default())
        )
        .is_err()
    );
    assert_eq!(fs::read(&index).unwrap(), newer);
    let status = Repository::open(&repo.root)
        .unwrap()
        .lfs_migration_status()
        .unwrap()
        .unwrap();
    repo.execute(Operation::AbortLfsMigration {
        expected: status.expected,
    })
    .unwrap();
    assert_eq!(fs::read(index).unwrap(), original_index);
    assert!(
        fs::read_dir(status.recovery)
            .unwrap()
            .map(|e| e.unwrap().path().join("index"))
            .filter(|p| p.is_file())
            .any(|p| fs::read(p).unwrap() == newer)
    );
}

#[test]
fn abort_preserves_foreign_index_locks_and_reclaims_only_its_stale_session_marker() {
    let (_dir, repo) = setup();
    fs::write(repo.root.join("z.bin"), "original").unwrap();
    commit(&repo, "original");
    interrupt(&repo, "LFS migration: checkout pending");
    let status = repo.lfs_migration_status().unwrap().unwrap();
    let index_lock = git2::Repository::open(&repo.root)
        .unwrap()
        .path()
        .join("index.lock");
    fs::write(&index_lock, "unrelated Git index writer").unwrap();
    assert!(
        repo.execute(Operation::AbortLfsMigration {
            expected: status.expected.clone()
        })
        .is_err()
    );
    assert_eq!(
        fs::read(&index_lock).unwrap(),
        b"unrelated Git index writer"
    );
    fs::remove_file(&index_lock).unwrap();
    let marker = format!(
        "GitBuddy LFS Abort index reservation {}\n",
        status.recovery.file_name().unwrap().to_str().unwrap()
    );
    fs::write(&index_lock, marker).unwrap();
    let active = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&index_lock)
        .unwrap();
    active.try_lock().unwrap();
    assert!(
        repo.execute(Operation::AbortLfsMigration {
            expected: status.expected.clone()
        })
        .is_err()
    );
    active.unlock().unwrap();
    drop(active);
    Repository::open(&repo.root)
        .unwrap()
        .execute(Operation::AbortLfsMigration {
            expected: status.expected,
        })
        .unwrap();
    assert!(!index_lock.exists());
    assert!(repo.lfs_migration_status().unwrap().is_none());
}
