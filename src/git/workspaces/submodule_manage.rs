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

impl Repository {
    pub(in crate::git) fn deinit_submodule(
        &self,
        expected: &SubmoduleInfo,
        remove: bool,
    ) -> Result<String> {
        let parent = self.checked_submodule_parent(expected)?;
        let repo = raw(&parent)?;
        clean(&repo)?;
        let mut refs = repo.transaction()?;
        refs.lock_ref("HEAD")?;
        let before = super::super::recovery::head_state(&repo)?;
        if before.reference != "HEAD" {
            refs.lock_ref(&before.reference)?;
        }
        let module = repo.find_submodule(&expected.name)?;
        let path = module_path(&repo, &module)?;
        let child = checked_child(&repo, &module)?;
        if let Some(child) = &child {
            clean_for_removal(child, 0)?;
            ensure!(
                child.head()?.target().map(|id| id.to_string()) == expected.workdir,
                "Submodule HEAD changed; refresh first"
            );
        } else if path.exists() {
            ensure!(
                path.is_dir() && fs::read_dir(&path)?.next().is_none(),
                "Uninitialized submodule contains files"
            );
        }
        let index = repo.index()?;
        let index_path = index.path().context("Missing parent index")?.to_path_buf();
        let mut lock = super::super::conflicts::IndexLock::acquire(&index_path)?;
        let modules = parent.root.join(".gitmodules");
        let meta = fs::symlink_metadata(&modules)?;
        ensure!(
            meta.is_file() && !meta.file_type().is_symlink() && meta.len() <= 2 * 1024 * 1024,
            ".gitmodules must be a regular file up to 2 MiB"
        );
        let original = fs::read(&modules)?;
        let backup = repo.path().join("gitbuddy-submodule-backups").join(
            chrono::Utc::now()
                .timestamp_nanos_opt()
                .unwrap_or_default()
                .to_string(),
        );
        fs::create_dir_all(&backup)?;
        fs::copy(&modules, backup.join("gitmodules.before"))?;
        fs::copy(&index_path, backup.join("index.before"))?;
        let manifest = serde_json::json!({"version":1,"parent":parent.root,"path":module.path(),"name":expected.name,"remove":remove});
        let mut journal = fs::File::create(backup.join("manifest.json"))?;
        journal.write_all(&serde_json::to_vec(&manifest)?)?;
        journal.sync_all()?;
        for name in ["gitmodules.before", "index.before"] {
            fs::File::open(backup.join(name))?.sync_all()?;
        }
        fs::File::open(&backup)?.sync_all()?;
        fs::File::open(backup.parent().context("Missing recovery directory")?)?.sync_all()?;
        fs::File::open(repo.path())?.sync_all()?;
        let mut replacement = None;
        let mut next_bytes = None;
        if remove {
            let scratch = tempfile::NamedTempFile::new_in(repo.path())?;
            fs::copy(&modules, scratch.path())?;
            {
                let mut cfg = git2::Config::open(scratch.path())?;
                let scope = format!("submodule.{}", expected.name);
                let mut keys = Vec::new();
                cfg.entries(None)?.for_each(|entry| {
                    if let Some(name) = entry.name().ok()
                        && name
                            .rsplit_once('.')
                            .is_some_and(|(section, _)| section == scope)
                    {
                        keys.push(name.to_owned());
                    }
                })?;
                ensure!(
                    !keys.is_empty(),
                    "Submodule section was not found in .gitmodules; inspect it before removing"
                );
                for key in keys {
                    cfg.remove_multivar(&key, ".*")?;
                }
            }
            let data = fs::read(scratch.path())?;
            let scratch_index = tempfile::NamedTempFile::new_in(repo.path())?;
            fs::copy(&index_path, scratch_index.path())?;
            let mut next = git2::Index::open(scratch_index.path())?;
            next.remove_path(module.path())?;
            let mut entry = next
                .get_path(Path::new(".gitmodules"), 0)
                .context("Stage .gitmodules before removing a submodule")?;
            entry.id = repo.blob(&data)?;
            entry.file_size = data.len() as u32;
            entry.mtime = git2::IndexTime::new(0, 0);
            entry.ctime = git2::IndexTime::new(0, 0);
            next.add(&entry)?;
            next.write()?;
            next_bytes = Some(fs::read(scratch_index.path())?);
            let mut temp = tempfile::NamedTempFile::new_in(&parent.root)?;
            super::super::patch_batch::copy_metadata(&modules, temp.as_file())?;
            temp.write_all(&data)?;
            temp.as_file().sync_all()?;
            replacement = Some(temp);
        }
        ensure!(
            fs::read(&modules)? == original && super::super::recovery::head_state(&repo)? == before,
            "Parent changed during preparation"
        );
        let moved = child.is_some();
        if moved {
            rename_checkout(&path, &backup.join("worktree"))?;
        }
        let deactivate = (|| -> Result<()> {
            let mut config = repo.config()?;
            for suffix in ["url", "active"] {
                match config.remove_multivar(&format!("submodule.{}.{suffix}", expected.name), ".*")
                {
                    Err(error) if error.code() == git2::ErrorCode::NotFound => {}
                    other => other?,
                }
            }
            Ok(())
        })();
        if let Err(error) = deactivate {
            if moved && !path.exists() {
                let _ = rename_checkout(&backup.join("worktree"), &path);
            }
            return Err(error.context(format!(
                "Could not deactivate submodule; checkout or recovery copy retained at {}",
                backup.display()
            )));
        }
        let result = (|| -> Result<()> {
            if let Some(temp) = replacement {
                temp.persist(&modules)?;
            }
            if let Some(bytes) = next_bytes {
                lock.write_and_install(&bytes, &index_path)?;
            }
            Ok(())
        })();
        if let Err(error) = result {
            // No user files are deleted on failure. Retain the snapshot for explicit recovery.
            if moved && !path.exists() {
                let _ = rename_checkout(&backup.join("worktree"), &path);
            }
            return Err(error.context(format!(
                "Submodule operation was interrupted; inspect backups at {}",
                backup.display()
            )));
        }
        Ok(format!(
            "Submodule {}; original checkout and metadata retained at {}{}",
            if remove {
                "removed from index and .gitmodules"
            } else {
                "deinitialized"
            },
            backup.display(),
            if remove {
                "; review and commit parent changes"
            } else {
                "; Initialize / Update can recreate its checkout"
            }
        ))
    }
}
