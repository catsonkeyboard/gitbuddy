use gitbuddy::git::{
    CancellationToken, NetworkControl, Operation, PullStrategy, RemoteInfo, Repository,
    is_cancelled,
};
use std::{
    fs,
    path::PathBuf,
    sync::{Arc, Mutex},
};
fn raw(repo: &Repository) -> git2::Repository {
    git2::Repository::open(&repo.root).unwrap()
}
fn head(repo: &Repository) -> String {
    raw(repo).head().unwrap().target().unwrap().to_string()
}
fn control() -> NetworkControl {
    NetworkControl::new(None, CancellationToken::default())
}
fn commit(repo: &Repository, path: &str, value: &str) -> String {
    fs::write(repo.root.join(path), value).unwrap();
    repo.execute(Operation::StageAll).unwrap();
    repo.execute(Operation::Commit(format!("Change {path}")))
        .unwrap();
    head(repo)
}
fn identity(repo: &Repository) {
    repo.execute(Operation::SetIdentity(
        "Test".into(),
        "test@example.invalid".into(),
    ))
    .unwrap();
}
struct Fixture {
    dir: tempfile::TempDir,
    source: Repository,
    client: Repository,
    server: PathBuf,
    base: String,
}
impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let source = Repository::init(&dir.path().join("source")).unwrap();
        identity(&source);
        let base = commit(&source, "base", "base\n");
        let r = raw(&source);
        let c = r.head().unwrap().peel_to_commit().unwrap();
        r.branch("topic", &c, false).unwrap();
        r.branch("spare", &c, false).unwrap();
        let server = dir.path().join("server.git");
        git2::Repository::init_bare(&server)
            .unwrap()
            .set_head("refs/heads/main")
            .unwrap();
        r.remote("origin", server.to_str().unwrap())
            .unwrap()
            .push(
                &[
                    "refs/heads/main:refs/heads/main",
                    "refs/heads/topic:refs/heads/topic",
                    "refs/heads/spare:refs/heads/spare",
                ],
                None,
            )
            .unwrap();
        let client =
            Repository::clone_repo(server.to_str().unwrap(), &dir.path().join("client")).unwrap();
        identity(&client);
        Self {
            dir,
            source,
            client,
            server,
            base,
        }
    }
    fn info(&self) -> RemoteInfo {
        self.client
            .remote_info()
            .unwrap()
            .into_iter()
            .find(|r| r.name == "origin")
            .unwrap()
    }
    fn publish(&self, branch: &str, file: &str, value: &str) -> String {
        self.source
            .execute(Operation::Checkout(branch.into()))
            .unwrap();
        let id = commit(&self.source, file, value);
        raw(&self.source)
            .find_remote("origin")
            .unwrap()
            .push(&[format!("refs/heads/{branch}:refs/heads/{branch}")], None)
            .unwrap();
        id
    }
    fn tags(&self) -> (String, String) {
        let r = raw(&self.source);
        let obj = r
            .find_object(git2::Oid::from_str(&self.base).unwrap(), None)
            .unwrap();
        r.tag_lightweight("release/light", &obj, false).unwrap();
        let annotated = r
            .tag(
                "release/annotated",
                &obj,
                &r.signature().unwrap(),
                "Release",
                false,
            )
            .unwrap();
        let blob = r.blob(b"non-commit tag").unwrap();
        r.tag_lightweight("blob", r.find_blob(blob).unwrap().as_object(), false)
            .unwrap();
        r.find_remote("origin")
            .unwrap()
            .push(
                &[
                    "refs/tags/release/light:refs/tags/release/light",
                    "refs/tags/release/annotated:refs/tags/release/annotated",
                    "refs/tags/blob:refs/tags/blob",
                ],
                None,
            )
            .unwrap();
        (annotated.to_string(), blob.to_string())
    }
    fn fetch(&self, branch: Option<&str>) -> anyhow::Result<String> {
        self.client.execute(Operation::FetchFrom(Arc::new(
            self.client.prepare_fetch(&self.info(), branch)?,
        )))
    }
    fn delete(&self, name: &str) -> anyhow::Result<String> {
        let refs = self.client.remote_refs(&self.info(), control())?;
        self.client.execute(Operation::DeleteRemoteTag(Arc::new(
            self.client.prepare_remote_tag_delete(&refs, name)?,
        )))
    }
}
#[test]
fn query_returns_real_tag_objects_peeling_and_branches_without_local_writes() {
    let f = Fixture::new();
    let (annotated, blob) = f.tags();
    let before = f.client.fingerprint().unwrap();
    let refs = f.client.remote_refs(&f.info(), control()).unwrap();
    assert_eq!(refs.branches.len(), 3);
    assert_eq!(refs.tags.len(), 3);
    let tag = refs
        .tags
        .iter()
        .find(|t| t.name == "release/annotated")
        .unwrap();
    assert_eq!(tag.id, annotated);
    assert_eq!(tag.peeled.as_deref(), Some(f.base.as_str()));
    assert_eq!(
        refs.tags.iter().find(|t| t.name == "blob").unwrap().id,
        blob
    );
    assert_eq!(before, f.client.fingerprint().unwrap());
    assert!(
        raw(&f.client)
            .references_glob("refs/tags/*")
            .unwrap()
            .next()
            .is_none()
    );
}
#[test]
fn query_empty_remote_is_successful() {
    let f = Fixture::new();
    let path = f.dir.path().join("empty.git");
    git2::Repository::init_bare(&path).unwrap();
    raw(&f.client)
        .remote_set_url("origin", path.to_str().unwrap())
        .unwrap();
    let refs = f.client.remote_refs(&f.info(), control()).unwrap();
    assert!(refs.branches.is_empty() && refs.tags.is_empty());
}
#[test]
fn fetch_selected_branch_leaves_other_refs_tags_dirty_files_and_head_unchanged() {
    let f = Fixture::new();
    f.tags();
    let tip = f.publish("topic", "topic", "remote topic");
    f.publish("main", "remote", "remote main");
    git2::Repository::open_bare(&f.server)
        .unwrap()
        .find_reference("refs/heads/spare")
        .unwrap()
        .delete()
        .unwrap();
    fs::write(f.client.root.join("base"), "dirty preserve").unwrap();
    let before = head(&f.client);
    f.fetch(Some("topic")).unwrap();
    let r = raw(&f.client);
    assert_eq!(
        r.find_reference("refs/remotes/origin/topic")
            .unwrap()
            .target()
            .unwrap()
            .to_string(),
        tip
    );
    assert_eq!(
        r.find_reference("refs/remotes/origin/main")
            .unwrap()
            .target()
            .unwrap()
            .to_string(),
        f.base
    );
    assert!(r.find_reference("refs/remotes/origin/spare").is_ok());
    assert!(r.references_glob("refs/tags/*").unwrap().next().is_none());
    assert_eq!(head(&f.client), before);
    assert_eq!(
        fs::read_to_string(f.client.root.join("base")).unwrap(),
        "dirty preserve"
    );
}
#[test]
fn fetch_remote_uses_only_its_configured_refs_and_prunes() {
    let f = Fixture::new();
    let tip = f.publish("topic", "topic", "topic");
    git2::Repository::open_bare(&f.server)
        .unwrap()
        .find_reference("refs/heads/spare")
        .unwrap()
        .delete()
        .unwrap();
    f.fetch(None).unwrap();
    let r = raw(&f.client);
    assert!(r.find_reference("refs/remotes/origin/spare").is_err());
    assert_eq!(
        r.find_reference("refs/remotes/origin/topic")
            .unwrap()
            .target()
            .unwrap()
            .to_string(),
        tip
    );
    assert_eq!(head(&f.client), f.base);
}
#[test]
fn selected_source_honors_custom_fetch_mapping_and_remote_names_with_slashes() {
    let f = Fixture::new();
    raw(&f.client)
        .remote_rename("origin", "team/origin")
        .unwrap();
    let r = raw(&f.client);
    let mut c = r.config().unwrap();
    c.set_str(
        "remote.team/origin.fetch",
        "+refs/heads/*:refs/remotes/custom/*",
    )
    .unwrap();
    let info = f.client.remote_info().unwrap().remove(0);
    let tip = f.publish("topic", "topic", "topic");
    f.client
        .execute(Operation::FetchFrom(Arc::new(
            f.client.prepare_fetch(&info, Some("topic")).unwrap(),
        )))
        .unwrap();
    assert_eq!(
        r.find_reference("refs/remotes/custom/topic")
            .unwrap()
            .target()
            .unwrap()
            .to_string(),
        tip
    );
    assert_eq!(
        r.find_reference("refs/remotes/team/origin/topic")
            .unwrap()
            .target()
            .unwrap()
            .to_string(),
        f.base
    );
}
#[test]
fn unmapped_branch_can_be_fetched_and_pulled_without_creating_a_tracking_ref() {
    let f = Fixture::new();
    raw(&f.client)
        .config()
        .unwrap()
        .set_str(
            "remote.origin.fetch",
            "+refs/heads/main:refs/remotes/origin/main",
        )
        .unwrap();
    let tip = f.publish("topic", "topic", "topic");
    f.fetch(Some("topic")).unwrap();
    assert_eq!(
        raw(&f.client)
            .find_reference("refs/remotes/origin/topic")
            .unwrap()
            .target()
            .unwrap()
            .to_string(),
        f.base
    );
    assert!(
        fs::read_to_string(raw(&f.client).path().join("FETCH_HEAD"))
            .unwrap()
            .contains(&tip)
    );
    let preview = f.client.prepare_pull_from(&f.info(), "topic").unwrap();
    f.client
        .execute(Operation::PullWith {
            context: Arc::new(preview),
            strategy: PullStrategy::FastForwardOnly,
        })
        .unwrap();
    assert_eq!(head(&f.client), tip);
}
#[test]
fn ambiguous_or_unsafe_mapping_and_invalid_branch_are_refused() {
    let f = Fixture::new();
    let r = raw(&f.client);
    r.config()
        .unwrap()
        .set_multivar(
            "remote.origin.fetch",
            "^$",
            "+refs/heads/*:refs/remotes/other/*",
        )
        .unwrap();
    assert!(f.client.prepare_fetch(&f.info(), Some("topic")).is_err());
    r.config()
        .unwrap()
        .remove_multivar("remote.origin.fetch", ".*")
        .unwrap();
    r.config()
        .unwrap()
        .set_str("remote.origin.fetch", "+refs/heads/*:refs/heads/*")
        .unwrap();
    assert!(f.client.prepare_fetch(&f.info(), Some("topic")).is_err());
    assert!(
        f.client
            .prepare_fetch(&f.info(), Some("bad*branch"))
            .is_err()
    );
}
#[test]
fn chosen_pull_remote_and_branch_never_changes_upstream_for_any_strategy() {
    for strategy in [
        PullStrategy::FastForwardOnly,
        PullStrategy::Merge,
        PullStrategy::Rebase,
    ] {
        let f = Fixture::new();
        let alternate = f.dir.path().join("alternate.git");
        git2::build::RepoBuilder::new()
            .bare(true)
            .clone(f.server.to_str().unwrap(), &alternate)
            .unwrap();
        raw(&f.client)
            .remote("backup", alternate.to_str().unwrap())
            .unwrap();
        let tip = f.publish("topic", "topic", "topic");
        raw(&f.source)
            .remote_anonymous(alternate.to_str().unwrap())
            .unwrap()
            .push(&["refs/heads/topic:refs/heads/topic"], None)
            .unwrap();
        let before_remote = raw(&f.client)
            .config()
            .unwrap()
            .get_string("branch.main.remote")
            .unwrap();
        let before_merge = raw(&f.client)
            .config()
            .unwrap()
            .get_string("branch.main.merge")
            .unwrap();
        let original = if strategy == PullStrategy::FastForwardOnly {
            f.base.clone()
        } else {
            commit(&f.client, "local", "local")
        };
        let backup = f
            .client
            .remote_info()
            .unwrap()
            .into_iter()
            .find(|r| r.name == "backup")
            .unwrap();
        let p = f.client.prepare_pull_from(&backup, "topic").unwrap();
        f.client
            .execute(Operation::PullWith {
                context: Arc::new(p),
                strategy,
            })
            .unwrap();
        let r = raw(&f.client);
        let c = r.head().unwrap().peel_to_commit().unwrap();
        match strategy {
            PullStrategy::FastForwardOnly => assert_eq!(head(&f.client), tip),
            PullStrategy::Merge => assert_eq!(
                c.parent_ids().map(|id| id.to_string()).collect::<Vec<_>>(),
                [original, tip]
            ),
            PullStrategy::Rebase => assert_eq!(c.parent_id(0).unwrap().to_string(), tip),
        }
        assert_eq!(
            r.config()
                .unwrap()
                .get_string("branch.main.remote")
                .unwrap(),
            before_remote
        );
        assert_eq!(
            r.config().unwrap().get_string("branch.main.merge").unwrap(),
            before_merge
        );
        assert_eq!(
            r.find_reference("refs/remotes/origin/topic")
                .unwrap()
                .target()
                .unwrap()
                .to_string(),
            f.base
        );
    }
}
#[test]
fn explicit_pull_works_without_upstream_and_uses_this_fetchs_tip() {
    let f = Fixture::new();
    f.client
        .execute(Operation::SetUpstream {
            branch: "main".into(),
            upstream: None,
        })
        .unwrap();
    let p = f.client.prepare_pull_from(&f.info(), "topic").unwrap();
    let tip = f.publish("topic", "topic", "latest");
    f.client
        .execute(Operation::PullWith {
            context: Arc::new(p),
            strategy: PullStrategy::FastForwardOnly,
        })
        .unwrap();
    assert_eq!(head(&f.client), tip);
    assert!(
        raw(&f.client)
            .config()
            .unwrap()
            .get_string("branch.main.remote")
            .is_err()
    );
}
#[test]
fn stale_fetch_plans_cross_repository_and_cancellation_preserve_local_refs() {
    let f = Fixture::new();
    let plan = Arc::new(f.client.prepare_fetch(&f.info(), Some("topic")).unwrap());
    let token = CancellationToken::default();
    token.cancel();
    assert!(is_cancelled(
        &f.client
            .execute_with_control(
                Operation::FetchFrom(plan.clone()),
                NetworkControl::new(None, token)
            )
            .unwrap_err()
    ));
    let other = Repository::init(&f.dir.path().join("other")).unwrap();
    assert!(other.execute(Operation::FetchFrom(plan.clone())).is_err());
    raw(&f.client)
        .config()
        .unwrap()
        .set_str(
            "remote.origin.fetch",
            "+refs/heads/main:refs/remotes/origin/main",
        )
        .unwrap();
    assert!(f.client.execute(Operation::FetchFrom(plan)).is_err());
    assert_eq!(head(&f.client), f.base);
}
#[test]
fn query_refuses_stale_configuration_and_cancellation_after_connection_phase() {
    let f = Fixture::new();
    let info = f.info();
    let r = raw(&f.client);
    r.config()
        .unwrap()
        .set_str("remote.origin.url", "missing")
        .unwrap();
    assert!(f.client.remote_refs(&info, control()).is_err());
    r.config()
        .unwrap()
        .set_str("remote.origin.url", f.server.to_str().unwrap())
        .unwrap();
    let token = CancellationToken::default();
    let cancel = token.clone();
    let sink: gitbuddy::git::ProgressSink = Arc::new(Mutex::new(move |_: &str| {
        cancel.cancel();
    }));
    assert!(is_cancelled(
        &f.client
            .remote_refs(&info, NetworkControl::new(Some(sink), token))
            .unwrap_err()
    ));
}
#[test]
fn missing_remote_branch_never_integrates_cached_tracking_contents() {
    let f = Fixture::new();
    let preview = f.client.prepare_pull_from(&f.info(), "topic").unwrap();
    git2::Repository::open_bare(&f.server)
        .unwrap()
        .find_reference("refs/heads/topic")
        .unwrap()
        .delete()
        .unwrap();
    assert!(
        f.client
            .execute(Operation::PullWith {
                context: Arc::new(preview),
                strategy: PullStrategy::Merge
            })
            .is_err()
    );
    assert_eq!(head(&f.client), f.base);
}
#[test]
fn remote_tag_delete_preserves_all_local_tags_and_handles_annotated_and_blob_tags() {
    for tag in ["release/light", "release/annotated", "blob"] {
        let f = Fixture::new();
        f.tags();
        let r = raw(&f.client);
        let object = r
            .find_object(git2::Oid::from_str(&f.base).unwrap(), None)
            .unwrap();
        r.tag_lightweight("release/light", &object, false).unwrap();
        r.tag(
            "release/annotated",
            &object,
            &r.signature().unwrap(),
            "Local annotated tag",
            false,
        )
        .unwrap();
        let blob = r.blob(b"local blob tag").unwrap();
        r.tag_lightweight("blob", r.find_blob(blob).unwrap().as_object(), false)
            .unwrap();
        let mut names = Vec::new();
        for reference in r.references_glob("refs/tags/*").unwrap() {
            let reference = reference.unwrap();
            names.push((
                reference.name().unwrap().to_owned(),
                reference.target().unwrap(),
            ));
        }
        f.delete(tag).unwrap();
        assert!(
            git2::Repository::open_bare(&f.server)
                .unwrap()
                .find_reference(&format!("refs/tags/{tag}"))
                .is_err()
        );
        for (name, id) in names {
            assert_eq!(r.find_reference(&name).unwrap().target(), Some(id));
        }
    }
}
#[test]
fn moved_missing_or_locked_remote_tags_refuse_deletion() {
    for case in 0..3 {
        let f = Fixture::new();
        f.tags();
        let refs = f.client.remote_refs(&f.info(), control()).unwrap();
        let plan = Arc::new(
            f.client
                .prepare_remote_tag_delete(&refs, "release/light")
                .unwrap(),
        );
        let r = git2::Repository::open_bare(&f.server).unwrap();
        let mut tx = r.transaction().unwrap();
        if case == 0 {
            let id = r.blob(b"changed tag object").unwrap();
            r.reference("refs/tags/release/light", id, true, "test move")
                .unwrap();
        } else if case == 1 {
            r.find_reference("refs/tags/release/light")
                .unwrap()
                .delete()
                .unwrap();
        } else {
            tx.lock_ref("refs/tags/release/light").unwrap();
        }
        assert!(f.client.execute(Operation::DeleteRemoteTag(plan)).is_err());
        if case != 1 {
            assert!(r.find_reference("refs/tags/release/light").is_ok());
        }
    }
}
#[test]
fn tag_deletion_rejects_other_repo_endpoint_changes_and_separate_push_url() {
    let f = Fixture::new();
    f.tags();
    let refs = f.client.remote_refs(&f.info(), control()).unwrap();
    let plan = Arc::new(
        f.client
            .prepare_remote_tag_delete(&refs, "release/light")
            .unwrap(),
    );
    let other = Repository::init(&f.dir.path().join("other")).unwrap();
    assert!(
        other
            .prepare_remote_tag_delete(&refs, "release/light")
            .is_err()
    );
    assert!(
        other
            .execute(Operation::DeleteRemoteTag(plan.clone()))
            .is_err()
    );
    raw(&f.client)
        .remote_set_pushurl("origin", Some("https://example.invalid/different"))
        .unwrap();
    assert!(
        f.client
            .prepare_remote_tag_delete(&refs, "release/light")
            .is_err()
    );
    assert!(f.client.execute(Operation::DeleteRemoteTag(plan)).is_err());
}
#[test]
fn remote_tag_delete_cancels_before_writing_and_seals_cancel_at_publication() {
    for cancel_early in [true, false] {
        let f = Fixture::new();
        f.tags();
        let refs = f.client.remote_refs(&f.info(), control()).unwrap();
        let plan = Arc::new(
            f.client
                .prepare_remote_tag_delete(&refs, "release/light")
                .unwrap(),
        );
        let token = CancellationToken::default();
        if cancel_early {
            token.cancel();
        }
        let seen = Arc::new(Mutex::new(false));
        let observed = seen.clone();
        let cancel = token.clone();
        let sink: gitbuddy::git::ProgressSink = Arc::new(Mutex::new(move |phase: &str| {
            if phase.contains("updating locked") {
                *observed.lock().unwrap() = true;
                assert!(!cancel.cancel());
            }
        }));
        let result = f.client.execute_with_control(
            Operation::DeleteRemoteTag(plan),
            NetworkControl::new(Some(sink), token),
        );
        if cancel_early {
            assert!(is_cancelled(&result.unwrap_err()));
            assert!(!*seen.lock().unwrap());
        } else {
            result.unwrap();
            assert!(*seen.lock().unwrap());
        }
        assert_eq!(
            git2::Repository::open_bare(&f.server)
                .unwrap()
                .find_reference("refs/tags/release/light")
                .is_ok(),
            cancel_early
        );
    }
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
        assert!(daemon.0.try_wait().unwrap().is_none());
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            ready = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(ready);
    raw(&f.client)
        .remote_set_url("origin", &format!("git://127.0.0.1:{port}/server.git"))
        .unwrap();
    daemon
}
#[cfg(unix)]
#[test]
fn network_query_and_fetch_selected_branch_use_native_transport() {
    let f = Fixture::new();
    let tip = f.publish("topic", "topic", "topic");
    f.tags();
    let _server = network_remote(&f);
    let refs = f.client.remote_refs(&f.info(), control()).unwrap();
    assert_eq!(refs.tags.len(), 3);
    f.fetch(Some("topic")).unwrap();
    assert_eq!(
        raw(&f.client)
            .find_reference("refs/remotes/origin/topic")
            .unwrap()
            .target()
            .unwrap()
            .to_string(),
        tip
    );
    assert!(
        raw(&f.client)
            .find_reference("refs/tags/release/light")
            .is_err()
    );
}
#[cfg(unix)]
#[test]
fn network_tag_deletion_checks_the_tag_object_lease_and_preserves_local_tags() {
    for moved in [false, true] {
        let f = Fixture::new();
        f.tags();
        let r = raw(&f.client);
        let object = r
            .find_object(git2::Oid::from_str(&f.base).unwrap(), None)
            .unwrap();
        let local_id = r
            .tag(
                "release/annotated",
                &object,
                &r.signature().unwrap(),
                "Local tag preserved",
                false,
            )
            .unwrap();
        let _server = network_remote(&f);
        let refs = f.client.remote_refs(&f.info(), control()).unwrap();
        let plan = Arc::new(
            f.client
                .prepare_remote_tag_delete(&refs, "release/annotated")
                .unwrap(),
        );
        if moved {
            let r = git2::Repository::open_bare(&f.server).unwrap();
            let blob = r.blob(b"replacement object").unwrap();
            r.reference("refs/tags/release/annotated", blob, true, "move")
                .unwrap();
        }
        let result = f.client.execute(Operation::DeleteRemoteTag(plan));
        assert_eq!(result.is_err(), moved);
        assert!(
            raw(&f.client)
                .find_reference("refs/tags/release/annotated")
                .is_ok()
        );
        assert_eq!(
            raw(&f.client)
                .find_reference("refs/tags/release/annotated")
                .unwrap()
                .target(),
            Some(local_id)
        );
        assert_eq!(
            git2::Repository::open_bare(&f.server)
                .unwrap()
                .find_reference("refs/tags/release/annotated")
                .is_ok(),
            moved
        );
    }
}
#[cfg(unix)]
#[test]
fn network_tag_deletion_reports_rejection_and_receive_pack_race() {
    for race in [false, true] {
        let f = Fixture::new();
        f.tags();
        let _server = network_remote(&f);
        let refs = f.client.remote_refs(&f.info(), control()).unwrap();
        let plan = Arc::new(
            f.client
                .prepare_remote_tag_delete(&refs, "release/light")
                .unwrap(),
        );
        if !race {
            use std::os::unix::fs::PermissionsExt;
            let hook = f.server.join("hooks/update");
            fs::write(
                &hook,
                "#!/bin/sh\nprintf 'tag is protected\\n' >&2\nexit 1\n",
            )
            .unwrap();
            fs::set_permissions(hook, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let path = f.server.clone();
        let changed = Arc::new(Mutex::new(None));
        let slot = changed.clone();
        let sink: gitbuddy::git::ProgressSink = Arc::new(Mutex::new(move |phase: &str| {
            if race && phase.contains("waiting for server confirmation") {
                let r = git2::Repository::open_bare(&path).unwrap();
                let id = r.blob(b"moved after negotiation").unwrap();
                r.reference("refs/tags/release/light", id, true, "race")
                    .unwrap();
                *slot.lock().unwrap() = Some(id);
            }
        }));
        let result = f.client.execute_with_control(
            Operation::DeleteRemoteTag(plan),
            NetworkControl::new(Some(sink), CancellationToken::default()),
        );
        assert!(result.is_err());
        let r = git2::Repository::open_bare(&f.server).unwrap();
        let tag = r.find_reference("refs/tags/release/light").unwrap();
        if race {
            assert_eq!(tag.target(), *changed.lock().unwrap());
        }
    }
}
#[cfg(unix)]
#[test]
fn network_tag_delete_can_cancel_or_refuse_changed_config_during_negotiation() {
    for cancel_operation in [false, true] {
        let f = Fixture::new();
        f.tags();
        let _server = network_remote(&f);
        let refs = f.client.remote_refs(&f.info(), control()).unwrap();
        let plan = Arc::new(
            f.client
                .prepare_remote_tag_delete(&refs, "release/light")
                .unwrap(),
        );
        let token = CancellationToken::default();
        let cancelled = token.clone();
        let path = f.client.root.clone();
        let sink: gitbuddy::git::ProgressSink = Arc::new(Mutex::new(move |phase: &str| {
            if phase.contains("preparing deletion") {
                if cancel_operation {
                    cancelled.cancel();
                } else {
                    git2::Repository::open(&path)
                        .unwrap()
                        .config()
                        .unwrap()
                        .set_str("remote.origin.url", "changed")
                        .unwrap();
                }
            }
        }));
        let error = f
            .client
            .execute_with_control(
                Operation::DeleteRemoteTag(plan),
                NetworkControl::new(Some(sink), token),
            )
            .unwrap_err();
        assert_eq!(is_cancelled(&error), cancel_operation);
        assert!(
            git2::Repository::open_bare(&f.server)
                .unwrap()
                .find_reference("refs/tags/release/light")
                .is_ok()
        );
    }
}
#[test]
fn explicit_pull_revalidates_head_worktree_and_remote_config_after_fetch() {
    for case in 0..3 {
        let f = Fixture::new();
        let tip = f.publish("topic", "topic", "topic");
        let preview = Arc::new(f.client.prepare_pull_from(&f.info(), "topic").unwrap());
        let path = f.client.root.clone();
        let original = f.base.clone();
        let sink: gitbuddy::git::ProgressSink = Arc::new(Mutex::new(move |phase: &str| {
            if phase.contains("reviewing fetched upstream") {
                let r = git2::Repository::open(&path).unwrap();
                if case == 0 {
                    fs::write(path.join("base"), "preserve changed work").unwrap();
                } else if case == 1 {
                    r.find_reference("refs/heads/main")
                        .unwrap()
                        .set_target(git2::Oid::from_str(&tip).unwrap(), "external move")
                        .unwrap();
                } else {
                    r.config()
                        .unwrap()
                        .set_str(
                            "remote.origin.fetch",
                            "+refs/heads/*:refs/remotes/changed/*",
                        )
                        .unwrap();
                }
            }
        }));
        assert!(
            f.client
                .execute_with_control(
                    Operation::PullWith {
                        context: preview,
                        strategy: PullStrategy::Merge
                    },
                    NetworkControl::new(Some(sink), CancellationToken::default())
                )
                .is_err()
        );
        if case != 1 {
            assert_eq!(head(&f.client), original);
        }
        assert_eq!(raw(&f.client).state(), git2::RepositoryState::Clean);
        if case == 0 {
            assert_eq!(
                fs::read_to_string(f.client.root.join("base")).unwrap(),
                "preserve changed work"
            );
        }
    }
}

#[cfg(unix)]
#[test]
fn non_utf8_advertised_names_return_an_error_instead_of_unwinding_the_query() {
    let f = Fixture::new();
    // packed-refs can contain byte-oriented names even on filesystems that
    // refuse non-UTF-8 filenames (including the macOS test filesystem).
    let mut packed =
        format!("# pack-refs with: sorted\n{} refs/heads/non-utf8-", f.base).into_bytes();
    packed.extend_from_slice(b"\xff\n");
    fs::write(f.server.join("packed-refs"), packed).unwrap();
    let error = f.client.remote_refs(&f.info(), control()).unwrap_err();
    assert!(format!("{error:#}").contains("not valid UTF-8"));
    assert_eq!(head(&f.client), f.base);
}

#[test]
#[ignore = "requires network access to the public GitHub repository"]
fn github_https_read_only_remote_refs_smoke() {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();
    raw(&repo)
        .remote("origin", "https://github.com/catsonkeyboard/gitbuddy.git")
        .unwrap();
    let origin = repo
        .remote_info()
        .unwrap()
        .into_iter()
        .find(|r| r.name == "origin")
        .unwrap();
    let refs = repo.remote_refs(&origin, control()).unwrap();
    assert!(!refs.branches.is_empty());
}

#[test]
#[ignore = "requires GitHub SSH access and a configured SSH agent"]
fn github_ssh_authenticated_remote_refs_smoke() {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();
    raw(&repo)
        .remote("origin", "git@github.com:catsonkeyboard/gitbuddy.git")
        .unwrap();
    let origin = repo
        .remote_info()
        .unwrap()
        .into_iter()
        .find(|r| r.name == "origin")
        .unwrap();
    let refs = repo.remote_refs(&origin, control()).unwrap();
    assert!(!refs.branches.is_empty());
}

#[test]
#[ignore = "requires GitHub SSH-over-443 access, trusted host key and configured SSH agent"]
fn github_ssh_443_remote_refs_smoke() {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();
    raw(&repo)
        .remote(
            "origin",
            "ssh://git@ssh.github.com:443/catsonkeyboard/gitbuddy.git",
        )
        .unwrap();
    let origin = repo
        .remote_info()
        .unwrap()
        .into_iter()
        .find(|r| r.name == "origin")
        .unwrap();
    let refs = repo.remote_refs(&origin, control()).unwrap();
    assert!(!refs.branches.is_empty());
}

#[test]
#[ignore = "requires an authenticated private HTTPS remote supplied by GITBUDDY_TEST_PRIVATE_REMOTE"]
fn github_https_private_authenticated_refs_smoke() {
    let url =
        std::env::var("GITBUDDY_TEST_PRIVATE_REMOTE").expect("private test remote URL required");
    assert!(url.starts_with("https://github.com/"));
    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();
    let raw = raw(&repo);
    let mut cfg = raw.config().unwrap();
    cfg.set_str(
        "credential.https://github.com.helper",
        "!gh auth git-credential",
    )
    .unwrap();
    raw.remote("origin", &url).unwrap();
    let origin = repo
        .remote_info()
        .unwrap()
        .into_iter()
        .find(|r| r.name == "origin")
        .unwrap();
    let refs = repo.remote_refs(&origin, control()).unwrap();
    assert!(!refs.branches.is_empty());
    let branch = &refs.branches[0];
    let fetch = repo.prepare_fetch(&origin, Some(&branch.name)).unwrap();
    repo.execute(Operation::FetchFrom(Arc::new(fetch))).unwrap();
    assert!(
        raw.find_commit(git2::Oid::from_str(&branch.id).unwrap())
            .is_ok()
    );
}

#[test]
#[ignore = "writes and deletes unique QA references; requires GITBUDDY_TEST_WRITE_REMOTE"]
fn github_https_push_lease_tags_and_cleanup_smoke() -> anyhow::Result<()> {
    use gitbuddy::git::PushSelection;
    let url = std::env::var("GITBUDDY_TEST_WRITE_REMOTE")?;
    anyhow::ensure!(
        url == "https://github.com/catsonkeyboard/gitbuddy.git",
        "Use the dedicated project test endpoint"
    );
    let dir = tempfile::tempdir()?;
    let repo = Repository::init(dir.path())?;
    let r = raw(&repo);
    let mut cfg = r.config()?;
    cfg.set_str(
        "credential.https://github.com.helper",
        "!gh auth git-credential",
    )?;
    cfg.set_str("core.hooksPath", "/dev/null")?;
    cfg.set_bool("commit.gpgsign", false)?;
    repo.execute(Operation::SetIdentity(
        "catsonkeyboard".into(),
        "124647734+catsonkeyboard@users.noreply.github.com".into(),
    ))?;
    r.remote("origin", &url)?;
    let name = format!(
        "codex/gitbuddy-qa-{}",
        chrono::Utc::now().timestamp_nanos_opt().unwrap()
    );
    let tag = name.replace('/', "-");
    let first = commit(&repo, "fixture", "GitBuddy isolated remote QA\n");
    let origin = repo.remote_info()?.remove(0);
    let selection = |lease| PushSelection {
        remote: "origin".into(),
        branch: Some("main".into()),
        target: name.clone(),
        force_with_lease: lease,
        ..Default::default()
    };
    let result = (|| -> anyhow::Result<()> {
        repo.execute(Operation::PushTo(Arc::new(
            repo.prepare_push(selection(true))?,
        )))?;
        repo.execute(Operation::FetchFrom(Arc::new(
            repo.prepare_fetch(&origin, Some(&name))?,
        )))?;
        commit(&repo, "fixture", "Updated QA content\n");
        let stale = repo.prepare_push(selection(true))?;
        repo.execute(Operation::PushTo(Arc::new(
            repo.prepare_push(selection(false))?,
        )))?;
        anyhow::ensure!(
            repo.execute(Operation::PushTo(Arc::new(stale))).is_err(),
            "Stale lease was accepted"
        );
        repo.execute(Operation::FetchFrom(Arc::new(
            repo.prepare_fetch(&origin, Some(&name))?,
        )))?;
        r.reset(
            &r.find_object(first.parse()?, None)?,
            git2::ResetType::Hard,
            None,
        )?;
        repo.execute(Operation::PushTo(Arc::new(
            repo.prepare_push(selection(true))?,
        )))?;
        let refs = repo.remote_refs(&origin, control())?;
        anyhow::ensure!(
            refs.branches
                .iter()
                .any(|b| b.name == name && b.id == first),
            "Lease push result differs"
        );
        r.tag_lightweight(&tag, &r.find_object(first.parse()?, None)?, false)?;
        repo.execute(Operation::PushTo(Arc::new(repo.prepare_push(
            PushSelection {
                remote: "origin".into(),
                tags: vec![tag.clone()],
                ..Default::default()
            },
        )?)))?;
        anyhow::ensure!(
            repo.remote_refs(&origin, control())?
                .tags
                .iter()
                .any(|t| t.name == tag),
            "Tag push missing"
        );
        Ok(())
    })();
    // Query exact current IDs before deleting only this run's references.
    let refs = repo.remote_refs(&origin, control())?;
    if refs.tags.iter().any(|t| t.name == tag) {
        repo.execute(Operation::DeleteRemoteTag(Arc::new(
            repo.prepare_remote_tag_delete(&refs, &tag)?,
        )))?;
    }
    if refs.branches.iter().any(|b| b.name == name) {
        repo.execute(Operation::FetchFrom(Arc::new(
            repo.prepare_fetch(&origin, Some(&name))?,
        )))?;
        repo.execute(Operation::DeleteRemoteBranch(Arc::new(
            repo.prepare_remote_branch_delete(&format!("origin/{name}"))?,
        )))?;
    }
    let refs = repo.remote_refs(&origin, control())?;
    anyhow::ensure!(
        !refs.branches.iter().any(|b| b.name == name) && !refs.tags.iter().any(|t| t.name == tag),
        "QA references were not cleaned up"
    );
    result
}

#[test]
#[ignore = "uploads a tiny LFS fixture and creates/removes its own lock; needs official git-lfs and GITBUDDY_TEST_WRITE_REMOTE"]
fn github_lfs_transfer_and_lock_smoke() -> anyhow::Result<()> {
    let url = std::env::var("GITBUDDY_TEST_WRITE_REMOTE")?;
    anyhow::ensure!(
        url == "https://github.com/catsonkeyboard/gitbuddy.git",
        "Use the dedicated endpoint"
    );
    let dir = tempfile::tempdir()?;
    let repo = Repository::init(dir.path())?;
    let r = raw(&repo);
    let mut cfg = r.config()?;
    cfg.set_str(
        "credential.https://github.com.helper",
        "!gh auth git-credential",
    )?;
    cfg.set_str("core.hooksPath", "/dev/null")?;
    cfg.set_bool("commit.gpgsign", false)?;
    repo.execute(Operation::SetIdentity(
        "catsonkeyboard".into(),
        "124647734+catsonkeyboard@users.noreply.github.com".into(),
    ))?;
    r.remote("origin", &url)?;
    repo.execute(Operation::LfsTrack {
        pattern: "*.bin".into(),
        track: true,
    })?;
    let path = format!(
        "gitbuddy-qa-{}.bin",
        chrono::Utc::now().timestamp_nanos_opt().unwrap()
    );
    fs::write(
        repo.root.join(&path),
        b"GitBuddy isolated LFS network fixture\n",
    )?;
    repo.execute(Operation::StageAll)?;
    repo.execute(Operation::Commit("Isolated LFS network QA".into()))?;
    repo.execute(Operation::LfsPush("origin".into()))?;
    let file = repo
        .lfs_status()?
        .files
        .into_iter()
        .find(|f| f.path == std::path::Path::new(&path))
        .unwrap();
    let object = r
        .commondir()
        .join("lfs/objects")
        .join(&file.oid[..2])
        .join(&file.oid[2..4])
        .join(&file.oid);
    let backup = dir.path().join("cached-backup");
    fs::rename(&object, &backup)?;
    repo.execute(Operation::LfsFetch("origin".into()))?;
    anyhow::ensure!(
        fs::read(&object)? == fs::read(&backup)?,
        "Downloaded LFS object differs"
    );
    let result = repo.execute(Operation::LfsLock {
        remote: "origin".into(),
        path: path.clone().into(),
    });
    let locks = repo.lfs_locks("origin", control())?;
    if let Some(lock) = locks.ours.into_iter().find(|l| l.path == path) {
        repo.execute(Operation::LfsUnlock {
            remote: "origin".into(),
            lock,
            force: false,
        })?;
    }
    result?;
    anyhow::ensure!(
        !repo
            .lfs_locks("origin", control())?
            .ours
            .iter()
            .any(|l| l.path == path),
        "QA lock was not cleaned up"
    );
    Ok(())
}

#[cfg(unix)]
#[test]
fn selected_fetch_can_automatically_download_lfs_for_the_actual_fetched_commit() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new();
    let id = f.publish("topic", "new", "new remote content");
    let program = f.dir.path().join("fake-lfs");
    fs::write(&program,"#!/bin/sh\nif [ \"$1\" = version ]; then echo 'git-lfs/test'; exit 0; fi\n[ \"$1\" = fetch ] || exit 9\nprintf '%s\\n' \"$@\" > auto-lfs-args\n").unwrap();
    fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
    let r = raw(&f.client);
    let mut cfg = r.config().unwrap();
    cfg.set_bool("gitbuddy.lfsAutoFetch", true).unwrap();
    cfg.set_str("gitbuddy.lfsProgram", program.to_str().unwrap())
        .unwrap();
    f.fetch(Some("topic")).unwrap();
    assert_eq!(
        fs::read_to_string(f.client.root.join("auto-lfs-args")).unwrap(),
        format!("fetch\norigin\n{id}\n")
    );
    assert_eq!(
        r.find_reference("refs/remotes/origin/topic")
            .unwrap()
            .target()
            .unwrap()
            .to_string(),
        id
    );
}

#[cfg(unix)]
#[test]
fn automatic_lfs_failure_reports_that_git_refs_were_already_fetched() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new();
    let id = f.publish("topic", "new", "new");
    let program = f.dir.path().join("fail-lfs");
    fs::write(
        &program,
        "#!/bin/sh\necho deliberate-LFS-failure >&2\nexit 7\n",
    )
    .unwrap();
    fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
    let r = raw(&f.client);
    let mut cfg = r.config().unwrap();
    cfg.set_bool("gitbuddy.lfsAutoFetch", true).unwrap();
    cfg.set_str("gitbuddy.lfsProgram", program.to_str().unwrap())
        .unwrap();
    let error = f.fetch(Some("topic")).unwrap_err();
    assert!(format!("{error:#}").contains("Git refs were fetched"));
    assert!(format!("{error:#}").contains("deliberate-LFS-failure"));
    assert_eq!(
        r.find_reference("refs/remotes/origin/topic")
            .unwrap()
            .target()
            .unwrap()
            .to_string(),
        id
    );
}

#[cfg(unix)]
#[test]
fn automatic_lfs_network_phase_can_be_cancelled_after_git_ref_update() {
    let f = Fixture::new();
    let id = f.publish("topic", "new", "new");
    let r = raw(&f.client);
    r.config()
        .unwrap()
        .set_bool("gitbuddy.lfsAutoFetch", true)
        .unwrap();
    let token = CancellationToken::default();
    let requested = token.clone();
    let sink: gitbuddy::git::ProgressSink = Arc::new(Mutex::new(move |text: &str| {
        if text.starts_with("LFS:") {
            assert!(requested.cancel());
        }
    }));
    let plan = f.client.prepare_fetch(&f.info(), Some("topic")).unwrap();
    let error = f
        .client
        .execute_with_control(
            Operation::FetchFrom(Arc::new(plan)),
            NetworkControl::new(Some(sink), token),
        )
        .unwrap_err();
    assert!(is_cancelled(&error));
    assert_eq!(
        r.find_reference("refs/remotes/origin/topic")
            .unwrap()
            .target()
            .unwrap()
            .to_string(),
        id
    );
}

#[test]
#[ignore = "internal child fixture; launched by clone_can_download_lfs_after_installation"]
fn clone_auto_lfs_internal_child() {
    let config = std::env::var_os("GITBUDDY_CLONE_QA_CONFIG").unwrap();
    // This test process owns libgit2's global configuration search path.
    unsafe {
        git2::opts::set_search_path(
            git2::ConfigLevel::Global,
            std::path::Path::new(&config).parent().unwrap(),
        )
        .unwrap();
    }
    let source = std::env::var("GITBUDDY_CLONE_QA_SOURCE").unwrap();
    let destination = std::env::var("GITBUDDY_CLONE_QA_DEST").unwrap();
    let repo = Repository::clone_repo(&source, std::path::Path::new(&destination)).unwrap();
    let raw = raw(&repo);
    assert!(
        raw.config()
            .unwrap()
            .get_bool("gitbuddy.lfsAutoFetch")
            .unwrap(),
        "Clone did not inherit automatic LFS configuration"
    );
    assert!(raw.head().is_ok(), "Cloned HEAD is missing");
}

#[cfg(unix)]
#[test]
fn clone_can_download_lfs_after_installation() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new();
    let program = f.dir.path().join("clone-lfs");
    fs::write(
        &program,
        "#!/bin/sh\n[ \"$1\" = fetch ] || exit 9\nprintf '%s\\n' \"$@\" > clone-lfs-args\n",
    )
    .unwrap();
    fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
    let config = f.dir.path().join(".gitconfig");
    fs::write(&config, "").unwrap();
    let mut cfg = git2::Config::open(&config).unwrap();
    cfg.set_bool("gitbuddy.lfsAutoFetch", true).unwrap();
    cfg.set_str("gitbuddy.lfsProgram", program.to_str().unwrap())
        .unwrap();
    let destination = f.dir.path().join("auto-clone");
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "clone_auto_lfs_internal_child", "--ignored"])
        .env("GIT_CONFIG_GLOBAL", &config)
        .env("GITBUDDY_CLONE_QA_CONFIG", &config)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GITBUDDY_CLONE_QA_SOURCE", &f.source.root)
        .env("GITBUDDY_CLONE_QA_DEST", &destination)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(destination.join("clone-lfs-args")).unwrap(),
        format!("fetch\norigin\n{}\n", f.base)
    );
    assert!(destination.join(".git").is_dir());
}
