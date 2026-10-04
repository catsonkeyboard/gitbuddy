//! Isolated native UI fixture; refuses to overwrite an existing directory.
use gitbuddy::git::{Operation, Repository};
use std::path::PathBuf;
fn commit(repo: &Repository, path: &str, text: &str, message: &str) -> anyhow::Result<()> {
    std::fs::write(repo.root.join(path), text)?;
    repo.execute(Operation::StageAll)?;
    repo.execute(Operation::Commit(message.into()))?;
    Ok(())
}
fn init(path: &std::path::Path) -> anyhow::Result<Repository> {
    let repo = Repository::init(path)?;
    repo.execute(Operation::SetIdentity(
        "Demo Author".into(),
        "demo@example.invalid".into(),
    ))?;
    Ok(repo)
}
fn main() -> anyhow::Result<()> {
    let destination = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| "target/advanced-ui-20261004".into());
    anyhow::ensure!(!destination.exists(), "Destination already exists");
    std::fs::create_dir_all(&destination)?;
    let destination = destination.canonicalize()?;
    let source = init(&destination.join("library"))?;
    commit(
        &source,
        "README.md",
        "# Shared library\n",
        "Create shared library",
    )?;
    let repo = init(&destination.join("project"))?;
    commit(
        &repo,
        "README.md",
        "# Demo workspace\n",
        "Initialize workspace",
    )?;
    repo.execute(Operation::AddSubmodule {
        url: source.root.display().to_string(),
        path: "library".into(),
    })?;
    repo.execute(Operation::Commit("Add shared library submodule".into()))?;
    repo.execute(Operation::LfsTrack {
        pattern: "*.bin".into(),
        track: true,
    })?;
    commit(
        &repo,
        "asset.bin",
        "Demo LFS object\n",
        "Track binary assets with LFS",
    )?;
    commit(&repo, "one.txt", "first\n", "Add first component")?;
    commit(&repo, "two.txt", "second\n", "Add second component")?;
    repo.execute(Operation::CreateWorktree {
        name: "topic".into(),
        path: destination.join("topic"),
    })?;
    println!("{}", repo.root.display());
    Ok(())
}
