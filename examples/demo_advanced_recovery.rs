//! Isolated fixtures for replay recovery, recursive modules, worktrees and LFS.
use anyhow::{Result, ensure};
use gitbuddy::{
    git::{Operation, Repository},
    session::{Session, Store, Tab},
};
use std::{
    fs,
    path::{Path, PathBuf},
};
fn init(path: &Path) -> Result<Repository> {
    let repo = Repository::init(path)?;
    repo.execute(Operation::SetIdentity(
        "Advanced Demo".into(),
        "demo@example.invalid".into(),
    ))?;
    Ok(repo)
}
fn commit(repo: &Repository, path: &str, content: &str, message: &str) -> Result<()> {
    fs::write(repo.root.join(path), content)?;
    repo.execute(Operation::StageAll)?;
    repo.execute(Operation::Commit(message.into()))?;
    Ok(())
}
fn main() -> Result<()> {
    let target = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| "target/demo-advanced-recovery".into());
    ensure!(!target.exists(), "Target exists: {}", target.display());
    fs::create_dir_all(&target)?;
    let target = target.canonicalize()?;
    let leaf = init(&target.join("leaf"))?;
    commit(&leaf, "README.md", "# Leaf module\n", "Leaf module")?;
    let middle = init(&target.join("middle"))?;
    commit(&middle, "README.md", "# Middle module\n", "Middle module")?;
    middle.execute(Operation::AddSubmodule {
        url: leaf.root.display().to_string(),
        path: "nested".into(),
    })?;
    middle.execute(Operation::Commit("Add nested module".into()))?;
    let repo = init(&target.join("project"))?;
    commit(
        &repo,
        "README.md",
        "# Advanced workspace\n",
        "Initial project",
    )?;
    repo.execute(Operation::AddSubmodule {
        url: middle.root.display().to_string(),
        path: "modules".into(),
    })?;
    repo.execute(Operation::Commit("Add recursive modules".into()))?;
    repo.execute(Operation::UpdateSubmodules {
        names: vec![],
        recursive: true,
    })?;
    repo.execute(Operation::LfsTrack {
        pattern: "*.bin".into(),
        track: true,
    })?;
    commit(&repo, "asset.bin", "Cached asset data\n", "Add LFS asset")?;
    fs::write(
        repo.root.join("asset.bin"),
        "Unreferenced cache object for maintenance\n",
    )?;
    repo.execute(Operation::StageAll)?;
    repo.execute(Operation::UnstageAll)?;
    repo.execute(Operation::Discard("asset.bin".into()))?;
    repo.execute(Operation::CreateWorktree {
        name: "linked".into(),
        path: target.join("linked"),
    })?;
    commit(&repo, "feature.txt", "Feature\n", "Add feature")?;
    commit(&repo, "notes.txt", "Notes\n", "Add notes")?;
    commit(
        &repo,
        "feature.txt",
        "Feature fixed\n",
        "fixup! Add feature",
    )?;
    let recovery = init(&target.join("recovery"))?;
    commit(&recovery, "settings.txt", "mode=base\n", "Base")?;
    recovery.execute(Operation::CreateBranch("feature".into()))?;
    commit(
        &recovery,
        "settings.txt",
        "mode=feature\n",
        "Feature change",
    )?;
    recovery.execute(Operation::Checkout("main".into()))?;
    commit(&recovery, "settings.txt", "mode=main\n", "Main change")?;
    recovery.execute(Operation::Checkout("feature".into()))?;
    let preview = recovery.rebase_preview("main")?;
    ensure!(
        recovery
            .execute(Operation::Rebase {
                steps: preview.steps.clone(),
                context: std::sync::Arc::new(preview)
            })
            .is_err(),
        "Expected conflict"
    );
    let raw = git2::Repository::open(&recovery.root)?;
    let journal = raw.path().join("gitbuddy-rebase.json");
    let mut state: serde_json::Value = serde_json::from_slice(&fs::read(&journal)?)?;
    state["applied"] = false.into();
    fs::write(journal, serde_json::to_vec_pretty(&state)?)?;
    fs::write(
        recovery.root.join("settings.txt"),
        "mode=my-unsaved-resolution\n",
    )?;
    Store::new(target.join("config/session.json")).save(
        1,
        &Session {
            tabs: vec![
                Tab {
                    path: repo.root,
                    ..Tab::default()
                },
                Tab {
                    path: recovery.root,
                    ..Tab::default()
                },
            ],
            ..Session::default()
        },
    )?;
    println!("{}", target.display());
    Ok(())
}
