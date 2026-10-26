use gitbuddy::git::{
    CancellationToken, NetworkControl, Operation, PushSelection, Repository, is_cancelled,
};
use std::{fs, path::PathBuf, sync::Arc};

struct Fixture {
    dir: tempfile::TempDir,
    repo: Repository,
    base: String,
    tip: String,
}
fn raw(repo: &Repository) -> git2::Repository {
    git2::Repository::open(&repo.root).unwrap()
}
fn commit(repo: &Repository, message: &str) -> String {
    repo.execute(Operation::StageAll).unwrap();
    repo.execute(Operation::Commit(message.into())).unwrap();
    raw(repo).head().unwrap().target().unwrap().to_string()
}
impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(&dir.path().join("client")).unwrap();
        repo.execute(Operation::SetIdentity(
            "Test".into(),
            "test@example.invalid".into(),
        ))
        .unwrap();
        fs::write(repo.root.join("file"), "base\n").unwrap();
        let base = commit(&repo, "base");
        fs::write(repo.root.join("file"), "tip\n").unwrap();
        let tip = commit(&repo, "tip");
        Self {
            dir,
            repo,
            base,
            tip,
        }
    }
    fn rename(&self, from: &str, to: &str) -> anyhow::Result<String> {
        self.repo.execute(Operation::RenameBranch {
            expected: Arc::new(self.repo.prepare_branch_rename(from)?),
            name: to.into(),
        })
    }
    fn detach(&self, revision: &str) -> anyhow::Result<String> {
        self.repo.execute(Operation::CheckoutDetached(Arc::new(
            self.repo.prepare_detached_checkout(revision)?,
        )))
    }
    fn remote(&self) -> PathBuf {
        let path = self.dir.path().join("server.git");
        git2::Repository::init_bare(&path)
            .unwrap()
            .set_head("refs/heads/main")
            .unwrap();
        self.repo
            .execute(Operation::AddRemote(
                "origin".into(),
                path.to_str().unwrap().into(),
            ))
            .unwrap();
        self.repo.execute(Operation::Push).unwrap();
        self.repo
            .execute(Operation::CreateBranchAt {
                name: "topic".into(),
                target: self.tip.clone(),
            })
            .unwrap();
        let plan = self
            .repo
            .prepare_push(PushSelection {
                remote: "origin".into(),
                branch: Some("topic".into()),
                target: "topic".into(),
                ..Default::default()
            })
            .unwrap();
        self.repo
            .execute(Operation::PushTo(Arc::new(plan)))
            .unwrap();
        self.repo.execute(Operation::Fetch).unwrap();
        path
    }
    fn delete(&self, tracking: &str) -> anyhow::Result<String> {
        self.repo.execute(Operation::DeleteRemoteBranch(Arc::new(
            self.repo.prepare_remote_branch_delete(tracking)?,
        )))
    }
}

#[test]
fn rename_current_preserves_head_files_index_upstream_configuration_and_reflog() {
    let f = Fixture::new();
    f.repo
        .execute(Operation::CreateBranchAt {
            name: "base".into(),
            target: f.base.clone(),
        })
        .unwrap();
    f.repo
        .execute(Operation::SetUpstream {
            branch: "main".into(),
            upstream: Some("base".into()),
        })
        .unwrap();
    raw(&f.repo)
        .config()
        .unwrap()
        .set_str("branch.main.description", "keep description")
        .unwrap();
    fs::write(f.repo.root.join("file"), "staged\n").unwrap();
    f.repo.execute(Operation::StageAll).unwrap();
    fs::write(f.repo.root.join("file"), "working\n").unwrap();
    let tree = raw(&f.repo).index().unwrap().write_tree().unwrap();
    let original_log = raw(&f.repo).reflog("refs/heads/main").unwrap();
    let original_entries = original_log
        .iter()
        .map(|entry| {
            (
                entry.id_old(),
                entry.id_new(),
                entry.message().unwrap().map(str::to_owned),
                entry.committer().when(),
            )
        })
        .collect::<Vec<_>>();
    f.rename("main", "feature/renamed").unwrap();
    let r = raw(&f.repo);
    assert_eq!(
        r.head().unwrap().name().unwrap(),
        "refs/heads/feature/renamed"
    );
    assert_eq!(r.head().unwrap().target().unwrap().to_string(), f.tip);
    assert!(r.find_reference("refs/heads/main").is_err());
    assert_eq!(r.index().unwrap().write_tree().unwrap(), tree);
    assert_eq!(
        fs::read_to_string(f.repo.root.join("file")).unwrap(),
        "working\n"
    );
    assert_eq!(
        r.config()
            .unwrap()
            .get_string("branch.feature/renamed.merge")
            .unwrap(),
        "refs/heads/base"
    );
    assert_eq!(
        r.config()
            .unwrap()
            .get_string("branch.feature/renamed.description")
            .unwrap(),
        "keep description"
    );
    assert!(r.reflog("refs/heads/feature/renamed").unwrap().len() >= 2);
    let log = r.reflog("refs/heads/feature/renamed").unwrap();
    let preserved = log
        .iter()
        .skip(1)
        .map(|entry| {
            (
                entry.id_old(),
                entry.id_new(),
                entry.message().unwrap().map(str::to_owned),
                entry.committer().when(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(preserved, original_entries);
}

#[test]
fn rename_from_linked_worktree_keeps_primary_head_and_shared_configuration() {
    let f = Fixture::new();
    let path = f.dir.path().join("linked");
    f.repo
        .execute(Operation::CreateWorktree {
            name: "linked".into(),
            path: path.clone(),
        })
        .unwrap();
    let linked = Repository::open(path).unwrap();
    linked
        .execute(Operation::SetUpstream {
            branch: "linked".into(),
            upstream: Some("main".into()),
        })
        .unwrap();
    let expected = Arc::new(linked.prepare_branch_rename("linked").unwrap());
    linked
        .execute(Operation::RenameBranch {
            expected,
            name: "feature/linked".into(),
        })
        .unwrap();
    assert_eq!(
        raw(&linked).head().unwrap().name().unwrap(),
        "refs/heads/feature/linked"
    );
    assert_eq!(
        raw(&f.repo).head().unwrap().name().unwrap(),
        "refs/heads/main"
    );
    assert_eq!(
        raw(&f.repo)
            .config()
            .unwrap()
            .get_string("branch.feature/linked.merge")
            .unwrap(),
        "refs/heads/main"
    );
}
#[test]
fn rename_legacy_quoted_headers_and_nested_path_rejection_preserve_config() {
    use std::io::Write;
    let f = Fixture::new();
    fs::OpenOptions::new().append(true).open(raw(&f.repo).path().join("config")).unwrap()
        .write_all(b"\n[branch.main]\r\n description = legacy\r\n[branch \"main\"] # keep comment\n custom = \"a\\nvalue\"\n").unwrap();
    assert!(f.rename("main", "main/topic").is_err());
    f.rename("main", "quoted\"name").unwrap();
    let config = raw(&f.repo).config().unwrap();
    assert_eq!(
        config
            .get_string("branch.quoted\"name.description")
            .unwrap(),
        "legacy"
    );
    assert_eq!(
        config.get_string("branch.quoted\"name.custom").unwrap(),
        "a\nvalue"
    );
    assert!(
        fs::read_to_string(raw(&f.repo).path().join("config"))
            .unwrap()
            .contains("# keep comment")
    );
}
#[test]
fn rename_inactive_and_detached_branches_never_changes_checkout() {
    let f = Fixture::new();
    f.repo
        .execute(Operation::CreateBranchAt {
            name: "topic".into(),
            target: f.base.clone(),
        })
        .unwrap();
    f.rename("topic", "renamed").unwrap();
    assert_eq!(
        raw(&f.repo).head().unwrap().name().unwrap(),
        "refs/heads/main"
    );
    f.detach(&f.tip).unwrap();
    f.rename("renamed", "done").unwrap();
    assert!(raw(&f.repo).head_detached().unwrap());
    assert_eq!(
        raw(&f.repo).head().unwrap().target().unwrap().to_string(),
        f.tip
    );
}
#[test]
fn rename_rejects_invalid_existing_names_or_orphaned_config_without_changes() {
    let f = Fixture::new();
    raw(&f.repo)
        .config()
        .unwrap()
        .set_str("branch.orphan.remote", "origin")
        .unwrap();
    for name in [
        "main",
        "bad..name",
        "-bad",
        "HEAD",
        "orphan",
        "",
        "bad\nname",
    ] {
        assert!(f.rename("main", name).is_err(), "{name:?}");
        assert_eq!(
            raw(&f.repo).head().unwrap().name().unwrap(),
            "refs/heads/main"
        );
    }
    f.repo
        .execute(Operation::CreateBranchAt {
            name: "other".into(),
            target: f.base.clone(),
        })
        .unwrap();
    assert!(f.rename("main", "other").is_err());
}
#[test]
fn rename_refuses_stale_head_source_cross_repository_and_operation_state() {
    let f = Fixture::new();
    let context = Arc::new(f.repo.prepare_branch_rename("main").unwrap());
    let other = Fixture::new();
    assert!(
        other
            .repo
            .execute(Operation::RenameBranch {
                expected: context.clone(),
                name: "new".into()
            })
            .is_err()
    );
    raw(&f.repo)
        .reference(
            "refs/heads/main",
            f.base.parse().unwrap(),
            true,
            "external move",
        )
        .unwrap();
    assert!(
        f.repo
            .execute(Operation::RenameBranch {
                expected: context,
                name: "new".into()
            })
            .is_err()
    );
    fs::write(
        raw(&f.repo).path().join("MERGE_HEAD"),
        format!("{}\n", f.tip),
    )
    .unwrap();
    assert!(f.repo.prepare_branch_rename("main").is_err());
}
#[test]
fn rename_checks_other_worktrees_and_ref_locks() {
    let f = Fixture::new();
    let path = f.dir.path().join("linked");
    f.repo
        .execute(Operation::CreateWorktree {
            name: "linked".into(),
            path,
        })
        .unwrap();
    assert!(f.repo.prepare_branch_rename("linked").is_err());
    let context = Arc::new(f.repo.prepare_branch_rename("main").unwrap());
    fs::write(
        raw(&f.repo).path().join("refs/heads/main.lock"),
        "owned by another process",
    )
    .unwrap();
    assert!(
        f.repo
            .execute(Operation::RenameBranch {
                expected: context,
                name: "new".into()
            })
            .is_err()
    );
    assert_eq!(
        raw(&f.repo).head().unwrap().name().unwrap(),
        "refs/heads/main"
    );
}

#[test]
fn rename_locks_head_destination_and_config_before_changing_any_reference() {
    for relative in ["HEAD.lock", "refs/heads/new.lock", "config.lock"] {
        let f = Fixture::new();
        let r = raw(&f.repo);
        let path = r.path().join(relative);
        let context = Arc::new(f.repo.prepare_branch_rename("main").unwrap());
        let config = fs::read(r.path().join("config")).unwrap();
        fs::write(&path, "another process owns this lock").unwrap();
        assert!(
            f.repo
                .execute(Operation::RenameBranch {
                    expected: context,
                    name: "new".into()
                })
                .is_err()
        );
        assert_eq!(r.head().unwrap().name().unwrap(), "refs/heads/main");
        assert_eq!(
            r.find_reference("refs/heads/main")
                .unwrap()
                .target()
                .unwrap()
                .to_string(),
            f.tip
        );
        assert!(r.find_reference("refs/heads/new").is_err());
        assert_eq!(
            fs::read_to_string(path).unwrap(),
            "another process owns this lock"
        );
        assert_eq!(fs::read(r.path().join("config")).unwrap(), config);
    }
}
#[test]
fn rename_preserves_multivalue_and_boolean_settings_and_rejects_included_settings() {
    let f = Fixture::new();
    let r = raw(&f.repo);
    let mut config = r.config().unwrap();
    for value in ["", "same", "same"] {
        config
            .set_multivar("branch.main.custom", "a^", value)
            .unwrap();
    }
    config.set_bool("branch.main.rebase", true).unwrap();
    use std::io::Write;
    let mut file = fs::OpenOptions::new()
        .append(true)
        .open(r.path().join("config"))
        .unwrap();
    file.write_all(b"\n[branch \"main\"]\n multiline = \"multi\\nline\"\n implicit\n")
        .unwrap();
    drop(file);
    f.rename("main", "new").unwrap();
    let mut values = Vec::new();
    r.config()
        .unwrap()
        .multivar("branch.new.custom", None)
        .unwrap()
        .for_each(|entry| {
            values.push(entry.value().unwrap().to_owned());
        })
        .unwrap();
    assert_eq!(values, ["", "same", "same"]);
    assert_eq!(
        r.config()
            .unwrap()
            .get_string("branch.new.multiline")
            .unwrap(),
        "multi\nline"
    );
    assert!(r.config().unwrap().get_bool("branch.new.implicit").unwrap());
    assert!(r.config().unwrap().get_bool("branch.new.rebase").unwrap());
    let include = f.dir.path().join("included.cfg");
    fs::write(&include, "[branch \"new\"]\n\tdescription = included\n").unwrap();
    r.config()
        .unwrap()
        .set_str("include.path", include.to_str().unwrap())
        .unwrap();
    assert!(f.rename("new", "other").is_err());
    assert!(r.find_reference("refs/heads/new").is_ok());
    assert!(r.find_reference("refs/heads/other").is_err());
}
#[test]
fn detached_checkout_commit_tag_and_back_preserve_branch_positions() {
    let f = Fixture::new();
    let r = raw(&f.repo);
    let signature = git2::Signature::now("Test", "test@example.invalid").unwrap();
    r.tag(
        "release",
        &r.find_object(f.base.parse().unwrap(), None).unwrap(),
        &signature,
        "annotated",
        false,
    )
    .unwrap();
    f.detach("refs/tags/release").unwrap();
    assert!(r.head_detached().unwrap());
    assert_eq!(r.head().unwrap().target().unwrap().to_string(), f.base);
    assert_eq!(
        r.find_reference("refs/heads/main")
            .unwrap()
            .target()
            .unwrap()
            .to_string(),
        f.tip
    );
    assert_eq!(
        fs::read_to_string(f.repo.root.join("file")).unwrap(),
        "base\n"
    );
    assert!(
        r.reflog("HEAD")
            .unwrap()
            .get(0)
            .unwrap()
            .message()
            .unwrap()
            .unwrap()
            .contains("detached")
    );
    f.repo.execute(Operation::Checkout("main".into())).unwrap();
    assert!(!r.head_detached().unwrap());
    assert_eq!(
        fs::read_to_string(f.repo.root.join("file")).unwrap(),
        "tip\n"
    );
    f.detach("HEAD~1").unwrap();
    assert_eq!(r.head().unwrap().target().unwrap().to_string(), f.base);
}
#[test]
fn detached_checkout_refuses_overwriting_staged_unstaged_or_untracked_files() {
    for staged in [false, true] {
        let f = Fixture::new();
        fs::write(f.repo.root.join("file"), "edited\n").unwrap();
        if staged {
            f.repo.execute(Operation::StageAll).unwrap();
        }
        let index = raw(&f.repo).index().unwrap().write_tree().unwrap();
        assert!(f.detach(&f.base).is_err());
        assert_eq!(
            raw(&f.repo).head().unwrap().name().unwrap(),
            "refs/heads/main"
        );
        assert_eq!(raw(&f.repo).index().unwrap().write_tree().unwrap(), index);
        assert_eq!(
            fs::read_to_string(f.repo.root.join("file")).unwrap(),
            "edited\n"
        );
    }
    let f = Fixture::new();
    fs::write(f.repo.root.join("added"), "tracked at target").unwrap();
    let target = commit(&f.repo, "added");
    f.detach(&f.tip).unwrap();
    fs::write(f.repo.root.join("added"), "untracked keep").unwrap();
    assert!(f.detach(&target).is_err());
    assert_eq!(
        fs::read_to_string(f.repo.root.join("added")).unwrap(),
        "untracked keep"
    );
}
#[test]
fn detached_checkout_preserves_unrelated_staged_and_untracked_files() {
    let f = Fixture::new();
    fs::write(f.repo.root.join("local"), "staged keep").unwrap();
    f.repo.execute(Operation::StageAll).unwrap();
    fs::write(f.repo.root.join("untracked"), "keep").unwrap();
    f.detach(&f.base).unwrap();
    assert!(
        raw(&f.repo)
            .index()
            .unwrap()
            .get_path(std::path::Path::new("local"), 0)
            .is_some()
    );
    assert_eq!(
        fs::read_to_string(f.repo.root.join("local")).unwrap(),
        "staged keep"
    );
    assert_eq!(
        fs::read_to_string(f.repo.root.join("untracked")).unwrap(),
        "keep"
    );
}
#[test]
fn detached_checkout_rejects_changed_preview_tag_and_cross_repository() {
    let f = Fixture::new();
    let context = Arc::new(f.repo.prepare_detached_checkout(&f.base).unwrap());
    assert!(
        Fixture::new()
            .repo
            .execute(Operation::CheckoutDetached(context.clone()))
            .is_err()
    );
    fs::write(f.repo.root.join("file"), "after review").unwrap();
    assert!(
        f.repo
            .execute(Operation::CheckoutDetached(context))
            .is_err()
    );
    fs::write(f.repo.root.join("file"), "tip\n").unwrap();
    f.repo.execute(Operation::Tag("tag".into())).unwrap();
    let context = Arc::new(f.repo.prepare_detached_checkout("refs/tags/tag").unwrap());
    raw(&f.repo)
        .reference("refs/tags/tag", f.base.parse().unwrap(), true, "moved tag")
        .unwrap();
    assert!(
        f.repo
            .execute(Operation::CheckoutDetached(context))
            .is_err()
    );
    assert_eq!(
        raw(&f.repo).head().unwrap().name().unwrap(),
        "refs/heads/main"
    );
}
#[test]
fn detached_checkout_requires_clean_operation_and_respects_head_lock() {
    let f = Fixture::new();
    let context = Arc::new(f.repo.prepare_detached_checkout(&f.base).unwrap());
    fs::write(raw(&f.repo).path().join("HEAD.lock"), "other process").unwrap();
    assert!(
        f.repo
            .execute(Operation::CheckoutDetached(context))
            .is_err()
    );
    assert_eq!(
        fs::read_to_string(f.repo.root.join("file")).unwrap(),
        "tip\n"
    );
    fs::remove_file(raw(&f.repo).path().join("HEAD.lock")).unwrap();
    fs::write(raw(&f.repo).path().join("CHERRY_PICK_HEAD"), &f.base).unwrap();
    assert!(f.repo.prepare_detached_checkout(&f.base).is_err());
    assert!(
        f.repo
            .prepare_detached_checkout("missing-revision")
            .is_err()
    );
}
#[test]
fn detached_checkout_index_lock_never_changes_head_or_working_file() {
    let f = Fixture::new();
    let expected = Arc::new(f.repo.prepare_detached_checkout(&f.base).unwrap());
    let r = raw(&f.repo);
    fs::write(r.path().join("index.lock"), "other process").unwrap();
    assert!(
        f.repo
            .execute(Operation::CheckoutDetached(expected))
            .is_err()
    );
    assert_eq!(r.head().unwrap().name().unwrap(), "refs/heads/main");
    assert_eq!(
        fs::read_to_string(f.repo.root.join("file")).unwrap(),
        "tip\n"
    );
    assert_eq!(
        fs::read_to_string(r.path().join("index.lock")).unwrap(),
        "other process"
    );
}
#[test]
fn detached_checkout_materializes_lfs_and_rejects_dirty_lfs_without_losing_content() {
    let f = Fixture::new();
    f.repo
        .execute(Operation::LfsTrack {
            pattern: "*.bin".into(),
            track: true,
        })
        .unwrap();
    fs::write(f.repo.root.join("data.bin"), b"old binary body").unwrap();
    let old = commit(&f.repo, "old LFS");
    fs::write(f.repo.root.join("data.bin"), b"new binary body").unwrap();
    commit(&f.repo, "new LFS");
    f.detach(&old).unwrap();
    assert_eq!(
        fs::read(f.repo.root.join("data.bin")).unwrap(),
        b"old binary body"
    );
    f.repo.execute(Operation::Checkout("main".into())).unwrap();
    fs::write(f.repo.root.join("data.bin"), b"dirty body keep").unwrap();
    assert!(f.detach(&old).is_err());
    assert_eq!(
        fs::read(f.repo.root.join("data.bin")).unwrap(),
        b"dirty body keep"
    );
}
#[test]
fn delete_remote_branch_preserves_local_branches_index_and_upstream() {
    let f = Fixture::new();
    let remote = f.remote();
    f.repo
        .execute(Operation::SetUpstream {
            branch: "topic".into(),
            upstream: Some("refs/remotes/origin/topic".into()),
        })
        .unwrap();
    fs::write(f.repo.root.join("file"), "dirty keep").unwrap();
    let index = raw(&f.repo).index().unwrap().write_tree().unwrap();
    f.delete("origin/topic").unwrap();
    assert!(
        git2::Repository::open_bare(remote)
            .unwrap()
            .find_reference("refs/heads/topic")
            .is_err()
    );
    let r = raw(&f.repo);
    assert!(r.find_reference("refs/heads/topic").is_ok());
    assert!(r.find_reference("refs/remotes/origin/topic").is_err());
    assert_eq!(
        r.config()
            .unwrap()
            .get_string("branch.topic.merge")
            .unwrap(),
        "refs/heads/topic"
    );
    assert_eq!(r.index().unwrap().write_tree().unwrap(), index);
    assert_eq!(
        fs::read_to_string(f.repo.root.join("file")).unwrap(),
        "dirty keep"
    );
}
#[test]
fn remote_delete_rejects_server_advance_stale_local_or_endpoint_and_wrong_repo() {
    for mutation in 0..4 {
        let f = Fixture::new();
        let remote = f.remote();
        let plan = Arc::new(f.repo.prepare_remote_branch_delete("origin/topic").unwrap());
        let r = raw(&f.repo);
        let server = git2::Repository::open_bare(remote).unwrap();
        match mutation {
            0 => {
                server
                    .reference(
                        "refs/heads/topic",
                        f.base.parse().unwrap(),
                        true,
                        "advanced elsewhere",
                    )
                    .unwrap();
            }
            1 => {
                r.reference(
                    "refs/remotes/origin/topic",
                    f.base.parse().unwrap(),
                    true,
                    "fetched elsewhere",
                )
                .unwrap();
            }
            2 => {
                r.remote_set_url("origin", "https://example.invalid/different.git")
                    .unwrap();
            }
            _ => {
                assert!(
                    Fixture::new()
                        .repo
                        .execute(Operation::DeleteRemoteBranch(plan.clone()))
                        .is_err()
                );
                continue;
            }
        }
        assert!(f.repo.execute(Operation::DeleteRemoteBranch(plan)).is_err());
        assert!(server.find_reference("refs/heads/topic").is_ok());
        assert!(r.find_reference("refs/remotes/origin/topic").is_ok());
    }
}
#[test]
fn remote_delete_protects_default_symbolic_missing_and_separate_push_urls() {
    let f = Fixture::new();
    f.remote();
    assert!(f.delete("origin/main").is_err());
    let r = raw(&f.repo);
    r.reference_symbolic(
        "refs/remotes/origin/HEAD",
        "refs/remotes/origin/main",
        true,
        "default",
    )
    .unwrap();
    assert!(f.repo.prepare_remote_branch_delete("origin/HEAD").is_err());
    assert!(
        f.repo
            .prepare_remote_branch_delete("origin/missing")
            .is_err()
    );
    r.remote_set_pushurl("origin", Some("https://example.invalid/push.git"))
        .unwrap();
    assert!(f.repo.prepare_remote_branch_delete("origin/topic").is_err());
}
#[test]
fn remote_delete_maps_custom_refspecs_and_remote_names_with_slashes() {
    let f = Fixture::new();
    let server = f.remote();
    f.repo
        .execute(Operation::RenameRemote {
            expected: f.repo.remote_info().unwrap().remove(0),
            name: "team/origin".into(),
        })
        .unwrap();
    let r = raw(&f.repo);
    r.config()
        .unwrap()
        .set_str(
            "remote.team/origin.fetch",
            "+refs/heads/*:refs/remotes/custom/*",
        )
        .unwrap();
    f.repo.execute(Operation::Fetch).unwrap();
    f.delete("custom/topic").unwrap();
    assert!(
        git2::Repository::open_bare(server)
            .unwrap()
            .find_reference("refs/heads/topic")
            .is_err()
    );
    assert!(r.find_reference("refs/remotes/custom/topic").is_err());
    // Old tracking refs outside the active mapping are intentionally unchanged.
    assert!(r.find_reference("refs/remotes/team/origin/topic").is_ok());
}
#[test]
fn remote_delete_cancellation_and_destination_lock_leave_refs_untouched() {
    let f = Fixture::new();
    let server_path = f.remote();
    let plan = Arc::new(f.repo.prepare_remote_branch_delete("origin/topic").unwrap());
    let token = CancellationToken::default();
    assert!(token.cancel());
    assert!(is_cancelled(
        &f.repo
            .execute_with_control(
                Operation::DeleteRemoteBranch(plan.clone()),
                NetworkControl::new(None, token)
            )
            .unwrap_err()
    ));
    let server = git2::Repository::open_bare(server_path).unwrap();
    let mut transaction = server.transaction().unwrap();
    transaction.lock_ref("refs/heads/topic").unwrap();
    assert!(f.repo.execute(Operation::DeleteRemoteBranch(plan)).is_err());
    assert!(server.find_reference("refs/heads/topic").is_ok());
    assert!(
        raw(&f.repo)
            .find_reference("refs/remotes/origin/topic")
            .is_ok()
    );
}

#[cfg(unix)]
struct Daemon(std::process::Child);
#[cfg(unix)]
impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
#[cfg(unix)]
fn network_remote(f: &Fixture) -> Daemon {
    use std::{
        net::{TcpListener, TcpStream},
        process::{Command, Stdio},
        time::Duration,
    };
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let mut daemon = Daemon(
        Command::new("git")
            .args([
                "daemon",
                "--reuseaddr",
                "--export-all",
                "--enable=receive-pack",
                "--listen=127.0.0.1",
                &format!("--port={port}"),
                &format!("--base-path={}", f.dir.path().display()),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let mut ready = false;
    for _ in 0..150 {
        assert!(
            daemon.0.try_wait().unwrap().is_none(),
            "Git test daemon exited"
        );
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            ready = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(ready, "Git test daemon did not listen");
    raw(&f.repo)
        .remote_set_url("origin", &format!("git://127.0.0.1:{port}/server.git"))
        .unwrap();
    daemon
}
#[cfg(unix)]
#[test]
fn network_delete_negotiates_lease_and_preserves_other_remote_branches() {
    for stale in [false, true] {
        let f = Fixture::new();
        let path = f.remote();
        let _daemon = network_remote(&f);
        let plan = Arc::new(f.repo.prepare_remote_branch_delete("origin/topic").unwrap());
        let server = git2::Repository::open_bare(path).unwrap();
        if stale {
            server
                .reference(
                    "refs/heads/topic",
                    f.base.parse().unwrap(),
                    true,
                    "external move",
                )
                .unwrap();
        }
        let result = f.repo.execute(Operation::DeleteRemoteBranch(plan));
        assert_eq!(result.is_err(), stale, "{result:?}");
        assert_eq!(server.find_reference("refs/heads/topic").is_ok(), stale);
        assert_eq!(
            raw(&f.repo)
                .find_reference("refs/remotes/origin/topic")
                .is_ok(),
            stale
        );
        assert!(server.find_reference("refs/heads/main").is_ok());
        assert!(raw(&f.repo).find_reference("refs/heads/topic").is_ok());
    }
}
#[cfg(unix)]
#[test]
fn network_delete_reports_server_rejection_without_pruning_tracking_ref() {
    let f = Fixture::new();
    let path = f.remote();
    git2::Repository::open_bare(&path)
        .unwrap()
        .config()
        .unwrap()
        .set_bool("receive.denyDeletes", true)
        .unwrap();
    let _daemon = network_remote(&f);
    let error = f.delete("origin/topic").unwrap_err();
    assert!(
        format!("{error:#}").contains("deletion prohibited"),
        "{error:#}"
    );
    assert!(
        git2::Repository::open_bare(&path)
            .unwrap()
            .find_reference("refs/heads/topic")
            .is_ok()
    );
    assert!(
        raw(&f.repo)
            .find_reference("refs/remotes/origin/topic")
            .is_ok()
    );
}

#[cfg(unix)]
#[test]
fn network_delete_default_branch_is_rejected_before_sending_delete() {
    let f = Fixture::new();
    let path = f.remote();
    git2::Repository::open_bare(&path)
        .unwrap()
        .config()
        .unwrap()
        .set_str("receive.denyDeleteCurrent", "ignore")
        .unwrap();
    let _daemon = network_remote(&f);
    let error = f.delete("origin/main").unwrap_err();
    assert!(
        format!("{error:#}").contains("remote default branch"),
        "{error:#}"
    );
    assert!(
        git2::Repository::open_bare(path)
            .unwrap()
            .find_reference("refs/heads/main")
            .is_ok()
    );
}
#[cfg(unix)]
#[test]
fn network_delete_can_cancel_during_preflight_without_deleting_the_branch() {
    let f = Fixture::new();
    let path = f.remote();
    let _daemon = network_remote(&f);
    let plan = Arc::new(f.repo.prepare_remote_branch_delete("origin/topic").unwrap());
    let token = CancellationToken::default();
    let cancel = token.clone();
    let progress: gitbuddy::git::ProgressSink =
        Arc::new(std::sync::Mutex::new(move |text: &str| {
            if text.contains("checking remote default") {
                cancel.cancel();
            }
        }));
    let error = f
        .repo
        .execute_with_control(
            Operation::DeleteRemoteBranch(plan),
            NetworkControl::new(Some(progress), token),
        )
        .unwrap_err();
    assert!(is_cancelled(&error), "{error:#}");
    assert!(
        git2::Repository::open_bare(path)
            .unwrap()
            .find_reference("refs/heads/topic")
            .is_ok()
    );
    assert!(
        raw(&f.repo)
            .find_reference("refs/remotes/origin/topic")
            .is_ok()
    );
}
#[test]
fn remote_delete_reports_local_cleanup_failure_after_server_success() {
    let f = Fixture::new();
    let path = f.remote();
    let lock = raw(&f.repo).path().join("refs/remotes/origin/topic.lock");
    fs::write(&lock, "held externally").unwrap();
    let error = f.delete("origin/topic").unwrap_err();
    assert!(format!("{error:#}").contains("succeeded"));
    assert!(
        git2::Repository::open_bare(path)
            .unwrap()
            .find_reference("refs/heads/topic")
            .is_err()
    );
    assert!(
        raw(&f.repo)
            .find_reference("refs/remotes/origin/topic")
            .is_ok()
    );
    assert_eq!(fs::read_to_string(lock).unwrap(), "held externally");
}
