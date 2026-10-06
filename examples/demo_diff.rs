//! Build isolated Diff / block-resolution fixtures and an independent UI session.
use anyhow::{Result, ensure};
use gitbuddy::{
    git::{Operation, Repository},
    session::{PatchKey, Selection, Session, Store, Tab},
};
use std::{fs, path::PathBuf};
fn init(path: PathBuf) -> Result<Repository> {
    let repo = Repository::init(&path)?;
    repo.execute(Operation::SetIdentity(
        "Diff Demo".into(),
        "demo@example.invalid".into(),
    ))?;
    Ok(repo)
}
fn commit(repo: &Repository) -> Result<()> {
    repo.execute(Operation::StageAll)?;
    repo.execute(Operation::Commit("Demo snapshot".into()))?;
    Ok(())
}
fn main() -> Result<()> {
    let target = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| "target/demo-diff".into());
    ensure!(!target.exists(), "Target exists: {}", target.display());
    fs::create_dir_all(&target)?;
    let diff = init(target.join("diff"))?;
    let base = (1..=80)
        .map(|n| {
            format!(
                "let setting_{n} = old_value_{n}; // unchanged documentation for this setting\n"
            )
        })
        .collect::<String>();
    fs::write(diff.root.join("settings.rs"), &base)?;
    commit(&diff)?;
    fs::write(
        diff.root.join("settings.rs"),
        base.replace("old_value_8;", "new_value_8;")
            .replace("old_value_64;", "new_value_64;"),
    )?;
    let conflicts = init(target.join("conflicts"))?;
    let base = (1..=50)
        .map(|n| format!("setting_{n}=base\n"))
        .collect::<String>();
    fs::write(conflicts.root.join("config.txt"), &base)?;
    commit(&conflicts)?;
    conflicts.execute(Operation::CreateBranch("incoming".into()))?;
    fs::write(
        conflicts.root.join("config.txt"),
        base.replace("setting_5=base", "setting_5=remote")
            .replace("setting_35=base", "setting_35=remote")
            .replace("setting_48=base", "setting_48=remote-clean-edit"),
    )?;
    commit(&conflicts)?;
    conflicts.execute(Operation::Checkout("main".into()))?;
    fs::write(
        conflicts.root.join("config.txt"),
        base.replace("setting_5=base", "setting_5=local")
            .replace("setting_35=base", "setting_35=local"),
    )?;
    commit(&conflicts)?;
    ensure!(
        conflicts
            .execute(Operation::Merge("incoming".into()))
            .is_err(),
        "Expected conflicts"
    );
    let session = Session {
        tabs: vec![
            Tab {
                path: diff.root.clone(),
                expanded: vec![PatchKey::Work("settings.rs".into(), false)],
                ..Tab::default()
            },
            Tab {
                path: conflicts.root.clone(),
                selection: Selection::Conflicts,
                conflict_file: Some("config.txt".into()),
                ..Tab::default()
            },
        ],
        ..Session::default()
    };
    Store::new(target.join("config/session.json")).save(1, &session)?;
    println!("{}", fs::canonicalize(target)?.display());
    Ok(())
}
