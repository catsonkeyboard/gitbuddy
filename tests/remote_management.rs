use gitbuddy::git::{
    CancellationToken, NetworkControl, Operation, ProgressSink, PushSelection, Repository,
    is_cancelled,
};
use std::{
    fs,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

struct Fixture {
    _dir: tempfile::TempDir,
    repo: Repository,
    remote: std::path::PathBuf,
    base: git2::Oid,
}
fn raw(repo: &Repository) -> git2::Repository {
    git2::Repository::open(&repo.root).unwrap()
}
fn commit(repo: &Repository, text: &str) -> git2::Oid {
    fs::write(repo.root.join("file"), text).unwrap();
    repo.execute(Operation::StageAll).unwrap();
    repo.execute(Operation::Commit(text.into())).unwrap();
    raw(repo).head().unwrap().target().unwrap()
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
        let base = commit(&repo, "base");
        let remote = dir.path().join("remote.git");
        git2::Repository::init_bare(&remote)
            .unwrap()
            .set_head("refs/heads/main")
            .unwrap();
        repo.execute(Operation::AddRemote(
            "origin".into(),
            remote.to_str().unwrap().into(),
        ))
        .unwrap();
        repo.execute(Operation::Push).unwrap();
        Self {
            _dir: dir,
            repo,
            remote,
            base,
        }
    }
    fn target(&self, name: &str) -> Option<git2::Oid> {
        git2::Repository::open_bare(&self.remote)
            .unwrap()
            .find_reference(name)
            .ok()
            .and_then(|r| r.target())
    }
    fn selection(&self, lease: bool) -> PushSelection {
        PushSelection {
            remote: "origin".into(),
            branch: Some("main".into()),
            target: "main".into(),
            force_with_lease: lease,
            ..PushSelection::default()
        }
    }
    fn rewind(&self) {
        raw(&self.repo)
            .reference("refs/heads/main", self.base, true, "test rewind")
            .unwrap();
    }
    fn advance_remote(&self) -> git2::Oid {
        let repo = git2::Repository::open_bare(&self.remote).unwrap();
        let parent = repo.head().unwrap().peel_to_commit().unwrap();
        let sig = git2::Signature::now("Other", "other@example.invalid").unwrap();
        repo.commit(
            Some("refs/heads/main"),
            &sig,
            &sig,
            "external change",
            &parent.tree().unwrap(),
            &[&parent],
        )
        .unwrap()
    }
}
#[test]
fn edit_urls_clear_push_url_and_reject_stale_configuration() {
    let f = Fixture::new();
    let original = f.repo.remote_info().unwrap().remove(0);
    f.repo
        .execute(Operation::EditRemote {
            expected: original.clone(),
            url: original.url.clone(),
            push_url: Some("https://example.invalid/push.git".into()),
        })
        .unwrap();
    let updated = f.repo.remote_info().unwrap().remove(0);
    assert_eq!(updated.destination(), "https://example.invalid/push.git");
    assert!(f.repo.execute(Operation::DeleteRemote(original)).is_err());
    assert!(
        f.repo
            .execute(Operation::EditRemote {
                expected: updated.clone(),
                url: "".into(),
                push_url: None
            })
            .is_err()
    );
    assert_eq!(f.repo.remote_info().unwrap()[0], updated);
    f.repo
        .execute(Operation::EditRemote {
            expected: updated.clone(),
            url: updated.url.clone(),
            push_url: None,
        })
        .unwrap();
    assert!(f.repo.remote_info().unwrap()[0].push_url.is_none());
}
#[test]
fn rename_and_delete_update_tracking_without_deleting_local_branches_or_server() {
    let f = Fixture::new();
    let info = f.repo.remote_info().unwrap().remove(0);
    f.repo
        .execute(Operation::RenameRemote {
            expected: info,
            name: "team/origin".into(),
        })
        .unwrap();
    let state = f.repo.remote_state().unwrap();
    assert_eq!(state.default_push.remote, "team/origin");
    assert_eq!(
        state.upstreams[0],
        ("main".into(), "team/origin/main".into())
    );
    assert_eq!(f.repo.snapshot(10).unwrap().upstream, "team/origin/main");
    f.repo
        .execute(Operation::DeleteRemote(state.remotes[0].clone()))
        .unwrap();
    assert!(f.repo.remote_info().unwrap().is_empty());
    assert!(
        raw(&f.repo)
            .find_reference("refs/remotes/team/origin/main")
            .is_err()
    );
    assert_eq!(raw(&f.repo).head().unwrap().target(), Some(f.base));
    assert_eq!(f.target("refs/heads/main"), Some(f.base));
}
#[test]
fn set_clear_local_and_remote_upstreams_without_switching_branches() {
    let f = Fixture::new();
    let r = raw(&f.repo);
    r.branch("local-base", &r.find_commit(f.base).unwrap(), false)
        .unwrap();
    f.repo
        .execute(Operation::SetUpstream {
            branch: "main".into(),
            upstream: Some("local-base".into()),
        })
        .unwrap();
    assert_eq!(
        r.config()
            .unwrap()
            .get_string("branch.main.remote")
            .unwrap(),
        "."
    );
    assert_eq!(f.repo.snapshot(10).unwrap().upstream, "local-base");
    f.repo
        .execute(Operation::SetUpstream {
            branch: "main".into(),
            upstream: Some("origin/main".into()),
        })
        .unwrap();
    assert_eq!(f.repo.snapshot(10).unwrap().upstream, "origin/main");
    assert!(
        f.repo
            .execute(Operation::SetUpstream {
                branch: "main".into(),
                upstream: Some("origin/missing".into())
            })
            .is_err()
    );
    assert_eq!(f.repo.snapshot(10).unwrap().upstream, "origin/main");
    f.repo
        .execute(Operation::SetUpstream {
            branch: "main".into(),
            upstream: None,
        })
        .unwrap();
    assert!(f.repo.snapshot(10).unwrap().upstream.is_empty());
    assert_eq!(r.head().unwrap().target(), Some(f.base));
}
#[test]
fn select_remote_different_target_and_set_upstream_after_push() {
    let f = Fixture::new();
    let r = raw(&f.repo);
    r.remote("team/origin", f.remote.to_str().unwrap()).unwrap();
    let id = commit(&f.repo, "release");
    let plan = f
        .repo
        .prepare_push(PushSelection {
            remote: "team/origin".into(),
            target: "release/stable".into(),
            set_upstream: true,
            ..f.selection(false)
        })
        .unwrap();
    f.repo.execute(Operation::PushTo(Arc::new(plan))).unwrap();
    assert_eq!(f.target("refs/heads/release/stable"), Some(id));
    assert_eq!(f.target("refs/heads/main"), Some(f.base));
    let state = f.repo.remote_state().unwrap();
    assert_eq!(state.default_push.remote, "team/origin");
    assert_eq!(state.default_push.target, "release/stable");
    assert_eq!(
        f.repo.snapshot(10).unwrap().upstream,
        "team/origin/release/stable"
    );
}
#[test]
fn normal_push_cannot_rewind_but_matching_lease_can() {
    let f = Fixture::new();
    let advanced = commit(&f.repo, "advanced");
    f.repo.execute(Operation::Push).unwrap();
    f.rewind();
    let regular = f.repo.prepare_push(f.selection(false)).unwrap();
    assert!(
        f.repo
            .execute(Operation::PushTo(Arc::new(regular)))
            .is_err()
    );
    assert_eq!(f.target("refs/heads/main"), Some(advanced));
    let leased = f.repo.prepare_push(f.selection(true)).unwrap();
    assert!(leased.summary().contains(&advanced.to_string()));
    f.repo.execute(Operation::PushTo(Arc::new(leased))).unwrap();
    assert_eq!(f.target("refs/heads/main"), Some(f.base));
    assert_eq!(
        raw(&f.repo)
            .find_reference("refs/remotes/origin/main")
            .unwrap()
            .target(),
        Some(f.base)
    );
}
#[test]
fn external_update_and_even_later_fetch_cannot_replace_pinned_lease() {
    let f = Fixture::new();
    let advanced = commit(&f.repo, "advanced");
    f.repo.execute(Operation::Push).unwrap();
    f.rewind();
    let plan = f.repo.prepare_push(f.selection(true)).unwrap();
    let external = f.advance_remote();
    assert_ne!(external, advanced);
    f.repo.execute(Operation::Fetch).unwrap();
    let error = f
        .repo
        .execute(Operation::PushTo(Arc::new(plan)))
        .unwrap_err();
    assert!(
        format!("{error:#}").contains("Force-with-lease rejected"),
        "{error:#}"
    );
    assert_eq!(f.target("refs/heads/main"), Some(external));
}
#[test]
fn unknown_branch_lease_requires_absence_and_can_create_new_branch() {
    let f = Fixture::new();
    let r = raw(&f.repo);
    r.find_reference("refs/remotes/origin/main")
        .unwrap()
        .delete()
        .unwrap();
    let plan = f.repo.prepare_push(f.selection(true)).unwrap();
    assert!(plan.summary().contains("must not exist"));
    assert!(f.repo.execute(Operation::PushTo(Arc::new(plan))).is_err());
    assert_eq!(f.target("refs/heads/main"), Some(f.base));
    let plan = f
        .repo
        .prepare_push(PushSelection {
            target: "new-branch".into(),
            set_upstream: true,
            ..f.selection(true)
        })
        .unwrap();
    f.repo.execute(Operation::PushTo(Arc::new(plan))).unwrap();
    assert_eq!(f.target("refs/heads/new-branch"), Some(f.base));
    assert_eq!(f.repo.snapshot(10).unwrap().upstream, "origin/new-branch");
}
#[test]
fn local_lease_locks_target_through_upload_and_cancellation_releases_locks() {
    for cancel in [true, false] {
        let f = Fixture::new();
        commit(&f.repo, "advanced");
        f.repo.execute(Operation::Push).unwrap();
        f.rewind();
        let plan = f.repo.prepare_push(f.selection(true)).unwrap();
        let path = f.remote.clone();
        let base = f.base;
        let token = CancellationToken::default();
        let other = token.clone();
        let called = Arc::new(AtomicBool::new(false));
        let marked = called.clone();
        let sink: ProgressSink = Arc::new(Mutex::new(move |text: &str| {
            if text == "Push: preparing upload…" {
                let r = git2::Repository::open_bare(&path).unwrap();
                assert_eq!(
                    r.reference("refs/heads/main", base, true, "external race")
                        .err()
                        .unwrap()
                        .code(),
                    git2::ErrorCode::Locked
                );
                marked.store(true, Ordering::SeqCst);
                if cancel {
                    other.cancel();
                }
            }
        }));
        let result = f.repo.execute_with_control(
            Operation::PushTo(Arc::new(plan)),
            NetworkControl::new(Some(sink), token),
        );
        assert!(called.load(Ordering::SeqCst));
        if cancel {
            assert!(is_cancelled(&result.unwrap_err()));
        } else {
            result.unwrap();
        }
        // The dropped transaction must release the ref lock in both cases.
        git2::Repository::open_bare(&f.remote)
            .unwrap()
            .reference("refs/heads/main", f.base, true, "after operation")
            .unwrap();
    }
}
#[test]
fn push_selected_lightweight_annotated_and_blob_tags_from_detached_head() {
    let f = Fixture::new();
    let r = raw(&f.repo);
    let object = r.find_object(f.base, None).unwrap();
    r.tag_lightweight("light", &object, false).unwrap();
    let annotated = r
        .tag(
            "annotated",
            &object,
            &r.signature().unwrap(),
            "release",
            false,
        )
        .unwrap();
    let blob = r.blob(b"tagged blob").unwrap();
    r.tag_lightweight("blob", &r.find_object(blob, None).unwrap(), false)
        .unwrap();
    r.tag_lightweight("unselected", &object, false).unwrap();
    r.set_head_detached(f.base).unwrap();
    let plan = f
        .repo
        .prepare_push(PushSelection {
            remote: "origin".into(),
            tags: vec!["light".into(), "annotated".into(), "blob".into()],
            ..PushSelection::default()
        })
        .unwrap();
    f.repo.execute(Operation::PushTo(Arc::new(plan))).unwrap();
    assert_eq!(f.target("refs/tags/light"), Some(f.base));
    assert_eq!(f.target("refs/tags/annotated"), Some(annotated));
    assert_eq!(f.target("refs/tags/blob"), Some(blob));
    assert!(f.target("refs/tags/unselected").is_none());
    assert_eq!(f.target("refs/heads/main"), Some(f.base));
}
#[test]
fn push_url_is_honored_and_existing_tags_are_never_force_replaced() {
    let f = Fixture::new();
    let backup = f._dir.path().join("backup.git");
    git2::Repository::init_bare(&backup).unwrap();
    let info = f.repo.remote_info().unwrap().remove(0);
    f.repo
        .execute(Operation::EditRemote {
            expected: info.clone(),
            url: info.url,
            push_url: Some(backup.to_str().unwrap().into()),
        })
        .unwrap();
    let advanced = commit(&f.repo, "advanced");
    let plan = f.repo.prepare_push(f.selection(false)).unwrap();
    f.repo.execute(Operation::PushTo(Arc::new(plan))).unwrap();
    assert_eq!(
        git2::Repository::open_bare(&backup)
            .unwrap()
            .find_reference("refs/heads/main")
            .unwrap()
            .target(),
        Some(advanced)
    );
    assert_eq!(f.target("refs/heads/main"), Some(f.base));
    assert!(f.repo.prepare_push(f.selection(true)).is_err());
    let r = raw(&f.repo);
    r.tag_lightweight("release", &r.find_object(f.base, None).unwrap(), false)
        .unwrap();
    let selection = PushSelection {
        remote: "origin".into(),
        tags: vec!["release".into()],
        ..PushSelection::default()
    };
    let plan = f.repo.prepare_push(selection.clone()).unwrap();
    f.repo.execute(Operation::PushTo(Arc::new(plan))).unwrap();
    r.tag_lightweight("release", &r.find_object(advanced, None).unwrap(), true)
        .unwrap();
    let plan = f.repo.prepare_push(selection).unwrap();
    assert!(f.repo.execute(Operation::PushTo(Arc::new(plan))).is_err());
    assert_eq!(
        git2::Repository::open_bare(&backup)
            .unwrap()
            .find_reference("refs/tags/release")
            .unwrap()
            .target(),
        Some(f.base)
    );
}
#[test]
fn changing_source_or_endpoint_invalidates_plan_and_precancel_does_not_write() {
    let f = Fixture::new();
    let plan = f.repo.prepare_push(f.selection(false)).unwrap();
    let new = commit(&f.repo, "new");
    assert!(f.repo.execute(Operation::PushTo(Arc::new(plan))).is_err());
    assert_eq!(f.target("refs/heads/main"), Some(f.base));
    let plan = f.repo.prepare_push(f.selection(false)).unwrap();
    let token = CancellationToken::default();
    token.cancel();
    assert!(is_cancelled(
        &f.repo
            .execute_with_control(
                Operation::PushTo(Arc::new(plan)),
                NetworkControl::new(None, token)
            )
            .unwrap_err()
    ));
    let plan = f.repo.prepare_push(f.selection(false)).unwrap();
    let info = f.repo.remote_info().unwrap().remove(0);
    f.repo
        .execute(Operation::EditRemote {
            expected: info,
            url: "https://example.invalid/replaced.git".into(),
            push_url: None,
        })
        .unwrap();
    assert!(f.repo.execute(Operation::PushTo(Arc::new(plan))).is_err());
    assert_eq!(f.target("refs/heads/main"), Some(f.base));
    assert_eq!(raw(&f.repo).head().unwrap().target(), Some(new));
}
#[test]
fn custom_fetch_mapping_controls_lease_and_upstream_instead_of_splitting_names() {
    let f = Fixture::new();
    let r = raw(&f.repo);
    r.remote("team/origin", f.remote.to_str().unwrap()).unwrap();
    r.config()
        .unwrap()
        .set_str(
            "remote.team/origin.fetch",
            "+refs/heads/*:refs/remotes/team/cache/*",
        )
        .unwrap();
    f.repo.execute(Operation::Fetch).unwrap();
    let plan = f
        .repo
        .prepare_push(PushSelection {
            remote: "team/origin".into(),
            target: "release/stable".into(),
            set_upstream: true,
            ..f.selection(true)
        })
        .unwrap();
    f.repo.execute(Operation::PushTo(Arc::new(plan))).unwrap();
    assert_eq!(
        f.repo.snapshot(10).unwrap().upstream,
        "team/cache/release/stable"
    );
    assert_eq!(
        r.config()
            .unwrap()
            .get_string("branch.main.remote")
            .unwrap(),
        "team/origin"
    );
    assert_eq!(
        r.config().unwrap().get_string("branch.main.merge").unwrap(),
        "refs/heads/release/stable"
    );
}
#[test]
fn invalid_or_empty_push_selection_is_rejected_without_changes() {
    let f = Fixture::new();
    let before = f.repo.fingerprint().unwrap();
    for selection in [
        PushSelection {
            remote: "origin".into(),
            ..PushSelection::default()
        },
        PushSelection {
            target: "bad:ref".into(),
            ..f.selection(false)
        },
        PushSelection {
            branch: None,
            force_with_lease: true,
            tags: vec!["missing".into()],
            ..f.selection(false)
        },
        PushSelection {
            remote: "missing".into(),
            ..f.selection(false)
        },
    ] {
        assert!(f.repo.prepare_push(selection).is_err());
    }
    assert_eq!(f.repo.fingerprint().unwrap(), before);
    assert_eq!(f.target("refs/heads/main"), Some(f.base));
}

#[test]
fn explicit_upstream_namespace_handles_identical_local_and_remote_names() {
    let f = Fixture::new();
    let r = raw(&f.repo);
    r.branch("origin/main", &r.find_commit(f.base).unwrap(), false)
        .unwrap();
    assert!(
        f.repo
            .execute(Operation::SetUpstream {
                branch: "main".into(),
                upstream: Some("origin/main".into())
            })
            .is_err()
    );
    for (target, remote, merge) in [
        ("refs/heads/origin/main", ".", "refs/heads/origin/main"),
        ("refs/remotes/origin/main", "origin", "refs/heads/main"),
    ] {
        f.repo
            .execute(Operation::SetUpstream {
                branch: "main".into(),
                upstream: Some(target.into()),
            })
            .unwrap();
        assert_eq!(
            r.config()
                .unwrap()
                .get_string("branch.main.remote")
                .unwrap(),
            remote
        );
        assert_eq!(
            r.config().unwrap().get_string("branch.main.merge").unwrap(),
            merge
        );
    }
    let plan = f
        .repo
        .prepare_push(PushSelection {
            set_upstream: true,
            ..f.selection(false)
        })
        .unwrap();
    f.repo.execute(Operation::PushTo(Arc::new(plan))).unwrap();
    assert_eq!(
        r.config()
            .unwrap()
            .get_string("branch.main.remote")
            .unwrap(),
        "origin"
    );
}

#[test]
fn repeated_and_combined_leased_tag_push_preserves_tag_objects_and_ancestors() {
    let f = Fixture::new();
    let r = raw(&f.repo);
    let new = commit(&f.repo, "new tagged commit");
    let tag = r
        .tag(
            "annotated",
            &r.find_object(new, None).unwrap(),
            &r.signature().unwrap(),
            "release",
            false,
        )
        .unwrap();
    let selection = PushSelection {
        tags: vec!["annotated".into()],
        ..f.selection(true)
    };
    for _ in 0..2 {
        let plan = f.repo.prepare_push(selection.clone()).unwrap();
        f.repo.execute(Operation::PushTo(Arc::new(plan))).unwrap();
    }
    assert_eq!(f.target("refs/tags/annotated"), Some(tag));
    let remote = git2::Repository::open_bare(&f.remote).unwrap();
    let parent = remote.find_commit(new).unwrap().parent(0).unwrap();
    assert_eq!(parent.id(), f.base);
    assert!(remote.find_tree(parent.tree_id()).is_ok());
    let tag_only = PushSelection {
        remote: "origin".into(),
        tags: vec!["annotated".into()],
        ..PushSelection::default()
    };
    f.repo
        .execute(Operation::PushTo(Arc::new(
            f.repo.prepare_push(tag_only).unwrap(),
        )))
        .unwrap();
    // Lease force applies only to the branch, never to a moved existing tag.
    r.tag_lightweight("annotated", &r.find_object(f.base, None).unwrap(), true)
        .unwrap();
    let plan = f.repo.prepare_push(selection).unwrap();
    assert!(f.repo.execute(Operation::PushTo(Arc::new(plan))).is_err());
    assert_eq!(f.target("refs/tags/annotated"), Some(tag));
}

#[test]
fn file_url_lease_and_configuration_changes_are_checked() {
    let f = Fixture::new();
    let info = f.repo.remote_info().unwrap().remove(0);
    let url = format!("file://{}", f.remote.display());
    f.repo
        .execute(Operation::EditRemote {
            expected: info,
            url,
            push_url: None,
        })
        .unwrap();
    let plan = f.repo.prepare_push(f.selection(true)).unwrap();
    f.repo.execute(Operation::PushTo(Arc::new(plan))).unwrap();
    let plan = f.repo.prepare_push(f.selection(true)).unwrap();
    raw(&f.repo)
        .config()
        .unwrap()
        .set_str(
            "remote.origin.fetch",
            "+refs/heads/*:refs/remotes/changed/*",
        )
        .unwrap();
    assert!(f.repo.execute(Operation::PushTo(Arc::new(plan))).is_err());
    assert_eq!(f.target("refs/heads/main"), Some(f.base));
}
