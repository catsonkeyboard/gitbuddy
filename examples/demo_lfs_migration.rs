//! A disposable, unsigned local history for native migration review/apply QA.
use anyhow::{Result, ensure};
use gitbuddy::git::{NetworkControl, Operation, ProgressSink, Repository};
use std::{
    fs,
    path::PathBuf,
    sync::{Arc, Mutex},
};
fn main() -> Result<()> {
    let interrupted = std::env::args_os()
        .nth(2)
        .is_some_and(|arg| arg == "--interrupt");
    let destination = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| "target/lfs-migration-ui".into());
    ensure!(!destination.exists(), "Destination already exists");
    fs::create_dir_all(&destination)?;
    let destination = destination.canonicalize()?;
    let repo = Repository::init(&destination.join("project"))?;
    repo.execute(Operation::SetIdentity(
        "Migration Demo".into(),
        "demo@example.invalid".into(),
    ))?;
    let r = git2::Repository::open(&repo.root)?;
    r.config()?.set_bool("commit.gpgsign", false)?;
    r.config()?.set_str("core.hooksPath", "/dev/null")?;
    let tool = std::env::var_os("GITBUDDY_TEST_GIT_LFS")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("target/tooling-lfs-v3.8.1/git-lfs-3.8.1/git-lfs")
        });
    if tool.is_file() {
        r.config()?
            .set_str("gitbuddy.lfsProgram", tool.to_str().unwrap())?;
    }
    fs::write(repo.root.join("README.md"), "# LFS migration demo\n")?;
    fs::write(repo.root.join("asset.bin"), b"first\0binary fixture\xff")?;
    repo.execute(Operation::StageAll)?;
    repo.execute(Operation::Commit("initial raw binary".into()))?;
    let base = r.head()?.peel_to_commit()?;
    r.branch("keep-original", &base, false)?;
    fs::write(repo.root.join("asset.bin"), b"second\0binary fixture\xff")?;
    repo.execute(Operation::StageAll)?;
    repo.execute(Operation::Commit("update raw binary".into()))?;
    let config = destination.join("config");
    fs::create_dir(&config)?;
    fs::write(
        config.join("settings.json"),
        serde_json::to_vec_pretty(&serde_json::json!({"recent":[repo.root]}))?,
    )?;
    if interrupted {
        let plan =
            Arc::new(repo.prepare_lfs_migration("*.bin", false, NetworkControl::default())?);
        let sink: ProgressSink = Arc::new(Mutex::new(|phase: &str| {
            assert_ne!(
                phase, "LFS migration: checkout verified",
                "disposable recovery fixture checkpoint"
            );
        }));
        let stopped = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            repo.execute_with_control(
                Operation::LfsMigrate(plan),
                NetworkControl::new(Some(sink), Default::default()),
            )
        }));
        ensure!(
            stopped.is_err()
                && repo
                    .lfs_migration_status()?
                    .is_some_and(|status| status.phase == "CheckedOut"),
            "Could not create the reviewed interrupted fixture"
        );
    }
    println!("{}", destination.display());
    Ok(())
}
