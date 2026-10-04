use super::*;
use gitbuddy::session as disk;
use std::{cell::RefCell, collections::BTreeMap};

/// Explicit handles isolate GPUI's scroll state between repository tabs.
#[derive(Clone, Default)]
pub(super) struct ViewScroll {
    handles: RefCell<BTreeMap<String, UniformListScrollHandle>>,
    pending: RefCell<BTreeMap<String, disk::Offset>>,
}
impl ViewScroll {
    pub fn list(&self, id: &str) -> UniformListScrollHandle {
        self.handles
            .borrow_mut()
            .entry(id.into())
            .or_default()
            .clone()
    }
    pub fn area(&self, id: &str) -> ScrollHandle {
        self.list(id).0.borrow().base_handle.clone()
    }
    fn capture(&self) -> BTreeMap<String, disk::Offset> {
        let mut values = self
            .handles
            .borrow()
            .iter()
            .map(|(id, handle)| {
                let offset = handle.0.borrow().base_handle.offset();
                (
                    id.clone(),
                    disk::Offset {
                        x: f32::from(offset.x),
                        y: f32::from(offset.y),
                    },
                )
            })
            .collect::<BTreeMap<_, _>>();
        // Inactive or still-loading panes must not erase their saved position.
        values.extend(self.pending.borrow().clone());
        values
    }
    fn restore(&self, values: BTreeMap<String, disk::Offset>) {
        *self.pending.borrow_mut() = values;
    }
    fn apply(&self) -> bool {
        let mut pending = self.pending.borrow_mut();
        let mut changed = false;
        for (id, handle) in self.handles.borrow().iter() {
            if let Some(offset) = pending.remove(id) {
                let valid = |value: f32| if value.is_finite() { value.min(0.) } else { 0. };
                handle
                    .0
                    .borrow()
                    .base_handle
                    .set_offset(point(px(valid(offset.x)), px(valid(offset.y))));
                changed = true;
            }
        }
        changed
    }
}
impl RepoTab {
    fn session_tab(&self) -> Option<disk::Tab> {
        let path = self
            .repo
            .as_ref()
            .map(|r| &r.root)
            .or(self.preparing.as_ref())?
            .clone();
        // Preserve unavailable and unfinished tabs without serializing task state.
        if let Some(saved) = &self.restoring {
            return Some(disk::Tab {
                path,
                message: self.message.clone(),
                query: self.query.clone(),
                ..saved.clone()
            });
        }
        let inspection = match &self.inspection {
            inspect::InspectState::Loading(request) | inspect::InspectState::Error(request, _) => {
                Some(request.clone())
            }
            inspect::InspectState::Ready(inspect::InspectResult::History(v)) => {
                Some(disk::Inspection::History(
                    v.path.clone(),
                    v.revision.id.clone(),
                    v.entries.len().max(100),
                ))
            }
            inspect::InspectState::Ready(inspect::InspectResult::Blame(v)) => Some(
                disk::Inspection::Blame(v.path.clone(), v.revision.id.clone()),
            ),
            inspect::InspectState::Ready(inspect::InspectResult::Compare(v)) => Some(
                disk::Inspection::Compare(v.base.id.clone(), v.target.id.clone()),
            ),
            inspect::InspectState::Empty => None,
        };
        let selection = match &self.selection {
            Selection::Work => disk::Selection::Work,
            Selection::File(file, staged) => disk::Selection::File(file.path.clone(), *staged),
            Selection::Commit(id) => disk::Selection::Commit(id.clone()),
            Selection::Inspect => disk::Selection::Inspect,
            Selection::Conflicts => disk::Selection::Conflicts,
        };
        let mut expanded = self.expanded.iter().cloned().collect::<Vec<_>>();
        expanded.sort();
        let mut lines = self.restored_lines.clone();
        for (key, state) in &self.patches {
            if let PatchState::Ready(patch, selected) = state {
                lines.remove(key);
                if !selected.rows.is_empty() {
                    lines.insert(
                        key.clone(),
                        disk::LineSelection {
                            key: key.clone(),
                            fingerprint: disk::patch_fingerprint(&patch.lines),
                            rows: selected.rows.iter().copied().collect(),
                            anchor: selected.anchor,
                        },
                    );
                }
            }
        }
        let mut lines = lines.into_values().collect::<Vec<_>>();
        lines.sort_by(|a, b| a.key.cmp(&b.key));
        let mut conflicts = self.restored_conflicts.clone();
        for (path, draft) in &self.conflicts.drafts {
            conflicts.insert(
                path.clone(),
                disk::ConflictDraft {
                    path: path.clone(),
                    text: draft.text.clone(),
                    choice: draft.choice,
                    dirty: draft.dirty,
                    fingerprint: disk::conflict_fingerprint(&draft.context),
                    editor: draft.editor.clone(),
                },
            );
        }
        let mut conflict_drafts = conflicts.into_values().collect::<Vec<_>>();
        conflict_drafts.sort_by(|a, b| a.path.cmp(&b.path));
        Some(disk::Tab {
            path,
            message: self.message.clone(),
            message_view: self.message_view.clone(),
            amend: self.amend_draft.clone(),
            query: self.query.clone(),
            selection,
            inspection,
            comparison_base: self.comparison_base.clone(),
            history_tab: self.history_tab,
            history_limit: self.limit,
            show_commit_body: self.show_commit_body,
            expanded,
            lines,
            scroll: self.scroll.capture(),
            conflict_file: self.conflicts.selected.clone(),
            conflict_drafts,
        })
    }
    pub(super) fn apply_session(&mut self, saved: disk::Tab) {
        self.message_view = saved.message_view;
        self.amend_draft = saved.amend;
        self.selection = match saved.selection {
            disk::Selection::Work => Selection::Work,
            disk::Selection::Commit(id) => Selection::Commit(id),
            disk::Selection::File(path, staged) => {
                // Current file UI selects a file by expanding it in the worktree.
                if self
                    .snapshot
                    .files
                    .iter()
                    .any(|f| f.path == path && if staged { f.staged() } else { f.unstaged() })
                {
                    self.expanded.insert(PatchKey::Work(path, staged));
                }
                Selection::Work
            }
            disk::Selection::Inspect if saved.inspection.is_some() => Selection::Inspect,
            disk::Selection::Inspect => Selection::Work,
            disk::Selection::Conflicts => Selection::Conflicts,
        };
        self.inspection = saved
            .inspection
            .map(inspect::InspectState::Loading)
            .unwrap_or_default();
        self.comparison_base = saved.comparison_base;
        self.history_tab = saved.history_tab.min(1);
        self.limit = saved.history_limit.max(HISTORY_PAGE_SIZE);
        self.show_commit_body = saved.show_commit_body;
        self.expanded.extend(saved.expanded);
        self.restored_lines = saved
            .lines
            .into_iter()
            .map(|lines| (lines.key.clone(), lines))
            .collect();
        self.scroll.restore(saved.scroll);
        self.conflicts.selected = saved.conflict_file;
        self.restored_conflicts = saved
            .conflict_drafts
            .into_iter()
            .map(|draft| (draft.path.clone(), draft))
            .collect();
    }
}
impl GitBuddy {
    pub(super) fn current_amend(&self, cx: &App) -> Option<disk::AmendDraft> {
        if self.restore_amend_message.is_none()
            && matches!(self.modal, Some(Modal::History(HistoryKind::Amend)))
            && let HistoryData::Edit(edit) = &self.history_data
            && let Some(head) = &edit.head.id
        {
            let state = self.amend_message.read(cx);
            Some(disk::AmendDraft {
                head: head.clone(),
                reference: edit.head.reference.clone(),
                message: state.value().to_string(),
                editor: editor_view(state.selected_range(), state.scroll_offset()),
            })
        } else {
            self.active.amend_draft.clone()
        }
    }
    pub(super) fn sync_editor_views(&mut self, cx: &App) {
        if self.restore_message.is_none() && !self.clear_message {
            let state = self.message.read(cx);
            self.active.message_view = editor_view(state.selected_range(), state.scroll_offset());
        }
        self.active.amend_draft = self.current_amend(cx);
        if self.restore_conflict.is_none() && matches!(self.selection, Selection::Conflicts) {
            let state = self.conflict_editor.read(cx);
            let view = editor_view(state.selected_range(), state.scroll_offset());
            if let Some(path) = &self.active.conflicts.selected
                && let Some(draft) = self.active.conflicts.drafts.get_mut(path)
            {
                draft.editor = view;
            }
        }
    }
    fn capture_session(&self, cx: &App) -> disk::Session {
        let mut tabs = Vec::new();
        let mut active = 0;
        for (index, slot) in self.tabs.iter().enumerate() {
            let tab = if self.active_index == Some(index) {
                &self.active
            } else if let Some(tab) = slot {
                tab
            } else {
                continue;
            };
            if let Some(mut saved) = tab.session_tab() {
                if self.active_index == Some(index) {
                    active = tabs.len();
                    saved.message = self.current_message(cx);
                    saved.query = self
                        .restore_search
                        .clone()
                        .unwrap_or_else(|| self.search.read(cx).value().to_string());
                    saved.amend = self.current_amend(cx);
                    if self.restore_message.is_none() && !self.clear_message {
                        let state = self.message.read(cx);
                        saved.message_view =
                            editor_view(state.selected_range(), state.scroll_offset());
                    }
                    if self.restore_conflict.is_none()
                        && matches!(self.selection, Selection::Conflicts)
                        && let Some(path) = &self.active.conflicts.selected
                        && let Some(draft) =
                            saved.conflict_drafts.iter_mut().find(|d| &d.path == path)
                    {
                        let state = self.conflict_editor.read(cx);
                        draft.editor = editor_view(state.selected_range(), state.scroll_offset());
                    }
                }
                tabs.push(saved);
            }
        }
        disk::Session {
            tabs,
            active,
            tab_scroll: self
                .tab_scroll
                .capture()
                .get("repository-tabs")
                .copied()
                .unwrap_or_default(),
            ..disk::Session::default()
        }
    }
    pub(super) fn flush_session(&mut self, cx: &App) {
        self.session_revision += 1;
        if let Err(error) = self
            .session_store
            .save(self.session_revision, &self.capture_session(cx))
        {
            eprintln!("Cannot save session: {error:#}");
        }
    }
    pub(super) fn start_session_autosave(&self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                let Ok((store, revision, saved)) = this.update(cx, |this, cx| {
                    this.session_revision += 1;
                    (
                        this.session_store.clone(),
                        this.session_revision,
                        this.capture_session(cx),
                    )
                }) else {
                    break;
                };
                let result = cx
                    .background_executor()
                    .spawn(async move { store.save(revision, &saved) })
                    .await;
                if let Err(error) = result {
                    let _ = this.update(cx, |this, cx| {
                        let warning = format!("Cannot save session: {error:#}");
                        if this.session_warning.as_ref() != Some(&warning) {
                            this.session_warning = Some(warning);
                            cx.notify();
                        }
                    });
                } else {
                    let _ = this.update(cx, |this, cx| {
                        if this
                            .session_warning
                            .as_ref()
                            .is_some_and(|warning| warning.starts_with("Cannot save session:"))
                        {
                            this.session_warning = None;
                            cx.notify();
                        }
                    });
                }
            }
        })
        .detach();
    }
    pub(super) fn restore_session(&mut self, saved: disk::Session, cx: &mut Context<Self>) {
        self.tab_scroll.restore(BTreeMap::from([(
            "repository-tabs".into(),
            saved.tab_scroll,
        )]));
        let selected_path = saved.tabs.get(saved.active).map(|t| t.path.clone());
        for saved_tab in saved.tabs {
            let path = saved_tab.path.clone();
            if path.as_os_str().is_empty() {
                continue;
            }
            // Avoid creating duplicate tabs even for an externally edited session.
            if self
                .tabs
                .iter()
                .flatten()
                .chain(std::iter::once(&self.active))
                .any(|t| {
                    t.preparing.as_ref() == Some(&path)
                        || t.repo.as_ref().is_some_and(|r| r.root == path)
                })
            {
                continue;
            }
            let index = self.tabs.len();
            self.tabs.push(Some(RepoTab {
                preparing: Some(path.clone()),
                query: saved_tab.query.clone(),
                message: saved_tab.message.clone(),
                message_view: saved_tab.message_view.clone(),
                amend_draft: saved_tab.amend.clone(),
                limit: saved_tab.history_limit.max(HISTORY_PAGE_SIZE),
                restoring: Some(saved_tab),
                ..RepoTab::default()
            }));
            self.activate_tab(index, cx);
            self.open(path, cx);
        }
        if let Some(path) = selected_path {
            let index = (0..self.tabs.len()).find(|&index| {
                let tab = if self.active_index == Some(index) {
                    Some(&self.active)
                } else {
                    self.tabs[index].as_ref()
                };
                tab.is_some_and(|t| {
                    t.preparing.as_ref() == Some(&path)
                        || t.repo.as_ref().is_some_and(|r| r.root == path)
                })
            });
            if let Some(index) = index {
                self.activate_tab(index, cx);
            }
        } else if !self.tabs.is_empty() {
            self.activate_tab(0, cx);
        }
    }
    pub(super) fn resume_expanded(&mut self, cx: &mut Context<Self>) {
        if self.busy() {
            return;
        }
        for source in self.work_and_commit_sources() {
            if self.active.expanded.contains(&source.key())
                && !self.active.patches.contains_key(&source.key())
            {
                self.load_patch(source, cx);
            }
        }
    }
    pub(super) fn restore_scroll_on_next_frame(&self, window: &mut Window, cx: &mut Context<Self>) {
        if self.tab_scroll.pending.borrow().is_empty()
            && self.active.scroll.pending.borrow().is_empty()
        {
            return;
        }
        let tab = self.active.id;
        let generation = self.selection_generation;
        // Wait for metadata and all expanded diffs; an empty layout would clamp
        // the saved position to zero before the real content arrived.
        let ready = !self.busy()
            && self.repo.is_some()
            && (!matches!(self.selection, Selection::Commit(_)) || self.commit_detail.is_some())
            && (!matches!(self.selection, Selection::Inspect)
                || !matches!(self.inspection, inspect::InspectState::Loading(_)))
            && (!matches!(self.selection, Selection::Conflicts) || self.conflicts.is_ready())
            && self
                .work_and_commit_sources()
                .iter()
                .filter(|s| self.expanded.contains(&s.key()))
                .all(|s| {
                    matches!(
                        self.patches.get(&s.key()),
                        Some(PatchState::Ready(..) | PatchState::Error(_))
                    )
                });
        cx.on_next_frame(window, move |this, _, cx| {
            let mut changed = this.tab_scroll.apply();
            if ready && this.active.id == tab && this.selection_generation == generation {
                changed |= this.active.scroll.apply();
            }
            if changed {
                cx.notify();
            }
        });
    }
}

fn editor_view(range: std::ops::Range<usize>, offset: Point<Pixels>) -> disk::EditorView {
    disk::EditorView {
        start: range.start,
        end: range.end,
        scroll: disk::Offset {
            x: f32::from(offset.x),
            y: f32::from(offset.y),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::{BTreeMap, Loaded, PatchKey, RepoTab, Repository, Selection, ViewScroll, disk};
    #[test]
    fn unavailable_tab_retains_draft_and_unloaded_views() {
        let saved = disk::Tab {
            path: "/missing/repo".into(),
            message: "old".into(),
            selection: disk::Selection::Commit("deadbeef".into()),
            scroll: BTreeMap::from([("commit-list".into(), disk::Offset { x: 0., y: -500. })]),
            ..disk::Tab::default()
        };
        let tab = RepoTab {
            preparing: Some(saved.path.clone()),
            message: "edited while opening".into(),
            restoring: Some(saved.clone()),
            ..RepoTab::default()
        };
        let captured = tab.session_tab().unwrap();
        assert_eq!(captured.message, "edited while opening");
        assert_eq!(captured.selection, saved.selection);
        assert_eq!(captured.scroll, saved.scroll);
    }
    #[test]
    fn scroll_handles_are_independent_and_pending_positions_survive_loading() {
        let a = ViewScroll::default();
        let b = ViewScroll::default();
        let position = disk::Offset { x: -25., y: -300. };
        a.restore(BTreeMap::from([("patch".into(), position)]));
        assert!(!a.apply()); // The pane has not been rendered yet.
        assert_eq!(a.capture()["patch"], position);
        let first = a.list("patch");
        let second = b.list("patch");
        assert!(a.apply());
        assert!(!a.apply()); // Never reapply after the user scrolls.
        assert_eq!(f32::from(first.0.borrow().base_handle.offset().y), -300.);
        assert_eq!(f32::from(second.0.borrow().base_handle.offset().y), 0.);
    }
    #[test]
    fn snapshot_loading_preserves_restored_commit_expansion_and_live_draft() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        let key = PatchKey::Commit("deadbeef".into(), "src/main.rs".into());
        let saved = disk::Tab {
            path: repo.root.clone(),
            message: "initial".into(),
            selection: disk::Selection::Commit("deadbeef".into()),
            expanded: vec![key.clone()],
            show_commit_body: true,
            history_tab: 1,
            history_limit: 600,
            inspection: Some(disk::Inspection::Compare("base".into(), "target".into())),
            ..disk::Tab::default()
        };
        let mut tab = RepoTab {
            message: "typed before snapshot returned".into(),
            restoring: Some(saved),
            ..RepoTab::default()
        };
        let loaded = Loaded::read(
            repo,
            Selection::Work,
            600,
            String::new(),
            false,
            false,
            true,
        )
        .unwrap();
        tab.apply_loaded(loaded, &Selection::Work);
        assert_eq!(tab.selection, Selection::Commit("deadbeef".into()));
        assert!(tab.expanded.contains(&key));
        assert!(tab.show_commit_body);
        assert_eq!(tab.history_tab, 1);
        assert_eq!(tab.message, "typed before snapshot returned");
        assert!(tab.restoring.is_none());
    }
}
