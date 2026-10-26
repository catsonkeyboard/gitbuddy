//! Disposable Merge / Pull fixtures; refuses existing destinations.
use anyhow::{Result, ensure};
use gitbuddy::{
    git::{Operation, Repository},
    session::{Selection, Session, Store, Tab},
};
use std::{fs, path::PathBuf};

fn commit(repo: &Repository, path: &str, text: &str, message: &str) -> Result<()> {
    fs::write(repo.root.join(path), text)?;
    repo.execute(Operation::StageAll)?;
    repo.execute(Operation::Commit(message.into()))?;
    Ok(())
}
fn identity(repo: &Repository) -> Result<()> {
    repo.execute(Operation::SetIdentity(
        "Integration Demo".into(),
        "demo@example.invalid".into(),
    ))?;
    Ok(())
}
fn main() -> Result<()> {
    let target = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| "target/demo-merge-pull".into());
    ensure!(
        !target.exists(),
        "Refusing to overwrite {}",
        target.display()
    );
    fs::create_dir_all(&target)?;
    let target = target.canonicalize()?;
    let server = target.join("origin.git");
    git2::Repository::init_bare(&server)?.set_head("refs/heads/main")?;
    let source = Repository::init(&target.join("source"))?;
    identity(&source)?;
    commit(
        &source,
        "settings.toml",
        "theme = \"dark\"\n",
        "Initial configuration",
    )?;
    source.execute(Operation::AddRemote(
        "origin".into(),
        server.display().to_string(),
    ))?;
    source.execute(Operation::Push)?;
    let mut repos = Vec::new();
    for name in [
        "fast-forward",
        "merge-or-squash",
        "pull-rebase",
        "pull-conflict",
    ] {
        let repo = Repository::clone_repo(server.to_str().unwrap(), &target.join(name))?;
        identity(&repo)?;
        if name == "pull-conflict" {
            commit(&repo, "settings.toml", "theme = \"local\"\n", "Local theme")?;
        } else if name != "fast-forward" {
            commit(
                &repo,
                "README.md",
                "# Local documentation\n",
                "Local documentation",
            )?;
        }
        repos.push(repo);
    }
    commit(
        &source,
        "settings.toml",
        "theme = \"remote\"\n",
        "Remote theme",
    )?;
    source.execute(Operation::Push)?;
    for repo in &repos {
        repo.execute(Operation::Fetch)?;
        let raw = git2::Repository::open(&repo.root)?;
        let remote = raw
            .find_reference("refs/remotes/origin/main")?
            .peel_to_commit()?;
        raw.branch("feature/remote", &remote, false)?;
    }
    Store::new(target.join("config/session.json")).save(
        1,
        &Session {
            tabs: repos
                .iter()
                .map(|repo| Tab {
                    path: repo.root.clone(),
                    selection: Selection::Work,
                    ..Tab::default()
                })
                .collect(),
            ..Session::default()
        },
    )?;
    fs::write(
        target.join("README.txt"),
        "Use feature/remote → Merge to compare default, no-ff and squash.\nPull: fast-forward has no local changes; merge-or-squash and pull-rebase diverge without conflict; pull-conflict conflicts in settings.toml.\nAll remotes are local disposable bare repositories. Recreate the fixture to compare strategies from the same starting point.\n",
    )?;
    println!(
        "Fixture: {}\nConfig: {}",
        target.display(),
        target.join("config").display()
    );
    Ok(())
}
