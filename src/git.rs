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
    /// The sequencer is paused for user-created replacement commits.
    pub rebase_editing: bool,
    pub squash_pending: bool,
    pub patch_recovery: bool,
    pub squash_message: Option<String>,
}
impl Snapshot {
    /// Shared eligibility for the button, keyboard action and backend entry.
    pub fn can_commit(&self) -> bool {
        if self.squash_pending || self.patch_recovery {
            return false;
        }
        Self::commit_allowed(
            &self.files,
            if self.rebase_editing {
                ConflictOperation::None
            } else {
                self.conflict_operation
            },
        )
    }
    pub(crate) fn commit_allowed(files: &[FileChange], operation: ConflictOperation) -> bool {
        matches!(
            operation,
            ConflictOperation::None | ConflictOperation::Merge | ConflictOperation::Squash
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
    RecoverRebaseStep {
        expected: String,
        skip: bool,
    },
    CommitRebase(String),
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
    PrepareWorktree(WorktreeInfo),
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
    UpdateSubmoduleAt(SubmoduleInfo),
    StageSubmoduleAt(SubmoduleInfo),
    LfsTrack {
        pattern: String,
        track: bool,
    },
    LfsCheckout,
    LfsFetch(String),
    LfsPush(String),
    LfsLock {
        remote: String,
        path: PathBuf,
    },
    LfsUnlock {
        remote: String,
        lock: LfsLock,
        force: bool,
    },
    LfsQuarantine(std::sync::Arc<LfsMaintenance>),
    LfsRestoreQuarantine,
    LfsPurgeQuarantine(std::sync::Arc<LfsMaintenance>),
    Stage(Vec<PathBuf>),
    Unstage(Vec<PathBuf>),
    ApplyPartial {
        patch: std::sync::Arc<PartialPatch>,
        selection: PatchSelection,
    },
    DiscardPartial {
        patch: std::sync::Arc<PartialPatch>,
        selection: PatchSelection,
    },
    ApplyPatch(std::sync::Arc<ApplyPatchPlan>),
    RecoverPatch,
    ResolveConflict {
        context: std::sync::Arc<ConflictContext>,
        resolution: ConflictResolution,
    },
    ContinueConflict(std::sync::Arc<ConflictSession>),
    StageAll,
    UnstageAll,
    Discard(PathBuf),
    Commit(String),
    CommitEmpty(String),
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
    CreateBranchAt {
        name: String,
        target: String,
    },
    Reset {
        context: std::sync::Arc<ResetContext>,
        mode: ResetMode,
    },
    Checkout(String),
    CheckoutDetached(std::sync::Arc<DetachedCheckout>),
    RenameBranch {
        expected: std::sync::Arc<BranchRename>,
        name: String,
    },
    DeleteRemoteBranch(std::sync::Arc<PushPlan>),
    DeleteBranch(String),
    Merge(String),
    MergeWith {
        context: std::sync::Arc<MergePreview>,
        mode: MergeMode,
    },
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
    FetchFrom(std::sync::Arc<FetchPlan>),
    DeleteRemoteTag(std::sync::Arc<RemoteTagDelete>),
    Pull,
    PullWith {
        context: std::sync::Arc<PullPreview>,
        strategy: PullStrategy,
    },
    Push,
    PushTo(std::sync::Arc<PushPlan>),
    Stash(String),
    ApplyStash(String),
    DropStash(String),
    SaveStash {
        context: std::sync::Arc<StashContext>,
        paths: Vec<PathBuf>,
        message: String,
    },
    RestoreStash {
        context: std::sync::Arc<StashContext>,
        id: String,
        action: StashAction,
        reinstate_index: bool,
    },
    Tag(String),
    CreateTag(std::sync::Arc<TagPlan>),
    DeleteTag(String),
    AddRemote(String, String),
    EditRemote {
        expected: RemoteInfo,
        url: String,
        push_url: Option<String>,
    },
    RenameRemote {
        expected: RemoteInfo,
        name: String,
    },
    DeleteRemote(RemoteInfo),
    SetUpstream {
        branch: String,
        upstream: Option<String>,
    },
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
#[path = "git/stash.rs"]
mod stash;
pub use stash::{StashAction, StashContext, StashPreview, StashSection};
#[path = "git/partial.rs"]
mod partial;
pub use partial::{FilePatch, PartialPatch, PatchSelection};
#[path = "git/diff_view.rs"]
mod diff_view;
pub use diff_view::{DiffDisplay, DiffPreferences, SplitRow, word_changes};
#[path = "git/recovery.rs"]
mod recovery;
pub use recovery::{CommitEdit, HeadState, ReflogEntry, ReflogPage, ReflogTarget};
#[path = "git/history.rs"]
mod history;
pub use history::{ResetContext, ResetMode};
#[path = "git/branches.rs"]
mod branches;
pub use branches::{BranchRename, DetachedCheckout};
#[path = "git/integration.rs"]
mod integration;
pub use integration::{MergeMode, MergePreview, PullPreview, PullStrategy};
#[path = "git/remote_refs.rs"]
mod remote_refs;
pub use remote_refs::{FetchPlan, RemoteRef, RemoteRefs, RemoteTagDelete};
#[path = "git/tags.rs"]
mod tags;
pub use tags::{TagDefaults, TagKind, TagPlan, TagSigner, TagSigningFormat, TagSpec, TagTarget};
#[path = "git/inspect.rs"]
mod inspect;
pub use inspect::{
    BlameLine, BlameView, Comparison, FileHistory, FileHistoryEntry, Revision, TreeFiles,
};
mod commit_policy;
mod external_editor;
pub use external_editor::ExternalEditor;
mod patch_file;
pub use patch_file::ApplyPatchPlan;
mod image_preview;
mod patch_batch;
pub use image_preview::{ImageData, ImageKind, ImagePair};
#[path = "git/conflict_blocks.rs"]
mod conflict_blocks;
#[path = "git/conflicts.rs"]
mod conflicts;
pub use conflict_blocks::{BlockChoice, ConflictBlock, choose_conflict_block, conflict_blocks};
#[path = "git/merge_tool.rs"]
mod merge_tool;
pub use conflicts::{
    ConflictContext, ConflictFile, ConflictOperation, ConflictResolution, ConflictSession,
    ConflictSide, has_conflict_markers,
};
pub use merge_tool::MergeTool;

/// Shared progress sink shown in the status bar during network operations.
/// `Arc<Mutex<...>>` because libgit2 callbacks fire from worker threads.
pub type ProgressSink = std::sync::Arc<std::sync::Mutex<dyn FnMut(&str) + Send>>;

#[path = "git/network.rs"]
mod network;
pub use network::{CancellationToken, Cancelled, NetworkControl, is_cancelled};

#[path = "git/remotes.rs"]
mod remotes;
pub use remotes::{PushPlan, PushSelection, RemoteInfo, RemoteState};

#[path = "git/rebase.rs"]
mod rebase;
pub use rebase::{RebaseAction, RebasePreview, RebaseStatus, RebaseStep, autosquash};
#[path = "git/workspaces.rs"]
mod workspaces;
pub use workspaces::{SubmoduleInfo, WorktreeInfo};
#[path = "git/lfs.rs"]
mod lfs;
pub use lfs::{LfsFile, LfsStatus};
#[path = "git/lfs_maintenance.rs"]
mod lfs_maintenance;
pub use lfs_maintenance::{LfsLock, LfsLockOwner, LfsLocks, LfsMaintenance};

impl Operation {
    pub fn is_cancellable(&self) -> bool {
        self.is_network() || matches!(self,Self::CreateTag(plan) if plan.kind()==TagKind::Signed)
    }
    pub fn is_network(&self) -> bool {
        matches!(
            self,
            Self::Fetch
                | Self::FetchFrom(_)
                | Self::DeleteRemoteTag(_)
                | Self::Pull
                | Self::PullWith { .. }
                | Self::Push
                | Self::PushTo(_)
                | Self::DeleteRemoteBranch(_)
                | Self::AddSubmodule { .. }
                | Self::UpdateSubmodules { .. }
                | Self::LfsFetch(_)
                | Self::LfsPush(_)
                | Self::PrepareWorktree(_)
                | Self::UpdateSubmoduleAt(_)
                | Self::LfsLock { .. }
                | Self::LfsUnlock { .. }
                | Self::LfsQuarantine(_)
                | Self::LfsRestoreQuarantine
                | Self::LfsPurgeQuarantine(_)
        )
    }
    pub fn label(&self) -> &'static str {
        match self {
            Self::ApplyPatch(_) => "Apply patch",
            Self::RecoverPatch => "Recover interrupted patch",
            Self::CommitEmpty(_) => "Create empty commit",
            Self::Fetch => "Fetch",
            Self::CreateTag(plan) => match plan.kind() {
                TagKind::Lightweight => "Create lightweight tag",
                TagKind::Annotated => "Create annotated tag",
                TagKind::Signed => "Create signed tag",
            },
            Self::FetchFrom(_) => "Fetch selected source",
            Self::DeleteRemoteTag(_) => "Delete remote tag",
            Self::Pull => "Pull",
            Self::PullWith { .. } => "Pull",
            Self::MergeWith { .. } => "Merge",
            Self::Push => "Push",
            Self::PushTo(_) => "Push",
            Self::DeleteRemoteBranch(_) => "Delete remote branch",
            Self::RenameBranch { .. } => "Rename branch",
            Self::CheckoutDetached(_) => "Checkout detached HEAD",
            Self::Rebase { .. } => "Rebase",
            Self::ContinueRebase => "Continue rebase",
            Self::AbortRebase => "Abort rebase",
            Self::RecoverRebaseStep { .. } => "Recover rebase step",
            Self::CreateWorktree { .. } => "Create worktree",
            Self::AddSubmodule { .. } | Self::UpdateSubmodules { .. } => "Update submodules",
            Self::LfsFetch(_) => "LFS fetch",
            Self::LfsPush(_) => "LFS push",
            Self::PrepareWorktree(_) => "Prepare worktree",
            Self::UpdateSubmoduleAt(_) => "Update nested submodule",
            Self::LfsLock { .. } => "LFS lock",
            Self::LfsUnlock { .. } => "LFS unlock",
            Self::LfsQuarantine(_) => "Prune LFS cache",
            Self::LfsRestoreQuarantine => "Restore LFS cache",
            Self::LfsPurgeQuarantine(_) => "Delete LFS quarantine",
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
    diff_lines_with_limit(diff, DIFF_LIMIT_LINES)
}
pub(super) fn diff_lines_with_limit(diff: &str, limit: usize) -> Vec<DiffLine> {
    let mut old = 0usize;
    let mut new = 0usize;
    let mut in_hunk = false;
    diff.lines()
        .take(limit)
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
