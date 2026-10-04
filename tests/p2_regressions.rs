//! Backend regressions for the P2 audit findings.
use gitbuddy::git::{ConflictResolution, Operation, Repository};
use std::{fs, path::Path, sync::Arc};

fn setup() -> (tempfile::TempDir, Repository) {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::init(&dir.path().join("repo")).unwrap();
    repo.execute(Operation::SetIdentity(
        "Bob".into(),
        "bob@example.invalid".into(),
    ))
    .unwrap();
    let r = raw(&repo);
    let mut config = r.config().unwrap();
    config.set_bool("commit.gpgsign", false).unwrap();
    config.set_bool("tag.gpgsign", false).unwrap();
    config.set_str("core.hooksPath", "/dev/null").unwrap();
    (dir, repo)
}
fn raw(repo: &Repository) -> git2::Repository {
    git2::Repository::open(&repo.root).unwrap()
}
fn head(repo: &Repository) -> git2::Oid {
    raw(repo).head().unwrap().target().unwrap()
}
fn write(repo: &Repository, path: &str, text: &str) {
    fs::write(repo.root.join(path), text).unwrap();
}
fn commit(repo: &Repository, message: &str) -> git2::Oid {
    repo.execute(Operation::StageAll).unwrap();
    repo.execute(Operation::Commit(message.into())).unwrap();
    head(repo)
}
fn alice_commit(repo: &Repository) -> git2::Oid {
    repo.execute(Operation::StageAll).unwrap();
    let r = raw(repo);
    let tree_id = r.index().unwrap().write_tree().unwrap();
    let tree = r.find_tree(tree_id).unwrap();
    let parent = r.head().unwrap().peel_to_commit().unwrap();
    let alice = git2::Signature::new(
        "Alice",
        "alice@example.invalid",
        &git2::Time::new(1_600_000_000, 480),
    )
    .unwrap();
    let bob = r.signature().unwrap();
    r.commit(
        Some("HEAD"),
        &alice,
        &bob,
        "Alice change",
        &tree,
        &[&parent],
    )
    .unwrap()
}
fn assert_author(repo: &Repository, picked: git2::Oid) {
    let r = raw(repo);
    let original = r.find_commit(picked).unwrap();
    let result = r.head().unwrap().peel_to_commit().unwrap();
    assert_eq!(result.author().name(), original.author().name());
    assert_eq!(result.author().email(), original.author().email());
    assert_eq!(
        result.author().when().seconds(),
        original.author().when().seconds()
    );
    assert_eq!(result.author().when().offset_minutes(), 480);
    assert_eq!(result.committer().name().unwrap(), "Bob");
    assert_eq!(result.committer().email().unwrap(), "bob@example.invalid");
}

#[test]
fn cherry_pick_preserves_original_author_and_time_without_conflict() {
    let (_dir, repo) = setup();
    write(&repo, "base", "base");
    commit(&repo, "base");
    repo.execute(Operation::CreateBranch("feature".into()))
        .unwrap();
    write(&repo, "feature", "feature");
    let picked = alice_commit(&repo);
    repo.execute(Operation::Checkout("main".into())).unwrap();
    repo.execute(Operation::CherryPick(picked.to_string()))
        .unwrap();
    assert_author(&repo, picked);
}
#[test]
fn both_cherry_pick_continue_entries_preserve_original_author() {
    for legacy in [true, false] {
        let (_dir, repo) = setup();
        write(&repo, "file", "base\n");
        commit(&repo, "base");
        repo.execute(Operation::CreateBranch("feature".into()))
            .unwrap();
        write(&repo, "file", "alice\n");
        let picked = alice_commit(&repo);
        repo.execute(Operation::Checkout("main".into())).unwrap();
        write(&repo, "file", "bob\n");
        commit(&repo, "Bob change");
        assert!(
            repo.execute(Operation::CherryPick(picked.to_string()))
                .is_err()
        );
        let session = repo.conflict_session().unwrap();
        assert_eq!(session.files.len(), 1);
        let context = repo.conflict_context(&session.files[0].path).unwrap();
        repo.execute(Operation::ResolveConflict {
            context: Arc::new(context),
            resolution: ConflictResolution::Theirs,
        })
        .unwrap();
        repo.execute(if legacy {
            Operation::ContinueCherryPick
        } else {
            Operation::ContinueConflict(Arc::new(repo.conflict_session().unwrap()))
        })
        .unwrap();
        assert_author(&repo, picked);
    }
}
#[test]
fn revert_uses_current_author_instead_of_reverted_commit_author() {
    let (_dir, repo) = setup();
    write(&repo, "file", "base\n");
    commit(&repo, "base");
    write(&repo, "file", "alice\n");
    let picked = alice_commit(&repo);
    repo.execute(Operation::Revert(picked.to_string())).unwrap();
    let r = raw(&repo);
    let result = r.head().unwrap().peel_to_commit().unwrap();
    assert_eq!(result.author().name().unwrap(), "Bob");
}

#[test]
fn pull_uses_complete_remote_name_and_fetches_missing_tracking_ref() {
    let (dir, source) = setup();
    write(&source, "file", "base");
    let base = commit(&source, "base");
    let repo =
        Repository::clone_repo(source.root.to_str().unwrap(), &dir.path().join("clone")).unwrap();
    let r = raw(&repo);
    r.remote_rename("origin", "team/origin").unwrap();
    // A remote branch can have a different name from the checked-out branch.
    let source_raw = raw(&source);
    source_raw
        .find_reference("refs/heads/main")
        .unwrap()
        .rename("refs/heads/release", true, "rename")
        .unwrap();
    source_raw.set_head("refs/heads/release").unwrap();
    write(&source, "file", "next");
    let next = commit(&source, "next");
    r.config()
        .unwrap()
        .set_str("branch.main.merge", "refs/heads/release")
        .unwrap();
    r.find_reference("refs/remotes/team/origin/main")
        .unwrap()
        .delete()
        .unwrap();
    assert_eq!(head(&repo), base);
    repo.execute(Operation::Pull).unwrap();
    assert_eq!(head(&repo), next);
    assert_eq!(fs::read(repo.root.join("file")).unwrap(), b"next");
    assert_eq!(
        r.find_reference("refs/remotes/team/origin/release")
            .unwrap()
            .target(),
        Some(next)
    );
}
#[test]
fn pull_from_local_upstream_does_not_require_remote() {
    let (_dir, repo) = setup();
    write(&repo, "base", "base");
    let base = commit(&repo, "base");
    repo.execute(Operation::CreateBranch("release".into()))
        .unwrap();
    write(&repo, "next", "next");
    let next = commit(&repo, "next");
    repo.execute(Operation::Checkout("main".into())).unwrap();
    let r = raw(&repo);
    r.config()
        .unwrap()
        .set_str("branch.main.remote", ".")
        .unwrap();
    r.config()
        .unwrap()
        .set_str("branch.main.merge", "refs/heads/release")
        .unwrap();
    assert_eq!(head(&repo), base);
    repo.execute(Operation::Pull).unwrap();
    assert_eq!(head(&repo), next);
}
#[test]
fn incomplete_pull_configuration_does_not_move_head() {
    let (_dir, repo) = setup();
    write(&repo, "base", "base");
    let base = commit(&repo, "base");
    raw(&repo)
        .config()
        .unwrap()
        .set_str("branch.main.remote", "team/origin")
        .unwrap();
    assert!(repo.execute(Operation::Pull).is_err());
    assert_eq!(head(&repo), base);
}

#[test]
fn fingerprint_detects_effective_config_changes_including_includes() {
    let (dir, repo) = setup();
    write(&repo, "file", "base");
    commit(&repo, "base");
    let initial = repo.fingerprint().unwrap();
    assert_eq!(repo.fingerprint().unwrap(), initial);
    let r = raw(&repo);
    r.remote("new-remote", "/test/nonexistent").unwrap();
    let with_remote = repo.fingerprint().unwrap();
    assert_ne!(initial, with_remote);
    assert!(
        repo.snapshot(100)
            .unwrap()
            .remotes
            .contains(&"new-remote".into())
    );
    r.config()
        .unwrap()
        .set_str("branch.main.remote", ".")
        .unwrap();
    r.config()
        .unwrap()
        .set_str("branch.main.merge", "refs/heads/main")
        .unwrap();
    let with_upstream = repo.fingerprint().unwrap();
    assert_ne!(with_remote, with_upstream);
    let include = dir.path().join("extra-config");
    fs::write(&include, "[credential]\n helper = first-fake-helper\n").unwrap();
    r.config()
        .unwrap()
        .set_str("include.path", include.to_str().unwrap())
        .unwrap();
    let first = repo.fingerprint().unwrap();
    assert_eq!(repo.fingerprint().unwrap(), first);
    fs::write(&include, "[credential]\n helper = second-fake-helper\n").unwrap();
    assert_ne!(repo.fingerprint().unwrap(), first);
}

#[test]
fn ordinary_commit_rejects_empty_and_unstaged_only_without_moving_head() {
    let (_dir, repo) = setup();
    assert!(!repo.snapshot(100).unwrap().can_commit());
    assert!(
        repo.execute(Operation::Commit("empty initial".into()))
            .is_err()
    );
    assert!(raw(&repo).head().is_err());
    write(&repo, "file", "base");
    let base = commit(&repo, "base");
    assert!(repo.execute(Operation::Commit("empty".into())).is_err());
    assert_eq!(head(&repo), base);
    write(&repo, "file", "unstaged");
    assert!(!repo.snapshot(100).unwrap().can_commit());
    assert!(repo.execute(Operation::Commit("unstaged".into())).is_err());
    assert_eq!(head(&repo), base);
    repo.execute(Operation::StageAll).unwrap();
    assert!(repo.snapshot(100).unwrap().can_commit());
    repo.execute(Operation::Commit("staged".into())).unwrap();
    assert_ne!(head(&repo), base);
}
#[test]
fn merge_with_unchanged_tree_can_still_be_completed() {
    let (_dir, repo) = setup();
    write(&repo, "file", "base");
    let base = commit(&repo, "base");
    let r = raw(&repo);
    let signature = r.signature().unwrap();
    let parent = r.find_commit(base).unwrap();
    let tree = parent.tree().unwrap();
    // Distinct commits on both sides with identical trees still need a merge commit.
    let ours = r
        .commit(
            Some("refs/heads/main"),
            &signature,
            &signature,
            "ours",
            &tree,
            &[&parent],
        )
        .unwrap();
    let theirs = r
        .commit(
            Some("refs/heads/feature"),
            &signature,
            &signature,
            "theirs",
            &tree,
            &[&parent],
        )
        .unwrap();
    let annotated = r.find_annotated_commit(theirs).unwrap();
    r.merge(&[&annotated], None, None).unwrap();
    assert!(repo.snapshot(100).unwrap().files.is_empty());
    assert!(repo.snapshot(100).unwrap().can_commit());
    repo.execute(Operation::Commit("merge".into())).unwrap();
    let result = r.head().unwrap().peel_to_commit().unwrap();
    assert_eq!(result.parent_id(0).unwrap(), ours);
    assert_eq!(result.parent_id(1).unwrap(), theirs);
    assert_eq!(result.tree_id(), tree.id());
}

#[cfg(unix)]
#[test]
fn http_network_operations_read_repository_credential_helper_and_http_path() {
    use std::{
        io::{Read, Write},
        net::{TcpListener, TcpStream},
        os::unix::fs::PermissionsExt,
        sync::atomic::{AtomicBool, Ordering},
        time::Duration,
    };
    struct Server {
        stop: Arc<AtomicBool>,
        worker: Option<std::thread::JoinHandle<()>>,
    }
    impl Drop for Server {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            let result = self.worker.take().unwrap().join();
            if !std::thread::panicking() {
                assert!(result.is_ok(), "HTTP fixture worker failed");
            }
        }
    }
    let (dir, source) = setup();
    write(&source, "file", "served commit");
    commit(&source, "served commit");
    let bare = dir.path().join("repo.git");
    git2::build::RepoBuilder::new()
        .bare(true)
        .clone(source.root.to_str().unwrap(), &bare)
        .unwrap();
    git2::Repository::open_bare(&bare)
        .unwrap()
        .config()
        .unwrap()
        .set_bool("http.receivepack", true)
        .unwrap();
    let server_root = dir.path().to_path_buf();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_worker = stop.clone();
    let authorized = Arc::new(AtomicBool::new(false));
    let seen = authorized.clone();
    let worker = std::thread::spawn(move || {
        while !stop_worker.load(Ordering::SeqCst) {
            let (mut stream, _) = match listener.accept() {
                Ok(pair) => pair,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                }
                Err(e) => panic!("{e}"),
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut request = Vec::new();
            let mut byte = [0];
            while !request.ends_with(b"\r\n\r\n") && request.len() < 32768 {
                if stream.read(&mut byte).unwrap_or(0) == 0 {
                    break;
                }
                request.push(byte[0]);
            }
            let request = String::from_utf8_lossy(&request);
            let auth = request
                .lines()
                .any(|l| l.eq_ignore_ascii_case("Authorization: Basic YXVkaXQ6ZmFrZS10b2tlbg=="));
            fn reply(stream: &mut TcpStream, status: &str, extra: &str, body: &[u8]) {
                write!(
                    stream,
                    "HTTP/1.1 {status}\r\nConnection: close\r\nContent-Length: {}\r\n{extra}\r\n",
                    body.len()
                )
                .unwrap();
                stream.write_all(body).unwrap();
            }
            if !auth {
                reply(
                    &mut stream,
                    "401 Unauthorized",
                    "WWW-Authenticate: Basic realm=\"test\"\r\n",
                    b"",
                );
                continue;
            }
            seen.store(true, Ordering::SeqCst);
            let mut first = request.split_whitespace();
            let method = first.next().unwrap();
            let uri = first.next().unwrap();
            let (path, query) = uri.split_once('?').unwrap_or((uri, ""));
            if !path.starts_with("/repo.git/")
                || Path::new(&path[1..])
                    .components()
                    .any(|part| !matches!(part, std::path::Component::Normal(_)))
            {
                reply(&mut stream, "404 Not Found", "", b"");
                continue;
            }
            let header = |name: &str| {
                request.lines().find_map(|line| {
                    let (key, value) = line.split_once(':')?;
                    key.eq_ignore_ascii_case(name)
                        .then(|| value.trim().to_string())
                })
            };
            let length = header("content-length")
                .map(|s| s.parse::<usize>().unwrap())
                .unwrap_or(0);
            if header("expect").is_some_and(|s| s.eq_ignore_ascii_case("100-continue")) {
                stream.write_all(b"HTTP/1.1 100 Continue\r\n\r\n").unwrap();
            }
            let mut body = Vec::new();
            if header("transfer-encoding").is_some_and(|s| s.eq_ignore_ascii_case("chunked")) {
                loop {
                    let mut line = Vec::new();
                    while !line.ends_with(b"\r\n") {
                        let mut byte = [0];
                        stream.read_exact(&mut byte).unwrap();
                        line.push(byte[0]);
                    }
                    let text = std::str::from_utf8(&line).unwrap().trim();
                    let size = usize::from_str_radix(text.split(';').next().unwrap(), 16).unwrap();
                    if size == 0 {
                        let mut end = [0; 2];
                        stream.read_exact(&mut end).unwrap();
                        assert_eq!(&end, b"\r\n");
                        break;
                    }
                    assert!(body.len() + size < 10_000_000);
                    let start = body.len();
                    body.resize(start + size, 0);
                    stream.read_exact(&mut body[start..]).unwrap();
                    let mut end = [0; 2];
                    stream.read_exact(&mut end).unwrap();
                    assert_eq!(&end, b"\r\n");
                }
            } else {
                body.resize(length, 0);
                stream.read_exact(&mut body).unwrap();
            }
            // Only the test server uses system Git to serve the smart HTTP protocol.
            let mut backend = std::process::Command::new("git");
            backend
                .arg("http-backend")
                .env("GIT_PROJECT_ROOT", &server_root)
                .env("GIT_HTTP_EXPORT_ALL", "1")
                .env("REQUEST_METHOD", method)
                .env("PATH_INFO", path)
                .env("QUERY_STRING", query)
                .env("CONTENT_TYPE", header("content-type").unwrap_or_default())
                .env("CONTENT_LENGTH", body.len().to_string())
                .env("REMOTE_USER", "audit")
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped());
            let mut child = backend.spawn().unwrap();
            child.stdin.take().unwrap().write_all(&body).unwrap();
            let output = child.wait_with_output().unwrap();
            assert!(
                output.status.success(),
                "HTTP fixture: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            let split = output
                .stdout
                .windows(4)
                .position(|s| s == b"\r\n\r\n")
                .unwrap();
            let headers = std::str::from_utf8(&output.stdout[..split]).unwrap();
            let status = headers
                .lines()
                .find_map(|l| l.strip_prefix("Status: "))
                .unwrap_or("200 OK");
            let extra = headers
                .lines()
                .filter(|l| !l.starts_with("Status:"))
                .map(|l| format!("{l}\r\n"))
                .collect::<String>();
            reply(&mut stream, status, &extra, &output.stdout[split + 4..]);
        }
    });
    let _server = Server {
        stop,
        worker: Some(worker),
    };
    for scoped in [true, false] {
        let target = git2::Repository::open_bare(&bare)
            .unwrap()
            .head()
            .unwrap()
            .target()
            .unwrap();
        let repo = Repository::init(&dir.path().join(format!("client-{scoped}"))).unwrap();
        let helper = dir.path().join(format!("credential-helper-{scoped}"));
        let input_file = dir.path().join(format!("helper-input-{scoped}"));
        let quoted = format!("'{}'", input_file.to_str().unwrap().replace('\'', "'\\''"));
        fs::write(
            &helper,
            format!(
                "#!/bin/sh\ncat > {quoted}\nprintf 'username=audit\\npassword=fake-token\\n'\n"
            ),
        )
        .unwrap();
        fs::set_permissions(&helper, fs::Permissions::from_mode(0o755)).unwrap();
        let r = raw(&repo);
        let url = format!("http://{address}/repo.git");
        r.remote("origin", &url).unwrap();
        let prefix = if scoped {
            format!("credential.{url}")
        } else {
            "credential".into()
        };
        let mut config = r.config().unwrap();
        // No real credentials: both configurations use an isolated fake helper.
        config
            .set_str(&format!("{prefix}.helper"), helper.to_str().unwrap())
            .unwrap();
        config
            .set_bool(&format!("{prefix}.useHttpPath"), true)
            .unwrap();
        config
            .set_str(&format!("{prefix}.username"), "audit")
            .unwrap();
        authorized.store(false, Ordering::SeqCst);
        repo.execute(Operation::Fetch).unwrap();
        assert!(authorized.load(Ordering::SeqCst));
        assert_eq!(
            r.find_reference("refs/remotes/origin/main")
                .unwrap()
                .target(),
            Some(target)
        );
        let input = fs::read_to_string(input_file).unwrap();
        assert!(input.contains("path=repo.git"));
        repo.execute(Operation::SetIdentity(
            "Bob".into(),
            "bob@example.invalid".into(),
        ))
        .unwrap();
        repo.execute(Operation::TrackBranch("origin/main".into()))
            .unwrap();
        write(&repo, "pushed", &format!("from client {scoped}"));
        let pushed = commit(&repo, "push using repository credentials");
        authorized.store(false, Ordering::SeqCst);
        repo.execute(Operation::Push).unwrap();
        assert!(authorized.load(Ordering::SeqCst));
        assert_eq!(
            git2::Repository::open_bare(&bare)
                .unwrap()
                .head()
                .unwrap()
                .target(),
            Some(pushed)
        );
        authorized.store(false, Ordering::SeqCst);
        repo.execute(Operation::AddSubmodule {
            url: url.clone(),
            path: "module".into(),
        })
        .unwrap();
        assert!(authorized.load(Ordering::SeqCst));
        assert_eq!(
            git2::Repository::open(repo.root.join("module"))
                .unwrap()
                .head()
                .unwrap()
                .target(),
            Some(pushed)
        );
    }
}
