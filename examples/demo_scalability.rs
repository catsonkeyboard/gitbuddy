//! Isolated, reproducible large-list fixture. Never reuses an existing directory.
use anyhow::{Result, ensure};
use std::path::PathBuf;

fn main() -> Result<()> {
    let destination = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| "target/demo-scalability".into());
    ensure!(
        !destination.exists(),
        "Refusing to overwrite {}",
        destination.display()
    );
    std::fs::create_dir_all(&destination)?;
    let destination = destination.canonicalize()?;
    let repo_path = destination.join("large-repo");
    let repo = git2::Repository::init(&repo_path)?;
    let signature = git2::Signature::now("GitBuddy Demo", "demo@example.invalid")?;
    let empty = repo.treebuilder(None)?.write()?;
    let mut parent = repo.commit(
        Some("HEAD"),
        &signature,
        &signature,
        "Initial empty tree",
        &repo.find_tree(empty)?,
        &[],
    )?;
    for i in 1..5_000 {
        parent = repo.commit(
            Some("HEAD"),
            &signature,
            &signature,
            &format!("History entry {i:05}"),
            &repo.find_tree(empty)?,
            &[&repo.find_commit(parent)?],
        )?;
    }
    let mut index = repo.index()?;
    for i in 0..2_000 {
        let name = format!("file-{i:04}.txt");
        std::fs::write(repo_path.join(&name), format!("File {i}\nBase content\n"))?;
        index.add_path(std::path::Path::new(&name))?;
    }
    index.write()?;
    let tree = index.write_tree()?;
    repo.commit(
        Some("HEAD"),
        &signature,
        &signature,
        "Add 2,000 files for virtual lists",
        &repo.find_tree(tree)?,
        &[&repo.find_commit(parent)?],
    )?;
    for i in 0..2_000 {
        std::fs::write(
            repo_path.join(format!("file-{i:04}.txt")),
            format!("File {i}\nChanged content\nAdded line\n"),
        )?;
    }
    for i in 0..1_000 {
        repo.tag_lightweight(
            &format!("tag-{i:04}"),
            &repo.head()?.peel(git2::ObjectType::Commit)?,
            false,
        )?;
    }
    let config = destination.join("config");
    std::fs::create_dir(&config)?;
    std::fs::write(
        config.join("settings.json"),
        serde_json::to_vec_pretty(
            &serde_json::json!({"recent":[repo_path], "preferences":{"history_limit":5000}}),
        )?,
    )?;
    println!("{}", destination.display());
    println!(
        "5,001 commits · 2,000 changed files · 1,000 tags; isolated config: {}",
        config.display()
    );
    Ok(())
}
