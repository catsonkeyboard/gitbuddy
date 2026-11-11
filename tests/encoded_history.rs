use gitbuddy::git::{
    CancellationToken, MergeMode, NetworkControl, Operation, ProgressSink, PullStrategy, Repository,
};
use std::{
    fs,
    panic::{AssertUnwindSafe, catch_unwind},
    path::Path,
    process::Command,
    sync::{Arc, Mutex},
};

const BASE: &str = "base 中文\n";
const TARGET: &str = "target 日本語 😀\n";

fn utf16(text: &str, little: bool) -> Vec<u8> {
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
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", root.join(".git/empty-config"))
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "Git fixture {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

struct Fixture {
    _directory: tempfile::TempDir,
    repo: Repository,
}

impl Fixture {
    fn new(encoded: bool) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        git(root, &["init", "-q", "-b", "main"]);
        for (key, value) in [
            ("user.name", "Encoding fixture"),
            ("user.email", "encoding@example.invalid"),
            ("commit.gpgsign", "false"),
            ("core.hooksPath", "/dev/null"),
            ("core.attributesfile", ""),
            ("core.autocrlf", "false"),
            ("core.ignorecase", "false"),
        ] {
            git(root, &["config", key, value]);
        }
        if encoded {
            fs::write(
                root.join(".gitattributes"),
                "file.txt text working-tree-encoding=UTF-16LE eol=lf\n",
            )
            .unwrap();
            fs::write(root.join("file.txt"), utf16(BASE, true)).unwrap();
        } else {
            fs::write(root.join("file.txt"), BASE).unwrap();
        }
        git(root, &["add", "-A"]);
        git(root, &["commit", "-qm", "base"]);
        let repo = Repository::open(root).unwrap();
        Self {
            _directory: directory,
            repo,
        }
    }

    fn git(&self, args: &[&str]) -> String {
        git(&self.repo.root, args)
    }

    fn write_encoded_target(&self) {
        fs::write(
            self.repo.root.join(".gitattributes"),
            "file.txt text working-tree-encoding=UTF-16BE eol=lf\n",
        )
        .unwrap();
        fs::write(self.repo.root.join("file.txt"), utf16(TARGET, false)).unwrap();
        self.git(&["add", "-A"]);
        self.git(&["commit", "-qm", "encode target"]);
    }

    fn topic(&self) -> String {
        self.git(&["checkout", "-qb", "topic"]);
        self.write_encoded_target();
        let target = self.git(&["rev-parse", "HEAD"]).trim().to_owned();
        self.git(&["checkout", "-q", "main"]);
        target
    }

    fn merge(&self, mode: MergeMode) -> anyhow::Result<String> {
        self.repo.execute(Operation::MergeWith {
            context: Arc::new(self.repo.prepare_merge("topic")?),
            mode,
        })
    }

    fn pull(&self, strategy: PullStrategy) -> anyhow::Result<String> {
        self.repo.execute(Operation::PullWith {
            context: Arc::new(self.repo.prepare_pull()?),
            strategy,
        })
    }

    fn assert_encoded_result(&self, target: &str) {
        assert_eq!(self.git(&["rev-parse", "HEAD"]).trim(), target);
        assert_eq!(
            fs::read(self.repo.root.join("file.txt")).unwrap(),
            utf16(TARGET, false)
        );
        assert_eq!(self.git(&["show", ":file.txt"]), TARGET);
        assert_eq!(self.git(&["status", "--porcelain"]), "");
        assert!(self.repo.worktree_recovery_status().unwrap().is_none());
    }

    fn evidence(&self) -> Evidence {
        Evidence {
            head: fs::read(self.repo.root.join(".git/HEAD")).unwrap(),
            refs: self.git(&["show-ref"]),
            original: fs::read(self.repo.root.join(".git/ORIG_HEAD")).ok(),
            index: fs::read(self.repo.root.join(".git/index")).unwrap(),
            worktree: fs::read(self.repo.root.join("file.txt")).unwrap(),
            attributes: fs::read(self.repo.root.join(".gitattributes")).ok(),
        }
    }

    fn assert_refusal_preserves_repository(
        &self,
        result: anyhow::Result<String>,
        before: Evidence,
    ) {
        let error = result.expect_err("unsafe history operation should be refused");
        assert!(
            format!("{error:#}").contains("working-tree-encoding"),
            "{error:#}"
        );
        assert_eq!(self.evidence(), before);
        for marker in [
            "MERGE_HEAD",
            "CHERRY_PICK_HEAD",
            "REVERT_HEAD",
            "SQUASH_MSG",
            "gitbuddy-rebase.json",
            "gitbuddy-squash.json",
        ] {
            assert!(
                !self.repo.root.join(".git").join(marker).exists(),
                "left {marker}"
            );
        }
        assert!(self.repo.worktree_recovery_status().unwrap().is_none());
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Evidence {
    head: Vec<u8>,
    refs: String,
    original: Option<Vec<u8>>,
    index: Vec<u8>,
    worktree: Vec<u8>,
    attributes: Option<Vec<u8>>,
}

#[test]
fn merge_fast_forward_materializes_target_encoding_and_canonical_index() {
    let f = Fixture::new(true);
    let target = f.topic();
    let original = f.git(&["rev-parse", "HEAD"]);
    f.merge(MergeMode::Auto).unwrap();
    f.assert_encoded_result(&target);
    assert_eq!(f.git(&["rev-parse", "ORIG_HEAD"]), original);
}

#[test]
fn all_pull_strategies_use_encoded_checkout_on_fast_forward() {
    for strategy in [
        PullStrategy::FastForwardOnly,
        PullStrategy::Merge,
        PullStrategy::Rebase,
    ] {
        let f = Fixture::new(false);
        let target = f.topic();
        f.git(&["config", "branch.main.remote", "."]);
        f.git(&["config", "branch.main.merge", "refs/heads/topic"]);
        f.pull(strategy).unwrap();
        f.assert_encoded_result(&target);
    }
}

#[test]
fn remote_pull_fast_forward_uses_attributes_from_the_fetched_target() {
    let f = Fixture::new(false);
    let target = f.topic();
    let remote = tempfile::tempdir().unwrap();
    git2::Repository::init_bare(remote.path()).unwrap();
    f.git(&["remote", "add", "origin", remote.path().to_str().unwrap()]);
    f.git(&["push", "-q", "origin", "refs/heads/topic:refs/heads/main"]);
    f.git(&["update-ref", "-d", "refs/remotes/origin/main"]);
    f.git(&["config", "branch.main.remote", "origin"]);
    f.git(&["config", "branch.main.merge", "refs/heads/main"]);

    f.pull(PullStrategy::FastForwardOnly).unwrap();
    f.assert_encoded_result(&target);
    assert_eq!(
        f.git(&["rev-parse", "refs/remotes/origin/main"]).trim(),
        target
    );
}

#[test]
fn no_fast_forward_merges_encoded_target_with_both_parents() {
    let f = Fixture::new(false);
    let target = f.topic();
    let original = f.git(&["rev-parse", "HEAD"]);
    f.merge(MergeMode::NoFastForward).unwrap();
    let merged = f.git(&["rev-parse", "HEAD"]);
    assert_ne!(merged.trim(), target);
    assert_eq!(f.git(&["rev-parse", "HEAD^1"]), original);
    assert_eq!(f.git(&["rev-parse", "HEAD^2"]).trim(), target);
    assert_eq!(f.git(&["rev-parse", "ORIG_HEAD"]), original);
    f.assert_encoded_result(merged.trim());
}

#[test]
fn divergent_merge_repairs_an_existing_planned_tree_with_a_missing_merged_blob() {
    let f = Fixture::new(true);
    let middle = "one\ntwo\nthree\nfour\nfive\nsix\nseven\n";
    let base = format!("first 中文\n{middle}last 中文\n");
    let ours = format!("first 中文\n{middle}last 변경\n");
    let theirs = format!("first 日本語\n{middle}last 中文\n");
    let result = format!("first 日本語\n{middle}last 변경\n");
    let path = f.repo.root.join("file.txt");
    fs::write(&path, utf16(&base, true)).unwrap();
    f.git(&["add", "file.txt"]);
    f.git(&["commit", "-q", "--amend", "--no-edit"]);
    f.git(&["checkout", "-qb", "topic"]);
    fs::write(
        f.repo.root.join(".gitattributes"),
        "file.txt text working-tree-encoding=UTF-16BE eol=lf\n",
    )
    .unwrap();
    fs::write(&path, utf16(&theirs, false)).unwrap();
    f.git(&["add", "-A"]);
    f.git(&["commit", "-qm", "edit first line and change encoding"]);
    let target = f.git(&["rev-parse", "HEAD"]);
    f.git(&["checkout", "-q", "main"]);
    fs::write(&path, utf16(&ours, true)).unwrap();
    f.git(&["add", "file.txt"]);
    f.git(&["commit", "-qm", "edit last line"]);
    let original = f.git(&["rev-parse", "HEAD"]);

    // Model an interrupted installation which persisted the exact result
    // tree before persisting its newly merged blob. Existing input commits
    // remain complete, so replaying the merge can regenerate that blob.
    let raw = git2::Repository::open(&f.repo.root).unwrap();
    let database = raw.odb().unwrap();
    let attributes = f.git(&["rev-parse", "topic:.gitattributes"]);
    let attributes: git2::Oid = attributes.trim().parse().unwrap();
    let merged = git2::Oid::hash_object(git2::ObjectType::Blob, result.as_bytes()).unwrap();
    assert!(!database.exists(merged));
    let mut tree_bytes = b"100644 .gitattributes\0".to_vec();
    tree_bytes.extend(attributes.as_bytes());
    tree_bytes.extend(b"100644 file.txt\0");
    tree_bytes.extend(merged.as_bytes());
    let expected_tree = database.write(git2::ObjectType::Tree, &tree_bytes).unwrap();
    assert!(database.exists(expected_tree));
    assert!(!database.exists(merged));

    f.merge(MergeMode::Auto).unwrap();
    assert_eq!(f.git(&["rev-parse", "HEAD^1"]), original);
    assert_eq!(f.git(&["rev-parse", "HEAD^2"]), target);
    assert_eq!(
        f.git(&["rev-parse", "HEAD^{tree}"]).trim(),
        expected_tree.to_string()
    );
    assert!(database.exists(merged));
    assert_eq!(f.git(&["show", ":file.txt"]), result);
    assert_eq!(fs::read(path).unwrap(), utf16(&result, false));
    assert_eq!(f.git(&["status", "--porcelain"]), "");
}

#[test]
fn interrupted_encoded_divergent_pull_recovers_the_prepared_two_parent_commit() {
    for abort in [false, true] {
        let f = Fixture::new(true);
        let target = f.topic();
        fs::write(f.repo.root.join("local.txt"), "local\n").unwrap();
        f.git(&["add", "local.txt"]);
        f.git(&["commit", "-qm", "local change"]);
        f.git(&["config", "branch.main.remote", "."]);
        f.git(&["config", "branch.main.merge", "refs/heads/topic"]);
        let original = f.git(&["rev-parse", "HEAD"]);
        f.git(&["update-ref", "ORIG_HEAD", &target]);
        let before = f.evidence();
        let context = Arc::new(f.repo.prepare_pull().unwrap());
        let progress: ProgressSink = Arc::new(Mutex::new(|phase: &str| {
            assert_ne!(
                phase, "Worktree conversion: native completed",
                "simulate interruption before publishing the merge commit"
            );
        }));
        let interrupted = catch_unwind(AssertUnwindSafe(|| {
            f.repo.execute_with_control(
                Operation::PullWith {
                    context,
                    strategy: PullStrategy::Merge,
                },
                NetworkControl::new(Some(progress), CancellationToken::default()),
            )
        }));
        assert!(interrupted.is_err(), "fixture did not interrupt checkout");
        assert_eq!(f.git(&["rev-parse", "HEAD"]), original);
        assert_eq!(f.git(&["rev-parse", "ORIG_HEAD"]).trim(), target);
        let reopened = Repository::open(&f.repo.root).unwrap();
        let status = reopened.worktree_recovery_status().unwrap().unwrap();
        assert!(status.can_continue);
        if abort {
            reopened
                .execute(Operation::AbortWorktreeCheckout {
                    expected: status.expected,
                })
                .unwrap();
            let mut restored = f.evidence();
            // Abort deliberately retains private object pins with its backup.
            restored.refs = restored
                .refs
                .split_inclusive('\n')
                .filter(|line| !line.contains(" refs/gitbuddy/worktree-conversions/"))
                .collect();
            assert_eq!(restored, before);
            assert!(reopened.worktree_recovery_status().unwrap().is_none());
        } else {
            reopened
                .execute(Operation::ContinueWorktreeCheckout {
                    expected: status.expected,
                })
                .unwrap();
            let merged = f.git(&["rev-parse", "HEAD"]);
            assert_eq!(f.git(&["rev-parse", "HEAD^1"]), original);
            assert_eq!(f.git(&["rev-parse", "HEAD^2"]).trim(), target);
            assert_eq!(f.git(&["rev-parse", "ORIG_HEAD"]), original);
            assert_eq!(f.git(&["show", ":local.txt"]), "local\n");
            f.assert_encoded_result(merged.trim());
        }
    }
}

#[test]
fn conflicting_encoded_merge_refuses_without_publishing_operation_state() {
    let f = Fixture::new(true);
    let middle = "one\ntwo\nthree\nfour\nfive\nsix\nseven\n";
    let combined = f.repo.root.join("combined.txt");
    fs::write(&combined, format!("first\n{middle}last\n")).unwrap();
    f.git(&["add", "combined.txt"]);
    f.git(&["commit", "-qm", "add independent mergeable file"]);
    f.git(&["checkout", "-qb", "topic"]);
    f.write_encoded_target();
    fs::write(&combined, format!("first target\n{middle}last\n")).unwrap();
    f.git(&["add", "combined.txt"]);
    f.git(&["commit", "-qm", "edit the first line"]);
    f.git(&["checkout", "-q", "main"]);
    fs::write(
        f.repo.root.join("file.txt"),
        utf16("local conflict\n", true),
    )
    .unwrap();
    fs::write(&combined, format!("first\n{middle}last local\n")).unwrap();
    f.git(&["add", "file.txt", "combined.txt"]);
    f.git(&["commit", "-qm", "edit same line"]);
    let object_list = [
        "cat-file",
        "--batch-all-objects",
        "--batch-check=%(objectname)",
    ];
    let objects = f.git(&object_list);
    let before = f.evidence();
    let result = f.merge(MergeMode::Auto);
    assert!(format!("{:#}", result.as_ref().unwrap_err()).contains("conflict"));
    f.assert_refusal_preserves_repository(result, before);
    assert_eq!(f.git(&object_list), objects);
}

#[test]
fn squash_refuses_new_encoded_paths_before_writing() {
    let f = Fixture::new(false);
    f.topic();
    let before = f.evidence();
    f.assert_refusal_preserves_repository(f.merge(MergeMode::Squash), before);
}

#[test]
fn merge_applies_a_macro_that_only_activates_after_combining_two_trees() {
    let f = Fixture::new(false);
    f.git(&["checkout", "-qb", "topic"]);
    let nested = f.repo.root.join("nested");
    fs::create_dir(&nested).unwrap();
    fs::write(nested.join(".gitattributes"), "*.txt encoded\n").unwrap();
    fs::write(nested.join("new.txt"), "plain target\n").unwrap();
    f.git(&["add", "-A"]);
    f.git(&["commit", "-qm", "use an undefined macro"]);
    assert_eq!(
        f.git(&[
            "check-attr",
            "working-tree-encoding",
            "--",
            "nested/new.txt"
        ]),
        "nested/new.txt: working-tree-encoding: unspecified\n"
    );
    f.git(&["checkout", "-q", "main"]);
    fs::write(
        f.repo.root.join(".gitattributes"),
        "[attr]encoded working-tree-encoding=UTF-16LE\n",
    )
    .unwrap();
    f.git(&["add", "-A"]);
    f.git(&["commit", "-qm", "define an unused macro"]);
    assert_eq!(
        f.git(&[
            "check-attr",
            "working-tree-encoding",
            "--",
            "nested/new.txt"
        ]),
        "nested/new.txt: working-tree-encoding: unspecified\n"
    );
    f.merge(MergeMode::Auto).unwrap();
    assert_eq!(
        fs::read(nested.join("new.txt")).unwrap(),
        utf16("plain target\n", true)
    );
    assert_eq!(f.git(&["show", ":nested/new.txt"]), "plain target\n");
    assert_eq!(f.git(&["status", "--porcelain"]), "");
}

#[cfg(unix)]
#[test]
fn encoded_merge_refuses_active_prepublication_hooks_without_bypassing_them() {
    use std::os::unix::fs::PermissionsExt;
    for name in ["pre-merge-commit", "prepare-commit-msg", "commit-msg"] {
        let f = Fixture::new(false);
        f.topic();
        f.git(&["config", "core.hooksPath", ".git/custom-hooks"]);
        let hooks = f.repo.root.join(".git/custom-hooks");
        fs::create_dir(&hooks).unwrap();
        let hook = hooks.join(name);
        fs::write(&hook, "#!/bin/sh\ntouch \"$GIT_DIR/hook-ran\"\nexit 1\n").unwrap();
        fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
        let before = f.evidence();
        let result = f.merge(MergeMode::NoFastForward);
        assert!(format!("{:#}", result.as_ref().unwrap_err()).contains(name));
        f.assert_refusal_preserves_repository(result, before);
        assert!(!f.repo.root.join(".git/hook-ran").exists());
    }
}

#[cfg(unix)]
#[test]
fn encoded_merge_runs_post_merge_after_materialization() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new(false);
    f.topic();
    f.git(&["config", "core.hooksPath", ".git/custom-hooks"]);
    let hooks = f.repo.root.join(".git/custom-hooks");
    fs::create_dir(&hooks).unwrap();
    let hook = hooks.join("post-merge");
    fs::write(&hook, "#!/bin/sh\nprintf '%s' \"$1\" > \"$GIT_DIR/merge-hook-mode\"\ncp file.txt \"$GIT_DIR/merge-hook-worktree\"\n").unwrap();
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
    f.merge(MergeMode::NoFastForward).unwrap();
    assert_eq!(
        fs::read(f.repo.root.join(".git/merge-hook-mode")).unwrap(),
        b"0"
    );
    assert_eq!(
        fs::read(f.repo.root.join(".git/merge-hook-worktree")).unwrap(),
        utf16(TARGET, false)
    );
}

#[test]
fn encoded_merge_refuses_unsupported_merge_policies_without_starting() {
    for (key, value, message) in [
        ("merge.custom.driver", "false", "custom merge drivers"),
        ("merge.renormalize", "true", "merge.renormalize"),
    ] {
        let f = Fixture::new(false);
        f.topic();
        f.git(&["config", key, value]);
        let before = f.evidence();
        let result = f.merge(MergeMode::NoFastForward);
        assert!(format!("{:#}", result.as_ref().unwrap_err()).contains(message));
        f.assert_refusal_preserves_repository(result, before);
    }
}

#[test]
fn encoded_merge_signs_the_canonical_tree_and_both_parents() {
    if Command::new("ssh-keygen").arg("-?").output().is_err() {
        return;
    }
    let f = Fixture::new(false);
    let target = f.topic();
    let original = f.git(&["rev-parse", "HEAD"]);
    let signer = tempfile::tempdir().unwrap();
    let key = signer.path().join("key");
    assert!(
        Command::new("ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", "", "-f"])
            .arg(&key)
            .status()
            .unwrap()
            .success()
    );
    f.git(&["config", "commit.gpgsign", "true"]);
    f.git(&["config", "gpg.format", "ssh"]);
    f.git(&["config", "user.signingkey", key.to_str().unwrap()]);
    f.merge(MergeMode::NoFastForward).unwrap();
    let merged = f.git(&["rev-parse", "HEAD"]);
    assert_eq!(f.git(&["rev-parse", "HEAD^1"]), original);
    assert_eq!(f.git(&["rev-parse", "HEAD^2"]).trim(), target);
    assert!(
        f.repo
            .verify_commit_signature(merged.trim())
            .unwrap()
            .contains("cryptographically valid")
    );
    f.assert_encoded_result(merged.trim());
}

#[test]
fn unsupported_target_encoding_keeps_index_and_fast_forward_refs_unchanged() {
    let f = Fixture::new(false);
    let repo = git2::Repository::open(&f.repo.root).unwrap();
    let parent = repo.head().unwrap().peel_to_commit().unwrap();
    let mut tree = repo.treebuilder(Some(&parent.tree().unwrap())).unwrap();
    let attributes = repo
        .blob(b"file.txt text working-tree-encoding=not-a-real-encoding\n")
        .unwrap();
    tree.insert(".gitattributes", attributes, 0o100644).unwrap();
    let tree = repo.find_tree(tree.write().unwrap()).unwrap();
    let signature = repo.signature().unwrap();
    repo.commit(
        Some("refs/heads/topic"),
        &signature,
        &signature,
        "unsupported target encoding",
        &tree,
        &[&parent],
    )
    .unwrap();

    let before = f.evidence();
    let error = f.merge(MergeMode::Auto).unwrap_err();
    assert!(
        format!("{error:#}")
            .to_ascii_lowercase()
            .contains("not-a-real-encoding"),
        "{error:#}"
    );
    assert_eq!(f.evidence(), before);
    assert!(f.repo.worktree_recovery_status().unwrap().is_none());
}

#[test]
fn rebase_refuses_encoding_used_only_in_an_intermediate_replay_step() {
    let f = Fixture::new(false);
    f.git(&["checkout", "-qb", "onto"]);
    fs::write(f.repo.root.join("onto.txt"), "onto\n").unwrap();
    f.git(&["add", "onto.txt"]);
    f.git(&["commit", "-qm", "onto"]);
    f.git(&["checkout", "-q", "main"]);
    f.write_encoded_target();
    fs::remove_file(f.repo.root.join(".gitattributes")).unwrap();
    fs::write(f.repo.root.join("file.txt"), TARGET).unwrap();
    f.git(&["add", "-A"]);
    f.git(&["commit", "-qm", "return to plain UTF-8"]);
    let context = Arc::new(f.repo.rebase_preview("onto").unwrap());
    let steps = context.steps.clone();
    let before = f.evidence();
    f.assert_refusal_preserves_repository(
        f.repo.execute(Operation::Rebase { context, steps }),
        before,
    );
}

#[test]
fn revert_batch_refuses_to_restore_parent_encoding_before_writing() {
    let f = Fixture::new(true);
    fs::remove_file(f.repo.root.join(".gitattributes")).unwrap();
    fs::write(f.repo.root.join("file.txt"), TARGET).unwrap();
    f.git(&["add", "-A"]);
    f.git(&["commit", "-qm", "remove encoding"]);
    let id = f.git(&["rev-parse", "HEAD"]).trim().to_owned();
    let plan = f.repo.prepare_revert_batch(&[id]).unwrap();
    let before = f.evidence();
    f.assert_refusal_preserves_repository(
        f.repo.execute(Operation::RevertBatch(Arc::new(plan))),
        before,
    );
}
