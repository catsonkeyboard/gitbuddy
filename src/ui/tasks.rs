use super::*;
use std::sync::{
    Condvar, Mutex,
    atomic::{AtomicU64, Ordering},
};

// Completion is signalled by the worker itself, independently of UI delivery.
// Native app shutdown cannot wait for callbacks on the UI thread it occupies.
#[derive(Default)]
struct Completion {
    done: Mutex<bool>,
    changed: Condvar,
}
struct CompletionGuard(Arc<Completion>);
impl Drop for CompletionGuard {
    fn drop(&mut self) {
        *self.0.done.lock().unwrap_or_else(|e| e.into_inner()) = true;
        self.0.changed.notify_all();
    }
}
impl Completion {
    fn wait(&self) {
        let mut done = self.done.lock().unwrap_or_else(|e| e.into_inner());
        while !*done {
            done = self.changed.wait(done).unwrap_or_else(|e| e.into_inner());
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) struct TabId(u64);
impl TabId {
    pub fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Self(NEXT.fetch_add(1, Ordering::Relaxed))
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Ticket {
    tab: TabId,
    job: u64,
}
pub(super) struct RunningTask {
    pub label: String,
    pub cancellation: Option<git::CancellationToken>,
    ticket: Ticket,
    completion: Arc<Completion>,
}
#[derive(Default)]
pub(super) struct RepositoryTasks {
    next_job: u64,
    closing: bool,
    running: HashMap<TabId, RunningTask>,
}
impl RepositoryTasks {
    pub fn closing(&self) -> bool {
        self.closing
    }
    fn request_shutdown(&mut self) {
        self.closing = true;
        for task in self.running.values() {
            if let Some(token) = &task.cancellation {
                token.cancel();
            }
        }
    }
    pub fn shutdown_and_wait(&mut self) {
        self.request_shutdown();
        for task in self.running.values() {
            task.completion.wait();
        }
    }
    pub fn get(&self, tab: TabId) -> Option<&RunningTask> {
        self.running.get(&tab)
    }
    fn start(&mut self, tab: TabId, label: &str, cancellable: bool) -> Option<Ticket> {
        if self.closing || self.get(tab).is_some() {
            return None;
        }
        self.next_job += 1;
        let ticket = Ticket {
            tab,
            job: self.next_job,
        };
        self.running.insert(
            tab,
            RunningTask {
                label: label.into(),
                ticket,
                completion: Arc::new(Completion::default()),
                cancellation: cancellable.then(git::CancellationToken::default),
            },
        );
        Some(ticket)
    }
    fn accepts(&self, ticket: Ticket) -> bool {
        self.get(ticket.tab).is_some_and(|t| t.ticket == ticket)
    }
    fn finish(&mut self, ticket: Ticket) -> bool {
        if !self.accepts(ticket) {
            return false;
        }
        self.running.remove(&ticket.tab);
        true
    }
}

#[derive(Clone)]
pub(super) enum Preparation {
    Open(PathBuf),
    Init(PathBuf),
    Clone { url: String, destination: PathBuf },
}

impl GitBuddy {
    pub fn request_quit(&mut self, cx: &mut Context<Self>) -> bool {
        self.tasks.request_shutdown();
        if self.tasks.running.is_empty() {
            self.flush_session(cx);
            cx.quit();
            true
        } else {
            self.active.notice =
                "Stopping network transfers; waiting for repository writes before quitting…".into();
            cx.notify();
            false
        }
    }
    pub(super) fn busy(&self) -> bool {
        self.tasks.closing() || self.tasks.get(self.active.id).is_some()
    }
    fn tab_mut(&mut self, id: TabId) -> Option<&mut RepoTab> {
        tab_mut_by_id(&mut self.active, &mut self.tabs, id)
    }
    fn tab_index(&self, id: TabId) -> Option<usize> {
        if self.active.id == id {
            return self.active_index;
        }
        self.tabs
            .iter()
            .position(|t| t.as_ref().is_some_and(|t| t.id == id))
    }
    pub(super) fn current_message(&self, cx: &App) -> String {
        if self.clear_message {
            String::new()
        } else if let Some(value) = &self.restore_message {
            value.clone()
        } else {
            self.message.read(cx).value().to_string()
        }
    }
    pub(super) fn cancel_task(&mut self, cx: &mut Context<Self>) {
        if let Some(task) = self.tasks.get(self.active.id)
            && let Some(token) = &task.cancellation
            && token.cancel()
        {
            self.active.notice = format!(
                "Cancelling {}… waiting for the network operation to stop",
                task.label
            );
            cx.notify();
        }
    }
    pub(super) fn open(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        self.prepare_repository(Preparation::Open(path), cx);
    }
    pub(super) fn prepare_repository(&mut self, preparation: Preparation, cx: &mut Context<Self>) {
        let (path, label, cancellable) = match &preparation {
            Preparation::Open(path) => (path, "Opening repository", false),
            Preparation::Init(path) => (path, "Initializing repository", false),
            Preparation::Clone { destination, .. } => (destination, "Clone", true),
        };
        let path = canonical_target(path);
        let existing = std::iter::once(&self.active)
            .chain(self.tabs.iter().flatten())
            .find(|t| t.repo.as_ref().map(|r| &r.root).or(t.preparing.as_ref()) == Some(&path))
            .map(|t| (t.id, t.repo.is_some(), self.tasks.get(t.id).is_some()));
        if let Some((id, opened, busy)) = existing {
            if (opened || busy) && !matches!(preparation, Preparation::Open(_)) {
                self.modal_error =
                    Some("This destination is already open or has a task running.".into());
                cx.notify();
                return;
            }
            if let Some(index) = self.tab_index(id) {
                self.activate_tab(index, cx);
            }
            if opened || busy {
                self.modal = None;
                cx.notify();
                return;
            }
        } else {
            let index = self.tabs.len();
            self.tabs.push(Some(RepoTab {
                preparing: Some(path),
                ..RepoTab::default()
            }));
            self.activate_tab(index, cx);
        }
        self.modal = None;
        self.active.retry_preparation = Some(preparation.clone());
        let limit = self.active.limit;
        self.dispatch(
            label,
            cancellable,
            move |control| {
                let repo = match preparation {
                    Preparation::Open(path) => Repository::open(path)?,
                    Preparation::Init(path) => Repository::init(&path)?,
                    Preparation::Clone { url, destination } => {
                        Repository::clone_repo_with_control(&url, &destination, control)?
                    }
                };
                Loaded::read(
                    repo,
                    Selection::Work,
                    limit,
                    "Repository ready".into(),
                    false,
                    false,
                    true,
                )
            },
            cx,
        );
    }
    pub(super) fn dispatch(
        &mut self,
        label: &str,
        cancellable: bool,
        work: impl FnOnce(git::NetworkControl) -> anyhow::Result<Loaded> + Send + 'static,
        cx: &mut Context<Self>,
    ) {
        let Some(ticket) = self.tasks.start(self.active.id, label, cancellable) else {
            return;
        };
        let cancellation = self
            .tasks
            .get(ticket.tab)
            .and_then(|t| t.cancellation.clone())
            .unwrap_or_default();
        self.active.error = false;
        if !label.is_empty() {
            self.active.notice = format!("{label}…");
        }
        let requested_selection = self.active.selection.clone();
        let requested_message = self.current_message(cx);
        let retry_history = self.history_data.clone();
        let retry_message = self.amend_message.read(cx).value().to_string();
        let (tx, rx) = async_channel::bounded::<String>(64);
        let sink: git::ProgressSink = Arc::new(std::sync::Mutex::new(move |text: &str| {
            let _ = tx.try_send(text.to_owned());
        }));
        let completion = CompletionGuard(self.tasks.get(ticket.tab).unwrap().completion.clone());
        let task = cx.background_executor().spawn(async move {
            let _completion = completion;
            work(git::NetworkControl::new(Some(sink), cancellation))
        });
        cx.spawn(async move |this, cx| {
            while let Ok(text) = rx.recv().await {
                let _ = this.update(cx, |this, cx| {
                    if this.tasks.accepts(ticket)
                        && !this
                            .tasks
                            .get(ticket.tab)
                            .and_then(|t| t.cancellation.as_ref())
                            .is_some_and(|c| c.is_requested())
                        && let Some(tab) = this.tab_mut(ticket.tab)
                    {
                        tab.notice = text;
                        cx.notify();
                    }
                });
            }
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                let announce = this
                    .tasks
                    .get(ticket.tab)
                    .is_some_and(|t| !t.label.is_empty());
                if !this.tasks.finish(ticket) {
                    return;
                }
                let active = this.active.id == ticket.tab;
                if let Some(tab) = this.tab_mut(ticket.tab) {
                    tab.task_finished = !active && announce;
                }
                match result {
                    Ok(mut loaded) => {
                        if loaded.open_as_tab {
                            let existing = std::iter::once(&this.active)
                                .chain(this.tabs.iter().flatten())
                                .find(|t| {
                                    t.id != ticket.tab
                                        && t.repo
                                            .as_ref()
                                            .is_some_and(|r| r.root == loaded.repo.root)
                                })
                                .map(|t| t.id);
                            if let Some(id) = existing {
                                if let Some(index) = this.tab_index(ticket.tab) {
                                    this.close_tab(index, cx);
                                }
                                if active && let Some(index) = this.tab_index(id) {
                                    this.activate_tab(index, cx);
                                }
                                if this.tasks.closing && this.tasks.running.is_empty() {
                                    this.flush_session(cx);
                                    cx.quit();
                                }
                                cx.notify();
                                return;
                            }
                            if let Err(e) = this.settings.remember(loaded.repo.root.clone()) {
                                loaded.notice = format!(
                                    "{}; Cannot save recent repositories: {e:#}",
                                    loaded.notice
                                );
                                loaded.error = true;
                            }
                        }
                        let current_message = if active {
                            this.current_message(cx)
                        } else {
                            this.tab_mut(ticket.tab)
                                .map(|t| t.message.clone())
                                .unwrap_or_default()
                        };
                        loaded.clear_message &= current_message == requested_message;
                        let clear_message = loaded.clear_message;
                        let retry = loaded.retry_modal.take();
                        if let Some(tab) = this.tab_mut(ticket.tab) {
                            tab.apply_loaded(loaded, &requested_selection);
                            tab.history_retry =
                                retry.map(|modal| (modal, retry_history, retry_message));
                        }
                        if active {
                            this.clear_message = clear_message;
                            if this.modal.is_none()
                                && let Some((modal, history, message)) =
                                    this.active.history_retry.take()
                            {
                                this.modal = Some(modal);
                                this.history_data = history;
                                this.restore_amend_message = Some(message);
                            }
                            this.refresh_task_views(cx);
                        }
                    }
                    Err(error) => {
                        if let Some(tab) = this.tab_mut(ticket.tab) {
                            tab.error = !git::is_cancelled(&error);
                            tab.notice = format!("{error:#}");
                        }
                        if active {
                            this.refresh_task_views(cx);
                        }
                    }
                }
                if this.tasks.closing && this.tasks.running.is_empty() {
                    this.flush_session(cx);
                    cx.quit();
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
    pub(super) fn refresh_task_views(&mut self, cx: &mut Context<Self>) {
        if self.busy() || !self.active.refresh_views {
            return;
        }
        self.active.refresh_views = false;
        self.patch_generation += 1;
        self.active
            .patches
            .retain(|_, state| !matches!(state, PatchState::Loading));
        if matches!(self.active.selection, Selection::Conflicts) {
            self.reload_conflicts(None, cx);
            self.restore_conflict_editor();
        }
        if matches!(self.active.selection, Selection::Inspect)
            && let inspect::InspectState::Loading(request) = &self.active.inspection
        {
            self.load_inspection(request.clone(), true, cx);
        }
        // Cached commit/comparison patches survive switching; interrupted loads
        // are resumed only after this repository's write task has finished.
        for source in self.work_and_commit_sources() {
            if self.active.expanded.contains(&source.key())
                && !self.active.patches.contains_key(&source.key())
            {
                self.load_patch(source, cx);
            }
        }
        if self.active.commit_detail.is_none()
            && let Selection::Commit(id) = &self.active.selection
        {
            self.request_commit_detail(id.clone(), cx);
        }
    }
    pub(super) fn preparation_view(&self, cx: &mut Context<Self>) -> AnyElement {
        v_flex()
            .flex_1()
            .items_center()
            .justify_center()
            .gap_3()
            .bg(rgb(BG))
            .child(
                div()
                    .text_lg()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(if self.busy() {
                        "Preparing repository…"
                    } else {
                        "Repository is not open"
                    }),
            )
            .child(
                div().text_sm().text_color(rgb(MUTED)).child(
                    self.active
                        .preparing
                        .as_ref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_default(),
                ),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(rgb(MUTED))
                    .child(if self.busy() {
                        "You can switch tabs or open another repository while this task runs."
                    } else {
                        "Retry this task or close the tab to choose a different destination."
                    }),
            )
            .when(
                !self.busy() && self.active.retry_preparation.is_some(),
                |view| {
                    view.child(
                        self.button("retry-preparation", "Retry")
                            .primary()
                            .on_click(cx.listener(|this, _, _, cx| {
                                if let Some(preparation) = this.active.retry_preparation.clone() {
                                    this.prepare_repository(preparation, cx);
                                }
                            })),
                    )
                },
            )
            .child(
                self.button("pending-open", "Open another repository…")
                    .ghost()
                    .disabled(false)
                    .on_click(cx.listener(|this, _, w, cx| this.show_modal(Modal::Open, w, cx))),
            )
            .into_any_element()
    }
}

fn tab_mut_by_id<'a>(
    active: &'a mut RepoTab,
    tabs: &'a mut [Option<RepoTab>],
    id: TabId,
) -> Option<&'a mut RepoTab> {
    if active.id == id {
        Some(active)
    } else {
        tabs.iter_mut().flatten().find(|t| t.id == id)
    }
}

impl RepoTab {
    pub(super) fn apply_loaded(&mut self, mut loaded: Loaded, requested: &Selection) {
        let changed = self.snapshot.fingerprint != loaded.snapshot.fingerprint;
        if self.selection != *requested {
            loaded.selection = self.selection.clone();
            loaded.diff = self.diff.clone();
            loaded.commit_detail = self.commit_detail.clone();
        } else if let Selection::Commit(id) = &loaded.selection {
            loaded.commit_detail = self
                .commit_cache
                .get(id)
                .cloned()
                .or_else(|| self.commit_detail.clone().filter(|d| &d.id == id));
        }
        if self.commit_detail.as_ref().map(|d| &d.id)
            != loaded.commit_detail.as_ref().map(|d| &d.id)
            && !matches!(loaded.selection, Selection::Inspect)
        {
            self.expanded.clear();
            self.patches.clear();
            self.show_commit_body = false;
        }
        self.repo = Some(loaded.repo);
        self.preparing = None;
        self.retry_preparation = None;
        self.snapshot = loaded.snapshot;
        self.selection = loaded.selection;
        self.diff = loaded.diff;
        self.commit_detail = loaded.commit_detail;
        if !loaded.notice.is_empty() {
            self.notice = loaded.notice;
        }
        self.error = loaded.error;
        if loaded.clear_message {
            self.message.clear();
        }
        if changed && matches!(self.selection, Selection::Work | Selection::File(_, _)) {
            self.patches.clear();
        }
        self.refresh_views = true;
        if let Some(saved) = self.restoring.take() {
            self.apply_session(saved);
        }
    }
}

fn canonical_target(path: &Path) -> PathBuf {
    if let Ok(path) = path.canonicalize() {
        return path;
    }
    if let Some(parent) = path.parent()
        && let Some(name) = path.file_name()
    {
        return canonical_target(parent).join(name);
    }
    path.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::{RepositoryTasks, TabId, tab_mut_by_id};
    use crate::ui::{Loaded, PatchKey, PatchLineSelection, PatchState, RepoTab, Selection};
    use gitbuddy::git::{Repository, Snapshot};
    #[test]
    fn shutdown_cancels_all_tabs_and_waits_for_final_writes() {
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };
        let mut jobs = RepositoryTasks::default();
        let fetch = jobs.start(TabId::new(), "Fetch", true).unwrap();
        let push = jobs.start(TabId::new(), "Push", true).unwrap();
        let commit = jobs.start(TabId::new(), "Commit", false).unwrap();
        let fetch_token = jobs.get(fetch.tab).unwrap().cancellation.clone().unwrap();
        let push_token = jobs.get(push.tab).unwrap().cancellation.clone().unwrap();
        let guards = [fetch, push, commit]
            .map(|ticket| super::CompletionGuard(jobs.get(ticket.tab).unwrap().completion.clone()));
        let wrote = Arc::new(AtomicBool::new(false));
        let wrote_worker = wrote.clone();
        let cancellation = fetch_token.clone();
        let worker = std::thread::spawn(move || {
            let _guards = guards;
            // Shutdown must cancel the fetch, then wait for these final writes.
            while !cancellation.is_requested() {
                std::thread::yield_now();
            }
            wrote_worker.store(true, Ordering::SeqCst);
        });
        jobs.shutdown_and_wait();
        assert!(wrote.load(Ordering::SeqCst));
        assert!(fetch_token.is_requested());
        assert!(push_token.is_requested());
        assert!(jobs.start(TabId::new(), "New task", true).is_none());
        worker.join().unwrap();
    }
    #[test]
    fn worker_unwind_also_signals_completion() {
        let completion = std::sync::Arc::new(super::Completion::default());
        let guard = super::CompletionGuard(completion.clone());
        let worker = std::thread::spawn(move || {
            let _guard = guard;
            panic!("simulated worker failure");
        });
        completion.wait();
        assert!(worker.join().is_err());
    }
    #[test]
    fn operation_error_does_not_disable_automatic_refresh() {
        let tab = RepoTab {
            repo: Some(Repository {
                root: "/test/repo".into(),
            }),
            error: true,
            notice: "Previous operation failed".into(),
            ..RepoTab::default()
        };
        assert!(tab.ready_for_refresh(false, false));
        assert!(!tab.ready_for_refresh(true, false));
        assert!(!tab.ready_for_refresh(false, true));
        assert!(!RepoTab::default().ready_for_refresh(false, false));
    }
    #[test]
    fn repositories_run_independently_and_stale_job_results_are_rejected() {
        let mut jobs = RepositoryTasks::default();
        let a = TabId::new();
        let b = TabId::new();
        let first = jobs.start(a, "Fetch", true).unwrap();
        let second = jobs.start(b, "Commit", false).unwrap();
        assert!(jobs.start(a, "Push", true).is_none());
        assert!(jobs.get(a).unwrap().cancellation.as_ref().unwrap().cancel());
        assert!(jobs.accepts(second));
        assert!(jobs.finish(first));
        let replacement = jobs.start(a, "Fetch", true).unwrap();
        assert!(!jobs.finish(first));
        assert!(jobs.accepts(replacement));
        assert!(jobs.accepts(second));
        assert!(
            jobs.get(a)
                .unwrap()
                .cancellation
                .as_ref()
                .unwrap()
                .can_cancel()
        );
    }

    #[test]
    fn completion_routes_by_identity_after_tabs_are_reordered_or_closed() {
        let mut active = RepoTab {
            message: "B draft".into(),
            notice: "B task".into(),
            ..RepoTab::default()
        };
        let a = RepoTab::default();
        let a_id = a.id;
        let b_id = active.id;
        let mut tabs = vec![Some(RepoTab::default()), Some(a), None];
        tabs.remove(0);
        tabs.reverse();
        tab_mut_by_id(&mut active, &mut tabs, a_id).unwrap().notice = "A cancelled".into();
        assert_eq!(active.notice, "B task");
        assert_eq!(active.message, "B draft");
        let a = tabs[1].take().unwrap();
        tabs[0] = Some(std::mem::replace(&mut active, a));
        assert_eq!(
            tab_mut_by_id(&mut active, &mut tabs, a_id).unwrap().notice,
            "A cancelled"
        );
        assert_eq!(
            tab_mut_by_id(&mut active, &mut tabs, b_id).unwrap().message,
            "B draft"
        );
        tabs.remove(0);
        assert!(tab_mut_by_id(&mut active, &mut tabs, b_id).is_none());
        assert!(tab_mut_by_id(&mut active, &mut tabs, TabId::new()).is_none());
    }

    #[test]
    fn background_completion_preserves_view_and_commit_draft_changed_after_start() {
        let mut tab = RepoTab {
            selection: Selection::Commit("selected-later".into()),
            message: "new draft".into(),
            query: "keep search".into(),
            ..RepoTab::default()
        };
        let loaded = Loaded {
            repo: Repository {
                root: std::path::PathBuf::from("/test/repo"),
            },
            snapshot: Snapshot::default(),
            selection: Selection::Work,
            diff: Vec::new(),
            commit_detail: None,
            notice: "Remotes fetched".into(),
            error: false,
            clear_message: false,
            open_as_tab: false,
            retry_modal: None,
        };
        tab.apply_loaded(loaded, &Selection::Work);
        assert_eq!(tab.selection, Selection::Commit("selected-later".into()));
        assert_eq!(tab.message, "new draft");
        assert_eq!(tab.query, "keep search");
        assert_eq!(tab.notice, "Remotes fetched");
        assert!(tab.refresh_views);
    }

    #[test]
    fn unchanged_refresh_keeps_expanded_patch_and_line_selection() {
        let key = PatchKey::Work("file.txt".into(), false);
        let patch = std::sync::Arc::new(gitbuddy::git::FilePatch::read_only(
            "@@ -1 +1 @@\n-old\n+new\n",
        ));
        let mut tab = RepoTab::default();
        tab.expanded.insert(key.clone());
        tab.patches.insert(
            key.clone(),
            PatchState::Ready(
                patch.clone(),
                PatchLineSelection {
                    rows: [1, 2].into(),
                    anchor: Some(1),
                },
            ),
        );
        let loaded = Loaded {
            repo: Repository {
                root: "/test/repo".into(),
            },
            snapshot: tab.snapshot.clone(),
            selection: Selection::Work,
            diff: Vec::new(),
            commit_detail: None,
            notice: String::new(),
            error: false,
            clear_message: false,
            open_as_tab: false,
            retry_modal: None,
        };
        tab.apply_loaded(loaded, &Selection::Work);
        let Some(PatchState::Ready(retained, selection)) = tab.patches.get(&key) else {
            panic!("refresh lost the patch");
        };
        assert!(std::sync::Arc::ptr_eq(retained, &patch));
        assert_eq!(selection.rows, [1, 2].into());
        assert!(tab.expanded.contains(&key));
    }
}
