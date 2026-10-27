//! Isolated source-selection and remote-tag fixture, refusing existing paths.
use anyhow::{Result, ensure};
use gitbuddy::{
    git::{Operation, Repository},
    session::{Selection, Session, Store, Tab},
};
use std::{fs, path::PathBuf};
fn commit(repo: &Repository, file: &str, text: &str) -> Result<()> {
    fs::write(repo.root.join(file), text)?;
    repo.execute(Operation::StageAll)?;
    repo.execute(Operation::Commit(format!("Update {file}")))?;
    Ok(())
}
fn main() -> Result<()> {
    let target = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| "target/demo-remote-operations".into());
    ensure!(
        !target.exists(),
        "Refusing to overwrite {}",
        target.display()
    );
    fs::create_dir_all(&target)?;
    let target = target.canonicalize()?;
    let origin = target.join("origin.git");
    let backup = target.join("backup.git");
    for path in [&origin, &backup] {
        git2::Repository::init_bare(path)?.set_head("refs/heads/main")?;
    }
    let source = Repository::init(&target.join("source"))?;
    source.execute(Operation::SetIdentity(
        "Remote Demo".into(),
        "demo@example.invalid".into(),
    ))?;
    commit(&source, "README.md", "# Remote operations demo\n")?;
    let r = git2::Repository::open(&source.root)?;
    let base = r.head()?.peel_to_commit()?;
    r.branch("topic/demo", &base, false)?;
    for path in [&origin, &backup] {
        r.remote_anonymous(path.to_str().unwrap())?.push(
            &[
                "refs/heads/main:refs/heads/main",
                "refs/heads/topic/demo:refs/heads/topic/demo",
            ],
            None,
        )?;
    }
    let client = Repository::clone_repo(origin.to_str().unwrap(), &target.join("client"))?;
    let other = Repository::clone_repo(origin.to_str().unwrap(), &target.join("other-client"))?;
    for repo in [&client, &other] {
        repo.execute(Operation::SetIdentity(
            "Remote Demo".into(),
            "demo@example.invalid".into(),
        ))?;
        let raw = git2::Repository::open(&repo.root)?;
        raw.remote("team/backup", backup.to_str().unwrap())?;
        let local_base = raw.find_commit(base.id())?;
        raw.tag(
            "release/annotated",
            local_base.as_object(),
            &raw.signature()?,
            "Local tag must be preserved",
            false,
        )?;
    }
    source.execute(Operation::Checkout("topic/demo".into()))?;
    commit(&source, "origin.txt", "Change from origin topic\n")?;
    r.find_remote("origin")
        .or_else(|_| r.remote("origin", origin.to_str().unwrap()))?
        .push(&["refs/heads/topic/demo:refs/heads/topic/demo"], None)?;
    let topic = r.head()?.peel_to_commit()?;
    r.tag(
        "release/annotated",
        topic.as_object(),
        &r.signature()?,
        "Remote annotated release",
        false,
    )?;
    r.tag_lightweight("release/light", topic.as_object(), false)?;
    r.find_remote("origin")?.push(
        &[
            "refs/tags/release/annotated:refs/tags/release/annotated",
            "refs/tags/release/light:refs/tags/release/light",
        ],
        None,
    )?;
    source.execute(Operation::Checkout("main".into()))?;
    commit(&source, "backup.txt", "Change from backup topic\n")?;
    r.remote_anonymous(backup.to_str().unwrap())?
        .push(&["refs/heads/main:refs/heads/topic/demo"], None)?;
    commit(
        &source,
        "remote-main.txt",
        "Origin main advances independently\n",
    )?;
    r.find_remote("origin")?
        .push(&["refs/heads/main:refs/heads/main"], None)?;
    Store::new(target.join("config/session.json")).save(
        1,
        &Session {
            tabs: [&client, &other]
                .iter()
                .map(|repo| Tab {
                    path: repo.root.clone(),
                    selection: Selection::Work,
                    ..Tab::default()
                })
                .collect(),
            ..Session::default()
        },
    )?;
    fs::write(
        target.join("README.txt"),
        "Fetch origin → Query branches → topic/demo: only origin/topic/demo should move.\nPull team/backup → topic/demo: current main moves to the backup topic, preserving origin/main upstream configuration.\nRemote tags → origin → release/annotated: query the server object, confirm deletion, preserve the different local annotated tag.\nAll repositories and remotes here are disposable.\n",
    )?;
    println!(
        "Fixture: {}\nConfig: {}",
        target.display(),
        target.join("config").display()
    );
    Ok(())
}
