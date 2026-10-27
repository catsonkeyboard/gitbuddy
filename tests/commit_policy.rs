use gitbuddy::git::{Operation, Repository};
use std::{fs, process::Command};

fn setup() -> (tempfile::TempDir, Repository) {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::init(&dir.path().join("repo")).unwrap();
    repo.execute(Operation::SetIdentity(
        "Tester".into(),
        "tester@example.invalid".into(),
    ))
    .unwrap();
    let raw = git2::Repository::open(&repo.root).unwrap();
    let mut config = raw.config().unwrap();
    config.set_bool("commit.gpgsign", false).unwrap();
    config.set_str("core.hooksPath", "/dev/null").unwrap();
    (dir, repo)
}

#[cfg(unix)]
#[test]
fn hooks_can_edit_message_or_block_commit() {
    use std::os::unix::fs::PermissionsExt;
    let (_dir, repo) = setup();
    let hooks = repo.root.join("hooks");
    fs::create_dir(&hooks).unwrap();
    let raw = git2::Repository::open(&repo.root).unwrap();
    raw.config()
        .unwrap()
        .set_str("core.hooksPath", "hooks")
        .unwrap();
    let hook = hooks.join("commit-msg");
    fs::write(&hook, "#!/bin/sh\nprintf '\\nHook trailer\\n' >> \"$1\"\n").unwrap();
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
    repo.execute(Operation::CommitEmpty("first".into()))
        .unwrap();
    let first = raw.head().unwrap().peel_to_commit().unwrap();
    assert!(first.message().unwrap().contains("Hook trailer"));
    assert_eq!(
        raw.reflog("HEAD").unwrap().get(0).unwrap().id_new(),
        first.id()
    );
    fs::write(&hook, "#!/bin/sh\necho rejected >&2\nexit 1\n").unwrap();
    let error = repo
        .execute(Operation::CommitEmpty("second".into()))
        .unwrap_err();
    assert!(format!("{error:#}").contains("rejected"));
    assert_eq!(
        raw.head().unwrap().peel_to_commit().unwrap().id(),
        first.id()
    );
}

#[cfg(unix)]
#[test]
fn hooks_can_stage_changes_and_post_failure_reports_committed_result() {
    use std::os::unix::fs::PermissionsExt;
    let (_dir, repo) = setup();
    let hooks = repo.root.join("hooks");
    fs::create_dir(&hooks).unwrap();
    let raw = git2::Repository::open(&repo.root).unwrap();
    raw.config()
        .unwrap()
        .set_str("core.hooksPath", "hooks")
        .unwrap();
    fs::write(repo.root.join("file"), "v1").unwrap();
    repo.execute(Operation::StageAll).unwrap();
    let pre = hooks.join("pre-commit");
    fs::write(&pre, "#!/bin/sh\nprintf v2 > file\ngit add file\n").unwrap();
    fs::set_permissions(&pre, fs::Permissions::from_mode(0o755)).unwrap();
    let post = hooks.join("post-commit");
    fs::write(&post, "#!/bin/sh\nexit 7\n").unwrap();
    fs::set_permissions(&post, fs::Permissions::from_mode(0o755)).unwrap();
    let result = repo
        .execute(Operation::Commit("hook staging".into()))
        .unwrap();
    assert!(result.contains("post-commit hook failed after commit"));
    let commit = raw.head().unwrap().peel_to_commit().unwrap();
    let blob = commit
        .tree()
        .unwrap()
        .get_name("file")
        .unwrap()
        .to_object(&raw)
        .unwrap()
        .peel_to_blob()
        .unwrap();
    assert_eq!(blob.content(), b"v2");
    assert!(
        repo.verify_commit_signature(&commit.id().to_string())
            .unwrap()
            .contains("no signature")
    );
}

#[test]
fn ssh_signed_empty_commit_verifies_and_failure_does_not_move_head() {
    if Command::new("ssh-keygen").arg("-?").output().is_err() {
        return;
    }
    let (dir, repo) = setup();
    let key = dir.path().join("key");
    assert!(
        Command::new("ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", "", "-f"])
            .arg(&key)
            .status()
            .unwrap()
            .success()
    );
    let raw = git2::Repository::open(&repo.root).unwrap();
    let mut config = raw.config().unwrap();
    config.set_bool("commit.gpgsign", true).unwrap();
    config.set_str("gpg.format", "ssh").unwrap();
    config
        .set_str("user.signingkey", key.to_str().unwrap())
        .unwrap();
    repo.execute(Operation::CommitEmpty("signed".into()))
        .unwrap();
    let id = raw.head().unwrap().peel_to_commit().unwrap().id();
    assert!(
        repo.verify_commit_signature(&id.to_string())
            .unwrap()
            .contains("cryptographically valid")
    );
    let allowed = dir.path().join("allowed_signers");
    let public = fs::read_to_string(key.with_extension("pub")).unwrap();
    fs::write(&allowed, format!("tester@example.invalid {public}")).unwrap();
    let verify = Command::new("git")
        .arg("-C")
        .arg(&repo.root)
        .arg("-c")
        .arg(format!("gpg.ssh.allowedSignersFile={}", allowed.display()))
        .args(["verify-commit", &id.to_string()])
        .output()
        .unwrap();
    assert!(
        verify.status.success(),
        "{}",
        String::from_utf8_lossy(&verify.stderr)
    );
    let context = repo.commit_edit().unwrap();
    repo.execute(Operation::Amend {
        context: std::sync::Arc::new(context),
        message: "signed amend".into(),
    })
    .unwrap();
    let amended = raw.head().unwrap().peel_to_commit().unwrap().id();
    assert_ne!(id, amended);
    assert!(
        repo.verify_commit_signature(&amended.to_string())
            .unwrap()
            .contains("cryptographically valid")
    );
    config
        .set_str("user.signingkey", "/nonexistent-gitbuddy-test-key")
        .unwrap();
    assert!(
        repo.execute(Operation::CommitEmpty("failure".into()))
            .is_err()
    );
    assert_eq!(raw.head().unwrap().peel_to_commit().unwrap().id(), amended);
}

#[cfg(unix)]
#[test]
fn isolated_openpgp_commit_verifies_with_git() {
    use std::{os::unix::fs::PermissionsExt, path::PathBuf, process::Stdio};
    if Command::new("gpg")
        .arg("--version")
        .stdout(Stdio::null())
        .status()
        .is_err()
    {
        return;
    }
    let home = tempfile::Builder::new()
        .prefix("gitbuddy-commit-gpg-")
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
                .output();
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
            "Tester <tester@example.invalid>",
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
    let (dir, repo) = setup();
    let program = dir.path().join("gpg-wrapper.py");
    fs::write(&program, format!("#!/usr/bin/env python3\nimport os,sys\nos.execvp('gpg',['gpg','--homedir',{:?}]+sys.argv[1:])\n", home.path().to_str().unwrap())).unwrap();
    fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
    let raw = git2::Repository::open(&repo.root).unwrap();
    let mut config = raw.config().unwrap();
    config.set_bool("commit.gpgsign", true).unwrap();
    config.set_str("gpg.format", "openpgp").unwrap();
    config
        .set_str("user.signingkey", "tester@example.invalid")
        .unwrap();
    config
        .set_str("gpg.openpgp.program", program.to_str().unwrap())
        .unwrap();
    repo.execute(Operation::CommitEmpty("openpgp".into()))
        .unwrap();
    let id = raw
        .head()
        .unwrap()
        .peel_to_commit()
        .unwrap()
        .id()
        .to_string();
    assert!(
        repo.verify_commit_signature(&id)
            .unwrap()
            .contains("cryptographically valid")
    );
    let verify = Command::new("git")
        .arg("-C")
        .arg(&repo.root)
        .arg("-c")
        .arg(format!("gpg.program={}", program.display()))
        .args(["verify-commit", &id])
        .output()
        .unwrap();
    assert!(
        verify.status.success(),
        "{}",
        String::from_utf8_lossy(&verify.stderr)
    );
}
