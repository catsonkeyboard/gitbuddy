//! Stable-ID stash inspection and guarded, literal-file operations via libgit2.
use super::libgit::{author, file_changes, oid, raw, stash_position};
use super::recovery::{editable, head_state};
use super::{CommitFile, FileChange, HeadState, Repository};
use anyhow::{Context, Result, ensure};
use git2::{Index, Repository as RawRepo, StashApplyOptions, Tree, build::CheckoutBuilder};
use std::{
    collections::BTreeSet,
    fs,
    io::Write,
    path::{Component, Path, PathBuf},
};

#[derive(Clone, Debug)]
pub struct StashContext {
    pub files: Vec<FileChange>,
    head: HeadState,
    fingerprint: u64,
}
#[derive(Clone, Debug)]
pub enum StashAction {
    Apply,
    Pop,
    Branch(String),
}
#[derive(Clone, Debug)]
pub struct StashSection {
    pub stash: String,
    pub kind: String,
    pub files: Vec<CommitFile>,
    pub insertions: usize,
    pub deletions: usize,
    old: Option<String>,
    new: String,
}
#[derive(Clone, Debug)]
pub struct StashPreview {
    pub id: String,
    pub message: String,
    pub base: String,
    pub sections: Vec<StashSection>,
}

fn section(
    repo: &RawRepo,
    stash: &str,
    kind: &str,
    old: Option<&Tree<'_>>,
    new: &Tree<'_>,
) -> Result<StashSection> {
    let diff = super::inspect::tree_diff(repo, old, new)?;
    let stats = diff.stats()?;
    Ok(StashSection {
        stash: stash.into(),
        kind: kind.into(),
        files: diff.deltas().map(super::inspect::changed_file).collect(),
        insertions: stats.insertions(),
        deletions: stats.deletions(),
        old: old.map(|t| t.id().to_string()),
        new: new.id().to_string(),
    })
}
fn check_path(repo: &RawRepo, path: &Path) -> Result<()> {
    ensure!(
        !path.as_os_str().is_empty()
            && path.components().all(|c| matches!(c, Component::Normal(_))),
        "Invalid repository-relative stash path"
    );
    let root = repo.workdir().context("Stash requires a working tree")?;
    let mut current = root.to_path_buf();
    let components: Vec<_> = path.components().collect();
    for (i, component) in components.iter().enumerate() {
        current.push(component.as_os_str());
        match fs::symlink_metadata(&current) {
            Ok(meta) if i + 1 < components.len() => ensure!(
                meta.is_dir() && !meta.file_type().is_symlink(),
                "Stash path traverses a symlink or non-directory: {}",
                path.display()
            ),
            Ok(meta) => ensure!(
                meta.is_file() || meta.file_type().is_symlink(),
                "Stash supports files, not directories or submodule working trees: {}",
                path.display()
            ),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}
fn selected_paths(context: &StashContext, selected: &[PathBuf]) -> Result<BTreeSet<PathBuf>> {
    ensure!(!selected.is_empty(), "Select at least one changed file");
    let mut paths = BTreeSet::new();
    for path in selected {
        let file = context
            .files
            .iter()
            .find(|f| &f.path == path)
            .context("Selected file is no longer in the reviewed changes")?;
        paths.insert(path.clone());
        if let Some(original) = &file.original {
            ensure!(
                !context.files.iter().any(|f| &f.path == original) || selected.contains(original),
                "Select both the rename and the reused original path: {}",
                original.display()
            );
            paths.insert(original.clone());
        }
    }
    Ok(paths)
}
fn replace_entry(target: &mut Index, source: &Index, path: &Path) -> Result<()> {
    if let Some(entry) = source.get_path(path, 0) {
        target.add(&entry)?;
    } else if target.get_path(path, 0).is_some() {
        target.remove_path(path)?;
    }
    Ok(())
}
// Use a real Git index lock, and install a serialized copy atomically. This
// preserves every unselected entry (including its flags and stat information).
struct IndexLock {
    path: PathBuf,
    file: Option<fs::File>,
    installed: bool,
}
impl IndexLock {
    fn new(path: &Path) -> Result<Self> {
        let mut lock = path.as_os_str().to_os_string();
        lock.push(".lock");
        let path = PathBuf::from(lock);
        let file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .context("Index is locked by another Git operation")?;
        Ok(Self {
            path,
            file: Some(file),
            installed: false,
        })
    }
    fn install(mut self, path: &Path, bytes: &[u8]) -> Result<()> {
        let mut file = self.file.take().context("Missing index lock")?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&self.path, path)?;
        self.installed = true;
        Ok(())
    }
}
impl Drop for IndexLock {
    fn drop(&mut self) {
        if !self.installed {
            let _ = fs::remove_file(&self.path);
        }
    }
}

struct HydrateOnExit<'a>(&'a Repository);
impl Drop for HydrateOnExit<'_> {
    fn drop(&mut self) {
        if let Ok(repo) = raw(self.0) {
            let _ = super::lfs::hydrate(&repo);
        }
    }
}

fn drop_locked(repo: &RawRepo, mut lock: git2::Transaction<'_>, id: &str) -> Result<()> {
    let mut log = repo.reflog("refs/stash")?;
    let position = stash_position(&log, id)?;
    log.remove(position, true)?;
    if log.is_empty() {
        lock.remove("refs/stash")?;
    } else if position == 0 {
        lock.set_target(
            "refs/stash",
            log.get(0).context("Missing stash entry")?.id_new(),
            None,
            "",
        )?;
    }
    lock.set_reflog("refs/stash", log)?;
    lock.commit()?;
    Ok(())
}

impl Repository {
    pub fn stash_context(&self) -> Result<StashContext> {
        let repo = raw(self)?;
        editable(&repo)?;
        let head = head_state(&repo)?;
        ensure!(
            head.id.is_some(),
            "Create the initial commit before using Stash"
        );
        let snapshot = self.snapshot(1)?;
        ensure!(
            head_state(&repo)? == head && self.fingerprint()? == snapshot.fingerprint,
            "Repository changed while loading Stash. Reopen it and try again"
        );
        Ok(StashContext {
            files: snapshot.files,
            head,
            fingerprint: snapshot.fingerprint,
        })
    }
    fn validate_stash_context(&self, repo: &RawRepo, context: &StashContext) -> Result<()> {
        editable(repo)?;
        ensure!(
            head_state(repo)? == context.head && self.fingerprint()? == context.fingerprint,
            "Repository changed. Reopen Stash and review the changes again; no changes made"
        );
        ensure!(
            file_changes(repo)? == context.files,
            "Reviewed stash file list changed; reopen the action"
        );
        Ok(())
    }
    pub fn stash_preview(&self, id: &str) -> Result<StashPreview> {
        let repo = raw(self)?;
        let stash = repo.find_commit(oid(id)?)?;
        ensure!(
            matches!(stash.parent_count(), 2 | 3),
            "This object is not a supported stash"
        );
        let base = stash.parent(0)?;
        let old = base.tree()?;
        let mut sections = vec![
            section(&repo, id, "Working tree", Some(&old), &stash.tree()?)?,
            section(&repo, id, "Index", Some(&old), &stash.parent(1)?.tree()?)?,
        ];
        if stash.parent_count() == 3 {
            sections.push(section(
                &repo,
                id,
                "Untracked files",
                None,
                &stash.parent(2)?.tree()?,
            )?);
        }
        Ok(StashPreview {
            id: id.into(),
            message: stash.message().ok().unwrap_or_default().into(),
            base: base.id().to_string(),
            sections,
        })
    }
    pub fn stash_file_diff_context(
        &self,
        section: &StashSection,
        file: &CommitFile,
        full: bool,
    ) -> Result<String> {
        let repo = raw(self)?;
        let old = section
            .old
            .as_ref()
            .map(|id| oid(id).and_then(|id| Ok(repo.find_tree(id)?)))
            .transpose()?;
        let new = repo.find_tree(oid(&section.new)?)?;
        let paths = std::iter::once(file.path.as_path())
            .chain(file.original.as_deref())
            .collect::<Vec<_>>();
        if full {
            super::diff_view::check_full_tree(&repo, old.as_ref(), &paths)?;
            super::diff_view::check_full_tree(&repo, Some(&new), &paths)?;
        }
        super::inspect::single_file_patch(
            &super::inspect::tree_diff_context(&repo, old.as_ref(), &new, full, &paths)?,
            file,
        )
    }
    pub(super) fn save_stash_files(
        &self,
        context: &StashContext,
        selected: &[PathBuf],
        message: &str,
    ) -> Result<String> {
        let repo = raw(self)?;
        let mut head_lock = repo.transaction()?;
        head_lock.lock_ref("HEAD")?;
        if context.head.reference != "HEAD" {
            head_lock.lock_ref(&context.head.reference)?;
        }
        let mut stash_lock = repo.transaction()?;
        stash_lock.lock_ref("refs/stash")?;
        let current = repo.index()?;
        let index_path = current
            .path()
            .context("Missing on-disk index")?
            .to_path_buf();
        let index_lock = IndexLock::new(&index_path)?;
        self.validate_stash_context(&repo, context)?;
        let paths = selected_paths(context, selected)?;
        let base = repo.find_commit(oid(context.head.id.as_deref().context("Missing HEAD")?)?)?;
        let tree = base.tree()?;
        let mut base_index = Index::new()?;
        base_index.read_tree(&tree)?;
        for path in &paths {
            check_path(&repo, path)?;
            ensure!(
                current.get_path(path, 0).is_none_or(|e| e.mode != 0o160000)
                    && tree
                        .get_path(path)
                        .map_or(true, |e| e.filemode() != 0o160000),
                "Stash cannot capture submodule changes"
            );
        }
        let mut staged = Index::new()?;
        staged.read_tree(&tree)?;
        for path in &paths {
            replace_entry(&mut staged, &current, path)?;
        }
        let signature = author(&repo)?;
        let index_commit = repo.commit(
            None,
            &signature,
            &signature,
            "index on GitBuddy stash",
            &repo.find_tree(staged.write_tree_to(&repo)?)?,
            &[&base],
        )?;
        // Associate scratch indexes with their own repository handle, never the
        // real disk index. libgit2 applies Git filters when capturing file data.
        let capture = raw(self)?;
        let mut working = Index::new()?;
        capture.set_index(&mut working)?;
        working.read_tree(&tree)?;
        let untracked_repo = raw(self)?;
        let mut untracked = Index::new()?;
        untracked_repo.set_index(&mut untracked)?;
        for path in &paths {
            let is_untracked =
                current.get_path(path, 0).is_none() && base_index.get_path(path, 0).is_none();
            if is_untracked {
                super::lfs::stage(&untracked_repo, &mut untracked, path)?;
            } else {
                super::lfs::stage(&capture, &mut working, path)?;
            }
        }
        let index_commit = repo.find_commit(index_commit)?;
        let mut parents = vec![&base, &index_commit];
        let untracked_commit = if untracked.is_empty() {
            None
        } else {
            Some(repo.find_commit(repo.commit(
                None,
                &signature,
                &signature,
                "untracked files on GitBuddy stash",
                &repo.find_tree(untracked.write_tree_to(&repo)?)?,
                &[],
            )?)?)
        };
        if let Some(commit) = &untracked_commit {
            parents.push(commit);
        }
        let name = if message.trim().is_empty() {
            "GitBuddy stash"
        } else {
            message.trim()
        };
        let name = format!(
            "On {}: {name}",
            context
                .head
                .reference
                .strip_prefix("refs/heads/")
                .unwrap_or("HEAD")
        );
        let id = repo.commit(
            None,
            &signature,
            &signature,
            &name,
            &repo.find_tree(working.write_tree_to(&repo)?)?,
            &parents,
        )?;
        // Prepare the cleaned index before publishing the recoverable stash.
        let temp = tempfile::NamedTempFile::new_in(repo.path())?;
        fs::copy(&index_path, temp.path())?;
        let mut cleaned = Index::open(temp.path())?;
        for path in &paths {
            replace_entry(&mut cleaned, &base_index, path)?;
        }
        cleaned.write()?;
        let cleaned_bytes = fs::read(temp.path())?;
        self.validate_stash_context(&repo, context)?;
        repo.reference_ensure_log("refs/stash")?;
        let mut log = repo.reflog("refs/stash")?;
        log.append(id, &signature, Some(&name))?;
        stash_lock.set_target("refs/stash", id, Some(&signature), &name)?;
        stash_lock.set_reflog("refs/stash", log)?;
        stash_lock.commit()?;
        let cleanup = (|| {
            let mut checkout = CheckoutBuilder::new();
            checkout
                .force()
                .update_index(false)
                .disable_pathspec_match(true);
            for path in &paths {
                if base_index.get_path(path, 0).is_some() {
                    checkout.path(path);
                }
            }
            if paths.iter().any(|p| base_index.get_path(p, 0).is_some()) {
                repo.checkout_tree(tree.as_object(), Some(&mut checkout))?;
            }
            for path in &paths {
                if base_index.get_path(path, 0).is_none() {
                    let full = self.root.join(path);
                    match fs::remove_file(&full) {
                        Ok(()) => {}
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                        Err(e) => return Err(e.into()),
                    }
                }
            }
            index_lock.install(&index_path, &cleaned_bytes)?;
            super::lfs::hydrate(&raw(self)?)?;
            anyhow::Ok(())
        })();
        cleanup.with_context(|| format!("Stash {id} was saved, but cleanup failed. It remains available; do not save again before reviewing the working tree"))?;
        Ok(format!("Stashed {} selected file(s)", selected.len()))
    }
    pub(super) fn restore_stash(
        &self,
        context: &StashContext,
        id: &str,
        action: &StashAction,
        reinstate: bool,
    ) -> Result<String> {
        let mut repo = raw(self)?;
        let locked = raw(self)?;
        let mut head_lock = locked.transaction()?;
        head_lock.lock_ref("HEAD")?;
        if context.head.reference != "HEAD" {
            head_lock.lock_ref(&context.head.reference)?;
        }
        let mut stash_lock = locked.transaction()?;
        stash_lock.lock_ref("refs/stash")?;
        self.validate_stash_context(&repo, context)?;
        let position = stash_position(&repo.reflog("refs/stash")?, id)?;
        let branch = if let StashAction::Branch(name) = action {
            ensure!(
                !name.is_empty()
                    && !name.starts_with('-')
                    && git2::Reference::is_valid_name(&format!("refs/heads/{name}")),
                "Invalid new branch name"
            );
            ensure!(
                context.files.is_empty(),
                "Commit or stash the current changes before creating a branch from Stash"
            );
            let reference = format!("refs/heads/{name}");
            head_lock.lock_ref(&reference)?;
            match repo.find_reference(&reference) {
                Ok(_) => anyhow::bail!("The branch already exists"),
                Err(e) if e.code() == git2::ErrorCode::NotFound => {}
                Err(e) => return Err(e.into()),
            }
            super::workspaces::branch_available(&repo, &reference)?;
            Some((reference, repo.find_commit(oid(id)?)?.parent(0)?.id()))
        } else {
            None
        };
        let mut _hydrate_on_exit = None;
        if let Some((reference, base)) = &branch {
            // A branch starts with a clean index and worktree, checked above.
            let signature = author(&repo)?;
            _hydrate_on_exit = Some(HydrateOnExit(self));
            super::lfs::dehydrate(&repo)?;
            let checkout = repo.checkout_tree(
                repo.find_commit(*base)?.as_object(),
                Some(CheckoutBuilder::new().safe().overwrite_ignored(false)),
            );
            if let Err(e) = checkout {
                let _ = super::lfs::hydrate(&repo);
                return Err(e.into());
            }
            head_lock.set_target(
                reference,
                *base,
                Some(&signature),
                "branch: created from stash",
            )?;
            head_lock.set_symbolic_target(
                "HEAD",
                reference,
                Some(&signature),
                "checkout: branch from stash",
            )?;
            head_lock.commit()?;
            head_lock = locked.transaction()?;
            head_lock.lock_ref("HEAD")?;
            head_lock.lock_ref(reference)?;
            ensure!(
                repo.find_reference("HEAD")?.symbolic_target()? == Some(reference.as_str())
                    && repo.head()?.target() == Some(*base),
                "HEAD changed after branch creation; stash retained"
            );
        }
        // libgit2 requires a clean index even when its dirty entries are outside
        // the stash. Apply against an owned disk index containing HEAD entries
        // for unrelated staged files, then restore their exact original entries.
        // The real index stays locked throughout. Overlapping staged changes
        // are rejected before dehydrate or checkout.
        let preview = self.stash_preview(id)?;
        let affected: BTreeSet<_> = preview
            .sections
            .iter()
            .flat_map(|s| s.files.iter())
            .flat_map(|f| std::iter::once(f.path.clone()).chain(f.original.clone()))
            .collect();
        let original = repo.index()?;
        let index_path = original
            .path()
            .context("Missing on-disk index")?
            .to_path_buf();
        let index_lock = IndexLock::new(&index_path)?;
        let mut baseline = Index::new()?;
        baseline.read_tree(&repo.head()?.peel_to_tree()?)?;
        for path in &affected {
            let indexed = original.get_path(path, 0).map(|e| (e.id, e.mode));
            let committed = baseline.get_path(path, 0).map(|e| (e.id, e.mode));
            ensure!(
                indexed == committed,
                "Stash overlaps staged changes in {}. Commit or stash them before restoring; original stash retained",
                path.display()
            );
        }
        if branch.is_none() {
            _hydrate_on_exit = Some(HydrateOnExit(self));
            super::lfs::dehydrate(&repo)?;
        }
        let temp = tempfile::NamedTempFile::new_in(repo.path())?;
        fs::copy(&index_path, temp.path())?;
        let mut scratch = Index::open(temp.path())?;
        let paths: BTreeSet<_> = original
            .iter()
            .chain(baseline.iter())
            .map(|e| super::path_from_bytes(&e.path))
            .collect();
        for path in &paths {
            if !affected.contains(path) {
                replace_entry(&mut scratch, &baseline, path)?;
            }
        }
        scratch.write()?;
        repo.set_index(&mut scratch)?;
        let mut options = StashApplyOptions::new();
        let mut checkout = CheckoutBuilder::new();
        checkout.safe().overwrite_ignored(false);
        options.checkout_options(checkout);
        if reinstate || branch.is_some() {
            options.reinstantiate_index();
        }
        let result = repo.stash_apply(position, Some(&mut options));
        scratch.read(true)?;
        let all: BTreeSet<_> = paths
            .into_iter()
            .chain(scratch.iter().map(|e| super::path_from_bytes(&e.path)))
            .collect();
        for path in all {
            if !affected.contains(&path) {
                replace_entry(&mut scratch, &original, &path)?;
            }
        }
        scratch.write()?;
        index_lock.install(&index_path, &fs::read(temp.path())?)?;
        let hydration = super::lfs::hydrate(&raw(self)?);
        result.with_context(|| if let Some((reference, _)) = &branch { format!("Created {reference}, but Stash could not be applied. Stash retained; review the working tree before retrying") } else { "Stash retained. Apply failed; review the working tree for any restored untracked files before retrying".into() })?;
        let missing = hydration.context("Stash applied but LFS checkout failed; stash retained")?;
        ensure!(
            !raw(self)?.index()?.has_conflicts(),
            "Stash produced conflicts; original stash retained. Resolve the conflicting files before deleting it"
        );
        ensure!(
            missing == 0,
            "Stash applied, but {missing} LFS objects are missing. Stash retained; use LFS Fetch before deleting it"
        );
        if !matches!(action, StashAction::Apply) {
            drop_locked(&repo, stash_lock, id).context("Stash applied successfully, but deletion failed; stash retained. Do not apply again")?;
        }
        Ok(match action {
            StashAction::Apply => "Stash applied; original retained".into(),
            StashAction::Pop => "Stash popped".into(),
            StashAction::Branch(name) => format!("Created {name} from Stash; saved index restored"),
        })
    }
}
