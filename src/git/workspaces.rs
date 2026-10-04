use super::libgit::{network_callbacks, raw};
use super::{NetworkControl, Repository};
use anyhow::{Context, Result, ensure};
use git2::{
    FetchOptions, RepositoryState, SubmoduleIgnore, SubmoduleUpdateOptions, WorktreeAddOptions,
    WorktreeLockStatus, WorktreePruneOptions, build::CheckoutBuilder,
};
use std::path::{Component, Path, PathBuf};

#[derive(Clone, Debug)]
pub struct WorktreeInfo {
    pub name: String,
    pub path: PathBuf,
    pub branch: String,
    pub head: Option<String>,
    pub locked: bool,
    pub valid: bool,
    pub dirty: bool,
}
#[derive(Clone, Debug)]
pub struct SubmoduleInfo {
    pub name: String,
    pub path: PathBuf,
    pub url: String,
    pub head: Option<String>,
    pub index: Option<String>,
    pub workdir: Option<String>,
    pub initialized: bool,
    pub dirty: bool,
}
fn relative(path: &Path) -> Result<()> {
    ensure!(
        !path.as_os_str().is_empty()
            && path
                .components()
                .all(|c| matches!(c, Component::Normal(name) if name != ".git")),
        "Use a relative child path without '..'"
    );
    Ok(())
}
pub(super) fn branch_available(repo: &git2::Repository, reference: &str) -> Result<()> {
    let current = repo.path().canonicalize()?;
    let check = |other: &git2::Repository| -> Result<()> {
        if other.path().canonicalize()? != current {
            let head = other.find_reference("HEAD")?;
            ensure!(
                head.symbolic_target()? != Some(reference),
                "This branch is checked out in another worktree; open that worktree instead"
            );
        }
        Ok(())
    };
    check(&git2::Repository::open(repo.commondir())?)?;
    for name in repo.worktrees()?.iter() {
        let name = name?.context("Non UTF-8 worktree name")?;
        let tree = repo.find_worktree(name)?;
        if let Ok(other) = git2::Repository::open_from_worktree(&tree) {
            check(&other)?;
        }
    }
    Ok(())
}
fn clean(repo: &git2::Repository) -> Result<()> {
    ensure!(
        repo.state() == RepositoryState::Clean && !super::rebase::active(repo),
        "Finish the repository operation first"
    );
    ensure!(
        super::libgit::file_changes(repo)?.is_empty(),
        "This worktree has changes; commit or stash them first"
    );
    Ok(())
}
// A display ignore policy must never hide data from a deletion guard.
fn clean_for_removal(repo: &git2::Repository, depth: usize) -> Result<()> {
    ensure!(
        depth < 64,
        "Submodule nesting is too deep to safely remove this worktree"
    );
    clean(repo)?;
    for module in repo.submodules()? {
        let name = module.name()?;
        let status = repo.submodule_status(name, SubmoduleIgnore::None)?;
        ensure!(
            !status.intersects(
                git2::SubmoduleStatus::WD_MODIFIED
                    | git2::SubmoduleStatus::WD_INDEX_MODIFIED
                    | git2::SubmoduleStatus::WD_WD_MODIFIED
                    | git2::SubmoduleStatus::WD_UNTRACKED
                    | git2::SubmoduleStatus::WD_ADDED
                    | git2::SubmoduleStatus::WD_DELETED
            ),
            "Submodule {name} has changes; commit or stash them before removing the worktree"
        );
        let path = repo
            .workdir()
            .context("No working directory")?
            .join(module.path());
        if status.contains(git2::SubmoduleStatus::IN_WD)
            && !status.contains(git2::SubmoduleStatus::WD_UNINITIALIZED)
        {
            let child = module.open().with_context(|| {
                format!("Cannot inspect submodule {name}; worktree was not removed")
            })?;
            ensure!(
                child
                    .workdir()
                    .context("Submodule has no working directory")?
                    .canonicalize()?
                    == path.canonicalize()?,
                "Submodule {name} points outside its checkout; worktree was not removed"
            );
            clean_for_removal(&child, depth + 1)
                .with_context(|| format!("Submodule {name} is not safe to remove"))?;
        } else if path.exists() {
            ensure!(
                path.is_dir() && std::fs::read_dir(&path)?.next().is_none(),
                "Uninitialized submodule {name} contains files; worktree was not removed"
            );
        }
    }
    Ok(())
}

impl Repository {
    pub fn worktrees(&self) -> Result<Vec<WorktreeInfo>> {
        let repo = raw(self)?;
        let mut trees = Vec::new();
        for name in repo.worktrees()?.iter() {
            let name = name?.context("Worktree name is not UTF-8")?;
            let tree = repo.find_worktree(name)?;
            let child = git2::Repository::open_from_worktree(&tree).ok();
            let head = child.as_ref().and_then(|r| r.head().ok());
            trees.push(WorktreeInfo {
                name: name.into(),
                path: tree.path().into(),
                branch: head
                    .as_ref()
                    .and_then(|h| h.shorthand().ok())
                    .unwrap_or("unavailable")
                    .into(),
                head: head.and_then(|h| h.target()).map(|id| id.to_string()),
                locked: !matches!(tree.is_locked()?, WorktreeLockStatus::Unlocked),
                valid: tree.validate().is_ok(),
                dirty: child
                    .as_ref()
                    .is_none_or(|r| clean_for_removal(r, 0).is_err()),
            });
        }
        Ok(trees)
    }
    pub(super) fn create_worktree(&self, name: &str, path: &Path) -> Result<String> {
        let repo = raw(self)?;
        ensure!(!super::rebase::active(&repo), "Finish rebase first");
        ensure!(
            git2::Branch::name_is_valid(name)?
                && !name.contains('/')
                && !name.starts_with('.')
                && name != "HEAD",
            "Use a valid, simple new branch / worktree name"
        );
        ensure!(
            path.is_absolute() && !path.exists(),
            "Choose an absolute destination that does not exist"
        );
        ensure!(
            repo.find_branch(name, git2::BranchType::Local).is_err(),
            "That branch already exists; choose a new branch name"
        );
        let mut options = WorktreeAddOptions::new();
        options.checkout_existing(false);
        repo.worktree(name, path, Some(&options))?;
        Ok(format!(
            "Created worktree {} on new branch {name}",
            path.display()
        ))
    }
    pub(super) fn lock_worktree(&self, name: &str, locked: bool) -> Result<String> {
        let repo = raw(self)?;
        let tree = repo.find_worktree(name)?;
        if locked {
            tree.lock(Some("Locked in GitBuddy"))?;
        } else {
            tree.unlock()?;
        }
        Ok(if locked {
            "Worktree locked"
        } else {
            "Worktree unlocked"
        }
        .into())
    }
    pub(super) fn remove_worktree(&self, expected: &WorktreeInfo) -> Result<String> {
        let repo = raw(self)?;
        let tree = repo.find_worktree(&expected.name)?;
        ensure!(
            tree.path() == expected.path,
            "Worktree path changed; reload the list"
        );
        ensure!(
            matches!(tree.is_locked()?, WorktreeLockStatus::Unlocked),
            "Unlock the worktree first"
        );
        if tree.path().exists() {
            let child = git2::Repository::open_from_worktree(&tree)?;
            ensure!(
                child
                    .workdir()
                    .context("No working directory")?
                    .canonicalize()?
                    != self.root.canonicalize()?,
                "Cannot remove the current worktree"
            );
            clean_for_removal(&child, 0)?;
            ensure!(
                child.head()?.target().map(|id| id.to_string()) == expected.head,
                "Worktree HEAD changed; reload before removing"
            );
        }
        let mut options = WorktreePruneOptions::new();
        options.valid(true).working_tree(true);
        tree.prune(Some(&mut options))?;
        Ok("Worktree removed; its branch is retained".into())
    }
    pub fn submodules(&self) -> Result<Vec<SubmoduleInfo>> {
        let repo = raw(self)?;
        repo.submodules()?
            .into_iter()
            .map(|module| {
                let name = module.name()?.to_string();
                let child = module.open().ok();
                Ok(SubmoduleInfo {
                    name: name.clone(),
                    path: module.path().into(),
                    url: module.url()?.unwrap_or_default().into(),
                    head: module.head_id().map(|id| id.to_string()),
                    index: module.index_id().map(|id| id.to_string()),
                    workdir: module.workdir_id().map(|id| id.to_string()),
                    initialized: child.is_some(),
                    dirty: child.as_ref().is_some_and(|r| clean(r).is_err())
                        || repo
                            .submodule_status(&name, SubmoduleIgnore::None)?
                            .intersects(
                                git2::SubmoduleStatus::WD_INDEX_MODIFIED
                                    | git2::SubmoduleStatus::WD_WD_MODIFIED
                                    | git2::SubmoduleStatus::WD_UNTRACKED,
                            ),
                })
            })
            .collect()
    }
    pub(super) fn add_submodule(
        &self,
        url: &str,
        path: &Path,
        control: NetworkControl,
    ) -> Result<String> {
        relative(path)?;
        ensure!(
            !url.trim().is_empty() && !url.starts_with('-'),
            "Enter a repository URL"
        );
        let repo = raw(self)?;
        clean(&repo)?;
        let mut parent = self.root.clone();
        for component in path.components() {
            parent.push(component);
            if let Ok(metadata) = std::fs::symlink_metadata(&parent) {
                ensure!(
                    !metadata.file_type().is_symlink(),
                    "Submodule destinations must not traverse symlinks"
                );
            }
        }
        ensure!(
            !self.root.join(path).exists(),
            "Submodule destination must not exist"
        );
        let mut module = repo.submodule(url, path, true)?;
        let mut fetch = FetchOptions::new();
        fetch.remote_callbacks(network_callbacks(control.clone()));
        let mut options = SubmoduleUpdateOptions::new();
        options.fetch(fetch);
        module.clone(Some(&mut options)).context(
            "Submodule clone failed; its registered entry may remain for Update / retry",
        )?;
        control.cancellation.finish()?;
        module.add_finalize()?;
        Ok("Submodule added; review and commit .gitmodules and its gitlink".into())
    }
    pub(super) fn update_submodules(
        &self,
        names: &[String],
        recursive: bool,
        control: NetworkControl,
    ) -> Result<String> {
        let repo = raw(self)?;
        ensure!(
            repo.state() == RepositoryState::Clean && !super::rebase::active(&repo),
            "Finish the current operation first"
        );
        update(&repo, names, recursive, &control, 0)?;
        control.cancellation.finish()?;
        Ok("Submodules updated to the commits recorded in the index".into())
    }
    pub(super) fn sync_submodules(&self) -> Result<String> {
        let repo = raw(self)?;
        for mut module in repo.submodules()? {
            module.sync()?;
        }
        Ok("Submodule URLs synchronized from .gitmodules".into())
    }
    pub(super) fn stage_submodule(&self, name: &str) -> Result<String> {
        let repo = raw(self)?;
        ensure!(!repo.index()?.has_conflicts(), "Resolve conflicts first");
        repo.find_submodule(name)?.add_to_index(true)?;
        Ok("Submodule commit staged in parent repository".into())
    }
}
fn update(
    repo: &git2::Repository,
    names: &[String],
    recursive: bool,
    control: &NetworkControl,
    depth: usize,
) -> Result<()> {
    ensure!(depth < 32, "Submodule recursion exceeds 32 levels");
    let names = if names.is_empty() {
        repo.submodules()?
            .iter()
            .map(|m| m.name().map(str::to_string))
            .collect::<Result<Vec<_>, _>>()?
    } else {
        names.to_vec()
    };
    for name in names {
        control.check()?;
        let mut module = repo.find_submodule(&name)?;
        if let Ok(child) = module.open() {
            clean(&child).with_context(|| format!("Submodule {name} has changes"))?;
            super::lfs::dehydrate(&child)?;
        }
        module.init(false)?;
        let mut fetch = FetchOptions::new();
        fetch.remote_callbacks(network_callbacks(control.clone()));
        let mut checkout = CheckoutBuilder::new();
        checkout.safe();
        let mut options = SubmoduleUpdateOptions::new();
        options.fetch(fetch).checkout(checkout).allow_fetch(true);
        control.phase(&format!("Updating submodule {name}…"));
        let result = module.update(true, Some(&mut options));
        if let Ok(child) = module.open() {
            super::lfs::hydrate(&child)?;
        }
        result?;
        if recursive {
            update(&module.open()?, &[], true, control, depth + 1)?;
        }
    }
    Ok(())
}
