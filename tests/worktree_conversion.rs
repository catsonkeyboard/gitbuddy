use gitbuddy::git::{CheckoutHead, CheckoutMode, CheckoutOptions, NetworkControl, Repository};
use std::{
    fs,
    path::Path,
    sync::{Arc, Mutex},
};

fn encoded(text: &str, big: bool) -> Vec<u8> {
    text.encode_utf16()
        .flat_map(|unit| {
            if big {
                unit.to_be_bytes()
            } else {
                unit.to_le_bytes()
            }
        })
        .collect()
}
fn commit(
    raw: &git2::Repository,
    attributes: &[u8],
    text: &[u8],
    parent: Option<git2::Oid>,
) -> git2::Oid {
    let mut tree = raw.treebuilder(None).unwrap();
    tree.insert(".gitattributes", raw.blob(attributes).unwrap(), 0o100644)
        .unwrap();
    tree.insert("file.txt", raw.blob(text).unwrap(), 0o100644)
        .unwrap();
    tree.insert("ordinary", raw.blob(b"unconverted\n").unwrap(), 0o100644)
        .unwrap();
    let tree = raw.find_tree(tree.write().unwrap()).unwrap();
    let signature = git2::Signature::now("Checkout fixture", "checkout@example.invalid").unwrap();
    let parent = parent.map(|id| raw.find_commit(id).unwrap());
    raw.commit(
        None,
        &signature,
        &signature,
        "checkout fixture",
        &tree,
        &parent.iter().collect::<Vec<_>>(),
    )
    .unwrap()
}
struct Fixture {
    _dir: tempfile::TempDir,
    repo: Repository,
    old: git2::Oid,
    target: git2::Oid,
}
impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        let raw = git2::Repository::open(&repo.root).unwrap();
        raw.config()
            .unwrap()
            .set_str("core.attributesfile", "")
            .unwrap();
        raw.config()
            .unwrap()
            .set_bool("core.autocrlf", false)
            .unwrap();
        let old = commit(
            &raw,
            b"*.txt text eol=lf working-tree-encoding=UTF-16LE\n",
            b"old\n",
            None,
        );
        let target = commit(
            &raw,
            b"*.txt text eol=lf working-tree-encoding=UTF-16BE\n",
            b"new\n",
            Some(old),
        );
        raw.reference("refs/heads/main", old, true, "fixture")
            .unwrap();
        raw.set_head("refs/heads/main").unwrap();
        let mut index = raw.index().unwrap();
        index
            .read_tree(&raw.find_commit(old).unwrap().tree().unwrap())
            .unwrap();
        index.write().unwrap();
        fs::write(
            repo.root.join(".gitattributes"),
            "*.txt text eol=lf working-tree-encoding=UTF-16LE\n",
        )
        .unwrap();
        fs::write(repo.root.join("file.txt"), encoded("old\n", false)).unwrap();
        fs::write(repo.root.join("ordinary"), "unconverted\n").unwrap();
        Self {
            _dir: dir,
            repo,
            old,
            target,
        }
    }
    fn index(&self) -> Vec<u8> {
        let raw = git2::Repository::open(&self.repo.root).unwrap();
        fs::read(raw.path().join("index")).unwrap()
    }
}

#[test]
fn safe_checkout_changes_working_encoding_keeps_canonical_index_and_waits_for_ack() {
    let f = Fixture::new();
    let receipt = f
        .repo
        .checkout_worktree(
            &f.target.to_string(),
            CheckoutOptions::default(),
            NetworkControl::default(),
        )
        .unwrap();
    assert_eq!(
        fs::read(f.repo.root.join("file.txt")).unwrap(),
        encoded("new\n", true)
    );
    let raw = git2::Repository::open(&f.repo.root).unwrap();
    let index = raw.index().unwrap();
    assert_eq!(
        raw.find_blob(index.get_path(Path::new("file.txt"), 0).unwrap().id)
            .unwrap()
            .content(),
        b"new\n"
    );
    assert_eq!(raw.head().unwrap().target(), Some(f.old));
    assert_eq!(
        f.repo.worktree_recovery_status().unwrap().unwrap().phase,
        "Ready"
    );
    f.repo.acknowledge_worktree_checkout(&receipt).unwrap();
    assert!(f.repo.worktree_recovery_status().unwrap().is_none());
}

#[test]
fn dirty_encoded_source_or_unrepresentable_target_never_changes_worktree_or_index() {
    let f = Fixture::new();
    fs::write(f.repo.root.join("file.txt"), encoded("my edit\n", false)).unwrap();
    let index = f.index();
    let original = fs::read(f.repo.root.join("file.txt")).unwrap();
    assert!(
        f.repo
            .checkout_worktree(
                &f.target.to_string(),
                CheckoutOptions::default(),
                NetworkControl::default()
            )
            .is_err()
    );
    assert_eq!(f.index(), index);
    assert_eq!(fs::read(f.repo.root.join("file.txt")).unwrap(), original);
    assert!(f.repo.worktree_recovery_status().unwrap().is_none());
    fs::write(f.repo.root.join("file.txt"), encoded("old\n", false)).unwrap();
    let raw = git2::Repository::open(&f.repo.root).unwrap();
    let bad = commit(
        &raw,
        b"*.txt working-tree-encoding=ASCII\n",
        "不能编码\n".as_bytes(),
        Some(f.old),
    );
    assert!(
        f.repo
            .checkout_worktree(
                &bad.to_string(),
                CheckoutOptions::default(),
                NetworkControl::default()
            )
            .is_err()
    );
    assert_eq!(f.index(), index);
    assert_eq!(
        fs::read(f.repo.root.join("file.txt")).unwrap(),
        encoded("old\n", false)
    );
    assert!(f.repo.worktree_recovery_status().unwrap().is_none());
}

#[test]
fn declared_head_publication_survives_a_restart_between_checkout_and_ref_update() {
    let f = Fixture::new();
    let raw = git2::Repository::open(&f.repo.root).unwrap();
    raw.reference("refs/heads/next", f.target, false, "fixture")
        .unwrap();
    let options = CheckoutOptions {
        publication: Some(CheckoutHead {
            reference: Some("refs/heads/next".into()),
            id: f.target,
        }),
        ..Default::default()
    };
    let receipt = f
        .repo
        .checkout_worktree(&f.target.to_string(), options, NetworkControl::default())
        .unwrap();
    assert!(f.repo.acknowledge_worktree_checkout(&receipt).is_err());
    let reopened = Repository::open(&f.repo.root).unwrap();
    let status = reopened.worktree_recovery_status().unwrap().unwrap();
    reopened
        .continue_worktree_checkout(&status.expected)
        .unwrap();
    assert_eq!(raw.head().unwrap().name().unwrap(), "refs/heads/next");
    assert_eq!(raw.head().unwrap().target(), Some(f.target));
    assert!(reopened.worktree_recovery_status().unwrap().is_none());
}

#[test]
fn partial_native_checkout_requires_abort_and_raw_restore_does_not_parse_new_attributes() {
    let f = Fixture::new();
    let index = f.index();
    let sink: gitbuddy::git::ProgressSink = Arc::new(Mutex::new(|phase: &str| {
        assert_ne!(
            phase, "Worktree conversion: native pending",
            "simulated interruption"
        );
    }));
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f.repo.checkout_worktree(
            &f.target.to_string(),
            CheckoutOptions::default(),
            NetworkControl::new(Some(sink), Default::default())
        )))
        .is_err()
    );
    fs::write(
        f.repo.root.join(".gitattributes"),
        "*.txt working-tree-encoding=UNSUPPORTED-AFTER-CRASH\n",
    )
    .unwrap();
    fs::write(
        f.repo.root.join("file.txt"),
        b"new external bytes after interruption",
    )
    .unwrap();
    let status = f.repo.worktree_recovery_status().unwrap().unwrap();
    assert!(!status.can_continue);
    assert!(f.repo.continue_worktree_checkout(&status.expected).is_err());
    assert!(f.repo.abort_worktree_checkout(&status.expected).is_err());
    assert_eq!(
        fs::read(f.repo.root.join("file.txt")).unwrap(),
        b"new external bytes after interruption"
    );
    assert_eq!(
        fs::read(f.repo.root.join(".gitattributes")).unwrap(),
        b"*.txt working-tree-encoding=UNSUPPORTED-AFTER-CRASH\n"
    );
    assert_eq!(f.index(), index);
    fs::write(f.repo.root.join("file.txt"), b"old\n").unwrap();
    fs::write(
        f.repo.root.join(".gitattributes"),
        b"*.txt text eol=lf working-tree-encoding=UTF-16LE\n",
    )
    .unwrap();
    fs::write(
        f.repo.root.join(".git/info/attributes"),
        b"*.txt working-tree-encoding=UNSUPPORTED-AFTER-CRASH\n",
    )
    .unwrap();
    f.repo.abort_worktree_checkout(&status.expected).unwrap();
    assert_eq!(f.index(), index);
    assert_eq!(
        fs::read(f.repo.root.join("file.txt")).unwrap(),
        encoded("old\n", false)
    );
    assert!(
        fs::read_dir(status.recovery)
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.path())
            .any(|p| p
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("aborted-"))
    );
}

#[test]
fn external_edit_after_native_checkpoint_is_preserved_and_abort_can_retry() {
    let f = Fixture::new();
    let path = f.repo.root.join("file.txt");
    let sink: gitbuddy::git::ProgressSink = Arc::new(Mutex::new(move |phase: &str| {
        if phase == "Worktree conversion: native completed" {
            fs::write(&path, b"external edit").unwrap();
        }
    }));
    assert!(
        f.repo
            .checkout_worktree(
                &f.target.to_string(),
                CheckoutOptions::default(),
                NetworkControl::new(Some(sink), Default::default())
            )
            .is_err()
    );
    assert_eq!(
        fs::read(f.repo.root.join("file.txt")).unwrap(),
        b"external edit"
    );
    let status = f.repo.worktree_recovery_status().unwrap().unwrap();
    assert!(f.repo.continue_worktree_checkout(&status.expected).is_err());
    assert!(f.repo.abort_worktree_checkout(&status.expected).is_err());
    assert_eq!(
        fs::read(f.repo.root.join("file.txt")).unwrap(),
        b"external edit"
    );
    fs::write(f.repo.root.join("file.txt"), b"new\n").unwrap();
    f.repo.abort_worktree_checkout(&status.expected).unwrap();
    assert_eq!(
        fs::read(f.repo.root.join("file.txt")).unwrap(),
        encoded("old\n", false)
    );
}

#[test]
fn abort_checkout_refuses_to_overwrite_a_newer_index() {
    let f = Fixture::new();
    let _receipt = f
        .repo
        .checkout_worktree(
            &f.target.to_string(),
            CheckoutOptions::default(),
            NetworkControl::default(),
        )
        .unwrap();
    let raw = git2::Repository::open(&f.repo.root).unwrap();
    let mut index = raw.index().unwrap();
    let entry = index.get_path(Path::new("ordinary"), 0).unwrap();
    index
        .add_frombuffer(&entry, b"staged by another Git process")
        .unwrap();
    index.write().unwrap();
    let newer_index = f.index();
    let status = f.repo.worktree_recovery_status().unwrap().unwrap();
    assert!(f.repo.abort_worktree_checkout(&status.expected).is_err());
    assert_eq!(f.index(), newer_index);
    assert_eq!(
        fs::read(f.repo.root.join("file.txt")).unwrap(),
        encoded("new\n", true)
    );
    assert_eq!(raw.head().unwrap().target(), Some(f.old));
    assert!(f.repo.worktree_recovery_status().unwrap().is_some());
}

#[test]
fn edit_after_native_write_before_receipt_is_not_adopted_or_overwritten() {
    let f = Fixture::new();
    let path = f.repo.root.join("file.txt");
    let sink: gitbuddy::git::ProgressSink = Arc::new(Mutex::new(move |phase: &str| {
        if phase == "Worktree conversion: verifying native output" {
            fs::write(&path, b"external bytes before receipt").unwrap();
        }
    }));
    assert!(
        f.repo
            .checkout_worktree(
                &f.target.to_string(),
                CheckoutOptions::default(),
                NetworkControl::new(Some(sink), Default::default())
            )
            .is_err()
    );
    let status = f.repo.worktree_recovery_status().unwrap().unwrap();
    assert!(!status.can_continue);
    assert!(f.repo.abort_worktree_checkout(&status.expected).is_err());
    assert_eq!(
        fs::read(f.repo.root.join("file.txt")).unwrap(),
        b"external bytes before receipt"
    );
}

#[test]
fn encoded_clone_style_checkout_materializes_files_before_owner_publication() {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();
    let raw = git2::Repository::open(&repo.root).unwrap();
    raw.config()
        .unwrap()
        .set_str("core.attributesfile", "")
        .unwrap();
    let target = commit(
        &raw,
        b"*.txt working-tree-encoding=UTF-16BE\n",
        b"clone\n",
        None,
    );
    raw.reference("refs/heads/main", target, true, "fixture clone")
        .unwrap();
    raw.set_head("refs/heads/main").unwrap();
    let receipt = repo
        .checkout_worktree(
            &target.to_string(),
            CheckoutOptions {
                mode: CheckoutMode::Force,
                ..Default::default()
            },
            NetworkControl::default(),
        )
        .unwrap();
    assert_eq!(
        fs::read(repo.root.join("file.txt")).unwrap(),
        encoded("clone\n", true)
    );
    repo.acknowledge_worktree_checkout(&receipt).unwrap();
}

#[test]
fn source_configuration_changes_after_preparation_refuse_before_any_raw_write() {
    let f = Fixture::new();
    let index = f.index();
    let root = f.repo.root.clone();
    let sink: gitbuddy::git::ProgressSink = Arc::new(Mutex::new(move |phase: &str| {
        if phase == "Worktree conversion: prepared" {
            git2::Repository::open(&root)
                .unwrap()
                .config()
                .unwrap()
                .set_bool("core.autocrlf", true)
                .unwrap();
        }
    }));
    assert!(
        f.repo
            .checkout_worktree(
                &f.target.to_string(),
                CheckoutOptions::default(),
                NetworkControl::new(Some(sink), Default::default())
            )
            .is_err()
    );
    assert_eq!(f.index(), index);
    assert_eq!(
        fs::read(f.repo.root.join("file.txt")).unwrap(),
        encoded("old\n", false)
    );
    assert!(f.repo.worktree_recovery_status().unwrap().is_none());
}

#[test]
fn safe_checkout_keeps_unaffected_dirty_encoded_file() {
    let f = Fixture::new();
    let raw = git2::Repository::open(&f.repo.root).unwrap();
    let old = raw.find_commit(f.old).unwrap();
    let mut tree = raw.treebuilder(Some(&old.tree().unwrap())).unwrap();
    tree.insert(
        "ordinary",
        raw.blob(b"changed ordinary file\n").unwrap(),
        0o100644,
    )
    .unwrap();
    let signature = git2::Signature::now("Checkout fixture", "checkout@example.invalid").unwrap();
    let target = raw
        .commit(
            None,
            &signature,
            &signature,
            "other path only",
            &raw.find_tree(tree.write().unwrap()).unwrap(),
            &[&old],
        )
        .unwrap();
    let edited = encoded("keep my unrelated edit\n", false);
    fs::write(f.repo.root.join("file.txt"), &edited).unwrap();
    let receipt = f
        .repo
        .checkout_worktree(
            &target.to_string(),
            CheckoutOptions::default(),
            NetworkControl::default(),
        )
        .unwrap();
    assert_eq!(fs::read(f.repo.root.join("file.txt")).unwrap(), edited);
    assert_eq!(
        fs::read(f.repo.root.join("ordinary")).unwrap(),
        b"changed ordinary file\n"
    );
    assert_eq!(
        raw.find_blob(
            raw.index()
                .unwrap()
                .get_path(Path::new("file.txt"), 0)
                .unwrap()
                .id
        )
        .unwrap()
        .content(),
        b"old\n"
    );
    f.repo.acknowledge_worktree_checkout(&receipt).unwrap();
}

#[test]
fn force_checkout_preserves_invalid_raw_receipt_and_recovery_never_decodes_it() {
    let f = Fixture::new();
    fs::write(f.repo.root.join("file.txt"), [0xff]).unwrap();
    let before = f.index();
    let receipt = f
        .repo
        .checkout_worktree(
            &f.target.to_string(),
            CheckoutOptions {
                mode: CheckoutMode::Force,
                ..Default::default()
            },
            NetworkControl::default(),
        )
        .unwrap();
    assert_eq!(
        fs::read(f.repo.root.join("file.txt")).unwrap(),
        encoded("new\n", true)
    );
    let status = f.repo.worktree_recovery_status().unwrap().unwrap();
    assert!(
        fs::read_dir(&status.recovery)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().ends_with(".before"))
            .any(|e| fs::read(e.path()).unwrap() == [0xff])
    );
    f.repo.abort_worktree_checkout(&status.expected).unwrap();
    assert_eq!(fs::read(f.repo.root.join("file.txt")).unwrap(), [0xff]);
    assert_eq!(f.index(), before);
    // A stale receipt cannot acknowledge a later checkout session.
    assert!(f.repo.acknowledge_worktree_checkout(&receipt).is_err());
}

#[test]
fn force_checkout_initializes_already_canonical_temporary_clone_files() {
    let f = Fixture::new();
    let raw = git2::Repository::open(&f.repo.root).unwrap();
    let mut index = raw.index().unwrap();
    index
        .read_tree(&raw.find_commit(f.target).unwrap().tree().unwrap())
        .unwrap();
    index.write().unwrap();
    fs::write(
        f.repo.root.join(".gitattributes"),
        "*.txt text eol=lf working-tree-encoding=UTF-16BE\n",
    )
    .unwrap();
    fs::write(f.repo.root.join("file.txt"), b"new\n").unwrap();
    let receipt = f
        .repo
        .checkout_worktree(
            &f.target.to_string(),
            CheckoutOptions {
                mode: CheckoutMode::Force,
                ..Default::default()
            },
            NetworkControl::default(),
        )
        .unwrap();
    assert_eq!(
        fs::read(f.repo.root.join("file.txt")).unwrap(),
        encoded("new\n", true)
    );
    f.repo.acknowledge_worktree_checkout(&receipt).unwrap();
}

#[test]
fn linked_worktree_owns_recovery_and_the_common_journal_blocks_other_writers() {
    let f = Fixture::new();
    let directory = tempfile::tempdir().unwrap();
    let raw = git2::Repository::open(&f.repo.root).unwrap();
    let branch = raw
        .branch("linked", &raw.find_commit(f.old).unwrap(), false)
        .unwrap();
    let path = directory.path().join("linked");
    let mut options = git2::WorktreeAddOptions::new();
    options.reference(Some(branch.get()));
    raw.worktree("linked", &path, Some(&options)).unwrap();
    let linked = Repository::open(&path).unwrap();
    fs::write(linked.root.join("file.txt"), encoded("old\n", false)).unwrap();
    let receipt = linked
        .checkout_worktree(
            &f.target.to_string(),
            CheckoutOptions::default(),
            NetworkControl::default(),
        )
        .unwrap();
    let status = f.repo.worktree_recovery_status().unwrap().unwrap();
    assert_eq!(status.owner, linked.root.canonicalize().unwrap());
    assert!(f.repo.abort_worktree_checkout(&status.expected).is_err());
    assert!(f.repo.guard_worktree_write(None, &[]).is_err());
    assert_eq!(
        fs::read(f.repo.root.join("file.txt")).unwrap(),
        encoded("old\n", false)
    );
    linked.acknowledge_worktree_checkout(&receipt).unwrap();
    assert!(f.repo.worktree_recovery_status().unwrap().is_none());
}

#[test]
fn replacing_git_directory_during_preparation_refuses_before_worktree_changes() {
    let f = Fixture::new();
    let index = f.index();
    let root = f.repo.root.clone();
    let sink: gitbuddy::git::ProgressSink = Arc::new(Mutex::new(move |phase: &str| {
        if phase == "Worktree conversion: prepared" {
            fs::rename(root.join(".git"), root.join(".git.saved")).unwrap();
            git2::Repository::init(&root).unwrap();
        }
    }));
    assert!(
        f.repo
            .checkout_worktree(
                &f.target.to_string(),
                CheckoutOptions::default(),
                NetworkControl::new(Some(sink), Default::default())
            )
            .is_err()
    );
    assert_eq!(
        fs::read(f.repo.root.join("file.txt")).unwrap(),
        encoded("old\n", false)
    );
    assert_eq!(
        fs::read(f.repo.root.join(".git.saved/index")).unwrap(),
        index
    );
    assert!(f.repo.worktree_recovery_status().unwrap().is_none());
}

#[test]
fn source_info_override_controls_target_encoding_and_untracked_rules_fail_closed() {
    let f = Fixture::new();
    let raw = git2::Repository::open(&f.repo.root).unwrap();
    fs::create_dir_all(raw.commondir().join("info")).unwrap();
    fs::write(
        raw.commondir().join("info/attributes"),
        "*.txt -working-tree-encoding\n",
    )
    .unwrap();
    fs::write(f.repo.root.join("file.txt"), b"old\n").unwrap();
    let receipt = f
        .repo
        .checkout_worktree(
            &f.target.to_string(),
            CheckoutOptions::default(),
            NetworkControl::default(),
        )
        .unwrap();
    assert_eq!(fs::read(f.repo.root.join("file.txt")).unwrap(), b"new\n");
    f.repo.acknowledge_worktree_checkout(&receipt).unwrap();
    assert!(f.repo.worktree_recovery_status().unwrap().is_none());
}

#[test]
fn staged_and_unstaged_encoded_carry_is_preserved_when_target_only_changes_another_path() {
    let f = Fixture::new();
    let raw = git2::Repository::open(&f.repo.root).unwrap();
    let old = raw.find_commit(f.old).unwrap();
    let mut tree = raw.treebuilder(Some(&old.tree().unwrap())).unwrap();
    tree.insert(
        "ordinary",
        raw.blob(b"other branch change\n").unwrap(),
        0o100644,
    )
    .unwrap();
    let signature = git2::Signature::now("Checkout fixture", "checkout@example.invalid").unwrap();
    let target = raw
        .commit(
            None,
            &signature,
            &signature,
            "unrelated branch delta",
            &raw.find_tree(tree.write().unwrap()).unwrap(),
            &[&old],
        )
        .unwrap();
    let mut index = raw.index().unwrap();
    let entry = index.get_path(Path::new("file.txt"), 0).unwrap();
    index
        .add_frombuffer(&entry, b"staged local change\n")
        .unwrap();
    index.write().unwrap();
    let edited = encoded("unstaged beyond staged\n", false);
    fs::write(f.repo.root.join("file.txt"), &edited).unwrap();
    let receipt = f
        .repo
        .checkout_worktree(
            &target.to_string(),
            CheckoutOptions::default(),
            NetworkControl::default(),
        )
        .unwrap();
    assert_eq!(fs::read(f.repo.root.join("file.txt")).unwrap(), edited);
    assert_eq!(
        raw.find_blob(
            raw.index()
                .unwrap()
                .get_path(Path::new("file.txt"), 0)
                .unwrap()
                .id
        )
        .unwrap()
        .content(),
        b"staged local change\n"
    );
    assert_eq!(
        fs::read(f.repo.root.join("ordinary")).unwrap(),
        b"other branch change\n"
    );
    f.repo.acknowledge_worktree_checkout(&receipt).unwrap();
}

#[test]
fn unchanged_large_files_are_not_copied_and_successful_ack_cleans_only_this_session() {
    let f = Fixture::new();
    let raw = git2::Repository::open(&f.repo.root).unwrap();
    let bytes = vec![b'x'; 8 * 1024 * 1024];
    let blob = raw.blob(&bytes).unwrap();
    let with_large = |source: git2::Oid| {
        let original = raw.find_commit(source).unwrap();
        let mut tree = raw.treebuilder(Some(&original.tree().unwrap())).unwrap();
        tree.insert("unchanged-large.bin", blob, 0o100644).unwrap();
        raw.commit(
            None,
            &original.author(),
            &original.committer(),
            "large unchanged fixture",
            &raw.find_tree(tree.write().unwrap()).unwrap(),
            &[],
        )
        .unwrap()
    };
    let old = with_large(f.old);
    let target = with_large(f.target);
    raw.find_reference("refs/heads/main")
        .unwrap()
        .set_target(old, "fixture")
        .unwrap();
    let mut index = raw.index().unwrap();
    index
        .read_tree(&raw.find_commit(old).unwrap().tree().unwrap())
        .unwrap();
    index.write().unwrap();
    fs::write(f.repo.root.join("unchanged-large.bin"), &bytes).unwrap();
    raw.reference(
        "refs/gitbuddy/worktree-conversions/unrelated/original",
        old,
        false,
        "unrelated recovery",
    )
    .unwrap();
    let receipt = f
        .repo
        .checkout_worktree(
            &target.to_string(),
            CheckoutOptions::default(),
            NetworkControl::default(),
        )
        .unwrap();
    let status = f.repo.worktree_recovery_status().unwrap().unwrap();
    let copied: u64 = fs::read_dir(&status.recovery)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().ends_with(".before"))
        .map(|entry| entry.metadata().unwrap().len())
        .sum();
    assert!(
        copied < bytes.len() as u64,
        "unchanged large data was copied into checkout recovery"
    );
    let own_prefix = format!(
        "refs/gitbuddy/worktree-conversions/{}/*",
        status.recovery.file_name().unwrap().to_str().unwrap()
    );
    f.repo.acknowledge_worktree_checkout(&receipt).unwrap();
    assert!(
        !status.recovery.exists(),
        "successful checkout retained unbounded raw recovery data"
    );
    assert_eq!(raw.references_glob(&own_prefix).unwrap().count(), 0);
    assert_eq!(
        raw.refname_to_id("refs/gitbuddy/worktree-conversions/unrelated/original")
            .unwrap(),
        old
    );
}

#[test]
fn public_checkout_entry_refuses_other_repository_operations() {
    for marker in [
        "MERGE_HEAD",
        "gitbuddy-rebase.json",
        "gitbuddy-lfs-migration.json",
    ] {
        let f = Fixture::new();
        let raw = git2::Repository::open(&f.repo.root).unwrap();
        fs::write(
            raw.path().join(marker),
            if marker == "MERGE_HEAD" {
                f.old.to_string()
            } else {
                "unfinished".into()
            },
        )
        .unwrap();
        let before = f.index();
        assert!(
            f.repo
                .checkout_worktree(
                    &f.target.to_string(),
                    CheckoutOptions::default(),
                    NetworkControl::default()
                )
                .is_err(),
            "public checkout bypassed {marker}"
        );
        assert_eq!(f.index(), before);
        assert_eq!(
            fs::read(f.repo.root.join("file.txt")).unwrap(),
            encoded("old\n", false)
        );
        assert!(f.repo.worktree_recovery_status().unwrap().is_none());
    }
}

#[test]
fn successful_ack_releases_only_its_own_raw_receipts_and_private_refs() {
    let f = Fixture::new();
    let raw = git2::Repository::open(&f.repo.root).unwrap();
    raw.reference(
        "refs/gitbuddy/worktree-conversions/unrelated/original",
        f.old,
        false,
        "unrelated recovery",
    )
    .unwrap();
    let receipt = f
        .repo
        .checkout_worktree(
            &f.target.to_string(),
            CheckoutOptions::default(),
            NetworkControl::default(),
        )
        .unwrap();
    let status = f.repo.worktree_recovery_status().unwrap().unwrap();
    let prefix = format!(
        "refs/gitbuddy/worktree-conversions/{}/*",
        status.recovery.file_name().unwrap().to_str().unwrap()
    );
    assert!(raw.references_glob(&prefix).unwrap().count() >= 2);
    f.repo.acknowledge_worktree_checkout(&receipt).unwrap();
    assert!(
        !status.recovery.exists(),
        "successful ack leaked raw receipts"
    );
    assert_eq!(raw.references_glob(&prefix).unwrap().count(), 0);
    assert_eq!(
        raw.refname_to_id("refs/gitbuddy/worktree-conversions/unrelated/original")
            .unwrap(),
        f.old
    );
}

#[cfg(unix)]
#[test]
fn native_checkout_does_not_truncate_external_hardlink_aliases() {
    for (mode, utf8) in [(CheckoutMode::Force, false), (CheckoutMode::Safe, true)] {
        let f = Fixture::new();
        let raw = git2::Repository::open(&f.repo.root).unwrap();
        raw.config()
            .unwrap()
            .set_bool("core.ignorecase", false)
            .unwrap();
        let target = if utf8 {
            let old = commit(&raw, b"*.txt working-tree-encoding=UTF-8\n", b"old\n", None);
            let target = commit(
                &raw,
                b"*.txt working-tree-encoding=UTF-8\n",
                b"new\n",
                Some(old),
            );
            raw.find_reference("refs/heads/main")
                .unwrap()
                .set_target(old, "UTF8 fixture")
                .unwrap();
            let mut index = raw.index().unwrap();
            index
                .read_tree(&raw.find_commit(old).unwrap().tree().unwrap())
                .unwrap();
            index.write().unwrap();
            fs::write(
                f.repo.root.join(".gitattributes"),
                "*.txt working-tree-encoding=UTF-8\n",
            )
            .unwrap();
            fs::write(f.repo.root.join("file.txt"), b"old\n").unwrap();
            target
        } else {
            f.target
        };
        let external = tempfile::tempdir().unwrap();
        let alias = external.path().join("outside.txt");
        fs::hard_link(f.repo.root.join("file.txt"), &alias).unwrap();
        let before = fs::read(&alias).unwrap();
        let receipt = f
            .repo
            .checkout_worktree(
                &target.to_string(),
                CheckoutOptions {
                    mode,
                    ..Default::default()
                },
                NetworkControl::default(),
            )
            .unwrap();
        assert_eq!(
            fs::read(&alias).unwrap(),
            before,
            "native checkout truncated an external hardlink alias"
        );
        assert_eq!(
            fs::read(f.repo.root.join("file.txt")).unwrap(),
            if utf8 {
                b"new\n".to_vec()
            } else {
                encoded("new\n", true)
            }
        );
        f.repo.acknowledge_worktree_checkout(&receipt).unwrap();
    }
}

#[test]
fn native_checkout_preserves_other_git_processes_index_reservation() {
    let f = Fixture::new();
    let index_path = f.repo.root.join(".git/index.lock");
    let expected = f.index();
    let sink: gitbuddy::git::ProgressSink = Arc::new(Mutex::new(move |phase: &str| {
        if phase == "Worktree conversion: native pending" {
            assert!(
                fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&index_path)
                    .is_err()
            );
        }
    }));
    let receipt = f
        .repo
        .checkout_worktree(
            &f.target.to_string(),
            CheckoutOptions {
                update_index: false,
                ..Default::default()
            },
            NetworkControl::new(Some(sink), Default::default()),
        )
        .unwrap();
    assert_eq!(f.index(), expected);
    assert_eq!(
        fs::read(f.repo.root.join("file.txt")).unwrap(),
        encoded("new\n", true)
    );
    f.repo.acknowledge_worktree_checkout(&receipt).unwrap();
    assert!(!f.repo.root.join(".git/index.lock").exists());
}

fn with_lfs(
    raw: &git2::Repository,
    base: git2::Oid,
    payload: &[u8],
    keep_encoding: bool,
) -> (git2::Oid, String) {
    use sha2::{Digest, Sha256};
    let id = format!("{:x}", Sha256::digest(payload));
    let object = raw
        .commondir()
        .join("lfs/objects")
        .join(&id[..2])
        .join(&id[2..4])
        .join(&id);
    fs::create_dir_all(object.parent().unwrap()).unwrap();
    fs::write(object, payload).unwrap();
    let pointer = format!(
        "version https://git-lfs.github.com/spec/v1\noid sha256:{id}\nsize {}\n",
        payload.len()
    );
    let base = raw.find_commit(base).unwrap();
    let tree = base.tree().unwrap();
    let mut attrs = if keep_encoding {
        raw.find_blob(tree.get_name(".gitattributes").unwrap().id())
            .unwrap()
            .content()
            .to_vec()
    } else {
        vec![]
    };
    attrs.extend_from_slice(b"asset.bin filter=lfs diff=lfs merge=lfs -text\n");
    let mut builder = raw.treebuilder(Some(&tree)).unwrap();
    builder
        .insert(".gitattributes", raw.blob(&attrs).unwrap(), 0o100644)
        .unwrap();
    builder
        .insert("asset.bin", raw.blob(pointer.as_bytes()).unwrap(), 0o100644)
        .unwrap();
    let tree = raw.find_tree(builder.write().unwrap()).unwrap();
    let commit = raw
        .commit(
            None,
            &base.author(),
            &base.committer(),
            "LFS fixture",
            &tree,
            &[&base],
        )
        .unwrap();
    (commit, pointer)
}

fn install_lfs_base(f: &Fixture, raw: &git2::Repository, id: git2::Oid, bytes: &[u8]) {
    let tree = raw.find_commit(id).unwrap().tree().unwrap();
    let mut index = raw.index().unwrap();
    index.read_tree(&tree).unwrap();
    index.write().unwrap();
    raw.reference("refs/heads/main", id, true, "LFS fixture")
        .unwrap();
    fs::write(
        f.repo.root.join(".gitattributes"),
        raw.find_blob(tree.get_name(".gitattributes").unwrap().id())
            .unwrap()
            .content(),
    )
    .unwrap();
    fs::write(f.repo.root.join("asset.bin"), bytes).unwrap();
}

#[test]
fn encoded_checkout_leaves_unaffected_lfs_pointer_unhydrated() {
    let f = Fixture::new();
    let raw = git2::Repository::open(&f.repo.root).unwrap();
    let (old, pointer) = with_lfs(&raw, f.old, b"old LFS payload\0", true);
    let (target, _) = with_lfs(&raw, f.target, b"old LFS payload\0", true);
    install_lfs_base(&f, &raw, old, pointer.as_bytes());

    let receipt = f
        .repo
        .checkout_worktree(
            &target.to_string(),
            CheckoutOptions::default(),
            NetworkControl::default(),
        )
        .unwrap();
    f.repo.acknowledge_worktree_checkout(&receipt).unwrap();
    assert_eq!(
        fs::read(f.repo.root.join("asset.bin")).unwrap(),
        pointer.as_bytes()
    );
    assert_eq!(
        fs::read(f.repo.root.join("file.txt")).unwrap(),
        encoded("new\n", true)
    );
}

#[test]
fn encoded_checkout_pins_large_lfs_payload_for_recovery_without_the_cache() {
    use sha2::{Digest, Sha256};
    let f = Fixture::new();
    let raw = git2::Repository::open(&f.repo.root).unwrap();
    let payload = vec![b'X'; 17 * 1024 * 1024];
    let (old, _) = with_lfs(&raw, f.old, b"old raw LFS payload\0", true);
    let (target, pointer) = with_lfs(&raw, f.target, &payload, true);
    install_lfs_base(&f, &raw, old, b"old raw LFS payload\0");
    let id = format!("{:x}", Sha256::digest(&payload));
    let object = raw
        .commondir()
        .join("lfs/objects")
        .join(&id[..2])
        .join(&id[2..4])
        .join(id);
    let cancel = gitbuddy::git::CancellationToken::default();
    let callback_cancel = cancel.clone();
    let sink: gitbuddy::git::ProgressSink = Arc::new(Mutex::new(move |phase: &str| {
        if phase == "Worktree conversion: native completed" {
            fs::remove_file(&object).unwrap();
            callback_cancel.cancel();
        }
    }));
    let result = f.repo.checkout_worktree(
        &target.to_string(),
        CheckoutOptions::default(),
        NetworkControl::new(Some(sink), cancel),
    );
    assert!(result.is_err());
    let status = f
        .repo
        .worktree_recovery_status()
        .unwrap()
        .expect("prepared payload and native state must be retained");
    assert!(status.can_continue);
    f.repo.continue_worktree_checkout(&status.expected).unwrap();
    assert_eq!(fs::read(f.repo.root.join("asset.bin")).unwrap(), payload);
    assert_eq!(
        fs::read(f.repo.root.join("file.txt")).unwrap(),
        encoded("new\n", true)
    );
    let index = git2::Repository::open(&f.repo.root)
        .unwrap()
        .index()
        .unwrap();
    let oid = index.get_path(Path::new("asset.bin"), 0).unwrap().id;
    assert_eq!(raw.find_blob(oid).unwrap().content(), pointer.as_bytes());
}

#[test]
fn plain_checkout_preserves_lfs_dehydration_and_local_hydration() {
    let f = Fixture::new();
    let raw = git2::Repository::open(&f.repo.root).unwrap();
    let (old, _) = with_lfs(&raw, f.old, b"old payload\0", false);
    let (target, _) = with_lfs(&raw, f.target, b"new payload\0", false);
    install_lfs_base(&f, &raw, old, b"old payload\0");
    fs::write(f.repo.root.join("file.txt"), b"old\n").unwrap();
    let receipt = f
        .repo
        .checkout_worktree(
            &target.to_string(),
            CheckoutOptions::default(),
            NetworkControl::default(),
        )
        .unwrap();
    f.repo.acknowledge_worktree_checkout(&receipt).unwrap();
    assert_eq!(
        fs::read(f.repo.root.join("asset.bin")).unwrap(),
        b"new payload\0"
    );
    assert_eq!(fs::read(f.repo.root.join("file.txt")).unwrap(), b"new\n");
    assert!(f.repo.worktree_recovery_status().unwrap().is_none());
}

#[test]
fn plain_checkout_can_publish_head_before_reporting_corrupt_target_lfs() {
    use sha2::{Digest, Sha256};
    let f = Fixture::new();
    let raw = git2::Repository::open(&f.repo.root).unwrap();
    let old_payload = b"old payload\0";
    let new_payload = b"new payload\0";
    let (old, _) = with_lfs(&raw, f.old, old_payload, false);
    let (target, pointer) = with_lfs(&raw, f.target, new_payload, false);
    let id = format!("{:x}", Sha256::digest(new_payload));
    fs::write(
        raw.commondir()
            .join("lfs/objects")
            .join(&id[..2])
            .join(&id[2..4])
            .join(&id),
        b"bad payload\0",
    )
    .unwrap();
    install_lfs_base(&f, &raw, old, old_payload);
    fs::write(f.repo.root.join("file.txt"), b"old\n").unwrap();
    let receipt = f
        .repo
        .checkout_worktree(
            &target.to_string(),
            CheckoutOptions {
                publication: Some(CheckoutHead {
                    reference: Some("refs/heads/main".into()),
                    id: target,
                }),
                ..Default::default()
            },
            NetworkControl::default(),
        )
        .unwrap();
    assert_eq!(raw.head().unwrap().target(), Some(old));
    raw.find_reference("refs/heads/main")
        .unwrap()
        .set_target(target, "publish test checkout")
        .unwrap();
    let error = f.repo.acknowledge_worktree_checkout(&receipt).unwrap_err();
    assert!(format!("{error:#}").contains("Checkout completed"));
    assert_eq!(raw.head().unwrap().target(), Some(target));
    assert_eq!(
        fs::read(f.repo.root.join("asset.bin")).unwrap(),
        pointer.as_bytes()
    );
    assert_eq!(fs::read(f.repo.root.join("file.txt")).unwrap(), b"new\n");
    assert!(f.repo.worktree_recovery_status().unwrap().is_none());
}

#[test]
fn checkout_preserves_local_attributes_that_would_disagree_with_materialized_raw_bytes() {
    let f = Fixture::new();
    let raw = git2::Repository::open(&f.repo.root).unwrap();
    let same_rules = commit(
        &raw,
        b"*.txt text eol=lf working-tree-encoding=UTF-16LE\n",
        b"new\n",
        Some(f.old),
    );
    let local_rules = b"*.txt text eol=lf working-tree-encoding=UTF-16BE\n";
    let local_raw = encoded("old\n", true);
    fs::write(f.repo.root.join(".gitattributes"), local_rules).unwrap();
    fs::write(f.repo.root.join("file.txt"), &local_raw).unwrap();
    let index = f.index();
    assert!(
        f.repo
            .checkout_worktree(
                &same_rules.to_string(),
                CheckoutOptions::default(),
                NetworkControl::default()
            )
            .is_err()
    );
    assert_eq!(f.index(), index);
    assert_eq!(
        fs::read(f.repo.root.join(".gitattributes")).unwrap(),
        local_rules
    );
    assert_eq!(fs::read(f.repo.root.join("file.txt")).unwrap(), local_raw);
    assert!(f.repo.worktree_recovery_status().unwrap().is_none());
}

#[cfg(unix)]
#[test]
fn encoded_force_checkout_never_follows_an_external_symlink() {
    let f = Fixture::new();
    let raw = git2::Repository::open(&f.repo.root).unwrap();
    raw.config()
        .unwrap()
        .set_bool("core.ignorecase", false)
        .unwrap();
    let external = tempfile::tempdir().unwrap();
    let destination = external.path().join("untouched");
    fs::write(&destination, b"external secret").unwrap();
    fs::remove_file(f.repo.root.join("file.txt")).unwrap();
    std::os::unix::fs::symlink(&destination, f.repo.root.join("file.txt")).unwrap();
    let receipt = f
        .repo
        .checkout_worktree(
            &f.target.to_string(),
            CheckoutOptions {
                mode: CheckoutMode::Force,
                ..Default::default()
            },
            NetworkControl::default(),
        )
        .unwrap();
    assert_eq!(fs::read(&destination).unwrap(), b"external secret");
    assert_eq!(
        fs::read(f.repo.root.join("file.txt")).unwrap(),
        encoded("new\n", true)
    );
    f.repo.acknowledge_worktree_checkout(&receipt).unwrap();
}

#[test]
fn initialize_linked_checkout_accepts_native_eol_but_refuses_an_encoded_edit() {
    let f = Fixture::new();
    let raw = git2::Repository::open(&f.repo.root).unwrap();
    let encoded_target = commit(
        &raw,
        b"*.txt text eol=crlf working-tree-encoding=UTF-16BE\n",
        b"linked\n",
        Some(f.old),
    );
    let branch = raw
        .branch(
            "linked-eol",
            &raw.find_commit(encoded_target).unwrap(),
            false,
        )
        .unwrap();
    let destination = tempfile::tempdir().unwrap();
    let path = destination.path().join("linked-eol");
    let mut options = git2::WorktreeAddOptions::new();
    options.reference(Some(branch.get()));
    raw.worktree("linked-eol", &path, Some(&options)).unwrap();
    let linked = Repository::open(&path).unwrap();
    let working = linked.root.join("file.txt");
    let native = fs::read(&working).unwrap();
    assert_eq!(
        native, b"linked\r\n",
        "fixture should exercise builtin smudge"
    );
    fs::write(linked.root.join("ordinary"), b"unrelated edit").unwrap();
    fs::write(&working, b"user edit\r\n").unwrap();
    let initialize = CheckoutOptions {
        mode: CheckoutMode::Initialize,
        ..Default::default()
    };
    assert!(
        linked
            .checkout_worktree(
                &encoded_target.to_string(),
                initialize.clone(),
                NetworkControl::default()
            )
            .is_err()
    );
    assert_eq!(fs::read(&working).unwrap(), b"user edit\r\n");
    assert!(linked.worktree_recovery_status().unwrap().is_none());
    fs::write(&working, native).unwrap();
    let receipt = linked
        .checkout_worktree(
            &encoded_target.to_string(),
            initialize,
            NetworkControl::default(),
        )
        .unwrap();
    linked.acknowledge_worktree_checkout(&receipt).unwrap();
    assert_eq!(fs::read(&working).unwrap(), encoded("linked\r\n", true));
    assert_eq!(
        fs::read(linked.root.join("ordinary")).unwrap(),
        b"unrelated edit"
    );
    assert_eq!(
        git2::Repository::open(&path)
            .unwrap()
            .head()
            .unwrap()
            .target(),
        Some(encoded_target)
    );
}

#[test]
fn checkout_refuses_an_affected_configured_filter_but_carries_an_unaffected_path() {
    let f = Fixture::new();
    let raw = git2::Repository::open(&f.repo.root).unwrap();
    let config_path = raw.path().join("info/attributes");
    fs::create_dir_all(config_path.parent().unwrap()).unwrap();
    fs::write(config_path, b"ordinary filter=checkout-external\n").unwrap();
    raw.config()
        .unwrap()
        .set_str(
            "filter.checkout-external.smudge",
            "unavailable-checkout-filter",
        )
        .unwrap();
    let target = raw.find_commit(f.target).unwrap();
    let mut builder = raw.treebuilder(Some(&target.tree().unwrap())).unwrap();
    builder
        .insert("ordinary", raw.blob(b"new ordinary\n").unwrap(), 0o100644)
        .unwrap();
    let tree = raw.find_tree(builder.write().unwrap()).unwrap();
    let changed = raw
        .commit(
            None,
            &target.author(),
            &target.committer(),
            "affect configured filter",
            &tree,
            &[&target],
        )
        .unwrap();
    let index = f.index();
    assert!(
        f.repo
            .checkout_worktree(
                &changed.to_string(),
                CheckoutOptions::default(),
                NetworkControl::default()
            )
            .is_err()
    );
    assert_eq!(f.index(), index);
    assert_eq!(
        fs::read(f.repo.root.join("file.txt")).unwrap(),
        encoded("old\n", false)
    );
    assert_eq!(
        fs::read(f.repo.root.join("ordinary")).unwrap(),
        b"unconverted\n"
    );
    assert!(f.repo.worktree_recovery_status().unwrap().is_none());
    let receipt = f
        .repo
        .checkout_worktree(
            &f.target.to_string(),
            CheckoutOptions::default(),
            NetworkControl::default(),
        )
        .unwrap();
    f.repo.acknowledge_worktree_checkout(&receipt).unwrap();
    assert_eq!(
        fs::read(f.repo.root.join("ordinary")).unwrap(),
        b"unconverted\n"
    );
}
