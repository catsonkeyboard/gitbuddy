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
    pub parent: PathBuf,
    pub depth: usize,
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
        let missing = super::lfs::hydrate(&git2::Repository::open(path)?).with_context(|| {
            format!(
                "Worktree was created at {}; retry LFS checkout there",
                path.display()
            )
        })?;
        Ok(format!(
            "Created worktree {} on new branch {name}; {missing} LFS objects need fetching",
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
        list(&repo, Path::new(""), 0, false)
    }
    pub fn submodules_recursive(&self) -> Result<Vec<SubmoduleInfo>> {
        list(&raw(self)?, Path::new(""), 0, true)
    }
    pub(super) fn prepare_worktree(
        &self,
        expected: &WorktreeInfo,
        control: NetworkControl,
    ) -> Result<String> {
        let current = self
            .worktrees()?
            .into_iter()
            .find(|t| t.name == expected.name)
            .context("Worktree no longer exists")?;
        ensure!(
            current.path == expected.path && current.head == expected.head && current.valid,
            "Worktree changed; refresh before preparing it"
        );
        let child = Repository::open(&current.path)?;
        let modules = child.update_submodules(&[], true, control)?;
        let message = child.lfs_checkout()?;
        Ok(format!("Worktree prepared; {modules}; {message}"))
    }
    pub(super) fn update_submodule_at(
        &self,
        expected: &SubmoduleInfo,
        control: NetworkControl,
    ) -> Result<String> {
        let parent = self.checked_submodule_parent(expected)?;
        parent.update_submodules(std::slice::from_ref(&expected.name), true, control)
    }
    pub(super) fn stage_submodule_at(&self, expected: &SubmoduleInfo) -> Result<String> {
        self.checked_submodule_parent(expected)?
            .stage_submodule(&expected.name)
    }
    fn checked_submodule_parent(&self, expected: &SubmoduleInfo) -> Result<Repository> {
        let current = self
            .submodules_recursive()?
            .into_iter()
            .find(|m| m.path == expected.path && m.name == expected.name)
            .context("Submodule no longer exists")?;
        ensure!(
            current.parent == expected.parent && current.index == expected.index,
            "Submodule changed; refresh before operating on it"
        );
        Repository::open(&current.parent)
    }
}

fn module_path(repo: &git2::Repository, module: &git2::Submodule<'_>) -> Result<PathBuf> {
    relative(module.path())?;
    let root = repo
        .workdir()
        .context("Submodules require a working tree")?;
    let mut path = root.to_path_buf();
    for component in module.path().components() {
        path.push(component);
        if let Ok(metadata) = std::fs::symlink_metadata(&path) {
            ensure!(
                !metadata.file_type().is_symlink(),
                "Submodule path traverses a symlink: {}",
                path.display()
            );
        }
    }
    Ok(path)
}
fn checked_child(
    repo: &git2::Repository,
    module: &git2::Submodule<'_>,
) -> Result<Option<git2::Repository>> {
    let path = module_path(repo, module)?;
    if !path.join(".git").exists() {
        return Ok(None);
    }
    let child = git2::Repository::open(&path)?;
    ensure!(
        child.workdir().and_then(|p| p.canonicalize().ok()) == path.canonicalize().ok(),
        "Submodule Git metadata points at another worktree; refusing shared checkout"
    );
    Ok(Some(child))
}
fn list(
    repo: &git2::Repository,
    prefix: &Path,
    depth: usize,
    recursive: bool,
) -> Result<Vec<SubmoduleInfo>> {
    ensure!(depth < 32, "Submodule recursion exceeds 32 levels");
    let mut result = Vec::new();
    for module in repo.submodules()? {
        let name = module.name()?.to_string();
        let child = checked_child(repo, &module)?;
        let path = prefix.join(module.path());
        result.push(SubmoduleInfo {
            name: name.clone(),
            path: path.clone(),
            parent: repo.workdir().context("No parent worktree")?.into(),
            depth,
            url: module.url()?.unwrap_or_default().into(),
            head: module.head_id().map(|id| id.to_string()),
            index: module.index_id().map(|id| id.to_string()),
            workdir: child
                .as_ref()
                .and_then(|r| r.head().ok()?.target())
                .map(|id| id.to_string()),
            initialized: child.is_some(),
            dirty: child.as_ref().is_some_and(|r| clean(r).is_err())
                || repo
                    .submodule_status(&name, SubmoduleIgnore::None)?
                    .intersects(
                        git2::SubmoduleStatus::WD_INDEX_MODIFIED
                            | git2::SubmoduleStatus::WD_WD_MODIFIED
                            | git2::SubmoduleStatus::WD_UNTRACKED,
                    ),
        });
        if recursive && let Some(child) = child {
            result.extend(list(&child, &path, depth + 1, true)?);
        }
    }
    Ok(result)
}

impl Repository {
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
        let mut module = repo.submodule(url, path, !repo.is_worktree())?;
        let mut fetch = FetchOptions::new();
        fetch.remote_callbacks(network_callbacks(
            control.clone(),
            repo.config()?.snapshot()?,
        ));
        let mut options = SubmoduleUpdateOptions::new();
        options.fetch(fetch);
        module.clone(Some(&mut options)).context(
            "Submodule clone failed; its registered entry may remain for Update / retry",
        )?;
        control.cancellation.finish()?;
        module.add_finalize()?;
        let missing = super::lfs::hydrate(
            &checked_child(&repo, &module)?.context("Missing submodule checkout")?,
        )?;
        Ok(format!(
            "Submodule added; review and commit .gitmodules and its gitlink; {missing} LFS objects need fetching in its tab"
        ))
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
        preflight(&repo, names, recursive, 0)?;
        let missing = update(&repo, names, recursive, &control, 0)?;
        control.cancellation.finish()?;
        Ok(format!(
            "Submodules updated to the commits recorded in the index; {missing} LFS objects need fetching in their submodule tabs"
        ))
    }
    pub(super) fn sync_submodules(&self) -> Result<String> {
        let repo = raw(self)?;
        sync_recursive(&repo, 0)?;
        Ok("Submodule URLs synchronized recursively from .gitmodules".into())
    }
    pub(super) fn stage_submodule(&self, name: &str) -> Result<String> {
        let repo = raw(self)?;
        ensure!(!repo.index()?.has_conflicts(), "Resolve conflicts first");
        let mut module = repo.find_submodule(name)?;
        checked_child(&repo, &module)?.context("Initialize the submodule first")?;
        module.add_to_index(true)?;
        Ok("Submodule commit staged in parent repository".into())
    }
}
fn update(
    repo: &git2::Repository,
    names: &[String],
    recursive: bool,
    control: &NetworkControl,
    depth: usize,
) -> Result<usize> {
    ensure!(depth < 32, "Submodule recursion exceeds 32 levels");
    let mut missing = 0;
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
        module.init(false)?;
        // libgit2's module store is shared between linked worktrees. A fresh
        // embedded clone gives this worktree an independent submodule HEAD/index.
        if repo.is_worktree() && checked_child(repo, &module)?.is_none() {
            let url = repo
                .config()?
                .get_string(&format!("submodule.{name}.url"))?;
            let mut fetch = FetchOptions::new();
            fetch.remote_callbacks(network_callbacks(
                control.clone(),
                repo.config()?.snapshot()?,
            ));
            let mut builder = git2::build::RepoBuilder::new();
            builder.fetch_options(fetch);
            builder
                .clone(&url, &module_path(repo, &module)?)
                .with_context(|| {
                    format!("Independent submodule clone {name} failed; retry Update")
                })?;
        }
        if let Some(child) = checked_child(repo, &module)? {
            clean(&child).with_context(|| format!("Submodule {name} has changes"))?;
            if let Err(error) = super::lfs::dehydrate(&child) {
                let _ = super::lfs::hydrate(&child);
                return Err(error);
            }
        }
        let result = (|| -> Result<()> {
            let mut fetch = FetchOptions::new();
            fetch.remote_callbacks(network_callbacks(
                control.clone(),
                repo.config()?.snapshot()?,
            ));
            let mut checkout = CheckoutBuilder::new();
            checkout.safe();
            let mut options = SubmoduleUpdateOptions::new();
            options.fetch(fetch).checkout(checkout).allow_fetch(true);
            control.phase(&format!("Updating submodule {name}…"));
            module.update(true, Some(&mut options))?;
            Ok(())
        })();
        let hydration = checked_child(repo, &module)?
            .map(|child| super::lfs::hydrate(&child))
            .transpose();
        result?;
        missing += hydration?.unwrap_or_default();
        if recursive {
            missing += update(
                &checked_child(repo, &module)?.context("Submodule checkout is missing")?,
                &[],
                true,
                control,
                depth + 1,
            )?;
        }
    }
    Ok(missing)
}

fn preflight(
    repo: &git2::Repository,
    names: &[String],
    recursive: bool,
    depth: usize,
) -> Result<()> {
    ensure!(depth < 32, "Submodule recursion exceeds 32 levels");
    for module in repo.submodules()? {
        if !names.is_empty()
            && !names
                .iter()
                .any(|name| module.name().ok() == Some(name.as_str()))
        {
            continue;
        }
        let path = module_path(repo, &module)?;
        if let Some(child) = checked_child(repo, &module)? {
            clean(&child).with_context(|| format!("Submodule {} has changes", path.display()))?;
            if recursive {
                preflight(&child, &[], true, depth + 1)?;
            }
        } else if path.exists() {
            ensure!(
                path.is_dir() && std::fs::read_dir(&path)?.next().is_none(),
                "Uninitialized submodule {} contains files; preserve them first",
                path.display()
            );
        }
    }
    Ok(())
}
fn sync_recursive(repo: &git2::Repository, depth: usize) -> Result<()> {
    ensure!(depth < 32, "Submodule recursion exceeds 32 levels");
    for mut module in repo.submodules()? {
        let child = checked_child(repo, &module)?;
        module.sync()?;
        if let Some(child) = child {
            sync_recursive(&child, depth + 1)?;
        }
    }
    Ok(())
}
