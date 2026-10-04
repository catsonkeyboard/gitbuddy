use super::*;
use anyhow::{Context, Result, bail, ensure};
use chrono::{Local, TimeZone, Utc};
use git2::{
    BranchType, Cred, CredentialType, Delta, Diff, DiffFindOptions, DiffOptions, ErrorCode,
    FetchOptions, Oid, Patch, PushOptions, RemoteCallbacks, Repository as RawRepo, RepositoryState,
    ResetType, Sort, StashFlags, Status, StatusOptions,
    build::{CheckoutBuilder, RepoBuilder},
};
use std::{
    collections::{HashMap, hash_map::DefaultHasher},
    fs,
    hash::{Hash, Hasher},
};

pub(super) fn raw(repo: &Repository) -> Result<RawRepo> {
    RawRepo::open(&repo.root).context("无法打开 Git 仓库")
}

pub(super) fn oid(id: &str) -> Result<Oid> {
    ensure!(
        matches!(id.len(), 40 | 64) && id.bytes().all(|b| b.is_ascii_hexdigit()),
        "无效的提交 ID"
    );
    Ok(Oid::from_str(id)?)
}

pub(super) fn author(repo: &RawRepo) -> Result<git2::Signature<'static>> {
    let config = repo.config()?;
    let name = config
        .get_string("user.name")
        .context("请先设置提交作者姓名")?;
    let email = config
        .get_string("user.email")
        .context("请先设置提交作者邮箱")?;
    Ok(git2::Signature::now(&name, &email)?)
}

fn relative_time(seconds: i64) -> String {
    let elapsed = Utc::now().timestamp().saturating_sub(seconds).max(0);
    match elapsed {
        0..=59 => "just now".into(),
        60..=3599 => format!("{} minutes ago", elapsed / 60),
        3600..=86399 => format!("{} hours ago", elapsed / 3600),
        _ => format!("{} days ago", elapsed / 86400),
    }
}

fn status_chars(status: Status) -> (char, char) {
    if status.contains(Status::CONFLICTED) {
        return ('U', 'U');
    }
    if status.contains(Status::WT_NEW) {
        return ('?', '?');
    }
    let index = if status.contains(Status::INDEX_RENAMED) {
        'R'
    } else if status.contains(Status::INDEX_NEW) {
        'A'
    } else if status.contains(Status::INDEX_DELETED) {
        'D'
    } else if status.contains(Status::INDEX_TYPECHANGE) {
        'T'
    } else if status.contains(Status::INDEX_MODIFIED) {
        'M'
    } else {
        ' '
    };
    let worktree = if status.contains(Status::WT_RENAMED) {
        'R'
    } else if status.contains(Status::WT_DELETED) {
        'D'
    } else if status.contains(Status::WT_TYPECHANGE) {
        'T'
    } else if status.contains(Status::WT_MODIFIED) {
        'M'
    } else {
        ' '
    };
    (index, worktree)
}

fn patch_text(diff: &Diff<'_>, paths: Option<&[PathBuf]>) -> Result<String> {
    let mut out = Vec::new();
    for (index, delta) in diff.deltas().enumerate() {
        let old = delta.old_file().path();
        let new = delta.new_file().path();
        if paths.is_some_and(|paths| {
            !paths
                .iter()
                .any(|path| Some(path.as_path()) == old || Some(path.as_path()) == new)
        }) {
            continue;
        }
        if let Some(mut patch) = Patch::from_diff(diff, index)? {
            out.extend_from_slice(patch.to_buf()?.as_ref());
        } else {
            let path = new
                .or(old)
                .map_or_else(|| "file".into(), |p| p.display().to_string());
            out.extend_from_slice(
                format!(
                    "diff --git a/{path} b/{path}\nBinary files a/{path} and b/{path} differ\n"
                )
                .as_bytes(),
            );
        }
    }
    Ok(String::from_utf8_lossy(&out).into_owned())
}

pub(super) fn head_tree(repo: &RawRepo) -> Result<Option<git2::Tree<'_>>> {
    match repo.head() {
        Ok(head) => Ok(Some(head.peel_to_commit()?.tree()?)),
        Err(error)
            if error.code() == ErrorCode::UnbornBranch || error.code() == ErrorCode::NotFound =>
        {
            Ok(None)
        }
        Err(error) => Err(error.into()),
    }
}

pub(super) fn diff_for(repo: &RawRepo, staged: bool) -> Result<Diff<'_>> {
    let mut opts = DiffOptions::new();
    opts.include_typechange(true)
        .include_untracked(true)
        .recurse_untracked_dirs(true)
        .show_untracked_content(true);
    let index = repo.index()?;
    if staged {
        Ok(repo.diff_tree_to_index(head_tree(repo)?.as_ref(), Some(&index), Some(&mut opts))?)
    } else {
        Ok(repo.diff_index_to_workdir(Some(&index), Some(&mut opts))?)
    }
}

pub(super) fn file_changes(repo: &RawRepo) -> Result<Vec<FileChange>> {
    let mut opts = StatusOptions::new();
    opts.include_untracked(true)
        .recurse_untracked_dirs(true)
        .renames_head_to_index(true)
        .renames_index_to_workdir(true);
    let statuses = repo.statuses(Some(&mut opts))?;
    let mut files = Vec::new();
    for entry in statuses.iter() {
        let (index, worktree) = status_chars(entry.status());
        let indexed = entry.head_to_index();
        let working = entry.index_to_workdir();
        let indexed_path = indexed.as_ref().and_then(|d| d.new_file().path_bytes());
        let working_path = working.as_ref().and_then(|d| d.new_file().path_bytes());
        let indexed_original = indexed
            .as_ref()
            .filter(|d| d.status() == Delta::Renamed)
            .and_then(|d| d.old_file().path_bytes())
            .map(path_from_bytes);
        let working_original = working
            .as_ref()
            .filter(|d| d.status() == Delta::Renamed)
            .and_then(|d| d.old_file().path_bytes())
            .map(path_from_bytes);
        // A -> B in the index and B -> C on disk are two different deltas.
        // Keep each layer's paths for its diff and stage/unstage actions.
        if let (Some(indexed_path), Some(working_path)) = (indexed_path, working_path)
            && index != ' '
            && index != '?'
            && worktree != ' '
            && indexed_path != working_path
        {
            files.push(FileChange {
                path: path_from_bytes(indexed_path),
                original: indexed_original,
                index,
                worktree: ' ',
            });
            files.push(FileChange {
                path: path_from_bytes(working_path),
                original: working_original,
                index: ' ',
                worktree,
            });
        } else {
            files.push(FileChange {
                path: path_from_bytes(working_path.or(indexed_path).unwrap_or(entry.path_bytes())),
                original: indexed_original.or(working_original),
                index,
                worktree,
            });
        }
    }
    for file in &mut files {
        if file.worktree == 'M'
            && super::lfs::tracked(repo, &file.path)?
            && super::lfs::matches_index(repo, &file.path)?
        {
            file.worktree = ' ';
        }
    }
    files.retain(|f| f.index != ' ' || f.worktree != ' ');
    Ok(files)
}

fn refs_by_oid(repo: &RawRepo) -> Result<HashMap<Oid, Vec<String>>> {
    let mut refs: HashMap<Oid, Vec<String>> = HashMap::new();
    for item in repo.references()? {
        let reference = item?;
        let Ok(name) = reference.name() else {
            continue;
        };
        if name.ends_with("/HEAD") || name.starts_with("refs/gitbuddy/") {
            continue;
        }
        let Ok(commit) = reference.peel_to_commit() else {
            continue;
        };
        refs.entry(commit.id())
            .or_default()
            .push(reference.shorthand().unwrap_or(name).to_string());
    }
    Ok(refs)
}

/// Include file metadata and indexed blob IDs: status letters alone do not
/// change when an already-dirty file is edited or staged again.
fn fingerprint_of(
    repo: &RawRepo,
    head: Option<&git2::Reference<'_>>,
    files: &[FileChange],
) -> Result<u64> {
    let mut hasher = DefaultHasher::new();
    if let Some(target) = head.and_then(|head| head.target()) {
        target.to_string().hash(&mut hasher);
        if let Ok(name) = head.unwrap().name() {
            name.hash(&mut hasher);
        }
    } else {
        "unborn".hash(&mut hasher);
    }
    format!("{:?}", repo.state()).hash(&mut hasher);
    let mut references: Vec<String> = Vec::new();
    for item in repo.references()? {
        let Ok(reference) = item else {
            continue;
        };
        let (Ok(name), Some(target)) = (reference.name(), reference.target()) else {
            continue;
        };
        references.push(format!("{name}@{target}"));
    }
    references.sort();
    references.hash(&mut hasher);
    // Effective local/global/include configuration can change remotes, upstream
    // and authentication without touching HEAD, references or working files.
    repo.config()?
        .snapshot()?
        .entries(None)?
        .for_each(|entry| {
            entry.name_bytes().hash(&mut hasher);
            entry.value_bytes().hash(&mut hasher);
        })?;
    let index = repo.index()?;
    let workdir = repo.workdir().context("仓库没有工作区")?;
    for change in files {
        change.path.hash(&mut hasher);
        change.original.hash(&mut hasher);
        change.index.hash(&mut hasher);
        change.worktree.hash(&mut hasher);
        for path in std::iter::once(&change.path).chain(change.original.iter()) {
            for stage in 0..=3 {
                let entry = index.get_path(path, stage);
                entry
                    .as_ref()
                    .map(|entry| (entry.id, entry.mode))
                    .hash(&mut hasher);
            }
            hash_worktree_metadata(&workdir.join(path), &mut hasher)?;
        }
    }
    Ok(hasher.finish())
}

fn hash_worktree_metadata(path: &Path, hasher: &mut impl Hasher) -> Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            false.hash(hasher);
            return Ok(());
        }
        Err(error) => {
            return Err(error).with_context(|| format!("无法读取 {} 的修改信息", path.display()));
        }
    };
    true.hash(hasher);
    metadata.len().hash(hasher);
    metadata.modified().ok().hash(hasher);
    metadata.file_type().is_symlink().hash(hasher);
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        // ctime also detects replacement or edits that restore the old mtime.
        (
            metadata.dev(),
            metadata.ino(),
            metadata.mode(),
            metadata.ctime(),
            metadata.ctime_nsec(),
        )
            .hash(hasher);
    }
    if metadata.file_type().is_symlink() {
        fs::read_link(path)?.hash(hasher);
    }
    Ok(())
}

/// SSH agent / credential helper auth, plus optional transfer progress for
/// the status bar. Progress messages are rate-limited to one per 100ms so a
/// fast transfer cannot flood the UI thread.
pub(super) fn network_callbacks<'a>(
    control: NetworkControl,
    config: git2::Config,
) -> RemoteCallbacks<'a> {
    let mut callbacks = RemoteCallbacks::new();
    let sideband = control.clone();
    callbacks.sideband_progress(move |data| {
        sideband.progress(String::from_utf8_lossy(data).trim_end());
        !sideband.cancellation.is_requested()
    });
    let transfer = control.clone();
    let mut first_transfer = true;
    callbacks.transfer_progress(move |stats| {
        if first_transfer {
            first_transfer = false;
            transfer.phase("Transfer: receiving objects…");
        }
        transfer.progress(&format!(
            "Transfer: {} / {} objects ({})",
            stats.received_objects(),
            stats.total_objects(),
            percent(stats.received_objects(), stats.total_objects())
        ));
        !transfer.cancellation.is_requested()
    });
    let tips = control.clone();
    callbacks.update_tips(move |_, _, _| !tips.cancellation.is_requested());
    let negotiation = control.clone();
    callbacks.push_negotiation(move |_| {
        negotiation.phase("Push: preparing upload…");
        // git2's upload progress callback cannot return an abort signal. Stop
        // accepting cancellation before upload; never claim a remote write
        // was rolled back merely because the user clicked Cancel too late.
        negotiation.cancellation.finish().map_err(|_| {
            git2::Error::new(
                ErrorCode::User,
                git2::ErrorClass::Callback,
                "Operation cancelled",
            )
        })?;
        negotiation.phase("Push: uploading; waiting for remote confirmation…");
        Ok(())
    });
    let upload = control.clone();
    callbacks.push_transfer_progress(move |done, total, bytes| {
        upload.progress(&format!(
            "Push: {done} / {total} objects ({}) · {bytes} bytes",
            percent(done, total)
        ))
    });
    let packing = control.clone();
    callbacks.pack_progress(move |_, done, total| {
        packing.progress(&format!("Push: preparing {done} / {total} objects"))
    });
    callbacks.credentials(move |url, username, allowed| {
        control.check_git()?;
        if allowed.contains(CredentialType::SSH_KEY)
            && let Some(username) = username
            && let Ok(credential) = Cred::ssh_key_from_agent(username)
        {
            return Ok(credential);
        }
        if allowed.contains(CredentialType::USER_PASS_PLAINTEXT)
            && let Ok(credential) = Cred::credential_helper(&config, url, username)
        {
            return Ok(credential);
        }
        if allowed.contains(CredentialType::DEFAULT) {
            return Cred::default();
        }
        Err(git2::Error::from_str(
            "No usable Git credentials; configure an SSH agent or credential helper",
        ))
    });
    callbacks
}

fn percent(done: usize, total: usize) -> String {
    match done.checked_mul(100).and_then(|v| v.checked_div(total)) {
        Some(value) => format!("{value}%"),
        None => "0%".into(),
    }
}

impl Repository {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let repo = RawRepo::discover(path)?;
        let root = repo
            .workdir()
            .context("目前不支持裸仓库作为工作区")?
            .canonicalize()?;
        Ok(Self { root })
    }
    pub fn init(path: &Path) -> Result<Self> {
        fs::create_dir_all(path)?;
        let mut options = git2::RepositoryInitOptions::new();
        options.initial_head("main");
        RawRepo::init_opts(path, &options)?;
        Self::open(path)
    }
    pub fn clone_repo(url: &str, destination: &Path) -> Result<Self> {
        Self::clone_repo_with_progress(url, destination, None)
    }

    /// [`clone_repo`] with an optional progress sink receiving remote
    /// transfer messages.
    pub fn clone_repo_with_progress(
        url: &str,
        destination: &Path,
        progress: Option<ProgressSink>,
    ) -> Result<Self> {
        Self::clone_repo_with_control(
            url,
            destination,
            NetworkControl::new(progress, CancellationToken::default()),
        )
    }
    pub fn clone_repo_with_control(
        url: &str,
        destination: &Path,
        control: NetworkControl,
    ) -> Result<Self> {
        let result = (|| {
            control.check()?;
            ensure!(!url.trim().is_empty(), "请输入仓库 URL");
            let destination = if destination.is_absolute() {
                destination.to_path_buf()
            } else {
                std::env::current_dir()?.join(destination)
            };
            let parent = destination.parent().context("Missing destination parent")?;
            fs::create_dir_all(parent)?;
            // Reserve an empty destination. Cleanup never recursively removes
            // this user-visible path or deletes a directory that predated us.
            let created = match fs::create_dir(&destination) {
                Ok(()) => true,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    ensure!(
                        fs::symlink_metadata(&destination)?.file_type().is_dir(),
                        "Clone destination must be a directory, not a file or symbolic link"
                    );
                    ensure!(
                        fs::read_dir(&destination)?.next().is_none(),
                        "Clone destination must be empty"
                    );
                    false
                }
                Err(error) => return Err(error.into()),
            };
            struct EmptyReservation(PathBuf, bool);
            impl Drop for EmptyReservation {
                fn drop(&mut self) {
                    if self.1 {
                        let _ = fs::remove_dir(&self.0);
                    }
                }
            }
            let _reservation = EmptyReservation(destination.clone(), created);
            let staging = tempfile::tempdir_in(parent)?;
            let clone_path = staging.path().join("repository");
            let mut fetch = FetchOptions::new();
            fetch.remote_callbacks(network_callbacks(
                control.clone(),
                git2::Config::open_default()?.snapshot()?,
            ));
            // Unlike an existing working tree, this isolated checkout can safely
            // stop midway and be discarded with its owned temporary directory.
            let checkout_control = control.clone();
            let mut first_checkout = true;
            let mut checkout = CheckoutBuilder::new();
            checkout
                .notify_on(git2::CheckoutNotificationType::all())
                .notify(move |_, _, _, _, _| {
                    if first_checkout {
                        first_checkout = false;
                        checkout_control.phase("Clone: checking out files…");
                    }
                    !checkout_control.cancellation.is_requested()
                });
            let mut builder = RepoBuilder::new();
            builder
                .fetch_options(fetch)
                .with_checkout(checkout)
                .clone_local(git2::build::CloneLocal::None);
            control.phase("Clone: connecting…");
            control.check()?;
            builder.clone(url, &clone_path)?;
            control.cancellation.finish()?;
            control.phase("Clone: installing repository…");
            #[cfg(windows)]
            fs::remove_dir(&destination)?;
            fs::rename(&clone_path, &destination)?;
            Self::open(destination)
        })();
        control.normalize(result)
    }
    /// Cheap change detection for the refresh timer: hashes HEAD, repository
    /// state, references, statuses, indexed blobs, and changed-file metadata.
    /// This avoids a revwalk and does not read every worktree file's contents.
    pub fn fingerprint(&self) -> Result<u64> {
        let repo = raw(self)?;
        let files = file_changes(&repo)?;
        fingerprint_of(&repo, repo.head().ok().as_ref(), &files)
    }
    pub fn snapshot(&self, limit: usize) -> Result<Snapshot> {
        let mut repo = raw(self)?;
        let files = file_changes(&repo)?;
        let head = repo.head().ok();
        let head_id = head
            .as_ref()
            .and_then(|h| h.target())
            .map(|id| id.to_string());
        let fingerprint = fingerprint_of(&repo, head.as_ref(), &files)?;
        let branch = head
            .as_ref()
            .and_then(|h| h.shorthand().ok())
            .map(str::to_string)
            .or_else(|| {
                repo.find_reference("HEAD")
                    .ok()?
                    .symbolic_target()
                    .ok()?
                    .map(|name| name.trim_start_matches("refs/heads/").to_string())
            })
            .unwrap_or_else(|| "detached HEAD".into());
        let mut upstream = String::new();
        let mut ahead = 0;
        let mut behind = 0;
        if let Ok(local) = repo.find_branch(&branch, BranchType::Local)
            && let Ok(tracking) = local.upstream()
        {
            upstream = tracking.name()?.unwrap_or_default().to_string();
            if let (Some(local_id), Some(remote_id)) =
                (local.get().target(), tracking.get().target())
            {
                (ahead, behind) = repo.graph_ahead_behind(local_id, remote_id)?;
            }
        }
        let refs = refs_by_oid(&repo)?;
        let mut commits = Vec::new();
        if head.as_ref().and_then(|h| h.target()).is_some() {
            let mut walk = repo.revwalk()?;
            walk.set_sorting(Sort::TOPOLOGICAL | Sort::TIME)?;
            for id in refs.keys() {
                walk.push(*id)?;
            }
            if let Some(id) = head.as_ref().and_then(|h| h.target()) {
                walk.push(id)?;
            }
            for id in walk.take(limit) {
                let commit = repo.find_commit(id?)?;
                let names = refs.get(&commit.id()).cloned().unwrap_or_default();
                commits.push(Commit {
                    id: commit.id().to_string(),
                    parents: commit.parent_ids().map(|id| id.to_string()).collect(),
                    subject: commit
                        .summary()
                        .ok()
                        .flatten()
                        .unwrap_or_default()
                        .to_string(),
                    author: commit.author().name().unwrap_or_default().to_string(),
                    date: relative_time(commit.time().seconds()),
                    refs: names.join(", "),
                });
            }
        }
        drop(head);
        let mut branches = Vec::new();
        for branch in repo.branches(None)? {
            let (branch, kind) = branch?;
            let name = branch.name()?.unwrap_or_default().to_string();
            if name.ends_with("/HEAD") {
                continue;
            }
            branches.push(Branch {
                name,
                current: branch.is_head(),
                remote: kind == BranchType::Remote,
            });
        }
        branches.sort_by(|a, b| a.name.cmp(&b.name));
        let mut tags: Vec<_> = repo
            .tag_names(None)?
            .iter()
            .filter_map(|name| name.ok().flatten().map(str::to_owned))
            .collect();
        tags.sort();
        let mut stashes = Vec::new();
        repo.stash_foreach(|_, message, id| {
            stashes.push((id.to_string(), message.to_string()));
            true
        })?;
        let remotes = repo
            .remotes()?
            .iter()
            .filter_map(|name| name.ok().flatten().map(str::to_owned))
            .collect();
        let merging = repo.state() == RepositoryState::Merge;
        Ok(Snapshot {
            conflict_operation: if super::rebase::active(&repo) {
                ConflictOperation::Rebase
            } else {
                super::conflicts::operation(repo.state())
            },
            fingerprint,
            head_id,
            branch,
            upstream,
            ahead,
            behind,
            files,
            commits,
            branches,
            tags,
            stashes,
            remotes,
            merging,
        })
    }
    pub fn diff(&self, file: &FileChange, staged: bool) -> Result<String> {
        if file.index == '?' && !staged {
            let path = self.root.join(&file.path);
            let meta = fs::symlink_metadata(&path)?;
            ensure!(
                meta.len() <= 2 * 1024 * 1024,
                "Untracked file is larger than 2 MB"
            );
            let content = fs::read(path)?;
            if content.contains(&0) {
                return Ok("Binary files differ\n".into());
            }
            let content = String::from_utf8_lossy(&content);
            let lines = content.lines().count();
            return Ok(format!(
                "diff --git a/{0} b/{0}\nnew file mode 100644\n--- /dev/null\n+++ b/{0}\n@@ -0,0 +1,{lines} @@\n{1}",
                file.path.display(),
                content
                    .lines()
                    .map(|line| format!("+{line}\n"))
                    .collect::<String>()
            ));
        }
        let repo = raw(self)?;
        let mut diff = diff_for(&repo, staged)?;
        diff.find_similar(Some(DiffFindOptions::new().renames(true)))?;
        let mut paths = vec![file.path.clone()];
        if let Some(old) = &file.original {
            paths.push(old.clone());
        }
        patch_text(&diff, Some(&paths))
    }
    pub fn show(&self, id: &str) -> Result<String> {
        let detail = self.commit_detail(id)?;
        let mut output = format!("{}\n{}\n", detail.subject, detail.body);
        for file in &detail.files {
            output.push_str(&self.commit_file_diff(id, file)?);
        }
        Ok(output)
    }
    pub fn commit_detail(&self, id: &str) -> Result<CommitDetail> {
        let repo = raw(self)?;
        let commit = repo.find_commit(oid(id)?)?;
        let old = if commit.parent_count() > 0 {
            Some(commit.parent(0)?.tree()?)
        } else {
            None
        };
        let new = commit.tree()?;
        let diff = super::inspect::tree_diff(&repo, old.as_ref(), &new)?;
        let files = diff
            .deltas()
            .map(|delta| CommitFile {
                path: delta
                    .new_file()
                    .path()
                    .or_else(|| delta.old_file().path())
                    .unwrap_or(Path::new(""))
                    .to_path_buf(),
                original: (delta.status() == Delta::Renamed)
                    .then(|| delta.old_file().path().map(Path::to_path_buf))
                    .flatten(),
                status: match delta.status() {
                    Delta::Added => 'A',
                    Delta::Deleted => 'D',
                    Delta::Renamed => 'R',
                    Delta::Copied => 'C',
                    Delta::Typechange => 'T',
                    _ => 'M',
                },
            })
            .collect();
        let date = Local
            .timestamp_opt(commit.time().seconds(), 0)
            .single()
            .map_or_else(|| "Unknown date".to_string(), |date| date.to_rfc3339());
        Ok(CommitDetail {
            id: id.into(),
            subject: commit
                .summary()
                .ok()
                .flatten()
                .unwrap_or_default()
                .to_string(),
            body: commit
                .body()
                .ok()
                .flatten()
                .unwrap_or_default()
                .trim_end()
                .to_string(),
            author: format!(
                "{} <{}>",
                commit.author().name().unwrap_or_default(),
                commit.author().email().unwrap_or_default()
            ),
            date,
            files,
        })
    }
    pub fn commit_file_diff(&self, id: &str, file: &CommitFile) -> Result<String> {
        let repo = raw(self)?;
        let commit = repo.find_commit(oid(id)?)?;
        let old = if commit.parent_count() > 0 {
            Some(commit.parent(0)?.tree()?)
        } else {
            None
        };
        let new = commit.tree()?;
        let diff = super::inspect::tree_diff(&repo, old.as_ref(), &new)?;
        super::inspect::single_file_patch(&diff, file)
    }

    pub fn execute(&self, operation: Operation) -> Result<String> {
        self.execute_with_progress(operation, None)
    }

    /// [`execute`] with an optional progress sink that receives remote
    /// transfer messages while network operations run.
    pub fn execute_with_progress(
        &self,
        operation: Operation,
        progress: Option<ProgressSink>,
    ) -> Result<String> {
        self.execute_with_control(
            operation,
            NetworkControl::new(progress, CancellationToken::default()),
        )
    }
    pub fn execute_with_control(
        &self,
        operation: Operation,
        control: NetworkControl,
    ) -> Result<String> {
        let checkout = matches!(
            &operation,
            Operation::Checkout(_)
                | Operation::TrackBranch(_)
                | Operation::CreateBranch(_)
                | Operation::Merge(_)
                | Operation::Pull
                | Operation::Revert(_)
                | Operation::CherryPick(_)
                | Operation::AbortMerge
                | Operation::AbortRevert
                | Operation::AbortCherryPick
                | Operation::Stash(_)
                | Operation::ApplyStash(_)
                | Operation::Rebase { .. }
                | Operation::ContinueRebase
                | Operation::AbortRebase
                | Operation::Discard(_)
                | Operation::ContinueConflict(_)
        );
        let initial = raw(self)?;
        if super::rebase::active(&initial) {
            ensure!(
                matches!(
                    &operation,
                    Operation::ContinueRebase
                        | Operation::AbortRebase
                        | Operation::Stage(_)
                        | Operation::Unstage(_)
                        | Operation::StageAll
                        | Operation::UnstageAll
                        | Operation::Discard(_)
                        | Operation::ResolveConflict { .. }
                        | Operation::ContinueConflict(_)
                ),
                "A GitBuddy rebase is in progress; resolve, continue or abort it first"
            );
        }
        if checkout && let Err(error) = super::lfs::dehydrate(&initial) {
            let _ = super::lfs::hydrate(&initial);
            return Err(error);
        }
        let result = (|| {
            if operation.is_network() {
                control.check()?;
            }
            let mut repo = raw(self)?;
            match operation {
                Operation::Rebase { context, steps } => self.start_rebase(&context, steps),
                Operation::ContinueRebase => self.continue_rebase(),
                Operation::AbortRebase => self.abort_rebase(),
                Operation::CreateWorktree { name, path } => self.create_worktree(&name, &path),
                Operation::LockWorktree { name, locked } => self.lock_worktree(&name, locked),
                Operation::RemoveWorktree(expected) => self.remove_worktree(&expected),
                Operation::AddSubmodule { url, path } => {
                    self.add_submodule(&url, &path, control.clone())
                }
                Operation::UpdateSubmodules { names, recursive } => {
                    self.update_submodules(&names, recursive, control.clone())
                }
                Operation::SyncSubmodules => self.sync_submodules(),
                Operation::StageSubmodule(name) => self.stage_submodule(&name),
                Operation::LfsTrack { pattern, track } => self.lfs_track(&pattern, track),
                Operation::LfsCheckout => self.lfs_checkout(),
                Operation::LfsFetch(remote) => self.lfs_transfer(false, &remote, control.clone()),
                Operation::LfsPush(remote) => self.lfs_transfer(true, &remote, control.clone()),
                Operation::Amend { context, message } => self.amend(&context, &message),
                Operation::UndoLast(expected) => self.undo_last(&expected),
                Operation::RestoreReflog { expected, target } => {
                    self.restore_reflog(&expected, &target)
                }
                Operation::RecoverBranch { target, name } => self.recover_branch(&target, &name),
                Operation::ApplyPartial { patch, selection } => {
                    self.apply_partial(&patch, &selection)
                }
                Operation::ResolveConflict {
                    context,
                    resolution,
                } => self.resolve_conflict(&context, resolution),
                Operation::ContinueConflict(session) => self.continue_conflict(&session),
                Operation::Stage(paths) => {
                    ensure!(!paths.is_empty(), "没有选择文件");
                    let mut index = repo.index()?;
                    for path in paths {
                        super::lfs::stage(&repo, &mut index, &path)?;
                    }
                    index.write()?;
                    Ok("Files staged".into())
                }
                Operation::StageAll => {
                    let mut index = repo.index()?;
                    ensure!(!index.has_conflicts(), "Resolve conflicts first");
                    // Resolve all paths against the final worktree. A rename's
                    // old name may already have been reused by a new file.
                    let mut paths = std::collections::BTreeSet::new();
                    for file in file_changes(&repo)? {
                        paths.insert(file.path);
                        paths.extend(file.original);
                    }
                    for path in paths {
                        super::lfs::stage(&repo, &mut index, &path)?;
                    }
                    index.write()?;
                    Ok("All changes staged".into())
                }
                Operation::Unstage(paths) => {
                    ensure!(!paths.is_empty(), "没有选择文件");
                    unstage_paths(&repo, &paths)?;
                    Ok("Files unstaged".into())
                }
                Operation::UnstageAll => {
                    if let Ok(head) = repo.head() {
                        let object = head.peel(git2::ObjectType::Commit)?;
                        repo.reset(&object, ResetType::Mixed, None)?;
                    } else {
                        let mut index = repo.index()?;
                        index.clear()?;
                        index.write()?;
                    }
                    Ok("All files unstaged".into())
                }
                Operation::Discard(path) => {
                    let index = repo.index()?;
                    ensure!(
                        index.get_path(&path, 0).is_some(),
                        "只能丢弃已跟踪文件的工作区修改"
                    );
                    let mut checkout = CheckoutBuilder::new();
                    checkout.force().disable_pathspec_match(true).path(&path);
                    repo.checkout_index(None, Some(&mut checkout))?;
                    Ok("Working tree changes discarded".into())
                }
                Operation::Commit(message) => {
                    ensure!(
                        matches!(
                            repo.state(),
                            RepositoryState::Clean | RepositoryState::Merge
                        ),
                        "A repository operation is in progress; use its Continue action (external rebases must be continued with Git)"
                    );
                    ensure!(
                        Snapshot::commit_allowed(
                            &file_changes(&repo)?,
                            super::conflicts::operation(repo.state())
                        ),
                        "No staged changes to commit; empty commits require an explicit allow-empty action"
                    );
                    commit_index(&repo, &message, None)
                }
                Operation::CreateBranch(name) => {
                    validate_branch(&name)?;
                    let head = repo.head()?.peel_to_commit()?;
                    repo.branch(&name, &head, false)?;
                    checkout_branch(&repo, &name, BranchType::Local)?;
                    Ok(format!("Switched to {name}"))
                }
                Operation::Checkout(name) => {
                    validate_branch(&name)?;
                    checkout_branch(&repo, &name, BranchType::Local)?;
                    Ok(format!("Switched to {name}"))
                }
                Operation::TrackBranch(name) => {
                    validate_branch(&name)?;
                    let remote = repo.find_branch(&name, BranchType::Remote)?;
                    let local_name = name.split_once('/').map(|(_, part)| part).unwrap_or(&name);
                    let commit = remote.get().peel_to_commit()?;
                    let mut local = repo.branch(local_name, &commit, false)?;
                    local.set_upstream(Some(&name))?;
                    checkout_branch(&repo, local_name, BranchType::Local)?;
                    Ok(format!("Tracking {name}"))
                }
                Operation::DeleteBranch(name) => {
                    validate_branch(&name)?;
                    let mut branch = repo.find_branch(&name, BranchType::Local)?;
                    ensure!(!branch.is_head(), "不能删除当前分支");
                    super::workspaces::branch_available(&repo, &format!("refs/heads/{name}"))?;
                    let head = repo.head()?.target().context("HEAD 不存在")?;
                    let branch_id = branch.get().target().context("分支引用无效")?;
                    ensure!(
                        repo.graph_descendant_of(head, branch_id)? || head == branch_id,
                        "分支尚未合并"
                    );
                    branch.delete()?;
                    Ok(format!("Deleted branch {name}"))
                }
                Operation::Merge(name) => {
                    let reference = merge_reference(&repo, &name)?;
                    let target = reference.target().context("目标分支没有提交")?;
                    merge_target(&repo, target, &name, false)
                }
                Operation::AbortMerge | Operation::AbortRevert | Operation::AbortCherryPick => {
                    abort_operation(&repo)?;
                    Ok("Operation aborted".into())
                }
                Operation::Revert(id) => {
                    ensure!(
                        file_changes(&repo)?.is_empty(),
                        "Revert 前请先提交、Stash 或丢弃现有修改"
                    );
                    let commit = repo.find_commit(oid(&id)?)?;
                    repo.revert(&commit, None)?;
                    if repo.index()?.has_conflicts() {
                        bail!("Revert has conflicts; resolve or abort");
                    }
                    let message = format!(
                        "Revert \"{}\"",
                        commit.summary().ok().flatten().unwrap_or_default()
                    );
                    commit_index(&repo, &message, None)
                }
                Operation::CherryPick(id) => {
                    ensure!(
                        file_changes(&repo)?.is_empty(),
                        "Cherry-pick 前请先提交、Stash 或丢弃现有修改"
                    );
                    let commit = repo.find_commit(oid(&id)?)?;
                    repo.cherrypick(&commit, None)?;
                    if repo.index()?.has_conflicts() {
                        bail!("Cherry-pick has conflicts; resolve or abort");
                    }
                    commit_index(&repo, commit.message().ok().unwrap_or("Cherry-pick"), None)
                }
                Operation::ContinueCherryPick | Operation::ContinueRevert => {
                    let state = repo.state();
                    ensure!(
                        matches!(state, RepositoryState::CherryPick | RepositoryState::Revert),
                        "没有待继续的操作"
                    );
                    let session = self.conflict_session()?;
                    self.continue_conflict(&session)
                }
                Operation::SetIdentity(name, email) => {
                    ensure!(
                        !name.trim().is_empty() && !email.trim().is_empty(),
                        "请输入姓名和邮箱"
                    );
                    let mut config = repo.config()?;
                    config.set_str("user.name", &name)?;
                    config.set_str("user.email", &email)?;
                    Ok("Commit identity saved".into())
                }
                Operation::Fetch => {
                    fetch_all(&repo, control.clone())?;
                    control.cancellation.finish()?;
                    control.phase("Fetch: updating repository view…");
                    Ok("Remotes fetched".into())
                }
                Operation::Pull => {
                    let head = repo.head()?;
                    ensure!(head.is_branch(), "请先切换到本地分支再 Pull");
                    let source = head.name()?;
                    let remote = repo
                        .branch_upstream_remote(source)
                        .context("当前分支没有上游远程配置")?;
                    let remote_name = remote.as_str().context("上游远程名称编码无效")?;
                    let merge = repo
                        .branch_upstream_merge(source)
                        .context("当前分支没有上游分支配置")?;
                    let merge_name = merge.as_str().context("上游分支编码无效")?;
                    ensure!(
                        merge_name.starts_with("refs/heads/")
                            && git2::Reference::is_valid_name(merge_name),
                        "上游目标不是有效的分支引用"
                    );
                    let upstream_name = if remote_name == "." {
                        merge_name.to_string()
                    } else {
                        fetch_one(&repo, remote_name, control.clone())?;
                        repo.branch_upstream_name(source)?
                            .as_str()
                            .context("上游引用编码无效")?
                            .to_string()
                    };
                    let target = repo.find_reference(&upstream_name)?.peel_to_commit()?.id();
                    control.cancellation.finish()?;
                    control.phase("Pull: applying fast-forward…");
                    merge_target(&repo, target, &upstream_name, true)
                }
                Operation::Push => {
                    let head = repo.head()?;
                    ensure!(head.is_branch(), "请先切换到本地分支再推送");
                    let source = head.name()?.to_string();
                    let name = head.shorthand().context("当前不在分支上")?.to_string();
                    // Read configuration directly: a tracking ref may be absent,
                    // and neither the remote nor its branch can be inferred by splitting '/'.
                    let (remote_name, target, set_upstream) = match (
                        repo.branch_upstream_remote(&source),
                        repo.branch_upstream_merge(&source),
                    ) {
                        (Ok(remote), Ok(target)) => (
                            remote.as_str().context("远程名称编码无效")?.to_string(),
                            target.as_str().context("上游分支编码无效")?.to_string(),
                            false,
                        ),
                        (Err(remote), Err(target))
                            if remote.code() == ErrorCode::NotFound
                                && target.code() == ErrorCode::NotFound =>
                        {
                            ("origin".into(), source.clone(), true)
                        }
                        (Err(error), _) | (_, Err(error)) => {
                            return Err(error).context(
                                "上游配置不完整或无法读取，请检查分支的 remote 和 merge 配置",
                            );
                        }
                    };
                    ensure!(
                        target.starts_with("refs/heads/")
                            && git2::Reference::is_valid_name(&target),
                        "上游目标不是有效的分支引用"
                    );
                    let mut remote = repo.find_remote(&remote_name)?;
                    super::lfs::upload_before_push(&repo, &remote_name, &source, &control)?;
                    let mut callbacks =
                        network_callbacks(control.clone(), repo.config()?.snapshot()?);
                    let push_error = std::sync::Arc::new(std::sync::Mutex::new(None::<String>));
                    let error_slot = push_error.clone();
                    callbacks.push_update_reference(move |_, status| {
                        if let Some(status) = status {
                            *error_slot.lock().unwrap() = Some(status.to_string());
                        }
                        Ok(())
                    });
                    let mut options = PushOptions::new();
                    options.remote_callbacks(callbacks);
                    remote.push(&[format!("{source}:{target}")], Some(&mut options))?;
                    // Up-to-date pushes may skip negotiation entirely.
                    control.cancellation.finish()?;
                    if let Some(error) = push_error.lock().unwrap().take() {
                        bail!("Push rejected: {error}");
                    }
                    if set_upstream {
                        let mut local = repo.find_branch(&name, BranchType::Local)?;
                        local.set_upstream(Some(&format!("{remote_name}/{name}")))?;
                    }
                    Ok("Push completed".into())
                }
                Operation::Stash(message) => {
                    for file in file_changes(&repo)? {
                        ensure!(
                            !file.unstaged() || !super::lfs::tracked(&repo, &file.path)?,
                            "Stage modified LFS files before stashing; unstaged LFS content cannot be captured by libgit2's stash filter"
                        );
                    }
                    let signature = author(&repo)?;
                    let name = if message.trim().is_empty() {
                        "GitBuddy stash"
                    } else {
                        &message
                    };
                    repo.stash_save(&signature, name, Some(StashFlags::INCLUDE_UNTRACKED))?;
                    Ok("Changes stashed".into())
                }
                Operation::ApplyStash(id) => {
                    // Pin the stash reflog while libgit2 applies its index-based
                    // API, so an external push/drop cannot change that index.
                    let locked = RawRepo::open(&self.root)?;
                    let mut transaction = locked.transaction()?;
                    transaction.lock_ref("refs/stash")?;
                    let position = stash_position(&locked.reflog("refs/stash")?, &id)?;
                    repo.stash_apply(position, None)?;
                    Ok("Stash applied".into())
                }
                Operation::DropStash(id) => {
                    drop_stash(&repo, &id)?;
                    Ok("Stash deleted".into())
                }
                Operation::Tag(name) => {
                    validate_branch(&name)?;
                    let head = repo.head()?.peel(git2::ObjectType::Commit)?;
                    repo.tag_lightweight(&name, &head, false)?;
                    Ok(format!("Created tag {name}"))
                }
                Operation::DeleteTag(name) => {
                    validate_branch(&name)?;
                    repo.tag_delete(&name)?;
                    Ok(format!("Deleted tag {name}"))
                }
                Operation::AddRemote(name, url) => {
                    ensure!(
                        git2::Remote::is_valid_name(&name) && !url.is_empty(),
                        "请输入有效的远程名称和 URL"
                    );
                    repo.remote(&name, &url)?;
                    Ok(format!("Added remote {name}"))
                }
            }
        })();
        let result = if checkout {
            match super::lfs::hydrate(&raw(self)?) {
                Ok(missing) => result.map(|text| {
                    if missing > 0 {
                        format!("{text}; {missing} LFS objects missing locally (use LFS Fetch)")
                    } else {
                        text
                    }
                }),
                Err(error) => result.and_then(|_| Err(error)),
            }
        } else {
            result
        };
        control.normalize(result)
    }
}

fn merge_reference<'a>(repo: &'a RawRepo, name: &str) -> Result<git2::Reference<'a>> {
    if name.starts_with("refs/heads/") || name.starts_with("refs/remotes/") {
        ensure!(git2::Reference::is_valid_name(name), "无效的引用名称");
        return Ok(repo.find_reference(name)?);
    }
    validate_branch(name)?;
    let local = repo.find_reference(&format!("refs/heads/{name}")).ok();
    let remote = repo.find_reference(&format!("refs/remotes/{name}")).ok();
    ensure!(
        local.is_none() || remote.is_none(),
        "Ambiguous branch name; select a fully qualified local or remote reference"
    );
    local.or(remote).context("目标分支不存在")
}

fn validate_branch(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty()
            && !name.starts_with('-')
            && git2::Reference::is_valid_name(&format!("refs/heads/{name}")),
        "无效的引用名称"
    );
    Ok(())
}

pub(super) fn path_bytes(path: &Path) -> Vec<u8> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        path.as_os_str().as_bytes().to_vec()
    }
    #[cfg(not(unix))]
    {
        path.to_string_lossy().as_bytes().to_vec()
    }
}

fn unstage_paths(repo: &RawRepo, paths: &[PathBuf]) -> Result<()> {
    let mut index = repo.index()?;
    let tree = head_tree(repo)?;
    for path in paths {
        if let Some(entry) = tree.as_ref().and_then(|tree| tree.get_path(path).ok()) {
            let bytes = path_bytes(path);
            index.add(&git2::IndexEntry {
                ctime: git2::IndexTime::new(0, 0),
                mtime: git2::IndexTime::new(0, 0),
                dev: 0,
                ino: 0,
                mode: entry.filemode() as u32,
                uid: 0,
                gid: 0,
                file_size: 0,
                id: entry.id(),
                flags: bytes.len().min(0x0fff) as u16,
                flags_extended: 0,
                path: bytes,
            })?;
        } else if let Err(error) = index.remove_path(path)
            && error.code() != ErrorCode::NotFound
        {
            return Err(error.into());
        }
    }
    index.write()?;
    Ok(())
}

fn stash_position(log: &git2::Reflog, id: &str) -> Result<usize> {
    let expected = oid(id).context("Select a stash by its stable object ID, not stash@{n}")?;
    let matches: Vec<_> = log
        .iter()
        .enumerate()
        .filter(|(_, entry)| entry.id_new() == expected)
        .map(|(i, _)| i)
        .collect();
    ensure!(
        matches.len() == 1,
        "The selected stash disappeared or is ambiguous; reload the stash list. No changes made"
    );
    Ok(matches[0])
}

fn drop_stash(repo: &RawRepo, id: &str) -> Result<()> {
    // Match libgit2's stash-drop transaction, but resolve the selected object
    // only AFTER acquiring the ref lock rather than using a stale UI index.
    let mut transaction = repo.transaction()?;
    transaction.lock_ref("refs/stash")?;
    let mut log = repo.reflog("refs/stash")?;
    let position = stash_position(&log, id)?;
    log.remove(position, true)?;
    if log.is_empty() {
        transaction.remove("refs/stash")?;
    } else if position == 0 {
        transaction.set_target(
            "refs/stash",
            log.get(0).context("Missing stash entry")?.id_new(),
            None,
            "",
        )?;
    }
    transaction.set_reflog("refs/stash", log)?;
    transaction.commit()?;
    Ok(())
}

fn commit_index(repo: &RawRepo, message: &str, other_parent: Option<Oid>) -> Result<String> {
    ensure!(!message.trim().is_empty(), "请填写提交说明");
    ensure!(
        matches!(
            repo.state(),
            RepositoryState::Clean
                | RepositoryState::Merge
                | RepositoryState::CherryPick
                | RepositoryState::Revert
        ),
        "Cannot create a commit in this repository operation; continue it with its owning sequencer"
    );
    let mut index = repo.index()?;
    ensure!(!index.has_conflicts(), "请先解决冲突");
    let tree_id = index.write_tree()?;
    let tree = repo.find_tree(tree_id)?;
    let signature = author(repo)?;
    let head = repo.head().ok().and_then(|head| head.peel_to_commit().ok());
    let merge_parent = if other_parent.is_none() && repo.state() == RepositoryState::Merge {
        let value = fs::read_to_string(repo.path().join("MERGE_HEAD"))?;
        Some(oid(value.trim())?)
    } else {
        other_parent
    };
    let extra = merge_parent.map(|oid| repo.find_commit(oid)).transpose()?;
    let mut parents: Vec<&git2::Commit<'_>> = Vec::new();
    if let Some(head) = &head {
        parents.push(head);
    }
    if let Some(extra) = &extra {
        parents.push(extra);
    }
    let picked = if repo.state() == RepositoryState::CherryPick {
        let marker = fs::read_to_string(repo.path().join("CHERRY_PICK_HEAD"))?;
        Some(repo.find_commit(oid(marker.trim())?)?)
    } else {
        None
    };
    let original_author = picked
        .as_ref()
        .map_or_else(|| signature.clone(), |c| c.author().to_owned());
    let id = repo.commit(
        Some("HEAD"),
        &original_author,
        &signature,
        message,
        &tree,
        &parents,
    )?;
    // Only the operation that owns the state may clear it. This helper
    // completes merges, single cherry-picks and single reverts, never a rebase.
    if matches!(
        repo.state(),
        RepositoryState::Merge | RepositoryState::CherryPick | RepositoryState::Revert
    ) {
        repo.cleanup_state()?;
    }
    Ok(format!("Created commit {id}"))
}

fn checkout_branch(repo: &RawRepo, name: &str, kind: BranchType) -> Result<()> {
    super::workspaces::branch_available(repo, &format!("refs/heads/{name}"))?;
    let branch = repo.find_branch(name, kind)?;
    let commit = branch.get().peel_to_commit()?;
    let mut checkout = CheckoutBuilder::new();
    checkout.safe();
    repo.checkout_tree(commit.as_object(), Some(&mut checkout))?;
    repo.set_head(&format!("refs/heads/{name}"))?;
    Ok(())
}

fn abort_operation(repo: &RawRepo) -> Result<()> {
    ensure!(repo.state() != RepositoryState::Clean, "没有需要中止的操作");
    let head = repo.head()?.peel(git2::ObjectType::Commit)?;
    repo.reset(&head, ResetType::Hard, None)?;
    repo.cleanup_state()?;
    Ok(())
}

fn merge_target(repo: &RawRepo, target: Oid, name: &str, ff_only: bool) -> Result<String> {
    ensure!(
        file_changes(repo)?.is_empty(),
        "合并前请先提交、Stash 或丢弃现有修改"
    );
    let annotated = repo.find_annotated_commit(target)?;
    let (analysis, _) = repo.merge_analysis(&[&annotated])?;
    if analysis.is_up_to_date() {
        return Ok("Already up to date".into());
    }
    if analysis.is_fast_forward() {
        let commit = repo.find_commit(target)?;
        let mut checkout = CheckoutBuilder::new();
        checkout.safe();
        repo.checkout_tree(commit.as_object(), Some(&mut checkout))?;
        let mut head = repo.head()?;
        head.set_target(target, &format!("Fast-forward to {name}"))?;
        return Ok(format!("Fast-forwarded to {name}"));
    }
    ensure!(!ff_only, "不能快进合并上游分支");
    ensure!(analysis.is_normal(), "无法合并此分支");
    let mut checkout = CheckoutBuilder::new();
    checkout
        .safe()
        .allow_conflicts(true)
        .conflict_style_merge(true);
    repo.merge(&[&annotated], None, Some(&mut checkout))?;
    if repo.index()?.has_conflicts() {
        bail!("Merge has conflicts; resolve or abort");
    }
    commit_index(repo, &format!("Merge branch '{name}'"), Some(target))
}

fn fetch_one(repo: &RawRepo, name: &str, control: NetworkControl) -> Result<()> {
    control.check()?;
    control.phase(&format!("Fetch: connecting to {name}…"));
    control.check()?;
    let mut remote = repo.find_remote(name)?;
    let mut options = FetchOptions::new();
    options
        .remote_callbacks(network_callbacks(
            control.clone(),
            repo.config()?.snapshot()?,
        ))
        .prune(git2::FetchPrune::On);
    remote.fetch(&[] as &[&str], Some(&mut options), None)?;
    Ok(())
}

fn fetch_all(repo: &RawRepo, control: NetworkControl) -> Result<()> {
    for name in repo.remotes()?.iter().flatten().flatten() {
        fetch_one(repo, name, control.clone())?;
    }
    Ok(())
}
