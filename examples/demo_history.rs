//! Disposable history for trying selection, Reset and edit / split workflows.
use anyhow::{Result, ensure};
use gitbuddy::git::{Operation, Repository};
use std::{fs, path::PathBuf};
fn main() -> Result<()> {
    let destination = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("target/history-ui-20261005"));
    ensure!(!destination.exists(), "Destination already exists");
    let repo = Repository::init(&destination.join("project"))?;
    repo.execute(Operation::SetIdentity(
        "History Demo".into(),
        "demo@example.invalid".into(),
    ))?;
    let commit = |message: &str| -> Result<()> {
        repo.execute(Operation::StageAll)?;
        repo.execute(Operation::Commit(message.into()))?;
        Ok(())
    };
    let original = (0..20).map(|n| format!("line {n}\n")).collect::<String>();
    fs::write(repo.root.join("README.md"), &original)?;
    commit("Initial history demo")?;
    fs::write(
        repo.root.join("README.md"),
        original
            .replace("line 1\n", "first independent change\n")
            .replace("line 18\n", "second independent change\n"),
    )?;
    fs::write(
        repo.root.join("notes.txt"),
        "A new file to include in the second split commit.\n",
    )?;
    commit("Two changes ready to split")?;
    fs::write(
        repo.root.join("later.txt"),
        "A descendant that should replay after the split.\n",
    )?;
    commit("Keep this descendant")?;
    println!("{}", repo.root.display());
    Ok(())
}
