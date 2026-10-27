use gitbuddy::git::{
    CancellationToken, NetworkControl, Operation, Repository, TagKind, TagSigner, TagSigningFormat,
    TagSpec, is_cancelled,
};
use std::{
    fs,
    path::PathBuf,
    process::{Command, Stdio},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
fn raw(repo: &Repository) -> git2::Repository {
    git2::Repository::open(&repo.root).unwrap()
}
fn head(repo: &Repository) -> String {
    raw(repo).head().unwrap().target().unwrap().to_string()
}
fn commit(repo: &Repository, text: &str) -> String {
    fs::write(repo.root.join("file"), text).unwrap();
    repo.execute(Operation::StageAll).unwrap();
    repo.execute(Operation::Commit(text.into())).unwrap();
    head(repo)
}
struct Fixture {
    dir: tempfile::TempDir,
    repo: Repository,
    old: String,
    tip: String,
}
impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(&dir.path().join("repo")).unwrap();
        repo.execute(Operation::SetIdentity(
            "Tagger 测试".into(),
            "tagger@example.invalid".into(),
        ))
        .unwrap();
        raw(&repo)
            .config()
            .unwrap()
            .set_bool("tag.gpgsign", false)
            .unwrap();
        let old = commit(&repo, "old commit");
        let tip = commit(&repo, "latest commit");
        Self {
            dir,
            repo,
            old,
            tip,
        }
    }
    fn plan(
        &self,
        name: &str,
        kind: TagKind,
        signer: Option<TagSigner>,
    ) -> anyhow::Result<gitbuddy::git::TagPlan> {
        self.repo.prepare_tag(
            &self.repo.prepare_tag_target(&self.old)?,
            TagSpec {
                name: name.into(),
                kind,
                message: if kind == TagKind::Lightweight {
                    String::new()
                } else {
                    "Release 标注\n\nSecond paragraph\n".into()
                },
                signer,
            },
        )
    }
    fn create(
        &self,
        name: &str,
        kind: TagKind,
        signer: Option<TagSigner>,
    ) -> anyhow::Result<String> {
        self.repo.execute(Operation::CreateTag(Arc::new(
            self.plan(name, kind, signer)?,
        )))
    }
}
#[test]
fn lightweight_and_annotated_tags_target_old_commits_preserving_dirty_work_and_index() {
    for kind in [TagKind::Lightweight, TagKind::Annotated] {
        let f = Fixture::new();
        fs::write(f.repo.root.join("file"), "staged").unwrap();
        f.repo.execute(Operation::StageAll).unwrap();
        fs::write(f.repo.root.join("file"), "unstaged").unwrap();
        fs::write(f.repo.root.join("untracked"), "keep").unwrap();
        let index = fs::read(raw(&f.repo).path().join("index")).unwrap();
        f.create("release/测试", kind, None).unwrap();
        let r = raw(&f.repo);
        let reference = r.find_reference("refs/tags/release/测试").unwrap();
        assert_eq!(reference.peel_to_commit().unwrap().id().to_string(), f.old);
        if kind == TagKind::Annotated {
            let tag = reference.peel_to_tag().unwrap();
            assert_eq!(
                tag.message().unwrap().unwrap(),
                "Release 标注\n\nSecond paragraph\n"
            );
            assert_eq!(tag.tagger().unwrap().name().unwrap(), "Tagger 测试");
            assert_eq!(
                tag.tagger().unwrap().email().unwrap(),
                "tagger@example.invalid"
            );
        } else {
            assert_eq!(reference.target().unwrap().to_string(), f.old);
        }
        assert_eq!(head(&f.repo), f.tip);
        assert_eq!(fs::read(r.path().join("index")).unwrap(), index);
        assert_eq!(
            fs::read_to_string(f.repo.root.join("file")).unwrap(),
            "unstaged"
        );
        assert_eq!(
            fs::read_to_string(f.repo.root.join("untracked")).unwrap(),
            "keep"
        );
    }
}
#[test]
fn revisions_and_existing_annotated_tags_peel_to_the_reviewed_commit() {
    let f = Fixture::new();
    f.create("old", TagKind::Annotated, None).unwrap();
    for (i, revision) in ["HEAD~1", "refs/tags/old", f.old.as_str()]
        .iter()
        .enumerate()
    {
        let target = f.repo.prepare_tag_target(revision).unwrap();
        let plan = f
            .repo
            .prepare_tag(
                &target,
                TagSpec {
                    name: format!("copy-{i}"),
                    ..Default::default()
                },
            )
            .unwrap();
        f.repo
            .execute(Operation::CreateTag(Arc::new(plan)))
            .unwrap();
        assert_eq!(
            raw(&f.repo)
                .find_reference(&format!("refs/tags/copy-{i}"))
                .unwrap()
                .target()
                .unwrap()
                .to_string(),
            f.old
        );
    }
    let r = raw(&f.repo);
    let blob = r.blob(b"not a commit").unwrap();
    assert!(f.repo.prepare_tag_target(&blob.to_string()).is_err());
    assert!(f.repo.prepare_tag_target("").is_err());
    assert!(f.repo.prepare_tag_target("missing").is_err());
}
#[test]
fn legacy_tag_api_stays_lightweight_at_head() {
    let f = Fixture::new();
    f.repo.execute(Operation::Tag("legacy".into())).unwrap();
    assert_eq!(
        raw(&f.repo)
            .find_reference("refs/tags/legacy")
            .unwrap()
            .target()
            .unwrap()
            .to_string(),
        f.tip
    );
}
#[test]
fn tag_names_messages_signers_and_existing_refs_are_validated_without_overwrite() {
    let f = Fixture::new();
    for name in ["", "-bad", "a..b", "bad\nname", "x.lock"] {
        assert!(f.plan(name, TagKind::Lightweight, None).is_err());
    }
    let plan = Arc::new(f.plan("exists", TagKind::Annotated, None).unwrap());
    f.repo.execute(Operation::CreateTag(plan.clone())).unwrap();
    let id = raw(&f.repo)
        .find_reference("refs/tags/exists")
        .unwrap()
        .target();
    assert!(f.repo.execute(Operation::CreateTag(plan)).is_err());
    assert!(f.plan("exists", TagKind::Lightweight, None).is_err());
    assert_eq!(
        raw(&f.repo)
            .find_reference("refs/tags/exists")
            .unwrap()
            .target(),
        id
    );
    let target = f.repo.prepare_tag_target(&f.old).unwrap();
    for message in [
        String::new(),
        "\0bad".into(),
        "x".repeat(1024 * 1024 + 1),
        "-----BEGIN SSH SIGNATURE-----\npretend".into(),
    ] {
        assert!(
            f.repo
                .prepare_tag(
                    &target,
                    TagSpec {
                        name: "invalid".into(),
                        kind: TagKind::Annotated,
                        message,
                        signer: None
                    }
                )
                .is_err()
        );
    }
    assert!(
        f.repo
            .prepare_tag(
                &target,
                TagSpec {
                    name: "invalid".into(),
                    kind: TagKind::Lightweight,
                    message: "not lightweight".into(),
                    signer: None
                }
            )
            .is_err()
    );
    assert!(f.plan("invalid", TagKind::Signed, None).is_err());
    assert!(
        f.plan(
            "invalid",
            TagKind::Signed,
            Some(TagSigner {
                format: TagSigningFormat::OpenPgp,
                program: "gpg".into(),
                key: String::new()
            })
        )
        .is_err()
    );
}
#[test]
fn stale_target_head_configuration_and_cross_repository_plans_are_rejected() {
    for case in 0..4 {
        let f = Fixture::new();
        let r = raw(&f.repo);
        r.branch(
            "target",
            &r.find_commit(git2::Oid::from_str(&f.old).unwrap()).unwrap(),
            false,
        )
        .unwrap();
        let target = f.repo.prepare_tag_target("target").unwrap();
        let plan = Arc::new(
            f.repo
                .prepare_tag(
                    &target,
                    TagSpec {
                        name: "stale".into(),
                        ..Default::default()
                    },
                )
                .unwrap(),
        );
        if case == 0 {
            r.find_reference("refs/heads/target")
                .unwrap()
                .set_target(git2::Oid::from_str(&f.tip).unwrap(), "moved target")
                .unwrap();
        } else if case == 1 {
            r.find_reference("refs/heads/main")
                .unwrap()
                .set_target(git2::Oid::from_str(&f.old).unwrap(), "moved head")
                .unwrap();
        } else if case == 2 {
            r.config()
                .unwrap()
                .set_str("user.email", "changed@example.invalid")
                .unwrap();
        } else {
            let other = Repository::init(&f.dir.path().join("other")).unwrap();
            assert!(
                other
                    .prepare_tag(
                        &target,
                        TagSpec {
                            name: "cross".into(),
                            ..Default::default()
                        }
                    )
                    .is_err()
            );
            assert!(other.execute(Operation::CreateTag(plan)).is_err());
            continue;
        }
        assert!(f.repo.execute(Operation::CreateTag(plan)).is_err());
        assert!(r.find_reference("refs/tags/stale").is_err());
    }
}
#[test]
fn locks_and_nested_reference_collisions_never_overwrite_or_remove_others_locks() {
    let f = Fixture::new();
    let r = raw(&f.repo);
    let path = r.path().join("refs/tags/locked.lock");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, "other writer").unwrap();
    assert!(f.create("locked", TagKind::Annotated, None).is_err());
    assert_eq!(fs::read_to_string(&path).unwrap(), "other writer");
    f.create("parent", TagKind::Lightweight, None).unwrap();
    assert!(
        f.create("parent/child", TagKind::Lightweight, None)
            .is_err()
    );
    assert!(r.find_reference("refs/tags/parent").is_ok());
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(r.commondir().join("gitbuddy-writer.lock"))
        .unwrap();
    lock.try_lock().unwrap();
    assert!(f.create("writer", TagKind::Annotated, None).is_err());
    lock.unlock().unwrap();
}
#[test]
fn lightweight_does_not_require_identity_and_unborn_head_can_tag_an_existing_commit() {
    let f = Fixture::new();
    let r = raw(&f.repo);
    r.config().unwrap().set_str("user.name", "").unwrap();
    r.config().unwrap().set_str("user.email", "").unwrap();
    assert!(f.create("no-identity", TagKind::Lightweight, None).is_ok());
    assert!(f.plan("needs-identity", TagKind::Annotated, None).is_err());
    r.set_head("refs/heads/unborn").unwrap();
    f.create("from-unborn", TagKind::Lightweight, None).unwrap();
    assert_eq!(
        r.find_reference("refs/tags/from-unborn")
            .unwrap()
            .target()
            .unwrap()
            .to_string(),
        f.old
    );
}
#[test]
fn signing_defaults_read_effective_config_and_unsupported_formats_are_reported() {
    let f = Fixture::new();
    let r = raw(&f.repo);
    let mut c = r.config().unwrap();
    c.set_bool("tag.gpgsign", true).unwrap();
    c.set_str("user.signingkey", "key-id").unwrap();
    c.set_str("gpg.program", "legacy-gpg").unwrap();
    c.set_str("gpg.openpgp.program", "preferred-gpg").unwrap();
    c.set_str("gpg.ssh.program", "custom-ssh").unwrap();
    c.set_str("gpg.format", "ssh").unwrap();
    let defaults = f.repo.tag_defaults().unwrap();
    assert!(defaults.signed);
    assert_eq!(defaults.format, TagSigningFormat::Ssh);
    assert_eq!(defaults.key, "key-id");
    assert_eq!(defaults.program(TagSigningFormat::OpenPgp), "preferred-gpg");
    assert_eq!(defaults.program(TagSigningFormat::Ssh), "custom-ssh");
    c.set_str("gpg.format", "x509").unwrap();
    assert!(
        f.repo
            .tag_defaults()
            .unwrap()
            .notice
            .unwrap()
            .contains("unsupported")
    );
}

#[cfg(unix)]
fn mock_signer(f: &Fixture, mode: &str) -> TagSigner {
    use std::os::unix::fs::PermissionsExt;
    let program = f.dir.path().join(format!("signer {mode};literal$.py"));
    let payload = f.dir.path().join("payload");
    let args = f.dir.path().join("args.json");
    let started = f.dir.path().join("started");
    let script = format!(
        r#"#!/usr/bin/env python3
import sys,json,pathlib,time,subprocess
mode={mode:?}
args=sys.argv[1:]
payload=pathlib.Path({payload:?})
if '--verify' in args:
    assert pathlib.Path(args[-1]).read_bytes()==payload.read_bytes()
    assert pathlib.Path(args[-2]).read_text().startswith('-----BEGIN PGP SIGNATURE-----')
    sys.exit(3 if mode=='verify-fail' else 0)
data=sys.stdin.buffer.read()
payload.write_bytes(data)
pathlib.Path({args:?}).write_text(json.dumps(args))
if mode=='slow':
    pathlib.Path({started:?}).write_text('started')
    time.sleep(60)
if mode=='exit':
    sys.stderr.write('signing refused')
    sys.exit(2)
if mode=='oversize':
    sys.stdout.write('x'*(1024*1024+1))
elif mode=='stderr-oversize':
    sys.stderr.write('x'*(1024*1024+1))
elif mode=='malformed':
    sys.stdout.write('not a signature')
else:
    sys.stdout.write('-----BEGIN PGP SIGNATURE-----\nZmFrZQ==\n-----END PGP SIGNATURE-----\n')
"#,
        mode = mode,
        payload = payload.to_str().unwrap(),
        args = args.to_str().unwrap(),
        started = started.to_str().unwrap()
    );
    fs::write(&program, script).unwrap();
    fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
    TagSigner {
        format: TagSigningFormat::OpenPgp,
        program: program.to_string_lossy().into_owned(),
        key: "literal key; $(touch should-not-exist)".into(),
    }
}
#[cfg(unix)]
#[test]
fn signed_tag_uses_exact_native_payload_literal_arguments_and_is_cancellable() {
    let f = Fixture::new();
    let signer = mock_signer(&f, "success");
    let key = signer.key.clone();
    let plan = Arc::new(f.plan("signed", TagKind::Signed, Some(signer)).unwrap());
    let operation = Operation::CreateTag(plan);
    assert!(operation.is_cancellable());
    assert!(!operation.is_network());
    f.repo.execute(operation).unwrap();
    let r = raw(&f.repo);
    let id = r
        .find_reference("refs/tags/signed")
        .unwrap()
        .target()
        .unwrap();
    let odb = r.odb().unwrap();
    let object = odb.read(id).unwrap();
    let payload = fs::read(f.dir.path().join("payload")).unwrap();
    assert!(object.data().starts_with(&payload));
    assert!(
        String::from_utf8_lossy(&payload)
            .contains(&format!("object {}\ntype commit\ntag signed\n", f.old))
    );
    let args: Vec<String> =
        serde_json::from_slice(&fs::read(f.dir.path().join("args.json")).unwrap()).unwrap();
    assert!(args.contains(&format!("--local-user={key}")));
    assert!(!f.repo.root.join("should-not-exist").exists());
}
#[cfg(unix)]
#[test]
fn failed_malformed_oversized_and_unverified_signatures_never_publish_a_tag() {
    for mode in [
        "exit",
        "malformed",
        "verify-fail",
        "oversize",
        "stderr-oversize",
    ] {
        let f = Fixture::new();
        assert!(
            f.create("rejected", TagKind::Signed, Some(mock_signer(&f, mode)))
                .is_err()
        );
        let r = raw(&f.repo);
        assert!(r.find_reference("refs/tags/rejected").is_err());
        assert!(!r.path().join("refs/tags/rejected.lock").exists());
        assert_eq!(head(&f.repo), f.tip);
    }
    let f = Fixture::new();
    assert!(
        f.create(
            "missing",
            TagKind::Signed,
            Some(TagSigner {
                format: TagSigningFormat::OpenPgp,
                program: f.dir.path().join("missing-tool").display().to_string(),
                key: "key".into()
            })
        )
        .is_err()
    );
    assert!(raw(&f.repo).find_reference("refs/tags/missing").is_err());
}
#[cfg(unix)]
#[test]
fn signing_can_be_cancelled_while_the_tool_is_running_and_releases_the_reference_lock() {
    let f = Fixture::new();
    let plan = Arc::new(
        f.plan("cancel", TagKind::Signed, Some(mock_signer(&f, "slow")))
            .unwrap(),
    );
    let token = CancellationToken::default();
    let cancel = token.clone();
    let repo = f.repo.clone();
    let worker = std::thread::spawn(move || {
        repo.execute_with_control(Operation::CreateTag(plan), NetworkControl::new(None, token))
    });
    let start = Instant::now();
    while !f.dir.path().join("started").exists() {
        assert!(start.elapsed() < Duration::from_secs(5));
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(cancel.cancel());
    assert!(is_cancelled(&worker.join().unwrap().unwrap_err()));
    assert!(start.elapsed() < Duration::from_secs(6));
    let r = raw(&f.repo);
    assert!(r.find_reference("refs/tags/cancel").is_err());
    assert!(!r.path().join("refs/tags/cancel.lock").exists());
    f.create("cancel", TagKind::Lightweight, None).unwrap();
}
#[cfg(unix)]
#[test]
fn signing_revalidates_target_head_and_config_after_agent_work() {
    for case in 0..3 {
        let f = Fixture::new();
        let plan = Arc::new(
            f.plan(
                "stale-signed",
                TagKind::Signed,
                Some(mock_signer(&f, "success")),
            )
            .unwrap(),
        );
        let path = f.repo.root.clone();
        let old = f.old.clone();
        let sink: gitbuddy::git::ProgressSink = Arc::new(Mutex::new(move |phase: &str| {
            if phase.contains("verifying signature") {
                let r = git2::Repository::open(&path).unwrap();
                if case == 0 {
                    r.find_reference("refs/heads/main")
                        .unwrap()
                        .set_target(git2::Oid::from_str(&old).unwrap(), "external move")
                        .unwrap();
                } else if case == 1 {
                    r.config()
                        .unwrap()
                        .set_str("user.email", "changed@example.invalid")
                        .unwrap();
                } else {
                    fs::write(path.join("file"), "concurrent worktree edit kept").unwrap();
                }
            }
        }));
        let result = f.repo.execute_with_control(
            Operation::CreateTag(plan),
            NetworkControl::new(Some(sink), CancellationToken::default()),
        );
        if case < 2 {
            assert!(result.is_err());
            assert!(
                raw(&f.repo)
                    .find_reference("refs/tags/stale-signed")
                    .is_err()
            );
        } else {
            result.unwrap();
            assert_eq!(
                fs::read_to_string(f.repo.root.join("file")).unwrap(),
                "concurrent worktree edit kept"
            );
        }
    }
}
#[test]
fn cancellation_is_sealed_only_for_the_final_reference_publication() {
    for early in [false, true] {
        let f = Fixture::new();
        let plan = Arc::new(f.plan("publish", TagKind::Annotated, None).unwrap());
        let token = CancellationToken::default();
        if early {
            token.cancel();
        }
        let cancel = token.clone();
        let sink: gitbuddy::git::ProgressSink = Arc::new(Mutex::new(move |phase: &str| {
            if phase.contains("publishing reviewed") {
                assert!(!cancel.cancel());
            }
        }));
        let result = f.repo.execute_with_control(
            Operation::CreateTag(plan),
            NetworkControl::new(Some(sink), token),
        );
        if early {
            assert!(is_cancelled(&result.unwrap_err()));
        } else {
            result.unwrap();
        }
        assert_eq!(
            raw(&f.repo).find_reference("refs/tags/publish").is_ok(),
            !early
        );
    }
}
#[cfg(unix)]
#[test]
fn real_ssh_file_and_agent_public_key_forms_create_git_verifiable_tags() {
    if Command::new("ssh-keygen")
        .arg("-?")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_err()
    {
        eprintln!("ssh-keygen unavailable; skipping real SSH signing");
        return;
    }
    let f = Fixture::new();
    let key = f.dir.path().join("signing key");
    assert!(
        Command::new("ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", "", "-f"])
            .arg(&key)
            .status()
            .unwrap()
            .success()
    );
    let public = fs::read_to_string(key.with_extension("pub")).unwrap();
    let allowed = f.dir.path().join("allowed-signers");
    fs::write(&allowed, format!("tagger@example.invalid {public}")).unwrap();
    // The private-file form works without an agent; public-key form is covered
    // below by a dedicated isolated ssh-agent, never the user's agent.
    f.create(
        "ssh-signed",
        TagKind::Signed,
        Some(TagSigner {
            format: TagSigningFormat::Ssh,
            program: "ssh-keygen".into(),
            key: key.display().to_string(),
        }),
    )
    .unwrap();
    let verify = Command::new("git")
        .arg("-C")
        .arg(&f.repo.root)
        .arg("-c")
        .arg(format!("gpg.ssh.allowedSignersFile={}", allowed.display()))
        .arg("verify-tag")
        .arg("ssh-signed")
        .output()
        .unwrap();
    assert!(
        verify.status.success(),
        "{}",
        String::from_utf8_lossy(&verify.stderr)
    );
    assert_eq!(
        raw(&f.repo)
            .find_reference("refs/tags/ssh-signed")
            .unwrap()
            .peel_to_commit()
            .unwrap()
            .id()
            .to_string(),
        f.old
    );
    assert_eq!(head(&f.repo), f.tip);
    // Load only this disposable key into a dedicated agent. The signing wrapper
    // scopes SSH_AUTH_SOCK to its child, without changing process-wide env.
    use std::os::unix::fs::PermissionsExt;
    let agent_dir = tempfile::Builder::new()
        .prefix("gitbuddy-tag-agent-")
        .tempdir_in("/tmp")
        .unwrap();
    let socket = agent_dir.path().join("agent.sock");
    struct Agent(std::process::Child);
    impl Drop for Agent {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let mut agent = Agent(
        Command::new("ssh-agent")
            .args(["-D", "-a"])
            .arg(&socket)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let start = Instant::now();
    while !socket.exists() {
        assert!(start.elapsed() < Duration::from_secs(5));
        assert!(agent.0.try_wait().unwrap().is_none());
        std::thread::sleep(Duration::from_millis(20));
    }
    let added = Command::new("ssh-add")
        .env("SSH_AUTH_SOCK", &socket)
        .arg("-t")
        .arg("60")
        .arg(&key)
        .output()
        .unwrap();
    assert!(added.status.success());
    let wrapper = f.dir.path().join("isolated-ssh.py");
    fs::write(&wrapper,format!("#!/usr/bin/env python3\nimport os,sys\nos.environ['SSH_AUTH_SOCK']={:?}\nos.execvp('ssh-keygen',['ssh-keygen']+sys.argv[1:])\n",socket.to_str().unwrap())).unwrap();
    fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755)).unwrap();
    f.create(
        "ssh-agent-signed",
        TagKind::Signed,
        Some(TagSigner {
            format: TagSigningFormat::Ssh,
            program: wrapper.display().to_string(),
            key: format!("key::{}", public.trim()),
        }),
    )
    .unwrap();
    let verify = Command::new("git")
        .arg("-C")
        .arg(&f.repo.root)
        .arg("-c")
        .arg(format!("gpg.ssh.allowedSignersFile={}", allowed.display()))
        .args(["verify-tag", "ssh-agent-signed"])
        .output()
        .unwrap();
    assert!(
        verify.status.success(),
        "{}",
        String::from_utf8_lossy(&verify.stderr)
    );
}
#[cfg(unix)]
#[test]
fn real_openpgp_tag_is_accepted_by_git_verify_tag_with_an_isolated_keyring() {
    use std::os::unix::fs::PermissionsExt;
    if Command::new("gpg")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_err()
    {
        eprintln!("gpg unavailable; skipping real OpenPGP signing");
        return;
    }
    let home = tempfile::Builder::new()
        .prefix("gitbuddy-tag-gpg-")
        .tempdir_in("/tmp")
        .unwrap();
    fs::set_permissions(home.path(), fs::Permissions::from_mode(0o700)).unwrap();
    struct Agent(PathBuf);
    impl Drop for Agent {
        fn drop(&mut self) {
            let _ = Command::new("gpgconf")
                .arg("--homedir")
                .arg(&self.0)
                .args(["--kill", "gpg-agent"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }
    let _agent = Agent(home.path().into());
    let generated = Command::new("gpg")
        .arg("--homedir")
        .arg(home.path())
        .args([
            "--batch",
            "--pinentry-mode",
            "loopback",
            "--passphrase",
            "",
            "--quick-generate-key",
            "GitBuddy Test <tagger@example.invalid>",
            "ed25519",
            "sign",
            "0",
        ])
        .output()
        .unwrap();
    assert!(
        generated.status.success(),
        "{}",
        String::from_utf8_lossy(&generated.stderr)
    );
    let f = Fixture::new();
    let program = f.dir.path().join("isolated-gpg.py");
    fs::write(&program,format!("#!/usr/bin/env python3\nimport os,sys\nos.execvp('gpg',['gpg','--homedir',{:?}]+sys.argv[1:])\n",home.path().to_str().unwrap())).unwrap();
    fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
    f.create(
        "gpg-signed",
        TagKind::Signed,
        Some(TagSigner {
            format: TagSigningFormat::OpenPgp,
            program: program.display().to_string(),
            key: "tagger@example.invalid".into(),
        }),
    )
    .unwrap();
    let verify = Command::new("git")
        .arg("-C")
        .arg(&f.repo.root)
        .arg("-c")
        .arg("gpg.format=openpgp")
        .arg("-c")
        .arg(format!("gpg.program={}", program.display()))
        .args(["verify-tag", "gpg-signed"])
        .output()
        .unwrap();
    assert!(
        verify.status.success(),
        "{}",
        String::from_utf8_lossy(&verify.stderr)
    );
    assert_eq!(head(&f.repo), f.tip);
}
