//! Creates an isolated sample repository. Refuses an existing destination.
use gitbuddy::git::{Operation, Repository};
use std::{path::PathBuf, process::Command};
fn main() -> anyhow::Result<()> {
    let path = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("target/demo-repo"));
    anyhow::ensure!(
        !path.exists(),
        "Destination already exists: {}",
        path.display()
    );
    let repo = Repository::init(&path)?;
    repo.execute(Operation::SetIdentity(
        "Alex Chen".into(),
        "alex@example.invalid".into(),
    ))?;
    for (key, value) in [("commit.gpgsign", "false"), ("core.hooksPath", "/dev/null")] {
        let status = Command::new("git")
            .arg("-C")
            .arg(&repo.root)
            .args(["config", key, value])
            .status()?;
        anyhow::ensure!(status.success(), "Cannot configure sample repository");
    }
    std::fs::create_dir_all(repo.root.join("src"))?;
    std::fs::write(
        repo.root.join("README.md"),
        "# GitBuddy\n\nA calmer way to work with Git.\n\n## Features\n\n- Native performance\n- Commit history\n- Branch management\n",
    )?;
    std::fs::write(repo.root.join(".gitignore"), "/target\n.DS_Store\n")?;
    for (i, title) in [
        "Initial project structure",
        "Add repository discovery",
        "Build commit history panel",
        "Add branch navigation",
        "Render unified file diffs",
        "Support staging and commits",
        "Polish dark workspace theme",
    ]
    .iter()
    .enumerate()
    {
        std::fs::write(
            repo.root.join("src/app.rs"),
            format!(
                "// GitBuddy workspace\n// Revision {i}\npub const NAME: &str = \"GitBuddy\";\npub const REFRESH_SECONDS: u64 = 10;\n"
            ),
        )?;
        repo.execute(Operation::StageAll)?;
        repo.execute(Operation::Commit((*title).into()))?;
    }
    repo.execute(Operation::Tag("v0.1.0".into()))?;
    repo.execute(Operation::CreateBranch("feature/workspace".into()))?;
    std::fs::write(
        repo.root.join("src/app.rs"),
        "// GitBuddy workspace\n// Refresh when the workspace changes\npub const NAME: &str = \"GitBuddy\";\npub const REFRESH_SECONDS: u64 = 5;\n",
    )?;
    repo.execute(Operation::Stage(vec!["src/app.rs".into()]))?;
    std::fs::write(
        repo.root.join("README.md"),
        "# GitBuddy\n\nA calmer, clearer way to work with Git.\n\n## Features\n\n- Native performance\n- Commit history\n- Branch management\n- Staging and unified diffs\n- Remote synchronization\n",
    )?;
    println!("{}", repo.root.display());
    Ok(())
}
