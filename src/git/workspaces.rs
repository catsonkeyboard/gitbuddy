use super::libgit::{WriterLock, network_callbacks, raw};
use super::{NetworkControl, Repository};
use anyhow::{Context, Result, ensure};
use git2::{
    FetchOptions, RepositoryState, SubmoduleIgnore, SubmoduleUpdateOptions, WorktreeAddOptions,
    WorktreeLockStatus, WorktreePruneOptions, build::CheckoutBuilder,
};
use std::path::{Component, Path, PathBuf};
#[path = "workspaces/move_tree.rs"]
mod move_tree;
#[path = "workspaces/submodule_manage.rs"]
mod submodule_manage;

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
    pub(super) fn create_worktree_at(
        &self,
        source: &str,
        path: &Path,
        detached: bool,
    ) -> Result<String> {
        let repo = raw(self)?;
        ensure!(!super::rebase::active(&repo), "Finish rebase first");
        ensure!(
            path.is_absolute() && !path.exists(),
            "Choose a new absolute destination"
        );
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .context("Choose a UTF-8 worktree directory name")?;
        ensure!(
            git2::Branch::name_is_valid(name)? && !name.starts_with('.'),
            "Invalid worktree name"
        );
        let temporary = format!(
            "gitbuddy-worktree-{}",
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        );
        let branch = if detached {
            let commit = repo.revparse_single(source.trim())?.peel_to_commit()?;
            repo.branch(&temporary, &commit, false)?
        } else {
            let name = source
                .trim()
                .strip_prefix("refs/heads/")
                .unwrap_or(source.trim());
            let branch = repo.find_branch(name, git2::BranchType::Local)?;
            branch_available(&repo, branch.get().name()?)?;
            ensure!(!branch.is_head(), "This branch is already checked out here");
            branch
        };
        let mut options = WorktreeAddOptions::new();
        options.reference(Some(branch.get()));
        let result = repo.worktree(name, path, Some(&options));
        if result.is_err() && detached && !path.exists() {
            let _ = repo
                .find_branch(&temporary, git2::BranchType::Local)
                .and_then(|mut b| b.delete());
        }
        result.with_context(|| {
            format!(
                "Worktree creation failed; inspect {} before retrying",
                path.display()
            )
        })?;
        let child = git2::Repository::open(path)?;
        if detached {
            let id = child.head()?.peel_to_commit()?.id();
            child.set_head_detached(id)?;
            repo.find_branch(&temporary, git2::BranchType::Local)?
                .delete()?;
        }
        super::worktree_conversion::initialize_worktree(&child, &NetworkControl::default())
            .with_context(|| {
                format!(
                    "Worktree was created at {}; open it to inspect the checkout",
                    path.display()
                )
            })?;
        let missing = super::lfs::hydrate(&child)?;
        Ok(format!(
            "Created worktree {}; {missing} LFS objects need fetching",
            path.display()
        ))
    }
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
        let child = git2::Repository::open(path)?;
        super::worktree_conversion::initialize_worktree(&child, &NetworkControl::default())
            .with_context(|| {
                format!(
                    "Worktree was created at {}; open it to inspect the checkout",
                    path.display()
                )
            })?;
        let missing = super::lfs::hydrate(&child).with_context(|| {
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
    pub(super) fn update_submodule_remote(
        &self,
        expected: &SubmoduleInfo,
        branch: &str,
        control: NetworkControl,
    ) -> Result<String> {
        let parent = self.checked_submodule_parent(expected)?;
        let parent_raw = raw(&parent)?;
        let module = parent_raw.find_submodule(&expected.name)?;
        let child =
            checked_child(&parent_raw, &module)?.context("Initialize the submodule first")?;
        clean_for_removal(&child, 0)?;
        let previous = super::recovery::head_state(&child)?;
        ensure!(
            previous.id == expected.workdir,
            "Submodule HEAD changed; refresh first"
        );
        let child_repo =
            Repository::open(child.workdir().context("No submodule working directory")?)?;
        let origin = child_repo
            .remote_info()?
            .into_iter()
            .find(|r| r.name == "origin")
            .context("Submodule needs an origin remote")?;
        let fetch = child_repo.prepare_fetch(&origin, Some(branch))?;
        let tracking = fetch
            .tracking
            .clone()
            .context("Selected branch has no remote-tracking mapping")?;
        child_repo.execute_with_control(
            super::Operation::FetchFrom(std::sync::Arc::new(fetch)),
            control.clone(),
        )?;
        let child = raw(&child_repo)?;
        let _child_writer = WriterLock::acquire(&child)?;
        clean_for_removal(&child, 0)?;
        ensure!(
            super::recovery::head_state(&child)? == previous,
            "Submodule HEAD changed during fetch"
        );
        let parent_index = parent_raw.index()?;
        let _parent_index_lock = super::conflicts::IndexLock::acquire(
            parent_index.path().context("Missing parent index")?,
        )?;
        self.checked_submodule_parent(expected)?;
        let commit = child.find_reference(&tracking)?.peel_to_commit()?;
        control.cancellation.finish()?;
        let mut refs = child.transaction()?;
        refs.lock_ref("HEAD")?;
        refs.lock_ref(&tracking)?;
        if previous.reference != "HEAD" && previous.reference != tracking {
            refs.lock_ref(&previous.reference)?;
        }
        ensure!(
            super::recovery::head_state(&child)? == previous
                && child.find_reference(&tracking)?.peel_to_commit()?.id() == commit.id(),
            "Submodule checkout target changed; refresh first"
        );
        let receipt = super::worktree_conversion::checkout_tree(
            &child,
            commit.as_object(),
            super::CheckoutOptions {
                publication: Some(super::CheckoutHead {
                    reference: None,
                    id: commit.id(),
                }),
                ..Default::default()
            },
            &control,
        )?;
        refs.set_target("HEAD", commit.id(), None, "checkout: update submodule")?;
        refs.commit()?;
        let missing = super::worktree_conversion::acknowledge_checkout(&child, &receipt)?;
        Ok(format!(
            "Submodule checked out at {}; stage its gitlink in the parent when ready; {missing} LFS objects need fetching",
            commit.id()
        ))
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
        let child = module.clone(Some(&mut options)).context(
            "Submodule clone failed; its registered entry may remain for Update / retry",
        )?;
        let _child_writer = WriterLock::acquire(&child)?;
        super::worktree_conversion::initialize_worktree(&child, &control).with_context(|| {
            format!(
                "Submodule was cloned at {}; open it to inspect the checkout",
                self.root.join(path).display()
            )
        })?;
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
        submodule_manage::restore_checkout(repo, &module)?;
        module.reload(true)?;
        if let Some(child) = checked_child(repo, &module)? {
            clean_for_removal(&child, 0)?;
        }
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
            let child = builder
                .clone(&url, &module_path(repo, &module)?)
                .with_context(|| {
                    format!("Independent submodule clone {name} failed; retry Update")
                })?;
            let _child_writer = WriterLock::acquire(&child)?;
            super::worktree_conversion::initialize_worktree(&child, control)?;
        }
        let Some(target) = module.index_id() else {
            // A configuration-only entry is not a recorded gitlink yet.
            continue;
        };
        control.phase(&format!("Updating submodule {name}…"));
        if let Some(child) = checked_child(repo, &module)? {
            missing +=
                update_initialized(repo, &module, &child, target, control).with_context(|| {
                    format!(
                        "Cannot update submodule at {}",
                        module_path(repo, &module)
                            .map_or_else(|_| name.clone(), |p| p.display().to_string())
                    )
                })?;
        } else {
            let mut fetch = FetchOptions::new();
            fetch.remote_callbacks(network_callbacks(
                control.clone(),
                repo.config()?.snapshot()?,
            ));
            let mut checkout = CheckoutBuilder::new();
            checkout.safe();
            let mut options = SubmoduleUpdateOptions::new();
            options.fetch(fetch).checkout(checkout).allow_fetch(true);
            module.update(true, Some(&mut options))?;
            let child = checked_child(repo, &module)?.context("Missing submodule checkout")?;
            let _child_writer = WriterLock::acquire(&child)?;
            super::worktree_conversion::initialize_worktree(&child, control).with_context(
                || {
                    format!(
                        "Submodule was initialized at {}; open it to inspect the checkout",
                        module_path(repo, &module)
                            .map_or_else(|_| name.clone(), |p| p.display().to_string())
                    )
                },
            )?;
            missing += super::lfs::hydrate(&child)?;
        }
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

fn update_initialized(
    parent: &git2::Repository,
    module: &git2::Submodule<'_>,
    child: &git2::Repository,
    target: git2::Oid,
    control: &NetworkControl,
) -> Result<usize> {
    let _writer = WriterLock::acquire(child)?;
    clean(child).context("Submodule has changes")?;
    let before = super::recovery::head_state(child)?;
    let commit = match child.find_commit(target) {
        Ok(commit) => commit,
        Err(error) if error.code() == git2::ErrorCode::NotFound => {
            let configured = if let Some(name) = before.reference.strip_prefix("refs/heads/") {
                match child.config()?.get_string(&format!("branch.{name}.remote")) {
                    Ok(remote) => Some(remote),
                    Err(error) if error.code() == git2::ErrorCode::NotFound => None,
                    Err(error) => return Err(error.into()),
                }
            } else {
                None
            };
            let mut remote = match configured
                .as_deref()
                .map(|name| child.find_remote(name))
                .transpose()
            {
                Ok(Some(remote)) => remote,
                Ok(None) => child.find_remote("origin")?,
                Err(error) if error.code() == git2::ErrorCode::NotFound => {
                    child.find_remote("origin")?
                }
                Err(error) => return Err(error.into()),
            };
            let mut fetch = FetchOptions::new();
            fetch.remote_callbacks(network_callbacks(
                control.clone(),
                child.config()?.snapshot()?,
            ));
            remote.fetch(&[] as &[&str], Some(&mut fetch), None)?;
            child
                .find_commit(target)
                .context("Fetch did not provide the recorded gitlink commit")?
        }
        Err(error) => return Err(error.into()),
    };
    let mut refs = child.transaction()?;
    refs.lock_ref("HEAD")?;
    if before.reference != "HEAD" {
        refs.lock_ref(&before.reference)?;
    }
    let mut parent_index = parent.index()?;
    let _parent_index_lock =
        super::conflicts::IndexLock::acquire(parent_index.path().context("Missing parent index")?)?;
    parent_index.read(true)?;
    ensure!(
        parent_index
            .get_path(module.path(), 0)
            .is_some_and(|entry| entry.mode == 0o160000 && entry.id == target)
            && super::recovery::head_state(child)? == before,
        "Submodule gitlink or HEAD changed; refresh before updating"
    );
    clean(child).context("Submodule has changes after fetch")?;
    let receipt = super::worktree_conversion::checkout_tree(
        child,
        commit.as_object(),
        super::CheckoutOptions {
            publication: Some(super::CheckoutHead {
                reference: None,
                id: target,
            }),
            ..Default::default()
        },
        control,
    )?;
    refs.set_target("HEAD", target, None, "checkout: update recorded submodule")?;
    refs.commit()?;
    super::worktree_conversion::acknowledge_checkout(child, &receipt)
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
