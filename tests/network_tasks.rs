use gitbuddy::git::{
    CancellationToken, NetworkControl, Operation, ProgressSink, Repository, is_cancelled,
};
use std::{
    fs,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};
use tempfile::TempDir;

struct Fixture {
    dir: TempDir,
    source: Repository,
    remote: PathBuf,
    local: Repository,
}
fn commit(repo: &Repository, name: &str) {
    fs::write(repo.root.join("file.txt"), format!("{name}\n")).unwrap();
    repo.execute(Operation::StageAll).unwrap();
    repo.execute(Operation::Commit(name.into())).unwrap();
}
fn identity(repo: &Repository) {
    repo.execute(Operation::SetIdentity(
        "Network tests".into(),
        "test@example.invalid".into(),
    ))
    .unwrap();
}
impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let source = Repository::init(&dir.path().join("source")).unwrap();
        identity(&source);
        commit(&source, "base");
        let remote = dir.path().join("remote.git");
        let bare = git2::Repository::init_bare(&remote).unwrap();
        bare.set_head("refs/heads/main").unwrap();
        git2::Repository::open(&source.root)
            .unwrap()
            .remote("origin", remote.to_str().unwrap())
            .unwrap();
        source.execute(Operation::Push).unwrap();
        let local =
            Repository::clone_repo(remote.to_str().unwrap(), &dir.path().join("local")).unwrap();
        identity(&local);
        Self {
            dir,
            source,
            remote,
            local,
        }
    }
    fn advance_remote(&self) {
        for i in 0..64 {
            fs::write(
                self.source.root.join(format!("extra-{i}.txt")),
                format!("{i} {}\n", "content ".repeat(400)),
            )
            .unwrap();
        }
        commit(&self.source, "remote change");
        self.source.execute(Operation::Push).unwrap();
    }
    fn remote_head(&self) -> git2::Oid {
        git2::Repository::open_bare(&self.remote)
            .unwrap()
            .head()
            .unwrap()
            .target()
            .unwrap()
    }
}
fn head(repo: &Repository) -> String {
    repo.snapshot(10).unwrap().head_id.unwrap()
}
fn at_phase(phase: &'static str, action: impl FnMut(&str) + Send + 'static) -> ProgressSink {
    let mut action = action;
    Arc::new(Mutex::new(move |text: &str| {
        if text.starts_with(phase) {
            action(text);
        }
    }))
}
fn cancelling(
    phase: &'static str,
    token: &CancellationToken,
    called: &Arc<AtomicBool>,
) -> NetworkControl {
    let cancel = token.clone();
    let called = called.clone();
    NetworkControl::new(
        Some(at_phase(phase, move |_| {
            called.store(true, Ordering::SeqCst);
            assert!(cancel.cancel());
        })),
        token.clone(),
    )
}

#[test]
fn precancelled_network_operations_leave_existing_repository_untouched() {
    let f = Fixture::new();
    f.advance_remote();
    let before = f.local.fingerprint().unwrap();
    let remote = f.remote_head();
    for op in [Operation::Fetch, Operation::Pull, Operation::Push] {
        let token = CancellationToken::default();
        assert!(token.cancel());
        let error = f
            .local
            .execute_with_control(op, NetworkControl::new(None, token))
            .unwrap_err();
        assert!(is_cancelled(&error), "{error:#}");
        assert_eq!(f.local.fingerprint().unwrap(), before);
        assert_eq!(f.remote_head(), remote);
    }
    let token = CancellationToken::default();
    token.cancel();
    let dest = f.dir.path().join("new-parent/destination");
    let error = Repository::clone_repo_with_control(
        f.remote.to_str().unwrap(),
        &dest,
        NetworkControl::new(None, token),
    )
    .unwrap_err();
    assert!(is_cancelled(&error));
    assert!(!dest.parent().unwrap().exists());
}

#[test]
fn fetch_and_pull_cancel_during_transfer_without_updating_worktree_or_head() {
    for operation in [Operation::Fetch, Operation::Pull] {
        let f = Fixture::new();
        f.advance_remote();
        let before_head = head(&f.local);
        let index_path = git2::Repository::open(&f.local.root)
            .unwrap()
            .path()
            .join("index");
        let before_index = fs::read(&index_path).unwrap();
        let token = CancellationToken::default();
        let called = Arc::new(AtomicBool::new(false));
        let error = f
            .local
            .execute_with_control(operation, cancelling("Transfer:", &token, &called))
            .unwrap_err();
        assert!(called.load(Ordering::SeqCst), "no transfer callback");
        assert!(is_cancelled(&error), "{error:#}");
        assert_eq!(head(&f.local), before_head);
        assert_eq!(fs::read(index_path).unwrap(), before_index);
        assert_eq!(
            fs::read_to_string(f.local.root.join("file.txt")).unwrap(),
            "base\n"
        );
        assert!(!f.local.root.join("extra-0.txt").exists());
        f.local.execute(Operation::Pull).unwrap();
        assert_eq!(head(&f.local), head(&f.source));
    }
}

#[test]
fn pull_rejects_late_cancellation_and_completes_fast_forward() {
    let f = Fixture::new();
    f.advance_remote();
    let token = CancellationToken::default();
    let cancel = token.clone();
    let called = Arc::new(AtomicBool::new(false));
    let observed = called.clone();
    let progress = at_phase("Pull: applying", move |_| {
        observed.store(true, Ordering::SeqCst);
        assert!(cancel.is_finishing());
        assert!(!cancel.cancel());
    });
    f.local
        .execute_with_control(Operation::Pull, NetworkControl::new(Some(progress), token))
        .unwrap();
    assert!(called.load(Ordering::SeqCst));
    assert_eq!(head(&f.local), head(&f.source));
}

#[test]
fn push_can_cancel_before_upload_but_not_after_upload_starts() {
    let f = Fixture::new();
    commit(&f.local, "local change");
    let before = f.remote_head();
    let token = CancellationToken::default();
    let called = Arc::new(AtomicBool::new(false));
    let error = f
        .local
        .execute_with_control(
            Operation::Push,
            cancelling("Push: preparing upload", &token, &called),
        )
        .unwrap_err();
    assert!(called.load(Ordering::SeqCst));
    assert!(is_cancelled(&error), "{error:#}");
    assert_eq!(f.remote_head(), before);
    let token = CancellationToken::default();
    let cancel = token.clone();
    let called = Arc::new(AtomicBool::new(false));
    let observed = called.clone();
    let progress = at_phase("Push: uploading", move |_| {
        observed.store(true, Ordering::SeqCst);
        assert!(!cancel.cancel());
        assert!(cancel.is_finishing());
    });
    f.local
        .execute_with_control(Operation::Push, NetworkControl::new(Some(progress), token))
        .unwrap();
    assert!(called.load(Ordering::SeqCst));
    assert_eq!(f.remote_head().to_string(), head(&f.local));
}

#[test]
fn cancelled_clone_cleans_transfer_and_partial_checkout() {
    let f = Fixture::new();
    f.advance_remote();
    for phase in ["Transfer:", "Clone: checking out"] {
        let destination = f.dir.path().join("cancelled-clone");
        let before: Vec<_> = fs::read_dir(f.dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        let token = CancellationToken::default();
        let called = Arc::new(AtomicBool::new(false));
        let error = Repository::clone_repo_with_control(
            f.remote.to_str().unwrap(),
            &destination,
            cancelling(phase, &token, &called),
        )
        .unwrap_err();
        assert!(called.load(Ordering::SeqCst), "missing callback: {phase}");
        assert!(is_cancelled(&error), "{error:#}");
        assert!(!destination.exists());
        let after: Vec<_> = fs::read_dir(f.dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(after.len(), before.len(), "temporary clone was not cleaned");
    }
}

#[test]
fn clone_never_deletes_preexisting_or_concurrently_added_files() {
    let f = Fixture::new();
    let destination = f.dir.path().join("protected");
    fs::create_dir(&destination).unwrap();
    fs::write(destination.join("keep.txt"), "keep").unwrap();
    assert!(Repository::clone_repo(f.remote.to_str().unwrap(), &destination).is_err());
    assert_eq!(
        fs::read_to_string(destination.join("keep.txt")).unwrap(),
        "keep"
    );
    let destination = f.dir.path().join("concurrent");
    let path = destination.clone();
    let progress = at_phase("Clone: connecting", move |_| {
        fs::write(path.join("keep.txt"), "concurrent").unwrap();
    });
    assert!(
        Repository::clone_repo_with_control(
            f.remote.to_str().unwrap(),
            &destination,
            NetworkControl::new(Some(progress), CancellationToken::default())
        )
        .is_err()
    );
    assert_eq!(
        fs::read_to_string(destination.join("keep.txt")).unwrap(),
        "concurrent"
    );
    assert!(!destination.join(".git").exists());
}

#[test]
fn an_existing_empty_clone_directory_is_preserved_on_cancel_and_reusable() {
    let f = Fixture::new();
    let destination = f.dir.path().join("existing-empty");
    fs::create_dir(&destination).unwrap();
    let token = CancellationToken::default();
    let called = Arc::new(AtomicBool::new(false));
    let error = Repository::clone_repo_with_control(
        f.remote.to_str().unwrap(),
        &destination,
        cancelling("Clone: connecting", &token, &called),
    )
    .unwrap_err();
    assert!(is_cancelled(&error));
    assert!(called.load(Ordering::SeqCst));
    assert!(destination.is_dir());
    assert!(fs::read_dir(&destination).unwrap().next().is_none());
    let cloned = Repository::clone_repo(f.remote.to_str().unwrap(), &destination).unwrap();
    assert_eq!(head(&cloned), head(&f.source));
}

#[test]
fn completed_clone_publishes_repository_only_after_cancellation_is_sealed() {
    let f = Fixture::new();
    let destination = f.dir.path().join("finished");
    let token = CancellationToken::default();
    let cancel = token.clone();
    let path = destination.clone();
    let called = Arc::new(AtomicBool::new(false));
    let observed = called.clone();
    let progress = at_phase("Clone: installing", move |_| {
        observed.store(true, Ordering::SeqCst);
        assert!(!cancel.cancel());
        assert!(!path.join(".git").exists());
    });
    let cloned = Repository::clone_repo_with_control(
        f.remote.to_str().unwrap(),
        &destination,
        NetworkControl::new(Some(progress), token),
    )
    .unwrap();
    assert!(called.load(Ordering::SeqCst));
    assert_eq!(cloned.root, destination.canonicalize().unwrap());
    assert_eq!(head(&cloned), head(&f.source));
}

#[test]
fn cancelling_a_network_task_does_not_block_another_repository_commit() {
    let f = Fixture::new();
    f.advance_remote();
    let independent = Repository::init(&f.dir.path().join("independent")).unwrap();
    identity(&independent);
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let token = CancellationToken::default();
    let control = NetworkControl::new(
        Some(at_phase("Fetch: connecting", move |_| {
            started_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        })),
        token.clone(),
    );
    let local = f.local.clone();
    let worker = std::thread::spawn(move || local.execute_with_control(Operation::Fetch, control));
    started_rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .unwrap();
    commit(&independent, "independent commit");
    assert_eq!(
        independent.snapshot(10).unwrap().commits[0].subject,
        "independent commit"
    );
    assert!(token.cancel());
    release_tx.send(()).unwrap();
    assert!(is_cancelled(&worker.join().unwrap().unwrap_err()));
}

#[test]
fn cancellation_waits_for_a_blocked_transport_to_return() {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    let f = Fixture::new();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/delayed.git", listener.local_addr().unwrap());
    git2::Repository::open(&f.local.root)
        .unwrap()
        .remote_set_url("origin", &url)
        .unwrap();
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        let mut request = [0u8; 4096];
        assert!(stream.read(&mut request).unwrap() > 0);
        started_tx.send(()).unwrap();
        release_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        let _ = stream
            .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
    });
    let token = CancellationToken::default();
    let control = NetworkControl::new(None, token.clone());
    let local = f.local.clone();
    let worker = std::thread::spawn(move || local.execute_with_control(Operation::Fetch, control));
    started_rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .unwrap();
    assert!(token.cancel());
    assert!(
        !worker.is_finished(),
        "cancellation must not report completion before transport stops"
    );
    release_tx.send(()).unwrap();
    assert!(is_cancelled(&worker.join().unwrap().unwrap_err()));
    server.join().unwrap();
}
