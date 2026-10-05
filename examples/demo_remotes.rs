//! Disposable local bare remotes for trying remote management and push options.
use anyhow::{Result, ensure};
use gitbuddy::git::{Operation, Repository};
use std::{fs, path::PathBuf};

fn main() -> Result<()> {
    let destination = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("target/remote-ui-20261005"));
    ensure!(!destination.exists(), "Destination already exists");
    fs::create_dir_all(&destination)?;
    let destination = destination.canonicalize()?;
    let origin = git2::Repository::init_bare(destination.join("origin.git"))?;
    origin.set_head("refs/heads/main")?;
    git2::Repository::init_bare(destination.join("backup.git"))?;
    let repo = Repository::init(&destination.join("project"))?;
    repo.execute(Operation::SetIdentity(
        "Demo".into(),
        "demo@example.invalid".into(),
    ))?;
    fs::write(repo.root.join("README.md"), "# Remote management demo\n")?;
    repo.execute(Operation::StageAll)?;
    repo.execute(Operation::Commit("Initial demo".into()))?;
    repo.execute(Operation::AddRemote(
        "origin".into(),
        destination.join("origin.git").display().to_string(),
    ))?;
    repo.execute(Operation::AddRemote(
        "backup".into(),
        destination.join("backup.git").display().to_string(),
    ))?;
    repo.execute(Operation::Push)?;
    repo.execute(Operation::Tag("v1.0.0".into()))?;
    let raw = git2::Repository::open(&repo.root)?;
    raw.branch("feature/demo", &raw.head()?.peel_to_commit()?, false)?;
    raw.tag(
        "v1.0.0-notes",
        &raw.head()?.peel(git2::ObjectType::Commit)?,
        &raw.signature()?,
        "Annotated demo release",
        false,
    )?;
    fs::write(
        repo.root.join("README.md"),
        "# Remote management demo\n\nOne local commit ready to push.\n",
    )?;
    repo.execute(Operation::StageAll)?;
    repo.execute(Operation::Commit("Prepare a local change".into()))?;
    println!("{}", repo.root.display());
    Ok(())
}
