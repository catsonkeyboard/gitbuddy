//! Isolated stash fixtures; refuses to overwrite any existing destination.
use anyhow::{Result, ensure};
use gitbuddy::{
    git::{Operation, Repository},
    session::{Inspection, Selection, Session, Store, Tab},
};
use std::{fs, path::PathBuf, sync::Arc};
fn main() -> Result<()> {
    let target = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| "target/demo-stash".into());
    ensure!(!target.exists(), "Target exists: {}", target.display());
    fs::create_dir_all(&target)?;
    let repo = Repository::init(&target.join("project"))?;
    repo.execute(Operation::SetIdentity(
        "Stash Demo".into(),
        "demo@example.invalid".into(),
    ))?;
    for (path, text) in [
        ("config.toml", "theme = \"dark\"\nfont_size = 12\n"),
        ("README.md", "# Stash demo\n\nOriginal documentation.\n"),
        ("keep.txt", "Keep this file unchanged.\n"),
    ] {
        fs::write(repo.root.join(path), text)?;
    }
    repo.execute(Operation::StageAll)?;
    repo.execute(Operation::Commit("Initial demo project".into()))?;
    fs::write(
        repo.root.join("config.toml"),
        "theme = \"light\"\nfont_size = 12\n",
    )?;
    repo.execute(Operation::Stage(vec!["config.toml".into()]))?;
    fs::write(
        repo.root.join("config.toml"),
        "theme = \"light\"\nfont_size = 14\n",
    )?;
    fs::write(
        repo.root.join("README.md"),
        "# Stash demo\n\nSaved documentation draft.\n",
    )?;
    fs::write(repo.root.join("notes.txt"), "Untracked release notes.\n")?;
    repo.execute(Operation::SaveStash {
        context: Arc::new(repo.stash_context()?),
        paths: vec!["config.toml".into(), "README.md".into(), "notes.txt".into()],
        message: "Theme experiment with staged and unstaged changes".into(),
    })?;
    let id = repo.snapshot(1)?.stashes[0].0.clone();
    let session = Session {
        tabs: vec![Tab {
            path: repo.root.clone(),
            selection: Selection::Inspect,
            inspection: Some(Inspection::Stash(id)),
            ..Tab::default()
        }],
        ..Session::default()
    };
    Store::new(target.join("config/session.json")).save(1, &session)?;
    println!("{}", fs::canonicalize(target)?.display());
    Ok(())
}
