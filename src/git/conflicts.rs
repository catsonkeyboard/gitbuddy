//! Index-stage conflict inspection and guarded, per-file resolution.
use super::libgit::{author, oid, raw};
use super::recovery::head_state;
use super::{HeadState, Repository};
use anyhow::{Context, Result, ensure};
use git2::{Index, IndexEntry, IndexTime, Oid, RepositoryState};
use std::{
    fs,
    io::Write,
    path::{Component, Path, PathBuf},
};

const EDIT_LIMIT: usize = 2 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ConflictOperation {
    #[default]
    None,
    Merge,
    CherryPick,
    Revert,
    Rebase,
    Other,
}
impl ConflictOperation {
    pub fn label(self) -> &'static str {
        match self {
            Self::None => "Index conflicts",
            Self::Merge => "Merge",
            Self::CherryPick => "Cherry-pick",
            Self::Revert => "Revert",
            Self::Rebase => "Rebase",
            Self::Other => "Repository operation",
        }
    }
    pub fn can_continue(self) -> bool {
        matches!(
            self,
            Self::Merge | Self::CherryPick | Self::Revert | Self::Rebase
        )
    }
}
pub(super) fn operation(state: RepositoryState) -> ConflictOperation {
    match state {
        RepositoryState::Clean => ConflictOperation::None,
        RepositoryState::Merge => ConflictOperation::Merge,
        RepositoryState::CherryPick => ConflictOperation::CherryPick,
        RepositoryState::Revert => ConflictOperation::Revert,
        _ => ConflictOperation::Other,
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
struct Entry {
    path: PathBuf,
    bytes: Vec<u8>,
    id: Oid,
    mode: u32,
}
impl From<IndexEntry> for Entry {
    fn from(e: IndexEntry) -> Self {
        Self {
            path: super::path_from_bytes(&e.path),
            bytes: e.path,
            id: e.id,
            mode: e.mode,
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConflictFile {
    pub path: PathBuf,
    pub kind: String,
    pub supported: bool,
    entries: [Option<Entry>; 3],
}
#[derive(Clone, Debug)]
pub struct ConflictSide {
    pub path: PathBuf,
    pub mode: u32,
    pub text: Option<String>,
    pub description: String,
    pub lines: std::sync::Arc<Vec<String>>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
struct Token {
    head: HeadState,
    state: RepositoryState,
    markers: Vec<Option<Vec<u8>>>,
}
#[derive(Clone, Debug)]
pub struct ConflictSession {
    pub operation: ConflictOperation,
    pub message: String,
    pub files: Vec<ConflictFile>,
    token: Token,
    index: Vec<u8>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
struct WorkVersion {
    id: Option<Oid>,
    mode: u32,
}
#[derive(Clone, Debug)]
pub struct ConflictContext {
    pub file: ConflictFile,
    pub base: Option<ConflictSide>,
    pub ours: Option<ConflictSide>,
    pub theirs: Option<ConflictSide>,
    pub result: Option<String>,
    pub editable: bool,
    pub worktree_exists: bool,
    pub description: String,
    token: Token,
    work: WorkVersion,
}
#[derive(Clone, Debug)]
pub enum ConflictResolution {
    Edited(String),
    Ours,
    Theirs,
    Worktree,
    Delete,
}

pub fn has_conflict_markers(text: &str) -> bool {
    text.lines().any(|line| {
        line.starts_with("<<<<<<<")
            || line.starts_with("|||||||")
            || line == "======="
            || line.starts_with(">>>>>>>")
    })
}
fn token(repo: &git2::Repository) -> Result<Token> {
    let markers = [
        "MERGE_HEAD",
        "CHERRY_PICK_HEAD",
        "REVERT_HEAD",
        "MERGE_MSG",
        "gitbuddy-rebase.json",
    ]
    .iter()
    .map(|name| match fs::read(repo.path().join(name)) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    })
    .collect::<Result<Vec<_>>>()?;
    Ok(Token {
        head: head_state(repo)?,
        state: repo.state(),
        markers,
    })
}
fn index_bytes(index: &Index) -> Result<Vec<u8>> {
    Ok(fs::read(
        index.path().context("Repository index is unavailable.")?,
    )?)
}
fn conflict_files(index: &Index) -> Result<Vec<ConflictFile>> {
    let mut files = Vec::new();
    for conflict in index.conflicts()? {
        let c = conflict?;
        let entries = [
            c.ancestor.map(Entry::from),
            c.our.map(Entry::from),
            c.their.map(Entry::from),
        ];
        let path = entries[1]
            .as_ref()
            .or(entries[2].as_ref())
            .or(entries[0].as_ref())
            .context("Empty conflict record")?
            .path
            .clone();
        let supported = entries
            .iter()
            .flatten()
            .all(|e| e.path == path && matches!(e.mode, 0o100644 | 0o100755));
        let kind = match (&entries[0], &entries[1], &entries[2]) {
            (None, Some(_), Some(_)) => "Both added",
            (_, None, Some(_)) => "Deleted by ours",
            (_, Some(_), None) => "Deleted by theirs",
            (_, None, None) => "Both deleted",
            _ => "Both modified",
        }
        .to_string();
        files.push(ConflictFile {
            path,
            kind,
            supported,
            entries,
        });
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(files)
}
fn side(repo: &git2::Repository, e: &Option<Entry>) -> Result<Option<ConflictSide>> {
    e.as_ref()
        .map(|e| {
            if !matches!(e.mode, 0o100644 | 0o100755) {
                return Ok(ConflictSide {
                    path: e.path.clone(),
                    mode: e.mode,
                    text: None,
                    description: "Non-regular file; resolve with an external tool.".into(),
                    lines: std::sync::Arc::new(Vec::new()),
                });
            }
            let b = repo.find_blob(e.id)?;
            let text = if b.size() <= EDIT_LIMIT && !b.is_binary() {
                std::str::from_utf8(b.content()).ok().map(str::to_owned)
            } else {
                None
            };
            let description = if text.is_some() {
                format!("{} bytes · {:o}", b.size(), e.mode)
            } else {
                format!("{} bytes · binary, non-UTF-8 or over 2 MB", b.size())
            };
            let lines = text
                .as_ref()
                .map(|s| s.lines().take(20_000).map(str::to_owned).collect())
                .unwrap_or_default();
            Ok(ConflictSide {
                path: e.path.clone(),
                mode: e.mode,
                text,
                description,
                lines: std::sync::Arc::new(lines),
            })
        })
        .transpose()
}
fn safe_path(root: &Path, path: &Path) -> Result<PathBuf> {
    ensure!(
        !path.as_os_str().is_empty()
            && path.components().all(|c| matches!(c, Component::Normal(_))),
        "Choose a repository-relative file path."
    );
    ensure!(
        !path
            .components()
            .any(|c| c.as_os_str().to_string_lossy().eq_ignore_ascii_case(".git")),
        "Git metadata paths cannot be resolved as working files."
    );
    let mut current = root.to_path_buf();
    for part in path.components() {
        current.push(part.as_os_str());
        match fs::symlink_metadata(&current) {
            Ok(m) => ensure!(
                !m.file_type().is_symlink()
                    && (current == root.join(path) && m.is_file() || m.is_dir()),
                "Symlinks and directory conflicts require an external tool."
            ),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(current)
}
fn worktree(root: &Path, path: &Path) -> Result<(WorkVersion, Option<Vec<u8>>)> {
    let full = safe_path(root, path)?;
    match fs::read(&full) {
        Ok(bytes) => {
            #[cfg(unix)]
            let mode = {
                use std::os::unix::fs::PermissionsExt;
                if fs::metadata(&full)?.permissions().mode() & 0o111 != 0 {
                    0o100755
                } else {
                    0o100644
                }
            };
            #[cfg(not(unix))]
            let mode = 0o100644;
            Ok((
                WorkVersion {
                    id: Some(Oid::hash_object(git2::ObjectType::Blob, &bytes)?),
                    mode,
                },
                Some(bytes),
            ))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Ok((WorkVersion { id: None, mode: 0 }, None))
        }
        Err(e) => Err(e.into()),
    }
}
// Own the standard index lock before reading or replacing either file. A copy
// of the latest index preserves unrelated staged entries and index extensions.
struct IndexLock {
    path: PathBuf,
    file: fs::File,
    cleanup: bool,
}
impl IndexLock {
    fn acquire(index: &Path) -> Result<Self> {
        let mut name = index.as_os_str().to_owned();
        name.push(".lock");
        let path = PathBuf::from(name);
        let file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .context("Index is locked. Finish the other Git operation and retry.")?;
        Ok(Self {
            path,
            file,
            cleanup: true,
        })
    }
    fn install(&mut self, target: &Path) -> std::io::Result<()> {
        // After rename the old path can immediately be owned by another Git
        // process. Do not unlink that new process's lock from Drop.
        self.cleanup = false;
        let result = fs::rename(&self.path, target);
        if result.is_err() {
            self.cleanup = true;
        }
        result
    }
}
impl Drop for IndexLock {
    fn drop(&mut self) {
        if self.cleanup {
            let _ = fs::remove_file(&self.path);
        }
    }
}

impl Repository {
    pub fn conflict_session(&self) -> Result<ConflictSession> {
        let repo = raw(self)?;
        let mut index = repo.index()?;
        index.read(true)?;
        let token = token(&repo)?;
        let operation = if super::rebase::active(&repo) {
            ConflictOperation::Rebase
        } else {
            operation(token.state)
        };
        let message = token.markers[3]
            .as_ref()
            .map(|b| String::from_utf8_lossy(b).into_owned())
            .unwrap_or_else(|| format!("Complete {}", operation.label()));
        let message = message
            .lines()
            .filter(|l| !l.starts_with('#'))
            .collect::<Vec<_>>()
            .join("\n")
            .trim()
            .to_owned();
        Ok(ConflictSession {
            operation,
            message,
            files: conflict_files(&index)?,
            token,
            index: index_bytes(&index)?,
        })
    }
    pub fn conflict_context(&self, path: &Path) -> Result<ConflictContext> {
        let repo = raw(self)?;
        let mut index = repo.index()?;
        index.read(true)?;
        let file = conflict_files(&index)?
            .into_iter()
            .find(|f| f.path == path)
            .context("This file no longer has an index conflict. Refresh the conflict list.")?;
        let base = side(&repo, &file.entries[0])?;
        let ours = side(&repo, &file.entries[1])?;
        let theirs = side(&repo, &file.entries[2])?;
        let (work, bytes) = worktree(&self.root, path)?;
        let result = bytes
            .as_ref()
            .filter(|b| b.len() <= EDIT_LIMIT && !b.contains(&0))
            .and_then(|b| std::str::from_utf8(b).ok())
            .map(str::to_owned);
        let editable = file.supported && bytes.as_ref().is_none_or(|_| result.is_some());
        let description = if !file.supported {
            "Renames, symlinks and submodules require an external resolution tool."
        } else if !editable {
            "Binary, non-UTF-8 or large file: choose a complete side or resolve externally."
        } else {
            "Edit the result, remove conflict markers, then save and mark resolved."
        }
        .into();
        Ok(ConflictContext {
            file,
            base,
            ours,
            theirs,
            result,
            editable,
            worktree_exists: bytes.is_some(),
            description,
            token: token(&repo)?,
            work,
        })
    }
    pub(super) fn resolve_conflict(
        &self,
        context: &ConflictContext,
        resolution: ConflictResolution,
    ) -> Result<String> {
        let repo = raw(self)?;
        let mut refs = repo.transaction()?;
        refs.lock_ref("HEAD")?;
        if context.token.head.reference != "HEAD" {
            refs.lock_ref(&context.token.head.reference)?;
        }
        ensure!(
            token(&repo)? == context.token,
            "The repository operation or HEAD changed. Reopen the conflict."
        );
        ensure!(
            context.file.supported,
            "This conflict requires an external resolution tool."
        );
        let index = repo.index()?;
        let index_path = index.path().context("Missing index")?.to_path_buf();
        let mut lock = IndexLock::acquire(&index_path)?;
        let scratch = tempfile::NamedTempFile::new_in(repo.path())?;
        fs::copy(&index_path, scratch.path())?;
        let mut next = Index::open(scratch.path())?;
        let actual = conflict_files(&next)?
            .into_iter()
            .find(|f| f.path == context.file.path);
        ensure!(
            actual.as_ref() == Some(&context.file),
            "The index conflict changed. Reopen the conflict."
        );
        let (version, old_bytes) = worktree(&self.root, &context.file.path)?;
        ensure!(
            version == context.work,
            "The working file changed outside this view. Reload it before resolving."
        );
        let fallback_mode = context.file.entries[1]
            .as_ref()
            .or(context.file.entries[2].as_ref())
            .map(|e| e.mode)
            .unwrap_or(0o100644);
        let (content, mode, replace_worktree) = match resolution {
            ConflictResolution::Edited(text) => {
                ensure!(
                    context.editable && text.len() <= EDIT_LIMIT,
                    "This file cannot be edited in the conflict view."
                );
                ensure!(
                    !has_conflict_markers(&text),
                    "Remove the remaining conflict markers before marking resolved."
                );
                (
                    Some(text.into_bytes()),
                    if version.mode != 0 {
                        version.mode
                    } else {
                        fallback_mode
                    },
                    true,
                )
            }
            ConflictResolution::Ours | ConflictResolution::Theirs => {
                let selected = if matches!(resolution, ConflictResolution::Ours) {
                    &context.file.entries[1]
                } else {
                    &context.file.entries[2]
                };
                match selected {
                    Some(e) => (Some(repo.find_blob(e.id)?.content().to_vec()), e.mode, true),
                    None => (None, 0, true),
                }
            }
            ConflictResolution::Delete => (None, 0, true),
            ConflictResolution::Worktree => {
                if let Some(bytes) = &old_bytes {
                    ensure!(
                        !std::str::from_utf8(bytes).is_ok_and(has_conflict_markers),
                        "Remove the remaining conflict markers first."
                    );
                }
                (old_bytes.clone(), version.mode, false)
            }
        };
        next.conflict_remove(&context.file.path)?;
        if let Some(bytes) = &content {
            let e = context
                .file
                .entries
                .iter()
                .flatten()
                .next()
                .context("Empty conflict")?;
            let clean;
            let staged_bytes = if super::lfs::tracked(&repo, &context.file.path)? {
                clean = super::lfs::clean_content(&repo, bytes)?;
                clean.as_slice()
            } else {
                bytes.as_slice()
            };
            next.add(&IndexEntry {
                ctime: IndexTime::new(0, 0),
                mtime: IndexTime::new(0, 0),
                dev: 0,
                ino: 0,
                mode,
                uid: 0,
                gid: 0,
                file_size: staged_bytes.len() as u32,
                id: repo.blob(staged_bytes)?,
                flags: 0,
                flags_extended: 0,
                path: e.bytes.clone(),
            })?;
        } else if next.get_path(&context.file.path, 0).is_some() {
            next.remove_path(&context.file.path)?;
        }
        next.write()?;
        lock.file.write_all(&fs::read(scratch.path())?)?;
        lock.file.sync_all()?;
        // Build the replacement before changing the working file or index.
        let full = safe_path(&self.root, &context.file.path)?;
        let mut replacement = if replace_worktree && let Some(bytes) = &content {
            let parent = full.parent().context("Missing parent directory")?;
            fs::create_dir_all(parent)?;
            let mut temp = tempfile::NamedTempFile::new_in(parent)?;
            temp.write_all(bytes)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                temp.as_file().set_permissions(fs::Permissions::from_mode(
                    if mode == 0o100755 { 0o755 } else { 0o644 },
                ))?;
            }
            temp.as_file().sync_all()?;
            Some(temp)
        } else {
            None
        };
        ensure!(
            token(&repo)? == context.token
                && worktree(&self.root, &context.file.path)?.0 == version,
            "The conflict changed while saving. Reload it and retry."
        );
        if replace_worktree {
            if let Some(temp) = replacement.take() {
                temp.persist(&full)?;
            } else if old_bytes.is_some() {
                fs::remove_file(&full)?;
            }
        }
        if let Err(error) = lock.install(&index_path) {
            // If final index installation fails, restore only our own write.
            if replace_worktree && worktree(&self.root, &context.file.path)?.1 == content {
                match old_bytes {
                    Some(bytes) => {
                        fs::write(&full, bytes)?;
                        #[cfg(unix)]
                        {
                            use std::os::unix::fs::PermissionsExt;
                            fs::set_permissions(
                                &full,
                                fs::Permissions::from_mode(if version.mode == 0o100755 {
                                    0o755
                                } else {
                                    0o644
                                }),
                            )?;
                        }
                    }
                    None => {
                        let _ = fs::remove_file(&full);
                    }
                }
            }
            return Err(error.into());
        }
        Ok(format!("Resolved {}", context.file.path.display()))
    }
    pub(super) fn continue_conflict(&self, session: &ConflictSession) -> Result<String> {
        let repo = raw(self)?;
        if session.operation == ConflictOperation::Rebase {
            ensure!(
                token(&repo)? == session.token && index_bytes(&repo.index()?)? == session.index,
                "Rebase or staged content changed; refresh before continuing"
            );
            return self.continue_rebase();
        }
        ensure!(
            session.operation.can_continue(),
            "This operation must be continued with an external tool."
        );
        let mut head_lock = repo.transaction()?;
        head_lock.lock_ref("HEAD")?;
        let mut branch_lock = if session.token.head.reference != "HEAD" {
            let mut lock = repo.transaction()?;
            lock.lock_ref(&session.token.head.reference)?;
            Some(lock)
        } else {
            None
        };
        ensure!(
            token(&repo)? == session.token,
            "The repository operation changed. Reload before continuing."
        );
        let mut index = repo.index()?;
        let _index_lock = IndexLock::acquire(index.path().context("Missing index")?)?;
        index.read(true)?;
        ensure!(
            !index.has_conflicts(),
            "Resolve every file before continuing."
        );
        ensure!(
            index_bytes(&index)? == session.index,
            "The index changed. Reload and review before continuing."
        );
        let tree = repo.find_tree(index.write_tree()?)?;
        let head = repo.find_commit(oid(session
            .token
            .head
            .id
            .as_deref()
            .context("HEAD has no commit.")?)?)?;
        let extra = if session.operation == ConflictOperation::Merge {
            let bytes = session.token.markers[0]
                .as_ref()
                .context("Missing merge target")?;
            Some(repo.find_commit(oid(std::str::from_utf8(bytes)?.trim())?)?)
        } else {
            None
        };
        let picked = if session.operation == ConflictOperation::CherryPick {
            let bytes = session.token.markers[1]
                .as_ref()
                .context("Missing cherry-pick target")?;
            Some(repo.find_commit(oid(std::str::from_utf8(bytes)?.trim())?)?)
        } else {
            None
        };
        let signature = author(&repo)?;
        let original_author = picked.as_ref().map(|c| c.author());
        let mut parents = vec![&head];
        if let Some(extra) = &extra {
            parents.push(extra);
        }
        let message = session
            .message
            .lines()
            .filter(|l| !l.starts_with('#'))
            .collect::<Vec<_>>()
            .join("\n");
        ensure!(
            !message.trim().is_empty(),
            "The operation has no commit message."
        );
        let id = repo.commit(
            None,
            original_author.as_ref().unwrap_or(&signature),
            &signature,
            message.trim(),
            &tree,
            &parents,
        )?;
        ensure!(
            token(&repo)? == session.token,
            "The operation changed while continuing. Reload and retry."
        );
        repo.reference_ensure_log("HEAD")?;
        let transaction = branch_lock.as_mut().unwrap_or(&mut head_lock);
        transaction.set_target(
            &session.token.head.reference,
            id,
            Some(&signature),
            &format!("gitbuddy: complete {}", session.operation.label()),
        )?;
        if let Some(lock) = branch_lock {
            lock.commit()?;
        } else {
            head_lock.commit()?;
        }
        repo.cleanup_state()?;
        Ok(format!("Created commit {id}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn installed_index_lock_does_not_remove_another_process_lock() {
        let dir = tempfile::tempdir().unwrap();
        let index = dir.path().join("index");
        let mut lock = IndexLock::acquire(&index).unwrap();
        lock.file.write_all(b"new index").unwrap();
        lock.install(&index).unwrap();
        fs::write(dir.path().join("index.lock"), b"another process").unwrap();
        drop(lock);
        assert_eq!(fs::read(index).unwrap(), b"new index");
        assert_eq!(
            fs::read(dir.path().join("index.lock")).unwrap(),
            b"another process"
        );
    }
}
