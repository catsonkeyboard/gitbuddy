mod conflicts;
mod detail;
mod graph;
mod history;
mod history_actions;
mod inspect;
mod management;
mod modals;
mod patches;
mod recovery;
mod remotes;
mod session;
mod sidebar;
mod tasks;
mod toolbar;
use gitbuddy::session::PatchKey;
use gitbuddy::{
    git::{
        self, Commit, CommitDetail, CommitFile, DiffLine, FileChange, Operation, Repository,
        Snapshot,
    },
    settings::Settings,
};
use gpui_kit::{
    component::{
        button::{Button, ButtonVariants},
        input::{Editor, EditorState, Input, InputEvent, InputState, Textarea, TextareaState},
        menu::DropdownMenu,
        *,
    },
    prelude::*,
    *,
};
use std::{
    collections::{BTreeSet, HashMap, HashSet},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
gpui_kit::actions!(
    gitbuddy,
    [
        OpenRepository,
        CloneRepository,
        InitRepository,
        EditIdentity,
        Refresh,
        CommitChanges,
        AmendCommit,
        UndoCommit,
        OpenReflog,
        OpenFileTools,
        CompareRevisions,
        OpenConflicts,
        OpenRebase,
        OpenWorktrees,
        OpenSubmodules,
        OpenLfs,
        OpenRemotes,
        OpenPushOptions,
        Quit,
        CloseModal
    ]
);
const BG: u32 = 0x202328;
const PANEL: u32 = 0x25292f;
const EDITOR: u32 = 0x2b3038;
const BORDER: u32 = 0x373d46;
const MUTED: u32 = 0x8c96a6;
const TEXT: u32 = 0xdce1e8;
const ACCENT: u32 = 0x86b7f3;
const HISTORY_PAGE_SIZE: usize = 300;
#[derive(Clone, Debug, Default, PartialEq, Eq)]
enum Selection {
    #[default]
    Work,
    File(FileChange, bool),
    Commit(String),
    Inspect,
    Conflicts,
}
#[derive(Clone)]
enum PatchSource {
    Work(FileChange, bool),
    Commit(String, CommitFile),
    Compare(Arc<git::Comparison>, CommitFile),
}
impl PatchSource {
    fn key(&self) -> PatchKey {
        match self {
            Self::Work(file, staged) => PatchKey::Work(file.path.clone(), *staged),
            Self::Commit(id, file) => PatchKey::Commit(id.clone(), file.path.clone()),
            Self::Compare(comparison, file) => PatchKey::Compare(
                comparison.base.id.clone(),
                comparison.target.id.clone(),
                file.path.clone(),
            ),
        }
    }
}
#[derive(Clone, Debug, Default)]
enum PatchState {
    #[default]
    Loading,
    Ready(Arc<git::FilePatch>, PatchLineSelection),
    Error(String),
}
#[derive(Clone, Debug, Default)]
struct PatchLineSelection {
    rows: BTreeSet<usize>,
    anchor: Option<usize>,
}
impl PatchLineSelection {
    fn select(&mut self, lines: &[DiffLine], row: usize, extend: bool, additive: bool) {
        if !lines.get(row).is_some_and(|l| matches!(l.kind, '+' | '-')) {
            return;
        }
        if extend {
            let anchor = self.anchor.unwrap_or(row);
            if !additive {
                self.rows.clear();
            }
            self.rows.extend(
                (anchor.min(row)..=anchor.max(row))
                    .filter(|&i| lines.get(i).is_some_and(|l| matches!(l.kind, '+' | '-'))),
            );
            self.anchor = Some(anchor);
        } else {
            if additive {
                if !self.rows.remove(&row) {
                    self.rows.insert(row);
                }
            } else {
                self.rows.clear();
                self.rows.insert(row);
            }
            self.anchor = Some(row);
        }
    }
}
#[derive(Clone)]
enum Modal {
    RebaseSetup,
    RebasePlan,
    RebaseMessage(usize),
    Worktrees,
    WorktreeCreate,
    Submodules,
    SubmoduleAdd,
    Lfs,
    LfsPattern(bool),
    Open,
    Init,
    Clone,
    Branch,
    BranchAt(String),
    ResetHistory(Option<Arc<git::ResetContext>>),
    CommitSelection,
    Tag,
    Remote,
    Remotes,
    EditRemote(git::RemoteInfo),
    RenameRemote(git::RemoteInfo),
    Upstream(String),
    PushSettings,
    Stash,
    Identity,
    BranchActions(String, bool),
    StashActions(String),
    CommitActions(String),
    TagActions(String),
    Confirm(String, Operation),
    History(HistoryKind),
    RecoveryBranch(git::ReflogTarget),
    FileTools,
    Compare,
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum HistoryKind {
    Amend,
    Undo,
    Reflog,
}
#[derive(Clone, Default)]
enum HistoryData {
    #[default]
    Loading,
    Edit(Arc<git::CommitEdit>),
    Reflog(Arc<git::ReflogPage>),
    Error(String),
}
struct Loaded {
    repo: Repository,
    snapshot: Snapshot,
    selection: Selection,
    diff: Vec<DiffLine>,
    commit_detail: Option<CommitDetail>,
    notice: String,
    error: bool,
    clear_message: bool,
    open_as_tab: bool,
    retry_modal: Option<Modal>,
}
impl Loaded {
    fn read(
        repo: Repository,
        selection: Selection,
        limit: usize,
        notice: String,
        error: bool,
        clear_message: bool,
        open_as_tab: bool,
    ) -> anyhow::Result<Self> {
        let snapshot = repo.snapshot(limit)?;
        let selection = match selection {
            Selection::File(file, staged) => snapshot
                .files
                .iter()
                .find(|f| f.path == file.path && if staged { f.staged() } else { f.unstaged() })
                .map(|f| Selection::File(f.clone(), staged))
                .unwrap_or(Selection::Work),
            other => other,
        };
        let raw = match &selection {
            Selection::File(file, staged) => repo.diff(file, *staged)?,
            _ => String::new(),
        };
        let diff = git::diff_preview(&raw);
        // Snapshot refreshes keep the current commit panel; selecting a commit loads
        // its metadata independently and does not rebuild the repository snapshot.
        let commit_detail = None;
        Ok(Self {
            repo,
            snapshot,
            selection,
            diff,
            commit_detail,
            notice,
            error,
            clear_message,
            open_as_tab,
            retry_modal: None,
        })
    }
}
/// Per-repository view state. One instance is active at a time; switching
/// tabs swaps this struct wholesale instead of copying fifteen fields.
#[derive(Clone)]
pub struct RepoTab {
    id: tasks::TabId,
    preparing: Option<PathBuf>,
    retry_preparation: Option<tasks::Preparation>,
    refresh_views: bool,
    task_finished: bool,
    history_retry: Option<(Modal, HistoryData, String)>,
    repo: Option<Repository>,
    snapshot: Snapshot,
    selection: Selection,
    diff: Vec<DiffLine>,
    commit_detail: Option<CommitDetail>,
    commit_cache: HashMap<String, CommitDetail>,
    commit_selection: history_actions::CommitSelection,
    expanded: HashSet<PatchKey>,
    patches: HashMap<PatchKey, PatchState>,
    show_commit_body: bool,
    inspection: inspect::InspectState,
    comparison_base: Option<String>,
    conflicts: conflicts::ConflictViewState,
    query: String,
    message: String,
    message_view: gitbuddy::session::EditorView,
    amend_draft: Option<gitbuddy::session::AmendDraft>,
    limit: usize,
    history_tab: usize,
    notice: String,
    error: bool,
    scroll: session::ViewScroll,
    restoring: Option<gitbuddy::session::Tab>,
    restored_lines: HashMap<PatchKey, gitbuddy::session::LineSelection>,
    restored_conflicts: HashMap<PathBuf, gitbuddy::session::ConflictDraft>,
}
impl RepoTab {
    fn ready_for_refresh(&self, busy: bool, modal_open: bool) -> bool {
        !busy && !modal_open && self.repo.is_some()
    }
}
impl Default for RepoTab {
    fn default() -> Self {
        Self {
            id: tasks::TabId::new(),
            preparing: None,
            retry_preparation: None,
            refresh_views: false,
            task_finished: false,
            history_retry: None,
            repo: None,
            snapshot: Snapshot::default(),
            selection: Selection::default(),
            diff: Vec::new(),
            commit_detail: None,
            commit_cache: HashMap::new(),
            commit_selection: history_actions::CommitSelection::default(),
            expanded: HashSet::new(),
            patches: HashMap::new(),
            show_commit_body: false,
            inspection: inspect::InspectState::default(),
            comparison_base: None,
            conflicts: conflicts::ConflictViewState::default(),
            query: String::new(),
            message: String::new(),
            message_view: gitbuddy::session::EditorView::default(),
            amend_draft: None,
            limit: HISTORY_PAGE_SIZE,
            history_tab: 0,
            notice: String::new(),
            error: false,
            scroll: session::ViewScroll::default(),
            restoring: None,
            restored_lines: HashMap::new(),
            restored_conflicts: HashMap::new(),
        }
    }
}
pub struct GitBuddy {
    /// Inactive tabs. The slot at `active_index` is `None` because its data
    /// lives in `active`; switching tabs is a `mem::take`, not a field copy.
    tabs: Vec<Option<RepoTab>>,
    active: RepoTab,
    active_index: Option<usize>,
    patch_generation: u64,
    selection_generation: u64,
    restore_message: Option<String>,
    restore_search: Option<String>,
    message: Entity<TextareaState>,
    search: Entity<InputState>,
    form_a: Entity<InputState>,
    form_b: Entity<InputState>,
    clear_message: bool,
    modal: Option<Modal>,
    modal_error: Option<String>,
    history_data: HistoryData,
    history_generation: u64,
    reflog_limit: usize,
    reflog_selected: Option<git::ReflogTarget>,
    amend_message: Entity<TextareaState>,
    restore_amend_message: Option<String>,
    tool_files: Option<Result<Arc<git::TreeFiles>, String>>,
    tool_generation: u64,
    management_data: management::Data,
    management_generation: u64,
    remote_data: remotes::Data,
    remote_generation: u64,
    push_choice: git::PushSelection,
    restore_push_target: Option<String>,
    push_preparing: bool,
    conflict_generation: u64,
    conflict_editor: Entity<EditorState>,
    restore_conflict: Option<String>,
    tasks: tasks::RepositoryTasks,
    settings: Settings,
    session_store: gitbuddy::session::Store,
    session_revision: u64,
    session_warning: Option<String>,
    tab_scroll: session::ViewScroll,
    focus: FocusHandle,
    _subscriptions: Vec<Subscription>,
}
impl std::ops::Deref for GitBuddy {
    type Target = RepoTab;
    fn deref(&self) -> &RepoTab {
        &self.active
    }
}
impl std::ops::DerefMut for GitBuddy {
    fn deref_mut(&mut self) -> &mut RepoTab {
        &mut self.active
    }
}
impl GitBuddy {
    pub fn new(path: Option<PathBuf>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let message = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder("Summary of your changes…\n\nOptional description")
        });
        let search = cx.new(|cx| {
            InputState::new(window, cx).placeholder("Search commits · message, author, hash")
        });
        let form_a = cx.new(|cx| InputState::new(window, cx));
        let form_b = cx.new(|cx| InputState::new(window, cx));
        let amend_message =
            cx.new(|cx| TextareaState::new(window, cx).placeholder("Commit message"));
        let conflict_editor = cx.new(|cx| EditorState::new(window, cx).line_number(true));
        let subscriptions = vec![
            cx.on_app_quit(|this, cx| {
                this.tasks.shutdown_and_wait();
                this.flush_session(cx);
                async {}
            }),
            cx.on_release(|this, cx| {
                this.tasks.shutdown_and_wait();
                this.flush_session(cx);
            }),
            cx.subscribe_in(
                &conflict_editor,
                window,
                |this, _, event: &InputEvent, _, cx| {
                    if matches!(event, InputEvent::Change)
                        && this.restore_conflict.is_none()
                        && matches!(this.selection, Selection::Conflicts)
                    {
                        let text = this.conflict_editor.read(cx).value().to_string();
                        if let Some(path) = this.active.conflicts.selected.clone()
                            && let Some(draft) = this.active.conflicts.drafts.get_mut(&path)
                            && draft.text != text
                        {
                            draft.text = text;
                            draft.choice = conflicts::ResultChoice::Edited;
                            draft.dirty = true;
                        }
                        cx.notify();
                    }
                },
            ),
            cx.subscribe_in(&search, window, |this, _, event: &InputEvent, _, cx| {
                if matches!(event, InputEvent::Change) {
                    this.active.query = this.search.read(cx).value().to_string();
                    cx.notify();
                }
            }),
            cx.subscribe_in(
                &amend_message,
                window,
                |this, _, event: &InputEvent, _, cx| {
                    if matches!(event, InputEvent::Change) {
                        this.active.amend_draft = this.current_amend(cx);
                        cx.notify();
                    }
                },
            ),
            cx.subscribe_in(&form_a, window, |_, _, event: &InputEvent, _, cx| {
                if matches!(event, InputEvent::Change) {
                    cx.notify();
                }
            }),
        ];
        let settings = Settings::load();
        let session_store =
            gitbuddy::session::Store::new(gitbuddy::settings::config_dir().join("session.json"));
        let saved = session_store.load();
        let load_error = saved.as_ref().err().map(|error| format!("{error:#}"));
        let initial = if matches!(saved, Ok(Some(_))) {
            path
        } else {
            path.or_else(|| settings.recent.first().cloned())
        };
        let focus = cx.focus_handle();
        focus.focus(window, cx);
        let mut this = Self {
            tabs: Vec::new(),
            active: RepoTab {
                notice: "Ready".into(),
                ..RepoTab::default()
            },
            active_index: None,
            patch_generation: 0,
            selection_generation: 0,
            restore_message: None,
            restore_search: None,
            message,
            search,
            form_a,
            form_b,
            clear_message: false,
            modal: None,
            modal_error: None,
            history_data: HistoryData::default(),
            history_generation: 0,
            reflog_limit: 100,
            reflog_selected: None,
            amend_message,
            restore_amend_message: None,
            tool_files: None,
            tool_generation: 0,
            management_data: management::Data::Loading,
            management_generation: 0,
            remote_data: remotes::Data::Loading,
            remote_generation: 0,
            push_choice: git::PushSelection::default(),
            restore_push_target: None,
            push_preparing: false,
            conflict_generation: 0,
            conflict_editor,
            restore_conflict: None,
            tasks: tasks::RepositoryTasks::default(),
            settings,
            session_store,
            session_revision: 0,
            session_warning: load_error.clone(),
            tab_scroll: session::ViewScroll::default(),
            focus,
            _subscriptions: subscriptions,
        };
        if let Ok(Some(saved)) = saved {
            this.restore_session(saved, cx);
        }
        if let Some(path) = initial {
            this.open(path, cx);
        }
        if let Some(error) = load_error {
            eprintln!("Cannot restore session: {error}");
            this.active.notice = error;
            this.active.error = true;
        }
        this.start_session_autosave(cx);
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_secs(10))
                    .await;
                if this
                    .update(cx, |this, cx| {
                        if this
                            .active
                            .ready_for_refresh(this.busy(), this.modal.is_some())
                        {
                            this.auto_refresh(cx);
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        this
    }
    fn save_active_tab(&mut self, cx: &Context<Self>) {
        let Some(index) = self.active_index else {
            return;
        };
        self.active.message = self.current_message(cx);
        self.active.query = self
            .restore_search
            .clone()
            .unwrap_or_else(|| self.search.read(cx).value().to_string());
        self.sync_editor_views(cx);
        self.tabs[index] = Some(std::mem::take(&mut self.active));
    }
    fn activate_tab(&mut self, index: usize, cx: &mut Context<Self>) {
        if self.active_index == Some(index) || index >= self.tabs.len() {
            return;
        }
        self.save_active_tab(cx);
        let Some(tab) = self.tabs[index].take() else {
            return;
        };
        self.active = tab;
        self.active.task_finished = false;
        self.active_index = Some(index);
        self.active
            .patches
            .retain(|_, state| !matches!(state, PatchState::Loading));
        self.restore_search = Some(self.active.query.clone());
        self.restore_message = Some(self.active.message.clone());
        self.clear_message = false;
        self.modal = None;
        self.modal_error = None;
        self.patch_generation += 1;
        self.selection_generation += 1;
        self.conflict_generation += 1;
        self.active.refresh_views = true;
        self.refresh_task_views(cx);
        if let Some((modal, history, message)) = self.active.history_retry.take() {
            self.modal = Some(modal);
            self.history_data = history;
            self.restore_amend_message = Some(message);
        }
        cx.notify();
    }
    fn close_tab(&mut self, index: usize, cx: &mut Context<Self>) {
        if index >= self.tabs.len() {
            return;
        }
        let id = if self.active_index == Some(index) {
            self.active.id
        } else if let Some(tab) = self.tabs[index].as_ref() {
            tab.id
        } else {
            return;
        };
        if self.tasks.get(id).is_some() {
            return;
        }
        let was_active = self.active_index == Some(index);
        self.tabs.remove(index);
        if was_active {
            self.active = RepoTab::default();
            self.active_index = None;
            self.patch_generation += 1;
            self.selection_generation += 1;
            if self.tabs.is_empty() {
                self.restore_message = Some(String::new());
                self.restore_search = Some(String::new());
                self.active.notice = "Ready".into();
                cx.notify();
            } else {
                self.activate_tab(index.min(self.tabs.len() - 1), cx);
            }
        } else if let Some(active) = self.active_index
            && index < active
        {
            self.active_index = Some(active - 1);
        }
        cx.notify();
    }
    /// Patch sources for every currently visible work file (staged and
    /// unstaged variants), or for the selected commit's files.
    fn work_and_commit_sources(&self) -> Vec<PatchSource> {
        match &self.active.selection {
            Selection::Conflicts => Vec::new(),
            Selection::Inspect => self.inspection_sources(),
            Selection::Commit(id) => {
                self.active
                    .commit_detail
                    .as_ref()
                    .map_or_else(Vec::new, |detail| {
                        detail
                            .files
                            .iter()
                            .cloned()
                            .map(|file| PatchSource::Commit(id.clone(), file))
                            .collect()
                    })
            }
            _ => self
                .active
                .snapshot
                .files
                .iter()
                .flat_map(|file| {
                    [false, true]
                        .into_iter()
                        .filter(|staged| {
                            if *staged {
                                file.staged()
                            } else {
                                file.unstaged()
                            }
                        })
                        .map(|staged| PatchSource::Work(file.clone(), staged))
                        .collect::<Vec<_>>()
                })
                .collect(),
        }
    }
    fn refresh_with_notice(&mut self, notice: String, cx: &mut Context<Self>) {
        let Some(repo) = self.active.repo.clone() else {
            return;
        };
        let selection = self.active.selection.clone();
        let limit = self.active.limit;
        self.dispatch(
            "",
            false,
            move |_progress| Loaded::read(repo, selection, limit, notice, false, false, false),
            cx,
        );
    }
    /// Timer entry point: compare a cheap fingerprint before paying for a
    /// full snapshot rebuild (revwalk + reference + status enumeration).
    fn auto_refresh(&mut self, cx: &mut Context<Self>) {
        let Some(repo) = self.active.repo.clone() else {
            return;
        };
        let expected = self.active.snapshot.fingerprint;
        let tab_id = self.active.id;
        let task = cx
            .background_executor()
            .spawn(async move { repo.fingerprint().map(|fp| fp != expected).unwrap_or(true) });
        cx.spawn(async move |this, cx| {
            if task.await {
                let _ = this.update(cx, |this, cx| {
                    if this.active.id == tab_id
                        && this
                            .active
                            .ready_for_refresh(this.busy(), this.modal.is_some())
                    {
                        this.refresh_with_notice(String::new(), cx);
                    }
                });
            }
        })
        .detach();
    }
    fn select(&mut self, selection: Selection, cx: &mut Context<Self>) {
        if self.busy() {
            return;
        }
        if let Selection::File(file, staged) = selection {
            if self.active.commit_detail.take().is_some()
                || matches!(self.active.selection, Selection::Inspect)
            {
                self.active.expanded.clear();
                self.active.patches.clear();
                self.patch_generation += 1;
            }
            self.active.selection = Selection::Work;
            self.selection_generation += 1;
            let source = PatchSource::Work(file, staged);
            self.active.expanded.insert(source.key());
            self.load_patch(source, cx);
            cx.notify();
            return;
        }
        match selection {
            Selection::Work => {
                if self.active.commit_detail.take().is_some()
                    || matches!(
                        self.active.selection,
                        Selection::Commit(_) | Selection::Inspect
                    )
                {
                    self.active.expanded.clear();
                    self.active.patches.clear();
                    self.patch_generation += 1;
                }
                self.active.selection = Selection::Work;
                self.selection_generation += 1;
                cx.notify();
            }
            Selection::Commit(id) => {
                if matches!(&self.active.selection, Selection::Commit(current) if current == &id) {
                    return;
                }
                if self.active.repo.is_none() {
                    return;
                }
                self.active.selection = Selection::Commit(id.clone());
                self.active.commit_detail = self.active.commit_cache.get(&id).cloned();
                self.active.expanded.clear();
                self.active.patches.clear();
                self.patch_generation += 1;
                self.selection_generation += 1;
                self.active.show_commit_body = false;
                cx.notify();
                if self.active.commit_detail.is_some() {
                    return;
                }
                self.request_commit_detail(id, cx);
            }
            Selection::File(..) => unreachable!(),
            Selection::Inspect => {
                self.active.selection = Selection::Inspect;
                self.active.commit_detail = None;
                self.active.expanded.clear();
                self.active.patches.clear();
                self.patch_generation += 1;
                self.selection_generation += 1;
                if let inspect::InspectState::Loading(request) = &self.active.inspection {
                    self.begin_inspection(request.clone(), cx);
                }
                cx.notify();
            }
            Selection::Conflicts => self.open_conflicts(None, cx),
        }
    }
    fn request_commit_detail(&mut self, id: String, cx: &mut Context<Self>) {
        let Some(repo) = self.active.repo.clone() else {
            return;
        };
        let generation = self.selection_generation;
        let task = cx
            .background_executor()
            .spawn(async move { repo.commit_detail(&id) });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                if this.selection_generation != generation {
                    return;
                }
                match result {
                    Ok(detail) => {
                        this.active
                            .commit_cache
                            .insert(detail.id.clone(), detail.clone());
                        this.active.commit_detail = Some(detail);
                        this.resume_expanded(cx);
                    }
                    Err(err) => {
                        this.active.notice = format!("{err:#}");
                        this.active.error = true;
                        // Saved objects may have been pruned while the app was closed.
                        this.active.selection = Selection::Work;
                        this.active.expanded.clear();
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }
    fn perform(&mut self, operation: Operation, cx: &mut Context<Self>) {
        if self.busy() {
            return;
        }
        let Some(repo) = self.active.repo.clone() else {
            return;
        };
        let clear_message = matches!(operation, Operation::Commit(_) | Operation::CommitRebase(_));
        let history_edit = matches!(
            operation,
            Operation::Amend { .. }
                | Operation::UndoLast(_)
                | Operation::RestoreReflog { .. }
                | Operation::Rebase { .. }
                | Operation::ContinueRebase
                | Operation::AbortRebase
                | Operation::Reset { .. }
        );
        let retry_modal = if matches!(operation, Operation::Amend { .. }) {
            self.modal.clone()
        } else {
            None
        };
        let selection = if clear_message || history_edit {
            Selection::Work
        } else {
            self.active.selection.clone()
        };
        let limit = self.active.limit;
        self.modal = None;
        self.dispatch(
            operation.label(),
            operation.is_network(),
            move |control| {
                let (notice, error) = match repo.execute_with_control(operation, control) {
                    Ok(out) => (
                        if out.is_empty() {
                            "Operation completed".into()
                        } else {
                            out
                        },
                        false,
                    ),
                    Err(err) => (format!("{err:#}"), !git::is_cancelled(&err)),
                };
                let mut loaded = Loaded::read(
                    repo,
                    selection,
                    limit,
                    notice,
                    error,
                    clear_message && !error,
                    false,
                )?;
                if error {
                    loaded.retry_modal = retry_modal;
                }
                Ok(loaded)
            },
            cx,
        );
    }
    fn show_modal(&mut self, modal: Modal, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy() && !matches!(modal, Modal::Open | Modal::Init | Modal::Clone) {
            return;
        }
        self.form_a.update(cx, |s, cx| s.set_value("", window, cx));
        self.form_b.update(cx, |s, cx| s.set_value("", window, cx));
        self.modal = Some(modal);
        self.modal_error = None;
        self.form_a.read(cx).focus_handle(cx).focus(window, cx);
        cx.notify();
    }
    fn commit(&mut self, cx: &mut Context<Self>) {
        if self.busy() || self.modal.is_some() || !self.snapshot.can_commit() {
            return;
        }
        let message = self.message.read(cx).value().to_string();
        self.perform(
            if self.snapshot.rebase_editing {
                Operation::CommitRebase(message)
            } else {
                Operation::Commit(message)
            },
            cx,
        );
    }
    fn button(&self, id: impl Into<ElementId>, label: impl Into<SharedString>) -> Button {
        Button::new(id)
            .label(label)
            .small()
            .h(px(24.))
            .text_size(px(11.))
            .disabled(self.busy())
    }
    fn sidebar_button(&self, id: impl Into<ElementId>, label: impl Into<SharedString>) -> Button {
        let label = label.into();
        Button::new(id)
            .accessibility_label(label.clone())
            .small()
            .h(px(24.))
            .w_full()
            .text_size(px(11.))
            .disabled(self.busy())
            .child(
                h_flex()
                    .w_full()
                    .min_w_0()
                    .justify_start()
                    .child(div().min_w_0().truncate().child(label)),
            )
    }
    fn browse(&mut self, cx: &mut Context<Self>) {
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Open repository".into()),
        });
        cx.spawn(async move |this, cx| match receiver.await {
            Ok(Ok(Some(paths))) => {
                if let Some(path) = paths.into_iter().next() {
                    let _ = this.update(cx, |this, cx| {
                        this.modal = None;
                        this.open(path, cx);
                    });
                }
            }
            Ok(Err(err)) => {
                let _ = this.update(cx, |this, cx| {
                    this.active.notice = err.to_string();
                    this.active.error = true;
                    cx.notify();
                });
            }
            _ => {}
        })
        .detach();
    }
}
impl Render for GitBuddy {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.restore_scroll_on_next_frame(window, cx);
        if let Some(value) = self.restore_push_target.take()
            && matches!(self.modal, Some(Modal::PushSettings))
        {
            self.form_a
                .update(cx, |s, cx| s.set_value(value, window, cx));
        }
        if let Some(value) = self.restore_search.take() {
            self.search
                .update(cx, |state, cx| state.set_value(value, window, cx));
        }
        if let Some(value) = self.restore_message.take() {
            let view = self.active.message_view.clone();
            self.message.update(cx, |state, cx| {
                state.set_value(value, window, cx);
                state.set_selected_range(view.start..view.end, cx);
                state.set_scroll_offset(point(px(view.scroll.x), px(view.scroll.y)), cx);
            });
        }
        if let Some(value) = self.restore_amend_message.take() {
            let view = self
                .active
                .amend_draft
                .as_ref()
                .map(|draft| draft.editor.clone())
                .unwrap_or_default();
            self.amend_message.update(cx, |state, cx| {
                state.set_value(value, window, cx);
                state.set_selected_range(view.start..view.end, cx);
                state.set_scroll_offset(point(px(view.scroll.x), px(view.scroll.y)), cx);
            });
            self.amend_message
                .read(cx)
                .focus_handle(cx)
                .focus(window, cx);
        }
        if let Some(value) = self.restore_conflict.take() {
            let view = self
                .active
                .conflicts
                .selected
                .as_ref()
                .and_then(|p| self.active.conflicts.drafts.get(p))
                .map(|draft| draft.editor.clone())
                .unwrap_or_default();
            self.conflict_editor.update(cx, |state, cx| {
                state.set_value(value, window, cx);
                state.set_selected_range(view.start..view.end, cx);
                state.set_scroll_offset(point(px(view.scroll.x), px(view.scroll.y)), cx);
            });
        }
        if self.clear_message {
            self.clear_message = false;
            self.message
                .update(cx, |state, cx| state.set_value("", window, cx));
        }
        v_flex()
            .relative()
            .size_full()
            .font_family(".SystemUIFont")
            .text_sm()
            .text_color(rgb(TEXT))
            .bg(rgb(BG))
            .track_focus(&self.focus)
            .on_action(
                cx.listener(|this, _: &OpenRepository, w, cx| this.show_modal(Modal::Open, w, cx)),
            )
            .on_action(cx.listener(|this, _: &Refresh, _, cx| {
                this.refresh_with_notice("Refreshed".into(), cx)
            }))
            .on_action(cx.listener(|this, _: &CommitChanges, _, cx| this.commit(cx)))
            .on_action(cx.listener(|this, _: &AmendCommit, w, cx| {
                this.begin_history(HistoryKind::Amend, w, cx)
            }))
            .on_action(cx.listener(|this, _: &UndoCommit, w, cx| {
                this.begin_history(HistoryKind::Undo, w, cx)
            }))
            .on_action(cx.listener(|this, _: &OpenReflog, w, cx| {
                this.begin_history(HistoryKind::Reflog, w, cx)
            }))
            .on_action(cx.listener(|this, _: &OpenFileTools, w, cx| this.open_file_tools(w, cx)))
            .on_action(cx.listener(|this, _: &OpenConflicts, _, cx| this.open_conflicts(None, cx)))
            .on_action(cx.listener(|this, _: &OpenRebase, w, cx| {
                this.open_management(Modal::RebaseSetup, w, cx)
            }))
            .on_action(cx.listener(|this, _: &OpenWorktrees, w, cx| {
                this.open_management(Modal::Worktrees, w, cx)
            }))
            .on_action(cx.listener(|this, _: &OpenSubmodules, w, cx| {
                this.open_management(Modal::Submodules, w, cx)
            }))
            .on_action(
                cx.listener(|this, _: &OpenLfs, w, cx| this.open_management(Modal::Lfs, w, cx)),
            )
            .on_action(cx.listener(|this, _: &OpenRemotes, w, cx| {
                this.open_remote_page(Modal::Remotes, w, cx)
            }))
            .on_action(cx.listener(|this, _: &OpenPushOptions, w, cx| {
                this.open_remote_page(Modal::PushSettings, w, cx)
            }))
            .on_action(
                cx.listener(|this, _: &CompareRevisions, w, cx| {
                    this.open_compare(None, None, w, cx)
                }),
            )
            .on_action(cx.listener(|this, _: &CloneRepository, window, cx| {
                this.show_modal(Modal::Clone, window, cx)
            }))
            .on_action(cx.listener(|this, _: &InitRepository, window, cx| {
                this.show_modal(Modal::Init, window, cx)
            }))
            .on_action(cx.listener(|this, _: &EditIdentity, window, cx| {
                this.show_modal(Modal::Identity, window, cx)
            }))
            .on_action(cx.listener(|this, _: &CloseModal, window, cx| {
                if this.modal.take().is_some() {
                    this.focus.focus(window, cx);
                    cx.notify();
                }
            }))
            .child(self.toolbar(cx))
            .child(self.repository_tabs(cx))
            .child(if self.active.repo.is_some() {
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .items_start()
                    .child(self.sidebar(cx))
                    .child(self.history(cx))
                    .child(self.detail(cx))
                    .into_any_element()
            } else if self.active.preparing.is_some() {
                self.preparation_view(cx)
            } else {
                self.welcome(cx).into_any_element()
            })
            .child(
                h_flex()
                    .min_h(px(30.))
                    .px_3()
                    .py_1()
                    .gap_3()
                    .flex_shrink_0()
                    .bg(rgb(if self.active.error { 0x4b3038 } else { BG }))
                    .border_t_1()
                    .border_color(rgb(BORDER))
                    .text_xs()
                    .child(
                        div()
                            .text_color(rgb(if self.active.error { 0xf0a4aa } else { ACCENT }))
                            .child(if self.busy() {
                                "◌"
                            } else if self.active.error {
                                "!"
                            } else {
                                "●"
                            }),
                    )
                    .child(
                        div().flex_1().overflow_hidden().child(
                            self.active
                                .notice
                                .lines()
                                .take(3)
                                .collect::<Vec<_>>()
                                .join("  "),
                        ),
                    )
                    .when(self.active.error, |row| {
                        row.child(self.button("copy-error", "Copy error").ghost().on_click(
                            cx.listener(|this, _, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(
                                    this.active.notice.clone(),
                                ))
                            }),
                        ))
                    })
                    .when_some(
                        self.tasks
                            .get(self.active.id)
                            .and_then(|t| t.cancellation.as_ref()),
                        |row, token| {
                            row.child(
                                self.button(
                                    "cancel-network",
                                    if token.is_requested() {
                                        "Cancelling…"
                                    } else if token.is_finishing() {
                                        "Finishing…"
                                    } else {
                                        "Cancel"
                                    },
                                )
                                .ghost()
                                .disabled(!token.can_cancel())
                                .on_click(cx.listener(|this, _, _, cx| this.cancel_task(cx))),
                            )
                        },
                    )
                    .child(
                        div()
                            .text_color(rgb(MUTED))
                            .child("GitBuddy  ·  Rust + GPUI"),
                    )
                    .when_some(self.session_warning.clone(), |row, warning| {
                        row.child(
                            div()
                                .max_w(px(420.))
                                .truncate()
                                .text_color(rgb(0xf0a4aa))
                                .child(format!("Session: {warning}")),
                        )
                    }),
            )
            .when_some(self.modal.clone(), |root, modal| {
                root.child(self.modal_view(modal, cx))
            })
    }
}
fn section(label: &'static str) -> impl IntoElement {
    div()
        .py_1()
        .text_size(px(10.))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(rgb(MUTED))
        .child(label)
}
fn expand_path(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    let path = PathBuf::from(path);
    if path.is_absolute() {
        path
    } else {
        std::env::current_dir().unwrap_or_default().join(path)
    }
}

#[cfg(test)]
mod tests {
    use super::{Loaded, PatchLineSelection, RepoTab, Selection};
    use gitbuddy::git::{Operation, Repository};

    #[test]
    fn line_selection_supports_ranges_toggles_and_ignores_context() {
        let lines = gitbuddy::git::diff_lines("@@ -1,4 +1,4 @@\n-a\n+b\n context\n-c\n+d\n end\n");
        let mut selection = PatchLineSelection::default();
        selection.select(&lines, 1, false, false);
        selection.select(&lines, 5, true, false);
        assert_eq!(
            selection.rows.iter().copied().collect::<Vec<_>>(),
            [1, 2, 4, 5]
        );
        selection.select(&lines, 2, true, false);
        assert_eq!(selection.rows.iter().copied().collect::<Vec<_>>(), [1, 2]);
        selection.select(&lines, 4, false, true);
        selection.select(&lines, 1, false, true);
        assert_eq!(selection.rows.iter().copied().collect::<Vec<_>>(), [2, 4]);
        selection.select(&lines, 3, false, false);
        selection.select(&lines, usize::MAX, false, false);
        assert_eq!(selection.rows.iter().copied().collect::<Vec<_>>(), [2, 4]);
    }

    #[test]
    fn new_tab_keeps_history_when_refreshed_after_an_operation() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        repo.execute(Operation::SetIdentity(
            "History Test".into(),
            "test@example.invalid".into(),
        ))
        .unwrap();
        std::fs::write(repo.root.join("file"), "base\n").unwrap();
        repo.execute(Operation::StageAll).unwrap();
        repo.execute(Operation::Commit("base".into())).unwrap();
        let mut tab = RepoTab {
            repo: Some(repo.clone()),
            snapshot: repo.snapshot(300).unwrap(),
            ..RepoTab::default()
        };
        let refresh = |repo, limit| {
            Loaded::read(
                repo,
                Selection::Work,
                limit,
                String::new(),
                false,
                false,
                false,
            )
            .unwrap()
        };
        let refreshed = refresh(repo.clone(), tab.limit);
        assert_eq!(refreshed.snapshot.commits.len(), 1);
        assert_eq!(refreshed.snapshot.commits[0].id, tab.snapshot.commits[0].id);

        // A tab saved via mem::take must keep its history limit too.
        tab = std::mem::take(&mut tab);
        std::fs::write(repo.root.join("file"), "next\n").unwrap();
        repo.execute(Operation::StageAll).unwrap();
        repo.execute(Operation::Commit("next".into())).unwrap();
        let refreshed = refresh(tab.repo.unwrap(), tab.limit);
        assert_eq!(refreshed.snapshot.commits.len(), 2);
        assert_eq!(refreshed.snapshot.commits[0].subject, "next");
    }
}
