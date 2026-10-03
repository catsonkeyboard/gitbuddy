mod detail;
mod graph;
mod history;
mod inspect;
mod modals;
mod patches;
mod recovery;
mod sidebar;
mod toolbar;
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
        input::{Input, InputEvent, InputState, Textarea, TextareaState},
        menu::DropdownMenu,
        *,
    },
    prelude::*,
    *,
};
use std::{
    collections::{BTreeSet, HashMap, HashSet},
    path::PathBuf,
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
#[derive(Clone, Debug, Default)]
enum Selection {
    #[default]
    Work,
    File(FileChange, bool),
    Commit(String),
    Inspect,
}
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum PatchKey {
    Work(PathBuf, bool),
    Commit(String, PathBuf),
    Compare(String, String, PathBuf),
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
    Open,
    Init,
    Clone,
    Branch,
    Tag,
    Remote,
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
#[derive(Default)]
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
    repo: Option<Repository>,
    snapshot: Snapshot,
    selection: Selection,
    diff: Vec<DiffLine>,
    commit_detail: Option<CommitDetail>,
    commit_cache: HashMap<String, CommitDetail>,
    expanded: HashSet<PatchKey>,
    patches: HashMap<PatchKey, PatchState>,
    show_commit_body: bool,
    inspection: inspect::InspectState,
    comparison_base: Option<String>,
    query: String,
    message: String,
    limit: usize,
    history_tab: usize,
    notice: String,
    error: bool,
}
impl Default for RepoTab {
    fn default() -> Self {
        Self {
            repo: None,
            snapshot: Snapshot::default(),
            selection: Selection::default(),
            diff: Vec::new(),
            commit_detail: None,
            commit_cache: HashMap::new(),
            expanded: HashSet::new(),
            patches: HashMap::new(),
            show_commit_body: false,
            inspection: inspect::InspectState::default(),
            comparison_base: None,
            query: String::new(),
            message: String::new(),
            limit: HISTORY_PAGE_SIZE,
            history_tab: 0,
            notice: String::new(),
            error: false,
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
    history_data: HistoryData,
    history_generation: u64,
    reflog_limit: usize,
    reflog_selected: Option<git::ReflogTarget>,
    amend_message: Entity<TextareaState>,
    restore_amend_message: Option<String>,
    tool_files: Option<Result<Arc<git::TreeFiles>, String>>,
    tool_generation: u64,
    busy: bool,
    settings: Settings,
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
        let subscriptions = vec![
            cx.subscribe_in(&search, window, |this, _, event: &InputEvent, _, cx| {
                if matches!(event, InputEvent::Change) {
                    this.active.query = this.search.read(cx).value().to_string().to_lowercase();
                    cx.notify();
                }
            }),
            cx.subscribe_in(&amend_message, window, |_, _, event: &InputEvent, _, cx| {
                if matches!(event, InputEvent::Change) {
                    cx.notify();
                }
            }),
            cx.subscribe_in(&form_a, window, |_, _, event: &InputEvent, _, cx| {
                if matches!(event, InputEvent::Change) {
                    cx.notify();
                }
            }),
        ];
        let settings = Settings::load();
        let initial = path.or_else(|| settings.recent.first().cloned());
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
            history_data: HistoryData::default(),
            history_generation: 0,
            reflog_limit: 100,
            reflog_selected: None,
            amend_message,
            restore_amend_message: None,
            tool_files: None,
            tool_generation: 0,
            busy: false,
            settings,
            focus,
            _subscriptions: subscriptions,
        };
        if let Some(path) = initial {
            this.open(path, cx);
        }
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_secs(10))
                    .await;
                if this
                    .update(cx, |this, cx| {
                        if !this.busy
                            && this.modal.is_none()
                            && this.active.repo.is_some()
                            && !this.active.error
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
        self.active.message = if self.clear_message {
            String::new()
        } else {
            self.message.read(cx).value().to_string()
        };
        self.tabs[index] = Some(std::mem::take(&mut self.active));
    }
    fn activate_tab(&mut self, index: usize, cx: &mut Context<Self>) {
        if self.active_index == Some(index) || self.busy {
            return;
        }
        self.save_active_tab(cx);
        let Some(tab) = self.tabs[index].take() else {
            return;
        };
        self.active = tab;
        self.active_index = Some(index);
        self.active
            .patches
            .retain(|_, state| !matches!(state, PatchState::Loading));
        self.restore_search = Some(self.active.query.clone());
        self.restore_message = Some(self.active.message.clone());
        self.clear_message = false;
        self.modal = None;
        self.patch_generation += 1;
        self.selection_generation += 1;
        if matches!(self.active.selection, Selection::Inspect)
            && let inspect::InspectState::Loading(request) = &self.active.inspection
        {
            self.begin_inspection(request.clone(), cx);
        }
        let sources: Vec<_> = self.work_and_commit_sources();
        for source in sources {
            if self.active.expanded.contains(&source.key())
                && !self.active.patches.contains_key(&source.key())
            {
                self.load_patch(source, cx);
            }
        }
        if self.active.commit_detail.is_none()
            && let Selection::Commit(id) = &self.active.selection
        {
            let id = id.clone();
            self.request_commit_detail(id, cx);
        }
        cx.notify();
    }
    fn close_tab(&mut self, index: usize, cx: &mut Context<Self>) {
        if self.busy || index >= self.tabs.len() {
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
    fn apply_opened_tab(&mut self, loaded: Loaded, cx: &mut Context<Self>) {
        let root = loaded.repo.root.clone();
        if let Err(err) = self.settings.remember(root.clone()) {
            eprintln!("Cannot save recent repositories: {err}");
        }
        if self
            .active
            .repo
            .as_ref()
            .is_some_and(|repo| repo.root == root)
        {
            self.active.snapshot = loaded.snapshot;
            self.active.notice = loaded.notice;
            self.active.error = loaded.error;
            cx.notify();
            return;
        }
        if let Some(index) = self.tabs.iter().position(|tab| {
            tab.as_ref()
                .is_some_and(|t| t.repo.as_ref().is_some_and(|repo| repo.root == root))
        }) {
            if let Some(tab) = self.tabs[index].as_mut() {
                tab.snapshot = loaded.snapshot;
                tab.notice = loaded.notice;
                tab.error = loaded.error;
            }
            self.activate_tab(index, cx);
            return;
        }
        let index = self.tabs.len();
        self.tabs.push(Some(RepoTab {
            repo: Some(loaded.repo),
            snapshot: loaded.snapshot,
            notice: loaded.notice,
            error: loaded.error,
            ..RepoTab::default()
        }));
        self.activate_tab(index, cx);
    }
    fn dispatch(
        &mut self,
        label: &str,
        work: impl FnOnce(Option<git::ProgressSink>) -> anyhow::Result<Loaded> + Send + 'static,
        cx: &mut Context<Self>,
    ) {
        if self.busy {
            return;
        }
        self.busy = true;
        self.active.error = false;
        if !label.is_empty() {
            self.active.notice = label.into();
        }
        cx.notify();
        let requested_selection = self.selection_generation;
        // Progress flows from libgit2 callbacks (worker threads) into the
        // status bar. The channel closes when the operation finishes and
        // drops the sender, which also ends the consumer loop below.
        let (progress_tx, progress_rx) = async_channel::bounded::<String>(64);
        let sink: git::ProgressSink =
            std::sync::Arc::new(std::sync::Mutex::new(move |text: &str| {
                let _ = progress_tx.try_send(text.to_string());
            }));
        let task = cx
            .background_executor()
            .spawn(async move { work(Some(sink)) });
        cx.spawn(async move |this, cx| {
            while let Ok(text) = progress_rx.recv().await {
                let _ = this.update(cx, |this, cx| {
                    if this.busy {
                        this.active.notice = text;
                        cx.notify();
                    }
                });
            }
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                this.busy = false;
                match result {
                    Ok(mut loaded) => {
                        if loaded.open_as_tab {
                            this.apply_opened_tab(loaded, cx);
                            return;
                        }
                        if requested_selection != this.selection_generation {
                            loaded.selection = this.active.selection.clone();
                            loaded.commit_detail = this.active.commit_detail.clone();
                        }
                        if let Selection::Commit(id) = &loaded.selection
                            && loaded.commit_detail.is_none()
                        {
                            loaded.commit_detail = this.active.commit_cache.get(id).cloned();
                        }
                        let changed =
                            this.active.repo.as_ref().map(|r| &r.root) != Some(&loaded.repo.root);
                        let old_id = this.active.commit_detail.as_ref().map(|d| &d.id);
                        let new_id = loaded.commit_detail.as_ref().map(|d| &d.id);
                        if changed
                            || (old_id != new_id && !matches!(loaded.selection, Selection::Inspect))
                        {
                            this.active.expanded.clear();
                            this.active.patches.clear();
                            this.patch_generation += 1;
                            this.active.show_commit_body = false;
                        }
                        this.active.commit_detail = loaded.commit_detail;
                        if changed
                            && let Err(err) = this.settings.remember(loaded.repo.root.clone())
                        {
                            eprintln!("Cannot save recent repositories: {err}");
                        }
                        this.active.repo = Some(loaded.repo);
                        this.active.snapshot = loaded.snapshot;
                        this.active.selection = loaded.selection;
                        this.active.diff = loaded.diff;
                        if !loaded.notice.is_empty() {
                            this.active.notice = loaded.notice;
                        }
                        this.active.error = loaded.error;
                        if let Some(modal) = loaded.retry_modal {
                            this.modal = Some(modal);
                        }
                        this.clear_message = loaded.clear_message || changed;
                        // Working tree content can change outside the app. Reload only open patches.
                        if this.active.commit_detail.is_none()
                            && !matches!(this.active.selection, Selection::Inspect)
                        {
                            this.patch_generation += 1;
                            let sources: Vec<_> = this
                                .work_and_commit_sources()
                                .into_iter()
                                .filter(|source| this.active.expanded.contains(&source.key()))
                                .collect();
                            // Collapsed patches must not retain stale content or line selections.
                            let visible: HashSet<_> =
                                sources.iter().map(PatchSource::key).collect();
                            this.active.patches.retain(|key, _| visible.contains(key));
                            for source in sources {
                                this.load_patch(source, cx);
                            }
                        }
                    }
                    Err(err) => {
                        this.active.notice = format!("{err:#}");
                        this.active.error = true;
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }
    /// Patch sources for every currently visible work file (staged and
    /// unstaged variants), or for the selected commit's files.
    fn work_and_commit_sources(&self) -> Vec<PatchSource> {
        match &self.active.selection {
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
    fn open(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let label = if self.active.repo.is_some() {
            ""
        } else {
            "Opening repository…"
        };
        self.dispatch(
            label,
            move |_progress| {
                Loaded::read(
                    Repository::open(path)?,
                    Selection::Work,
                    HISTORY_PAGE_SIZE,
                    "Repository opened".into(),
                    false,
                    false,
                    true,
                )
            },
            cx,
        );
    }
    fn refresh_with_notice(&mut self, notice: String, cx: &mut Context<Self>) {
        let Some(repo) = self.active.repo.clone() else {
            return;
        };
        let selection = self.active.selection.clone();
        let limit = self.active.limit;
        self.dispatch(
            "",
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
        let task = cx
            .background_executor()
            .spawn(async move { repo.fingerprint().map(|fp| fp != expected).unwrap_or(true) });
        cx.spawn(async move |this, cx| {
            if task.await {
                let _ = this.update(cx, |this, cx| {
                    if !this.busy && this.modal.is_none() && this.active.repo.is_some() {
                        this.refresh_with_notice(String::new(), cx);
                    }
                });
            }
        })
        .detach();
    }
    fn select(&mut self, selection: Selection, cx: &mut Context<Self>) {
        if self.busy {
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
                    }
                    Err(err) => {
                        this.active.notice = format!("{err:#}");
                        this.active.error = true;
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }
    fn perform(&mut self, operation: Operation, cx: &mut Context<Self>) {
        let Some(repo) = self.active.repo.clone() else {
            return;
        };
        let clear_message = matches!(operation, Operation::Commit(_));
        let history_edit = matches!(
            operation,
            Operation::Amend { .. } | Operation::UndoLast(_) | Operation::RestoreReflog { .. }
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
            "Running Git…",
            move |progress| {
                let (notice, error) = match repo.execute_with_progress(operation, progress) {
                    Ok(out) => (
                        if out.is_empty() {
                            "Operation completed".into()
                        } else {
                            out
                        },
                        false,
                    ),
                    Err(err) => (format!("{err:#}"), true),
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
        if self.busy {
            return;
        }
        self.form_a.update(cx, |s, cx| s.set_value("", window, cx));
        self.form_b.update(cx, |s, cx| s.set_value("", window, cx));
        self.modal = Some(modal);
        self.form_a.read(cx).focus_handle(cx).focus(window, cx);
        cx.notify();
    }
    fn commit(&mut self, cx: &mut Context<Self>) {
        if self.busy || self.modal.is_some() {
            return;
        }
        self.perform(
            Operation::Commit(self.message.read(cx).value().to_string()),
            cx,
        );
    }
    fn button(&self, id: impl Into<ElementId>, label: impl Into<SharedString>) -> Button {
        Button::new(id)
            .label(label)
            .small()
            .h(px(24.))
            .text_size(px(11.))
            .disabled(self.busy)
    }
    fn sidebar_button(&self, id: impl Into<ElementId>, label: impl Into<SharedString>) -> Button {
        let label = label.into();
        Button::new(id)
            .accessibility_label(label.clone())
            .small()
            .h(px(24.))
            .w_full()
            .text_size(px(11.))
            .disabled(self.busy)
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
        if let Some(value) = self.restore_search.take() {
            self.search
                .update(cx, |state, cx| state.set_value(value, window, cx));
        }
        if let Some(value) = self.restore_message.take() {
            self.message
                .update(cx, |state, cx| state.set_value(value, window, cx));
        }
        if let Some(value) = self.restore_amend_message.take() {
            self.amend_message
                .update(cx, |state, cx| state.set_value(value, window, cx));
            self.amend_message
                .read(cx)
                .focus_handle(cx)
                .focus(window, cx);
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
                            .child(if self.busy {
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
                    .child(
                        div()
                            .text_color(rgb(MUTED))
                            .child("GitBuddy  ·  Rust + GPUI"),
                    ),
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
