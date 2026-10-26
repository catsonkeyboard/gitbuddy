//! Disposable branch-management fixture; refuses existing destinations.
use anyhow::{Result, ensure};
use gitbuddy::git::{Operation, PushSelection, Repository};
use std::{fs, path::PathBuf, sync::Arc};

fn main() -> Result<()> {
    let destination = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| "target/demo-branches".into());
    ensure!(
        !destination.exists(),
        "Refusing to overwrite {}",
        destination.display()
    );
    fs::create_dir_all(&destination)?;
    let destination = destination.canonicalize()?;
    let remote_path = destination.join("origin.git");
    git2::Repository::init_bare(&remote_path)?.set_head("refs/heads/main")?;
    let repo = Repository::init(&destination.join("project"))?;
    repo.execute(Operation::SetIdentity(
        "GitBuddy Demo".into(),
        "demo@example.invalid".into(),
    ))?;
    fs::write(
        repo.root.join("README.md"),
        "# Branch management demo\n\nRelease version.\n",
    )?;
    repo.execute(Operation::StageAll)?;
    repo.execute(Operation::Commit("Release version".into()))?;
    let raw = git2::Repository::open(&repo.root)?;
    raw.tag(
        "v1.0",
        raw.head()?.peel_to_commit()?.as_object(),
        &raw.signature()?,
        "Annotated release",
        false,
    )?;
    repo.execute(Operation::CreateBranchAt {
        name: "feature/demo".into(),
        target: repo.snapshot(10)?.head_id.unwrap(),
    })?;
    fs::write(
        repo.root.join("README.md"),
        "# Branch management demo\n\nDevelopment version.\n",
    )?;
    repo.execute(Operation::StageAll)?;
    repo.execute(Operation::Commit("Development version".into()))?;
    repo.execute(Operation::AddRemote(
        "origin".into(),
        remote_path.display().to_string(),
    ))?;
    repo.execute(Operation::Push)?;
    let plan = repo.prepare_push(PushSelection {
        remote: "origin".into(),
        branch: Some("feature/demo".into()),
        target: "feature/demo".into(),
        ..Default::default()
    })?;
    repo.execute(Operation::PushTo(Arc::new(plan)))?;
    repo.execute(Operation::Fetch)?;
    let config = destination.join("config");
    fs::create_dir(&config)?;
    fs::write(
        config.join("settings.json"),
        serde_json::to_vec_pretty(&serde_json::json!({"recent": [repo.root]}))?,
    )?;
    println!(
        "Fixture: {}\nConfig: {}",
        destination.display(),
        config.display()
    );
    Ok(())
}
