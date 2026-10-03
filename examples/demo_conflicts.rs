//! Create an isolated conflict fixture; refuse to overwrite existing paths.
use anyhow::{Result, ensure};
use gitbuddy::git::{Operation, Repository};
use std::{fs, path::PathBuf};
fn commit(repo: &Repository, message: &str) -> Result<()> {
    repo.execute(Operation::StageAll)?;
    repo.execute(Operation::Commit(message.into()))?;
    Ok(())
}
fn main() -> Result<()> {
    let path = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("target/demo-conflicts"));
    ensure!(!path.exists(), "Target already exists: {}", path.display());
    let repo = Repository::init(&path)?;
    repo.execute(Operation::SetIdentity(
        "Conflict Demo".into(),
        "demo@example.invalid".into(),
    ))?;
    fs::create_dir(repo.root.join("src"))?;
    fs::write(
        repo.root.join("README.md"),
        "# Conflict resolution demo\n\nFour independent conflicts to review.\n",
    )?;
    fs::write(
        repo.root.join("src/main.rs"),
        "fn main() {\n    let timeout = 5;\n    println!(\"Timeout: {timeout}\");\n}\n",
    )?;
    fs::write(repo.root.join("config.txt"), "endpoint=base\nretries=1\n")?;
    fs::write(repo.root.join("obsolete.txt"), "original record\n")?;
    fs::write(repo.root.join("asset.bin"), b"base\0image")?;
    commit(&repo, "Initial settings")?;
    repo.execute(Operation::CreateBranch("feature/incoming".into()))?;
    fs::write(
        repo.root.join("src/main.rs"),
        "fn main() {\n    let timeout = 20;\n    println!(\"Timeout: {timeout}\");\n}\n",
    )?;
    fs::write(repo.root.join("config.txt"), "endpoint=remote\nretries=3\n")?;
    fs::write(repo.root.join("obsolete.txt"), "updated incoming record\n")?;
    fs::write(repo.root.join("asset.bin"), b"incoming\0image")?;
    commit(&repo, "Incoming configuration")?;
    repo.execute(Operation::Checkout("main".into()))?;
    fs::write(
        repo.root.join("src/main.rs"),
        "fn main() {\n    let timeout = 10;\n    println!(\"Timeout: {timeout}\");\n}\n",
    )?;
    fs::write(repo.root.join("config.txt"), "endpoint=local\nretries=2\n")?;
    fs::remove_file(repo.root.join("obsolete.txt"))?;
    fs::write(repo.root.join("asset.bin"), b"local\0image")?;
    commit(&repo, "Local configuration")?;
    ensure!(
        repo.execute(Operation::Merge("feature/incoming".into()))
            .is_err(),
        "Expected conflicts"
    );
    ensure!(
        repo.conflict_session()?.files.len() == 4,
        "Expected four conflict files"
    );
    println!("{}", repo.root.display());
    Ok(())
}
