//! Disposable tag fixtures; generate a test-only SSH key only with an explicit flag.
use anyhow::{Result, ensure};
use gitbuddy::{
    git::{Operation, Repository},
    session::{Selection, Session, Store, Tab},
};
use std::{
    fs,
    path::PathBuf,
    process::{Command, Stdio},
};
fn main() -> Result<()> {
    let args = std::env::args_os().skip(1).collect::<Vec<_>>();
    let target = args
        .first()
        .filter(|s| !s.to_string_lossy().starts_with("--"))
        .map(PathBuf::from)
        .unwrap_or_else(|| "target/demo-tags".into());
    ensure!(
        !target.exists(),
        "Refusing to overwrite {}",
        target.display()
    );
    fs::create_dir_all(&target)?;
    let target = target.canonicalize()?;
    let repo = Repository::init(&target.join("project"))?;
    repo.execute(Operation::SetIdentity(
        "GitBuddy Tag Demo".into(),
        "demo@example.invalid".into(),
    ))?;
    let mut commits = Vec::new();
    for (message, body) in [
        ("Initial release", "# Release 1\n"),
        ("Add release notes", "# Release 1\n\nRelease notes.\n"),
        (
            "Development version",
            "# Development version\n\nRelease notes.\n",
        ),
    ] {
        fs::write(repo.root.join("README.md"), body)?;
        repo.execute(Operation::StageAll)?;
        repo.execute(Operation::Commit(message.into()))?;
        commits.push(repo.snapshot(10)?.head_id.unwrap());
    }
    let raw = git2::Repository::open(&repo.root)?;
    if args.iter().any(|s| s == "--ssh-test-key") {
        let key_dir = target.join("test-keys");
        fs::create_dir(&key_dir)?;
        let key = key_dir.join("signing");
        ensure!(
            Command::new("ssh-keygen")
                .args(["-q", "-t", "ed25519", "-N", "", "-f"])
                .arg(&key)
                .stdout(Stdio::null())
                .status()?
                .success(),
            "Cannot create isolated SSH demo key"
        );
        let public = fs::read_to_string(key.with_extension("pub"))?;
        let allowed = key_dir.join("allowed-signers");
        fs::write(&allowed, format!("demo@example.invalid {public}"))?;
        let mut config = raw.config()?;
        config.set_bool("tag.gpgsign", true)?;
        config.set_str("gpg.format", "ssh")?;
        config.set_str("gpg.ssh.program", "ssh-keygen")?;
        config.set_str("user.signingkey", key.to_str().unwrap())?;
        config.set_str("gpg.ssh.allowedSignersFile", allowed.to_str().unwrap())?;
        fs::write(
            key_dir.join("README.txt"),
            "Unencrypted disposable signing key for this demo only. Never use it outside this fixture.\n",
        )?;
    }
    Store::new(target.join("config/session.json")).save(
        1,
        &Session {
            tabs: vec![Tab {
                path: repo.root.clone(),
                selection: Selection::Work,
                ..Tab::default()
            }],
            ..Session::default()
        },
    )?;
    fs::write(
        target.join("README.txt"),
        format!(
            "Old commit: {}\nHEAD: {}\nUse the initial commit's Actions → Create tag at this commit, or TAGS → + and enter HEAD~2. Try Lightweight / Annotated / Signed with a multiline message.\n",
            commits[0], commits[2]
        ),
    )?;
    println!(
        "Fixture: {}\nConfig: {}",
        target.display(),
        target.join("config").display()
    );
    Ok(())
}
