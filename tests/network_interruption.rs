//! Actual loopback transport faults. The daemon exposes upload-pack only;
//! fixture publication writes temporary bare objects directly, never pushes.
#![cfg(unix)]
use gitbuddy::git::{
    CancellationToken, NetworkControl, Operation, ProgressSink, PullStrategy, RemoteInfo,
    Repository, is_cancelled,
};
use std::{
    fs,
    io::{Read, Write},
    net::{Shutdown, SocketAddr, TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

const FAULT_AFTER: usize = 128 * 1024;
const PAYLOAD_BYTES: usize = 4 * 1024 * 1024;

struct Daemon(Child, SocketAddr);
impl Daemon {
    fn start(root: &Path) -> Self {
        let reservation = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = reservation.local_addr().unwrap();
        drop(reservation);
        let empty = root.join("empty-git-config");
        fs::write(&empty, "").unwrap();
        let mut command = Command::new("/usr/bin/git");
        command
            .args([
                "daemon",
                "--reuseaddr",
                "--export-all",
                "--disable=receive-pack",
                "--disable=upload-archive",
                "--listen=127.0.0.1",
            ])
            .arg(format!("--port={}", address.port()))
            .arg(format!("--base-path={}", root.display()))
            .current_dir(root)
            .env_clear()
            .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", &empty)
            .env("GIT_CONFIG_SYSTEM", &empty)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        use std::os::unix::process::CommandExt;
        command.process_group(0);
        let mut daemon = Self(command.spawn().unwrap(), address);
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            assert!(
                daemon.0.try_wait().unwrap().is_none(),
                "loopback git daemon stopped"
            );
            if TcpStream::connect(address).is_ok() {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "loopback git daemon did not start"
            );
            thread::sleep(Duration::from_millis(10));
        }
        daemon
    }
}
impl Drop for Daemon {
    fn drop(&mut self) {
        unsafe {
            libc::kill(-(self.0.id() as i32), libc::SIGKILL);
        }
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Fault {
    Pass,
    Cut,
    Pause,
}
#[derive(Debug)]
struct WireEvent {
    received: usize,
    sent: usize,
}
struct Proxy {
    address: SocketAddr,
    mode: Arc<(Mutex<Fault>, Condvar)>,
    stop: Arc<AtomicBool>,
    connections: Arc<AtomicUsize>,
    events: mpsc::Receiver<WireEvent>,
    worker: Option<thread::JoinHandle<()>>,
}
impl Proxy {
    fn new(upstream: SocketAddr, fault: Fault) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let mode = Arc::new((Mutex::new(fault), Condvar::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let connections = Arc::new(AtomicUsize::new(0));
        let (events_tx, events) = mpsc::channel();
        let worker_mode = mode.clone();
        let worker_stop = stop.clone();
        let worker_connections = connections.clone();
        let worker = thread::spawn(move || {
            while !worker_stop.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((client, _)) => {
                        worker_connections.fetch_add(1, Ordering::SeqCst);
                        if let Ok(server) =
                            TcpStream::connect_timeout(&upstream, Duration::from_secs(3))
                        {
                            forward(client, server, &worker_mode, &worker_stop, &events_tx);
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2))
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            address,
            mode,
            stop,
            connections,
            events,
            worker: Some(worker),
        }
    }
    fn url(&self, name: &str) -> String {
        format!("git://{}/{name}.git", self.address)
    }
    fn set(&self, mode: Fault) {
        *self.mode.0.lock().unwrap() = mode;
        self.mode.1.notify_all();
    }
    fn reached(&self) -> WireEvent {
        self.events
            .recv_timeout(Duration::from_secs(15))
            .expect("real pack traffic did not reach the fault boundary")
    }
}
impl Drop for Proxy {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.mode.1.notify_all();
        if let Some(worker) = self.worker.take() {
            worker.join().unwrap();
        }
    }
}
fn retryable(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::WouldBlock
            | std::io::ErrorKind::TimedOut
            | std::io::ErrorKind::Interrupted
    )
}
fn forward(
    mut client: TcpStream,
    mut server: TcpStream,
    mode: &Arc<(Mutex<Fault>, Condvar)>,
    stop: &Arc<AtomicBool>,
    events: &mpsc::Sender<WireEvent>,
) {
    for stream in [&client, &server] {
        // BSD accept() inherits O_NONBLOCK from the listener. Full write_all
        // forwarding needs blocking streams; only accept itself is polled.
        stream.set_nonblocking(false).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_millis(50)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        stream.set_nodelay(true).unwrap();
    }
    let done = Arc::new(AtomicBool::new(false));
    let sent = Arc::new(AtomicUsize::new(0));
    let mut client_reader = client.try_clone().unwrap();
    let mut server_writer = server.try_clone().unwrap();
    let outbound_done = done.clone();
    let outbound_stop = stop.clone();
    let outbound_sent = sent.clone();
    let outbound = thread::spawn(move || {
        let mut buffer = [0; 8192];
        while !outbound_stop.load(Ordering::SeqCst) && !outbound_done.load(Ordering::SeqCst) {
            match client_reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => {
                    if server_writer.write_all(&buffer[..n]).is_err() {
                        break;
                    }
                    outbound_sent.fetch_add(n, Ordering::SeqCst);
                }
                Err(e) if retryable(&e) => continue,
                Err(_) => break,
            }
        }
    });
    let mut received = 0;
    let mut limited = false;
    let mut buffer = [0; 8192];
    'connection: while !stop.load(Ordering::SeqCst) {
        let n = match server.read(&mut buffer) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if retryable(&e) => continue,
            Err(_) => break,
        };
        if !limited && received + n >= FAULT_AFTER && *mode.0.lock().unwrap() != Fault::Pass {
            let prefix = FAULT_AFTER - received;
            if client.write_all(&buffer[..prefix]).is_err() {
                break;
            }
            received += prefix;
            let _ = events.send(WireEvent {
                received,
                sent: sent.load(Ordering::SeqCst),
            });
            let deadline = Instant::now() + Duration::from_secs(15);
            let mut fault = mode.0.lock().unwrap();
            while *fault == Fault::Pause && !stop.load(Ordering::SeqCst) {
                if Instant::now() >= deadline {
                    break 'connection;
                }
                fault = mode
                    .1
                    .wait_timeout(fault, Duration::from_millis(50))
                    .unwrap()
                    .0;
            }
            if *fault == Fault::Cut || stop.load(Ordering::SeqCst) {
                break;
            }
            drop(fault);
            limited = true;
            if client.write_all(&buffer[prefix..n]).is_err() {
                break;
            }
            received += n - prefix;
        } else {
            if client.write_all(&buffer[..n]).is_err() {
                break;
            }
            received += n;
        }
    }
    done.store(true, Ordering::SeqCst);
    let _ = client.shutdown(Shutdown::Both);
    let _ = server.shutdown(Shutdown::Both);
    outbound.join().unwrap();
}

struct Fixture {
    daemon: Daemon,
    dir: tempfile::TempDir,
    remote: PathBuf,
    local: Repository,
    base: String,
    tip: String,
}
fn commit_bare(repo: &git2::Repository, payload: Option<(u64, &str)>) -> String {
    let parent = repo.head().ok().and_then(|h| h.peel_to_commit().ok());
    let parent_tree = parent.as_ref().map(|c| c.tree().unwrap());
    let mut tree = repo.treebuilder(parent_tree.as_ref()).unwrap();
    if let Some((mut state, name)) = payload {
        // Deterministic, incompressible bytes ensure the cut occurs inside a
        // real multi-megabyte pack rather than merely its advertisement.
        let bytes: Vec<_> = (0..PAYLOAD_BYTES)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                (state >> 24) as u8
            })
            .collect();
        tree.insert(name, repo.blob(&bytes).unwrap(), 0o100644)
            .unwrap();
    } else {
        tree.insert("note", repo.blob(b"base\n").unwrap(), 0o100644)
            .unwrap();
    }
    let tree = repo.find_tree(tree.write().unwrap()).unwrap();
    let signature = git2::Signature::new(
        "Loopback fixture",
        "loopback@example.invalid",
        &git2::Time::new(1_800_000_000, 0),
    )
    .unwrap();
    repo.commit(
        Some("HEAD"),
        &signature,
        &signature,
        if payload.is_some() {
            "socket payload"
        } else {
            "base"
        },
        &tree,
        &parent.iter().collect::<Vec<_>>(),
    )
    .unwrap()
    .to_string()
}
fn configure(repo: &Repository, url: &str) {
    let raw = git2::Repository::open(&repo.root).unwrap();
    raw.remote_set_url("origin", url).unwrap();
    let mut config = raw.config().unwrap();
    config.set_bool("gitbuddy.lfsAutoFetch", false).unwrap();
    config.set_str("core.hooksPath", "/dev/null").unwrap();
}
impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let remote = dir.path().join("remote.git");
        let bare = git2::Repository::init_bare(&remote).unwrap();
        bare.set_head("refs/heads/main").unwrap();
        let base = commit_bare(&bare, None);
        let local_path = dir.path().join("local");
        git2::Repository::clone(remote.to_str().unwrap(), &local_path).unwrap();
        let local = Repository::open(&local_path).unwrap();
        let tip = commit_bare(&bare, Some((0x1234abcd9876fedc, "payload.bin")));
        let daemon = Daemon::start(dir.path());
        Self {
            daemon,
            dir,
            remote,
            local,
            base,
            tip,
        }
    }
    fn proxy(&self, fault: Fault) -> Proxy {
        let proxy = Proxy::new(self.daemon.1, fault);
        configure(&self.local, &proxy.url("remote"));
        proxy
    }
    fn server_tip(&self) -> String {
        git2::Repository::open_bare(&self.remote)
            .unwrap()
            .head()
            .unwrap()
            .target()
            .unwrap()
            .to_string()
    }
}
fn info(repo: &Repository, name: &str) -> RemoteInfo {
    repo.remote_info()
        .unwrap()
        .into_iter()
        .find(|r| r.name == name)
        .unwrap()
}
fn fetch(repo: &Repository) -> Operation {
    Operation::FetchFrom(Arc::new(
        repo.prepare_fetch(&info(repo, "origin"), Some("main"))
            .unwrap(),
    ))
}
fn tracking(repo: &Repository, remote: &str) -> String {
    git2::Repository::open(&repo.root)
        .unwrap()
        .refname_to_id(&format!("refs/remotes/{remote}/main"))
        .unwrap()
        .to_string()
}
#[derive(PartialEq, Eq, Debug)]
struct LocalState {
    head: Vec<u8>,
    tip: String,
    index: Vec<u8>,
    note: Vec<u8>,
    untracked: Option<Vec<u8>>,
    entries: Vec<std::ffi::OsString>,
}
fn state(repo: &Repository) -> LocalState {
    let raw = git2::Repository::open(&repo.root).unwrap();
    let mut entries: Vec<_> = fs::read_dir(&repo.root)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .filter(|name| name != ".git")
        .collect();
    entries.sort();
    LocalState {
        head: fs::read(raw.path().join("HEAD")).unwrap(),
        tip: raw.head().unwrap().target().unwrap().to_string(),
        index: fs::read(raw.path().join("index")).unwrap(),
        note: fs::read(repo.root.join("note")).unwrap(),
        untracked: fs::read(repo.root.join("untracked")).ok(),
        entries,
    }
}
fn dirty(repo: &Repository) {
    fs::write(repo.root.join("note"), "staged\n").unwrap();
    repo.execute(Operation::StageAll).unwrap();
    fs::write(repo.root.join("note"), "unstaged\n").unwrap();
    fs::write(repo.root.join("untracked"), b"keep\0bytes").unwrap();
}
fn job<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> mpsc::Receiver<T> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let _ = tx.send(work());
    });
    rx
}
fn finish<T>(result: mpsc::Receiver<T>) -> T {
    result
        .recv_timeout(Duration::from_secs(15))
        .expect("application task did not finish after the socket resumed/stopped")
}
fn transfer_observer(token: CancellationToken) -> (NetworkControl, Arc<AtomicBool>) {
    let received = Arc::new(AtomicBool::new(false));
    let observed = received.clone();
    let sink: ProgressSink = Arc::new(Mutex::new(move |text: &str| {
        if text.starts_with("Transfer:") {
            observed.store(true, Ordering::SeqCst);
        }
    }));
    (NetworkControl::new(Some(sink), token), received)
}

#[test]
fn a_real_pack_socket_cut_fails_fetch_without_changing_local_work_and_retry_succeeds() {
    let f = Fixture::new();
    let proxy = f.proxy(Fault::Cut);
    dirty(&f.local);
    let before = state(&f.local);
    let operation = fetch(&f.local);
    let (control, transferred) = transfer_observer(Default::default());
    let error = f
        .local
        .execute_with_control(operation.clone(), control)
        .unwrap_err();
    let wire = proxy.reached();
    assert_eq!(wire.received, FAULT_AFTER);
    assert!(wire.sent > 0);
    assert!(
        transferred.load(Ordering::SeqCst),
        "socket cut preceded pack callbacks"
    );
    assert!(
        !is_cancelled(&error),
        "a socket fault must not pretend cancellation"
    );
    assert_eq!(state(&f.local), before);
    assert_eq!(tracking(&f.local, "origin"), f.base);
    proxy.set(Fault::Pass);
    f.local.execute(operation).unwrap();
    assert_eq!(tracking(&f.local, "origin"), f.tip);
    assert_eq!(state(&f.local), before);
    assert_eq!(f.server_tip(), f.tip);
}

#[test]
fn cancelling_an_inflight_socket_transfer_returns_cancelled_and_is_safely_retryable() {
    let f = Fixture::new();
    let proxy = f.proxy(Fault::Pause);
    dirty(&f.local);
    let before = state(&f.local);
    let operation = fetch(&f.local);
    let retried = operation.clone();
    let token = CancellationToken::default();
    let (control, transferred) = transfer_observer(token.clone());
    let local = f.local.clone();
    let result = job(move || local.execute_with_control(operation, control));
    let wire = proxy.reached();
    assert_eq!(wire.received, FAULT_AFTER);
    assert!(wire.sent > 0);
    let deadline = Instant::now() + Duration::from_secs(5);
    while !transferred.load(Ordering::SeqCst) {
        assert!(
            Instant::now() < deadline,
            "native transfer callback did not observe the paused pack"
        );
        thread::sleep(Duration::from_millis(1));
    }
    assert!(token.cancel());
    proxy.set(Fault::Pass);
    let error = finish(result).unwrap_err();
    assert!(is_cancelled(&error), "{error:#}");
    assert!(transferred.load(Ordering::SeqCst));
    assert_eq!(tracking(&f.local, "origin"), f.base);
    assert_eq!(state(&f.local), before);
    f.local.execute(retried).unwrap();
    assert_eq!(tracking(&f.local, "origin"), f.tip);
    assert_eq!(state(&f.local), before);
}

#[test]
fn a_later_remote_socket_failure_retains_only_the_tracking_refs_already_fetched() {
    let f = Fixture::new();
    let good = f.proxy(Fault::Pass);
    let other_path = f.dir.path().join("other.git");
    let other = git2::Repository::init_bare(&other_path).unwrap();
    other.set_head("refs/heads/main").unwrap();
    assert_eq!(commit_bare(&other, None), f.base);
    let other_tip = commit_bare(&other, Some((0xfedcba9876543210, "different.bin")));
    let bad = Proxy::new(f.daemon.1, Fault::Cut);
    let raw = git2::Repository::open(&f.local.root).unwrap();
    raw.remote_rename("origin", "a-ready").unwrap();
    raw.remote("z-cut", &bad.url("other")).unwrap();
    raw.reference(
        "refs/remotes/z-cut/main",
        f.base.parse().unwrap(),
        false,
        "fixture baseline",
    )
    .unwrap();
    dirty(&f.local);
    let before = state(&f.local);
    let error = f.local.execute(Operation::Fetch).unwrap_err();
    assert!(!is_cancelled(&error));
    assert_eq!(bad.reached().received, FAULT_AFTER);
    assert!(good.connections.load(Ordering::SeqCst) > 0);
    assert_eq!(tracking(&f.local, "a-ready"), f.tip);
    assert_eq!(tracking(&f.local, "z-cut"), f.base);
    assert_eq!(state(&f.local), before);
    bad.set(Fault::Pass);
    f.local.execute(Operation::Fetch).unwrap();
    assert_eq!(tracking(&f.local, "z-cut"), other_tip);
    assert_eq!(tracking(&f.local, "a-ready"), f.tip);
    assert_eq!(state(&f.local), before);
}

#[test]
fn failed_network_clones_preserve_existing_and_concurrently_populated_destinations() {
    for (existing, add_during_transfer) in [(false, false), (true, false), (false, true)] {
        let f = Fixture::new();
        let proxy = Proxy::new(f.daemon.1, Fault::Pause);
        let destination = f.dir.path().join("destination");
        if existing {
            fs::create_dir(&destination).unwrap();
        }
        let siblings = || {
            let mut names: Vec<_> = fs::read_dir(f.dir.path())
                .unwrap()
                .map(|e| e.unwrap().file_name())
                .filter(|n| n != "destination")
                .collect();
            names.sort();
            names
        };
        let before_siblings = siblings();
        let cloned_path = destination.clone();
        let url = proxy.url("remote");
        let result = job(move || {
            Repository::clone_repo_with_control(&url, &cloned_path, NetworkControl::default())
        });
        assert_eq!(proxy.reached().received, FAULT_AFTER);
        if add_during_transfer {
            fs::write(
                destination.join("keep"),
                "created while clone was transferring",
            )
            .unwrap();
        }
        proxy.set(Fault::Cut);
        assert!(finish(result).is_err());
        assert!(!destination.join(".git").exists());
        if add_during_transfer {
            assert_eq!(
                fs::read(destination.join("keep")).unwrap(),
                b"created while clone was transferring"
            );
        } else if existing {
            assert!(destination.is_dir());
            assert!(fs::read_dir(&destination).unwrap().next().is_none());
        } else {
            assert!(!destination.exists());
        }
        assert_eq!(
            siblings(),
            before_siblings,
            "owned staging clone was not removed"
        );
        if !add_during_transfer {
            proxy.set(Fault::Pass);
            let cloned = Repository::clone_repo(&proxy.url("remote"), &destination).unwrap();
            assert_eq!(
                git2::Repository::open(cloned.root)
                    .unwrap()
                    .head()
                    .unwrap()
                    .target()
                    .unwrap()
                    .to_string(),
                f.tip
            );
        }
        let protected = f.dir.path().join("protected");
        fs::create_dir(&protected).unwrap();
        fs::write(protected.join("keep"), "existing").unwrap();
        let connections = proxy.connections.load(Ordering::SeqCst);
        assert!(Repository::clone_repo(&proxy.url("remote"), &protected).is_err());
        assert_eq!(fs::read(protected.join("keep")).unwrap(), b"existing");
        assert_eq!(proxy.connections.load(Ordering::SeqCst), connections);
    }
}

#[test]
fn cancelling_one_socket_transfer_does_not_cancel_another_repository() {
    let f = Fixture::new();
    let paused = f.proxy(Fault::Pause);
    let healthy = Proxy::new(f.daemon.1, Fault::Pass);
    let independent_path = f.dir.path().join("independent");
    // Seed the same old graph locally so both actual sockets still need the new pack.
    let base_path = f.dir.path().join("base.git");
    let base = git2::Repository::init_bare(&base_path).unwrap();
    base.set_head("refs/heads/main").unwrap();
    assert_eq!(commit_bare(&base, None), f.base);
    git2::Repository::clone(base_path.to_str().unwrap(), &independent_path).unwrap();
    let independent = Repository::open(independent_path).unwrap();
    configure(&independent, &healthy.url("remote"));
    let before_primary = state(&f.local);
    let before_other = state(&independent);
    let token = CancellationToken::default();
    let independent_token = CancellationToken::default();
    let local = f.local.clone();
    let operation = fetch(&local);
    let cancel = token.clone();
    let first =
        job(move || local.execute_with_control(operation, NetworkControl::new(None, cancel)));
    assert_eq!(paused.reached().received, FAULT_AFTER);
    let other = independent.clone();
    let operation = fetch(&other);
    let other_control = independent_token.clone();
    let second = job(move || {
        other.execute_with_control(operation, NetworkControl::new(None, other_control))
    });
    assert!(token.cancel());
    paused.set(Fault::Pass);
    assert!(is_cancelled(&finish(first).unwrap_err()));
    finish(second).unwrap();
    assert!(independent_token.is_finishing());
    assert!(!independent_token.is_requested());
    assert_eq!(tracking(&f.local, "origin"), f.base);
    assert_eq!(tracking(&independent, "origin"), f.tip);
    assert_eq!(state(&f.local), before_primary);
    assert_eq!(state(&independent), before_other);
}

#[test]
fn an_interrupted_pull_does_not_integrate_until_a_successful_network_retry() {
    let f = Fixture::new();
    let proxy = f.proxy(Fault::Cut);
    let before = state(&f.local);
    let plan = Arc::new(
        f.local
            .prepare_pull_from(&info(&f.local, "origin"), "main")
            .unwrap(),
    );
    let operation = Operation::PullWith {
        context: plan,
        strategy: PullStrategy::FastForwardOnly,
    };
    assert!(f.local.execute(operation.clone()).is_err());
    assert_eq!(proxy.reached().received, FAULT_AFTER);
    assert_eq!(state(&f.local), before);
    assert_eq!(tracking(&f.local, "origin"), f.base);
    assert!(!f.local.root.join("payload.bin").exists());
    proxy.set(Fault::Pass);
    f.local.execute(operation).unwrap();
    assert_eq!(
        git2::Repository::open(&f.local.root)
            .unwrap()
            .head()
            .unwrap()
            .target()
            .unwrap()
            .to_string(),
        f.tip
    );
    assert_eq!(
        fs::metadata(f.local.root.join("payload.bin"))
            .unwrap()
            .len(),
        PAYLOAD_BYTES as u64
    );
}
