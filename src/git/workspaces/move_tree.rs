use super::*;
use serde::{Deserialize, Serialize};
use std::{fs, io::Write};

#[derive(Serialize, Deserialize)]
struct Move {
    version: u32,
    old: PathBuf,
    new: PathBuf,
    gitdir: PathBuf,
}
fn atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("No parent directory")?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    temp.write_all(bytes)?;
    temp.as_file().sync_all()?;
    temp.persist(path)?;
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}
fn admin(repo: &git2::Repository, name: &str) -> Result<PathBuf> {
    ensure!(
        !name.is_empty()
            && Path::new(name).components().count() == 1
            && !matches!(name, "." | ".."),
        "Invalid worktree name"
    );
    let root = repo.commondir().join("worktrees").canonicalize()?;
    let dir = root.join(name);
    ensure!(
        !fs::symlink_metadata(&dir)?.file_type().is_symlink()
            && dir.canonicalize()?.parent() == Some(root.as_path()),
        "Invalid worktree administration directory"
    );
    Ok(dir)
}
fn link_target(path: &Path) -> Result<PathBuf> {
    ensure!(path.is_absolute(), "Use an absolute worktree path");
    let root = fs::symlink_metadata(path)?;
    ensure!(
        root.is_dir() && !root.file_type().is_symlink(),
        "Worktree path must be an ordinary directory"
    );
    let link = path.join(".git");
    let meta = fs::symlink_metadata(&link)?;
    ensure!(
        meta.is_file() && !meta.file_type().is_symlink() && meta.len() < 16384,
        "Worktree .git must be a regular link file"
    );
    let text = fs::read_to_string(link)?;
    let target = PathBuf::from(
        text.trim()
            .strip_prefix("gitdir: ")
            .context("Invalid worktree link")?,
    );
    Ok(target)
}
fn normalize(path: &Path) -> Result<PathBuf> {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => ensure!(normalized.pop(), "Invalid relative worktree link"),
            other => normalized.push(other.as_os_str()),
        }
    }
    Ok(normalized)
}
fn verify_link(path: &Path, dir: &Path) -> Result<()> {
    let target = link_target(path)?;
    let resolved = if target.is_absolute() {
        target
    } else {
        path.join(target)
    };
    ensure!(
        resolved.canonicalize()? == dir.canonicalize()?,
        "Worktree link points to another repository"
    );
    Ok(())
}
impl Repository {
    pub(in crate::git) fn move_worktree(
        &self,
        expected: &WorktreeInfo,
        destination: &Path,
    ) -> Result<String> {
        let repo = raw(self)?;
        let dir = admin(&repo, &expected.name)?;
        let journal = dir.join("gitbuddy-move.json");
        ensure!(
            !journal.exists(),
            "An interrupted move exists; use Repair first"
        );
        let current = self
            .worktrees()?
            .into_iter()
            .find(|t| t.name == expected.name)
            .context("Worktree disappeared")?;
        ensure!(
            current.path == expected.path
                && current.head == expected.head
                && !current.locked
                && current.valid,
            "Worktree changed or is locked; refresh first"
        );
        let child = git2::Repository::open(&current.path)?;
        ensure!(
            current.path.canonicalize()? != self.root.canonicalize()?,
            "Cannot move the current worktree"
        );
        clean_for_removal(&child, 0)?;
        ensure!(
            child.submodules()?.is_empty(),
            "Moving worktrees containing submodules is unsupported, matching git worktree move; submodule preparation remains available"
        );
        ensure!(
            destination.is_absolute() && !destination.exists(),
            "Choose a new absolute destination"
        );
        ensure!(
            destination
                .to_str()
                .is_some_and(|p| !p.contains(['\n', '\r'])),
            "Worktree destination must be UTF-8 without line breaks"
        );
        let parent = destination
            .parent()
            .context("Missing destination parent")?
            .canonicalize()?;
        let new = parent.join(
            destination
                .file_name()
                .context("Missing destination name")?,
        );
        ensure!(
            !new.starts_with(&current.path),
            "Cannot move a worktree into itself"
        );
        verify_link(&current.path, &dir)?;
        let plan = Move {
            version: 1,
            old: current.path.clone(),
            new: new.clone(),
            gitdir: dir.clone(),
        };
        atomic(&journal, &serde_json::to_vec(&plan)?)?;
        fs::rename(&current.path, &new)
            .context("Move failed; use Repair with the original or destination directory")?;
        if let Some(old_parent) = current.path.parent() {
            fs::File::open(old_parent)?.sync_all()?;
        }
        fs::File::open(parent)?.sync_all()?;
        self.repair_worktree(&expected.name, &new)
    }
    pub(in crate::git) fn repair_worktree(&self, name: &str, path: &Path) -> Result<String> {
        let repo = raw(self)?;
        let dir = admin(&repo, name)?;
        let link = link_target(path)?;
        let actual = path.canonicalize()?;
        let journal = dir.join("gitbuddy-move.json");
        let old_location = if journal.exists() {
            let meta = fs::symlink_metadata(&journal)?;
            ensure!(
                meta.is_file() && !meta.file_type().is_symlink() && meta.len() < 65536,
                "Invalid move journal"
            );
            let plan: Move = serde_json::from_slice(&fs::read(&journal)?)?;
            ensure!(
                plan.version == 1
                    && plan.gitdir == dir
                    && (actual == plan.old || actual == plan.new),
                "Repair path differs from saved move"
            );
            ensure!(
                !(plan.old.exists() && plan.new.exists()),
                "Both move paths exist; inspect them before repairing"
            );
            plan.old
        } else {
            let old = fs::read_to_string(dir.join("gitdir"))?;
            let old = Path::new(old.trim())
                .parent()
                .context("Invalid registered worktree path")?
                .to_path_buf();
            ensure!(
                !old.exists() || old.canonicalize()? == actual,
                "Registered worktree still exists elsewhere"
            );
            old
        };
        let resolved = if link.is_absolute() {
            link.clone()
        } else {
            normalize(&actual.join(&link))?
        };
        let valid = resolved.canonicalize().is_ok_and(|p| p == dir);
        if !valid {
            ensure!(
                !link.is_absolute() && normalize(&old_location.join(&link))?.canonicalize()? == dir,
                "Worktree link differs from the saved administration directory"
            );
        }
        if !link.is_absolute() {
            let text = format!(
                "gitdir: {}\n",
                dir.to_str()
                    .context("Worktree administration path must be UTF-8")?
            );
            atomic(&actual.join(".git"), text.as_bytes())?;
        }
        verify_link(&actual, &dir)?;
        atomic(
            &dir.join("gitdir"),
            format!(
                "{}\n",
                actual
                    .join(".git")
                    .to_str()
                    .context("Repair path must be UTF-8")?
            )
            .as_bytes(),
        )?;
        repo.find_worktree(name)?.validate()?;
        if journal.exists() {
            fs::remove_file(journal)?;
            fs::File::open(&dir)?.sync_all()?;
        }
        Ok(format!(
            "Worktree registration repaired at {}",
            actual.display()
        ))
    }
}
