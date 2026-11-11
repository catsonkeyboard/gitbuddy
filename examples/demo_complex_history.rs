//! Disposable UI fixtures for rename groups, graph rebase and merge batches.
use anyhow::{Result, ensure};
use gitbuddy::git::{Operation, Repository};
use std::{
    fs,
    path::{Path, PathBuf},
};
fn init(path: &Path) -> Result<Repository> {
    let repo = Repository::init(path)?;
    repo.execute(Operation::SetIdentity(
        "Demo".into(),
        "demo@example.invalid".into(),
    ))?;
    let r = git2::Repository::open(&repo.root)?;
    r.config()?.set_bool("commit.gpgsign", false)?;
    r.config()?.set_str("core.hooksPath", "/dev/null")?;
    Ok(repo)
}
fn commit(repo: &Repository, name: &str) -> Result<String> {
    repo.execute(Operation::StageAll)?;
    repo.execute(Operation::Commit(name.into()))?;
    Ok(repo.snapshot(1)?.head_id.unwrap())
}
fn main() -> Result<()> {
    let destination = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| "target/complex-history-ui".into());
    ensure!(!destination.exists(), "Destination exists");
    fs::create_dir_all(&destination)?;
    let destination = destination.canonicalize()?;
    let rename = init(&destination.join("rename"))?;
    fs::write(rename.root.join("original"), "rename fixture\n")?;
    commit(&rename, "base")?;
    rename.execute(Operation::CreateBranch("feature".into()))?;
    fs::rename(rename.root.join("original"), rename.root.join("theirs"))?;
    commit(&rename, "rename theirs")?;
    rename.execute(Operation::Checkout("main".into()))?;
    fs::rename(rename.root.join("original"), rename.root.join("ours"))?;
    commit(&rename, "rename ours")?;
    ensure!(
        rename.execute(Operation::Merge("feature".into())).is_err(),
        "Expected rename conflict"
    );
    ensure!(
        rename.conflict_session()?.renames.len() == 1,
        "Missing rename group"
    );
    let graph = init(&destination.join("graph"))?;
    fs::write(graph.root.join("base"), "base")?;
    let base = commit(&graph, "base")?;
    graph.execute(Operation::CreateBranch("feature".into()))?;
    fs::write(graph.root.join("feature"), "feature")?;
    commit(&graph, "feature")?;
    graph.execute(Operation::Checkout("main".into()))?;
    fs::write(graph.root.join("main"), "main")?;
    commit(&graph, "main")?;
    graph.execute(Operation::Merge("feature".into()))?;
    let r = git2::Repository::open(&graph.root)?;
    let root = r.find_commit(base.parse()?)?;
    let mut builder = r.treebuilder(Some(&root.tree()?))?;
    builder.insert("upstream", r.blob(b"target")?, 0o100644)?;
    let tree = r.find_tree(builder.write()?)?;
    let sig = r.signature()?;
    let target = r.commit(None, &sig, &sig, "target", &tree, &[&root])?;
    r.branch("onto", &r.find_commit(target)?, false)?;
    r.tag_lightweight("basepoint", root.as_object(), false)?;
    let batch = init(&destination.join("batch"))?;
    fs::write(batch.root.join("base"), "base")?;
    commit(&batch, "batch base")?;
    for (name, path) in [("first", "one"), ("second", "two")] {
        batch.execute(Operation::CreateBranch(format!("feature-{name}")))?;
        fs::write(batch.root.join(path), format!("feature {name}"))?;
        commit(&batch, &format!("feature {name}"))?;
        batch.execute(Operation::Checkout("main".into()))?;
        fs::write(batch.root.join(format!("main-{name}")), name)?;
        commit(&batch, &format!("main before {name} merge"))?;
        batch.execute(Operation::Merge(format!("feature-{name}")))?;
    }
    let config = destination.join("config");
    fs::create_dir(&config)?;
    fs::write(
        config.join("settings.json"),
        serde_json::to_vec_pretty(
            &serde_json::json!({"recent":[rename.root,graph.root,batch.root]}),
        )?,
    )?;
    println!("{}", destination.display());
    Ok(())
}
