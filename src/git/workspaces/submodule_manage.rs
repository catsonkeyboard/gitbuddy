use super::*;
use std::{fs, io::Write};
fn rename_checkout(source: &Path, destination: &Path) -> Result<()> {
    fs::rename(source, destination)?;
    fs::File::open(destination.parent().context("Missing destination parent")?)?.sync_all()?;
    fs::File::open(source.parent().context("Missing source parent")?)?.sync_all()?;
    Ok(())
}

pub(super) fn restore_checkout(
    repo: &git2::Repository,
    module: &git2::Submodule<'_>,
) -> Result<()> {
    let target = module_path(repo, module)?;
    if target.exists() {
        return Ok(());
    }
    let root = repo.path().join("gitbuddy-submodule-backups");
    if !root.exists() {
        return Ok(());
    }
    ensure!(
        !fs::symlink_metadata(&root)?.file_type().is_symlink(),
        "Invalid submodule backup directory"
    );
    let mut backups = fs::read_dir(&root)?.collect::<std::io::Result<Vec<_>>>()?;
    backups.sort_by_key(|e| std::cmp::Reverse(e.file_name()));
    for entry in backups {
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let saved = entry.path().join("worktree");
        if !saved.exists() {
            continue;
        }
        let saved_meta = fs::symlink_metadata(&saved)?;
        ensure!(
            saved_meta.is_dir() && !saved_meta.file_type().is_symlink(),
            "Submodule recovery checkout must not be a symlink"
        );
        let manifest = entry.path().join("manifest.json");
        let meta = fs::symlink_metadata(&manifest)?;
        ensure!(
            meta.is_file() && !meta.file_type().is_symlink() && meta.len() < 65536,
            "Invalid submodule recovery manifest"
        );
        let value: serde_json::Value = serde_json::from_slice(&fs::read(manifest)?)?;
        if value["version"] == 1
            && value["remove"] == false
            && value["name"].as_str() == Some(module.name()?)
            && value["path"].as_str().map(Path::new) == Some(module.path())
        {
            let parent = Path::new(
                value["parent"]
                    .as_str()
                    .context("Invalid recovery parent")?,
            );
            ensure!(
                parent.canonicalize()?
                    == repo
                        .workdir()
                        .context("No parent working directory")?
                        .canonicalize()?,
                "Recovery belongs to a moved parent; inspect before restoring"
            );
            fs::create_dir_all(target.parent().context("Missing module parent")?)?;
            rename_checkout(&saved, &target)?;
            let mut mark = fs::File::create(entry.path().join("restored"))?;
            mark.write_all(b"restored\n")?;
            mark.sync_all()?;
            fs::File::open(entry.path())?.sync_all()?;
            return Ok(());
        }
    }
    Ok(())
}
