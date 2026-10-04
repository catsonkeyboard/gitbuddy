//! Git repository adapter backed by git2/libgit2.
use std::{
    ffi::OsString,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug)]
pub struct Repository {
    pub root: PathBuf,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileChange {
    pub path: PathBuf,
    pub original: Option<PathBuf>,
    pub index: char,
    pub worktree: char,
}
impl FileChange {
    pub fn staged(&self) -> bool {
        !matches!(self.index, ' ' | '?' | '!')
    }
    pub fn unstaged(&self) -> bool {
        self.worktree != ' '
    }
    pub fn conflict(&self) -> bool {
        self.index == 'U'
            || self.worktree == 'U'
            || matches!((self.index, self.worktree), ('A', 'A') | ('D', 'D'))
    }
}
#[derive(Clone, Debug)]
pub struct Commit {
    pub id: String,
    pub parents: Vec<String>,
    pub subject: String,
    pub author: String,
    pub date: String,
    pub refs: String,
}
#[derive(Clone, Debug)]
pub struct CommitFile {
    pub path: PathBuf,
    pub original: Option<PathBuf>,
    pub status: char,
}
#[derive(Clone, Debug)]
pub struct CommitDetail {
    pub id: String,
    pub subject: String,
    pub body: String,
    pub author: String,
    pub date: String,
    pub files: Vec<CommitFile>,
}
#[derive(Clone, Debug)]
pub struct Branch {
    pub name: String,
    pub current: bool,
    pub remote: bool,
}
#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    /// State fingerprint including HEAD, refs, statuses, index blob IDs and
    /// changed-file metadata, used to avoid unnecessary snapshot rebuilds.
    pub fingerprint: u64,
    pub head_id: Option<String>,
    pub branch: String,
    pub upstream: String,
    pub ahead: usize,
    pub behind: usize,
    pub files: Vec<FileChange>,
    pub commits: Vec<Commit>,
    pub branches: Vec<Branch>,
    pub tags: Vec<String>,
    /// Stable stash commit OID and summary; positions like stash@{0} can change.
    pub stashes: Vec<(String, String)>,
    pub remotes: Vec<String>,
    pub merging: bool,
    pub conflict_operation: ConflictOperation,
}
impl Snapshot {
    /// Shared eligibility for the button, keyboard action and backend entry.
    pub fn can_commit(&self) -> bool {
        Self::commit_allowed(&self.files, self.conflict_operation)
    }
    pub(crate) fn commit_allowed(files: &[FileChange], operation: ConflictOperation) -> bool {
        matches!(
            operation,
            ConflictOperation::None | ConflictOperation::Merge
        ) && !files.iter().any(FileChange::conflict)
            && (files.iter().any(FileChange::staged) || operation == ConflictOperation::Merge)
    }
}
#[derive(Clone, Debug)]
pub enum Operation {
    Rebase {
        context: std::sync::Arc<RebasePreview>,
        steps: Vec<RebaseStep>,
    },
    ContinueRebase,
    AbortRebase,
    CreateWorktree {
        name: String,
        path: PathBuf,
    },
    LockWorktree {
        name: String,
        locked: bool,
    },
    RemoveWorktree(WorktreeInfo),
    AddSubmodule {
        url: String,
        path: PathBuf,
    },
    UpdateSubmodules {
        names: Vec<String>,
        recursive: bool,
    },
    SyncSubmodules,
    StageSubmodule(String),
    LfsTrack {
        pattern: String,
        track: bool,
    },
    LfsCheckout,
    LfsFetch(String),
    LfsPush(String),
    Stage(Vec<PathBuf>),
    Unstage(Vec<PathBuf>),
    ApplyPartial {
        patch: std::sync::Arc<PartialPatch>,
        selection: PatchSelection,
    },
    ResolveConflict {
        context: std::sync::Arc<ConflictContext>,
        resolution: ConflictResolution,
    },
    ContinueConflict(std::sync::Arc<ConflictSession>),
    StageAll,
    UnstageAll,
    Discard(PathBuf),
    Commit(String),
    Amend {
        context: std::sync::Arc<CommitEdit>,
        message: String,
    },
    UndoLast(HeadState),
    RestoreReflog {
        expected: HeadState,
        target: ReflogTarget,
    },
    RecoverBranch {
        target: ReflogTarget,
        name: String,
    },
    CreateBranch(String),
    Checkout(String),
    DeleteBranch(String),
    Merge(String),
    AbortMerge,
    Revert(String),
    CherryPick(String),
    AbortRevert,
    AbortCherryPick,
    TrackBranch(String),
    ContinueCherryPick,
    ContinueRevert,
    SetIdentity(String, String),
    Fetch,
    Pull,
    Push,
    Stash(String),
    ApplyStash(String),
    DropStash(String),
    Tag(String),
    DeleteTag(String),
    AddRemote(String, String),
}

fn path_from_bytes(b: &[u8]) -> PathBuf {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        PathBuf::from(OsString::from_vec(b.to_vec()))
    }
    #[cfg(not(unix))]
    {
        PathBuf::from(String::from_utf8_lossy(b).into_owned())
    }
}
pub fn parse_status(bytes: &[u8]) -> Vec<FileChange> {
    let mut records = bytes.split(|b| *b == 0);
    let mut files = Vec::new();
    while let Some(record) = records.next() {
        if record.len() < 4 {
            continue;
        }
        let index = record[0] as char;
        let worktree = record[1] as char;
        let original = if matches!(index, 'R' | 'C') || matches!(worktree, 'R' | 'C') {
            records.next().map(path_from_bytes)
        } else {
            None
        };
        files.push(FileChange {
            path: path_from_bytes(&record[3..]),
            original,
            index,
            worktree,
        });
    }
    files
}

#[path = "git/libgit.rs"]
mod libgit;
#[path = "git/partial.rs"]
mod partial;
pub use partial::{FilePatch, PartialPatch, PatchSelection};
#[path = "git/recovery.rs"]
mod recovery;
pub use recovery::{CommitEdit, HeadState, ReflogEntry, ReflogPage, ReflogTarget};
#[path = "git/inspect.rs"]
mod inspect;
pub use inspect::{
    BlameLine, BlameView, Comparison, FileHistory, FileHistoryEntry, Revision, TreeFiles,
};
#[path = "git/conflicts.rs"]
mod conflicts;
pub use conflicts::{
    ConflictContext, ConflictFile, ConflictOperation, ConflictResolution, ConflictSession,
    ConflictSide, has_conflict_markers,
};

/// Shared progress sink shown in the status bar during network operations.
/// `Arc<Mutex<...>>` because libgit2 callbacks fire from worker threads.
pub type ProgressSink = std::sync::Arc<std::sync::Mutex<dyn FnMut(&str) + Send>>;

#[path = "git/network.rs"]
mod network;
pub use network::{CancellationToken, Cancelled, NetworkControl, is_cancelled};

#[path = "git/rebase.rs"]
mod rebase;
pub use rebase::{RebaseAction, RebasePreview, RebaseStatus, RebaseStep};
#[path = "git/workspaces.rs"]
mod workspaces;
pub use workspaces::{SubmoduleInfo, WorktreeInfo};
#[path = "git/lfs.rs"]
mod lfs;
pub use lfs::{LfsFile, LfsStatus};

impl Operation {
    pub fn is_network(&self) -> bool {
        matches!(
            self,
            Self::Fetch
                | Self::Pull
                | Self::Push
                | Self::AddSubmodule { .. }
                | Self::UpdateSubmodules { .. }
                | Self::LfsFetch(_)
                | Self::LfsPush(_)
        )
    }
    pub fn label(&self) -> &'static str {
        match self {
            Self::Fetch => "Fetch",
            Self::Pull => "Pull",
            Self::Push => "Push",
            Self::Rebase { .. } => "Rebase",
            Self::ContinueRebase => "Continue rebase",
            Self::AbortRebase => "Abort rebase",
            Self::CreateWorktree { .. } => "Create worktree",
            Self::AddSubmodule { .. } | Self::UpdateSubmodules { .. } => "Update submodules",
            Self::LfsFetch(_) => "LFS fetch",
            Self::LfsPush(_) => "LFS push",
            _ => "Git operation",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffLine {
    pub old: String,
    pub new: String,
    pub text: String,
    pub kind: char,
}
pub fn diff_lines(diff: &str) -> Vec<DiffLine> {
    let mut old = 0usize;
    let mut new = 0usize;
    let mut in_hunk = false;
    diff.lines()
        .take(20_000)
        .map(|line| {
            let mut a = String::new();
            let mut b = String::new();
            let kind = if line.starts_with("@@") {
                let mut words = line.split_whitespace();
                words.next();
                old = words
                    .next()
                    .and_then(|w| w.trim_start_matches('-').split(',').next()?.parse().ok())
                    .unwrap_or(0);
                new = words
                    .next()
                    .and_then(|w| w.trim_start_matches('+').split(',').next()?.parse().ok())
                    .unwrap_or(0);
                in_hunk = true;
                '@'
            } else if line.starts_with("diff --git") {
                in_hunk = false;
                'h'
            } else if in_hunk && line.starts_with('+') {
                b = new.to_string();
                new += 1;
                '+'
            } else if in_hunk && line.starts_with('-') {
                a = old.to_string();
                old += 1;
                '-'
            } else if in_hunk && line.starts_with(' ') {
                a = old.to_string();
                b = new.to_string();
                old += 1;
                new += 1;
                ' '
            } else {
                'h'
            };
            DiffLine {
                old: a,
                new: b,
                text: line.into(),
                kind,
            }
        })
        .collect()
}

/// Preview cap shared by the file list and every expanded patch. Diff text
/// beyond [`DIFF_LIMIT_LINES`] lines is dropped with a marker line so huge
/// files cannot freeze the UI.
pub const DIFF_LIMIT_LINES: usize = 20_000;

/// [`diff_lines`] with the shared preview cap applied, appending a marker
/// line when the input was truncated.
pub fn diff_preview(diff: &str) -> Vec<DiffLine> {
    let truncated = diff.lines().count() > DIFF_LIMIT_LINES;
    let mut lines = diff_lines(diff);
    if truncated {
        lines.push(DiffLine {
            old: String::new(),
            new: String::new(),
            kind: 'h',
            text: "Preview limited to 20,000 lines.".into(),
        });
    }
    lines
}
