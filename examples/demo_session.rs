//! Isolated, scrollable fixtures for restart validation; never touches an existing directory.
use gitbuddy::git::{Operation, Repository};
use std::{fs, path::PathBuf};
fn main() -> anyhow::Result<()> {
    let root = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| "target/session-ui-20261003".into());
    anyhow::ensure!(!root.exists(), "Choose a new fixture directory");
    for name in ["session-A", "session-B"] {
        let path = root.join(name);
        fs::create_dir_all(&path)?;
        let repo = Repository::init(&path)?;
        repo.execute(Operation::SetIdentity(
            "Session test".into(),
            "test@example.invalid".into(),
        ))?;
        for revision in 0..45 {
            for file in 0..8 {
                let text = (0..140).map(|line| format!("{name} file {file}: revision {revision}, line {line:03}, long content for horizontal scroll — {}\n", "content ".repeat(8))).collect::<String>();
                fs::write(path.join(format!("file-{file:02}.txt")), text)?;
            }
            repo.execute(Operation::StageAll)?;
            repo.execute(Operation::Commit(format!(
                "Session revision {revision:02}\n\nRestart validation commit in {name}."
            )))?;
        }
        for file in 0..8 {
            let text = (0..140)
                .map(|line| format!("{name} unsaved file {file}, modified line {line:03}\n"))
                .collect::<String>();
            fs::write(path.join(format!("file-{file:02}.txt")), text)?;
        }
        repo.execute(Operation::Stage(vec![
            "file-00.txt".into(),
            "file-01.txt".into(),
        ]))?;
        println!("{}", repo.root.display());
    }
    Ok(())
}
