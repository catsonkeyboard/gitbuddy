use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ResultChoice {
    Edited,
    Ours,
    Theirs,
    Worktree,
    Delete,
}
#[derive(Clone)]
pub(super) struct ConflictDraft {
    pub context: Arc<git::ConflictContext>,
    pub text: String,
    pub choice: ResultChoice,
    pub dirty: bool,
}
#[derive(Clone, Default)]
pub(super) struct ConflictViewState {
    pub session: Option<Arc<git::ConflictSession>>,
    pub selected: Option<PathBuf>,
    pub drafts: HashMap<PathBuf, ConflictDraft>,
    loading: bool,
    loading_file: bool,
    error: Option<String>,
}
impl GitBuddy {
    pub(super) fn open_conflicts(&mut self, path: Option<PathBuf>, cx: &mut Context<Self>) {
        if self.busy() || self.repo.is_none() {
            return;
        }
        self.modal = None;
        self.active.selection = Selection::Conflicts;
        self.active.commit_detail = None;
        self.active.expanded.clear();
        self.active.patches.clear();
        self.patch_generation += 1;
        self.selection_generation += 1;
        self.reload_conflicts(path, cx);
        self.restore_conflict_editor();
    }
    pub(super) fn restore_conflict_editor(&mut self) {
        self.restore_conflict = self
            .active
            .conflicts
            .selected
            .as_ref()
            .and_then(|p| self.active.conflicts.drafts.get(p))
            .map(|d| d.text.clone());
    }
    pub(super) fn reload_conflicts(&mut self, preferred: Option<PathBuf>, cx: &mut Context<Self>) {
        let Some(repo) = self.repo.clone() else {
            return;
        };
        self.conflict_generation += 1;
        let generation = self.conflict_generation;
        let root = repo.root.clone();
        self.active.conflicts.loading = true;
        self.active.conflicts.loading_file = false;
        let task = cx
            .background_executor()
            .spawn(async move { repo.conflict_session() });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                if this.conflict_generation != generation
                    || this.repo.as_ref().map(|r| &r.root) != Some(&root)
                    || !matches!(this.selection, Selection::Conflicts)
                {
                    return;
                }
                this.active.conflicts.loading = false;
                match result {
                    Ok(session) => {
                        let desired = preferred.or_else(|| this.active.conflicts.selected.clone());
                        let path = desired
                            .filter(|p| session.files.iter().any(|f| &f.path == p))
                            .or_else(|| session.files.first().map(|f| f.path.clone()));
                        this.active
                            .conflicts
                            .drafts
                            .retain(|p, _| session.files.iter().any(|f| &f.path == p));
                        this.active.conflicts.session = Some(Arc::new(session));
                        this.active.conflicts.error = None;
                        if let Some(path) = path {
                            if this.active.conflicts.selected.as_ref() != Some(&path)
                                || !this.active.conflicts.drafts.contains_key(&path)
                            {
                                this.choose_conflict(path, false, cx);
                            }
                        } else {
                            this.active.conflicts.selected = None;
                        }
                    }
                    Err(e) => this.active.conflicts.error = Some(format!("{e:#}")),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
    fn choose_conflict(&mut self, path: PathBuf, reload: bool, cx: &mut Context<Self>) {
        if self.busy() || self.active.conflicts.loading {
            return;
        }
        self.active.conflicts.selected = Some(path.clone());
        self.active.conflicts.error = None;
        self.conflict_generation += 1;
        self.active.conflicts.loading_file = false;
        if !reload && self.active.conflicts.drafts.contains_key(&path) {
            self.restore_conflict_editor();
            cx.notify();
            return;
        }
        let Some(repo) = self.repo.clone() else {
            return;
        };
        let generation = self.conflict_generation;
        let root = repo.root.clone();
        let request = path.clone();
        self.active.conflicts.loading_file = true;
        let task = cx
            .background_executor()
            .spawn(async move { repo.conflict_context(&request) });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                if this.conflict_generation != generation
                    || this.repo.as_ref().map(|r| &r.root) != Some(&root)
                    || !matches!(this.selection, Selection::Conflicts)
                {
                    return;
                }
                this.active.conflicts.loading_file = false;
                match result {
                    Ok(context) => {
                        if reload {
                            this.active.error = false;
                            this.active.notice = "Conflict file reloaded.".into();
                        }
                        let mut draft = ConflictDraft {
                            text: context.result.clone().unwrap_or_default(),
                            context: Arc::new(context),
                            choice: ResultChoice::Edited,
                            dirty: false,
                        };
                        if reload
                            && let Some(previous) = this.active.conflicts.drafts.get(&path)
                            && previous.dirty
                        {
                            if previous.choice == ResultChoice::Edited {
                                draft.text = previous.text.clone();
                                draft.dirty = true;
                                this.active.notice =
                                    "Conflict reloaded; unsaved edits kept. Review before saving."
                                        .into();
                            } else {
                                this.active.notice =
                                    "Conflict reloaded; choose a complete version again before saving."
                                        .into();
                            }
                        }
                        this.active.conflicts.drafts.insert(path, draft);
                        this.restore_conflict_editor();
                    }
                    Err(e) => this.active.conflicts.error = Some(format!("{e:#}")),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
    fn choose_result(&mut self, choice: ResultChoice, cx: &mut Context<Self>) {
        if self.busy() {
            return;
        }
        let Some(path) = self.active.conflicts.selected.clone() else {
            return;
        };
        let Some(draft) = self.active.conflicts.drafts.get_mut(&path) else {
            return;
        };
        draft.text = match choice {
            ResultChoice::Ours => draft
                .context
                .ours
                .as_ref()
                .and_then(|s| s.text.clone())
                .unwrap_or_default(),
            ResultChoice::Theirs => draft
                .context
                .theirs
                .as_ref()
                .and_then(|s| s.text.clone())
                .unwrap_or_default(),
            ResultChoice::Worktree | ResultChoice::Edited => {
                draft.context.result.clone().unwrap_or_default()
            }
            ResultChoice::Delete => String::new(),
        };
        draft.choice = choice;
        draft.dirty = true;
        self.restore_conflict_editor();
        cx.notify();
    }
    fn save_conflict(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(path) = self.active.conflicts.selected.as_ref() else {
            return;
        };
        let Some(draft) = self.active.conflicts.drafts.get(path) else {
            return;
        };
        let resolution = match draft.choice {
            ResultChoice::Edited => git::ConflictResolution::Edited(draft.text.clone()),
            ResultChoice::Ours => git::ConflictResolution::Ours,
            ResultChoice::Theirs => git::ConflictResolution::Theirs,
            ResultChoice::Delete => git::ConflictResolution::Delete,
            ResultChoice::Worktree => git::ConflictResolution::Worktree,
        };
        let op = Operation::ResolveConflict {
            context: draft.context.clone(),
            resolution,
        };
        let deletes = draft.choice == ResultChoice::Delete
            || (draft.choice == ResultChoice::Ours && draft.context.ours.is_none())
            || (draft.choice == ResultChoice::Theirs && draft.context.theirs.is_none())
            || (draft.choice == ResultChoice::Worktree && !draft.context.worktree_exists);
        if deletes {
            self.show_modal(Modal::Confirm(format!("Resolve {} as deleted? The file will be removed from the working tree and index.", path.display()), op), window, cx);
        } else {
            self.perform(op, cx);
        }
    }
    pub(super) fn conflicts_view(&self, cx: &mut Context<Self>) -> AnyElement {
        let session = self.active.conflicts.session.clone();
        let count = session.as_ref().map(|s| s.files.len()).unwrap_or(0);
        let operation = session.as_ref().map(|s| s.operation).unwrap_or_default();
        v_flex().flex_1().min_h_0()
            .child(h_flex().h(px(38.)).px_3().gap_2().bg(rgb(PANEL)).border_b_1().border_color(rgb(BORDER))
                .child(div().flex_1().text_xs().child(format!("{} · {count} unresolved files", operation.label())))
                .child(self.button("reload-conflicts","Refresh list").ghost().disabled(self.busy()).on_click(cx.listener(|this,_,_,cx|this.reload_conflicts(None,cx))))
                .when(operation.can_continue(), |row| row.child(self.button("continue-conflict",format!("Continue {}…", operation.label())).primary().disabled(self.busy() || count != 0 || self.active.conflicts.loading)
                    .on_click(cx.listener(|this,_,w,cx| { if let Some(session)=this.active.conflicts.session.clone() {this.show_modal(Modal::Confirm(format!("Complete {} using all staged changes?\n\n{}",session.operation.label(),session.message.trim()),Operation::ContinueConflict(session)),w,cx);} }))))
                .when(operation.can_continue(), |row| row.child(self.button("abort-conflict","Abort…").disabled(self.busy()).on_click(cx.listener(|this,_,w,cx| {
                    let op = match this.active.conflicts.session.as_ref().map(|s|s.operation) { Some(git::ConflictOperation::CherryPick)=>Operation::AbortCherryPick,Some(git::ConflictOperation::Revert)=>Operation::AbortRevert,_=>Operation::AbortMerge };
                    this.show_modal(Modal::Confirm("Abort this operation and reset tracked files to HEAD? Saved resolutions and other tracked edits made during the operation will be discarded.".into(),op),w,cx);
                })))))
            .when_some(self.active.conflicts.error.clone(), |col,error| col.child(div().px_3().py_2().text_xs().text_color(rgb(0xe9a3a9)).child(error)))
            .when(self.active.error, |col| col.child(div().px_3().py_2().text_xs().text_color(rgb(0xe9a3a9)).child(self.active.notice.clone())))
            .child(h_flex().flex_1().min_h_0().items_start()
                .child(v_flex().id("conflict-files").w(px(170.)).h_full().flex_shrink_0().overflow_y_scroll().bg(rgb(BG)).border_r_1().border_color(rgb(BORDER))
                    .children(session.iter().flat_map(|s| s.files.iter()).enumerate().map(|(i,file)| {
                        let path = file.path.clone();
                        let selected = self.active.conflicts.selected.as_ref() == Some(&path);
                        let dirty = self.active.conflicts.drafts.get(&path).is_some_and(|d|d.dirty);
                        v_flex().id(("conflict-choice",i)).p_2().gap_1().cursor_pointer().bg(rgb(if selected {0x344251} else {BG})).border_b_1().border_color(rgb(BORDER))
                            .on_click(cx.listener(move|this,_,_,cx|this.choose_conflict(path.clone(),false,cx)))
                            .child(div().text_xs().truncate().child(format!("{}{}",file.path.display(),if dirty {" •"} else {""})))
                            .child(div().text_size(px(10.)).text_color(rgb(MUTED)).child(file.kind.clone()))
                    })))
                .child(if let Some(path)=&self.active.conflicts.selected {
                    if self.active.conflicts.loading_file { div().p_3().child("Loading conflict versions…").into_any_element() }
                    else if let Some(draft)=self.active.conflicts.drafts.get(path) { self.conflict_editor_view(draft,cx) }
                    else { div().p_3().text_color(rgb(MUTED)).child("Select another file, or refresh the list after resolving externally.").into_any_element() }
                } else {
                    v_flex().flex_1().h_full().items_center().justify_center().gap_2()
                        .child(if self.active.conflicts.loading {"Loading conflicts…"} else if operation.can_continue() {"All conflicts resolved. Review staged changes, then continue."} else {"No unresolved index conflicts."})
                        .child(self.button("review-staged","Review working tree").ghost().on_click(cx.listener(|this,_,_,cx|this.select(Selection::Work,cx))))
                        .into_any_element()
                }))
            .into_any_element()
    }
    fn conflict_editor_view(&self, draft: &ConflictDraft, cx: &mut Context<Self>) -> AnyElement {
        let context = &draft.context;
        let path = context.file.path.clone();
        let deletion = draft.choice == ResultChoice::Delete
            || (draft.choice == ResultChoice::Ours && context.ours.is_none())
            || (draft.choice == ResultChoice::Theirs && context.theirs.is_none())
            || (draft.choice == ResultChoice::Worktree && !context.worktree_exists);
        let has_markers = matches!(draft.choice, ResultChoice::Edited | ResultChoice::Worktree)
            && git::has_conflict_markers(&draft.text);
        let can_save = context.file.supported
            && (!matches!(draft.choice, ResultChoice::Edited) || context.editable)
            && !has_markers;
        let description = if has_markers {
            "Result still contains conflict markers.".to_owned()
        } else if deletion {
            "Result: delete this file.".to_owned()
        } else if !context.file.supported {
            context.description.clone()
        } else {
            match draft.choice {
                ResultChoice::Edited => context.description.clone(),
                ResultChoice::Ours => "Result: complete Ours version selected.".into(),
                ResultChoice::Theirs => "Result: complete Theirs version selected.".into(),
                ResultChoice::Worktree => "Result: keep the working file exactly as loaded.".into(),
                ResultChoice::Delete => unreachable!(),
            }
        };
        v_flex().flex_1().min_w_0().h_full().min_h_0()
            .child(h_flex().px_3().py_1().gap_2().border_b_1().border_color(rgb(BORDER))
                .child(div().flex_1().min_w_0().truncate().text_sm().child(format!("{}{}",path.display(),if draft.dirty {" · unsaved result"} else {""})))
                .child(self.button("reload-conflict-file","Reload file").ghost().disabled(self.busy()).on_click(cx.listener(move|this,_,_,cx|this.choose_conflict(path.clone(),true,cx)))))
            .child(h_flex().h(px(200.)).w_full().items_start().border_b_1().border_color(rgb(BORDER))
                .child(conflict_side("Ours · current side",context.ours.as_ref(),"ours-lines",0x2b423d))
                .child(conflict_side("Base · ancestor",context.base.as_ref(),"base-lines",0x303846))
                .child(conflict_side("Theirs · incoming side",context.theirs.as_ref(),"theirs-lines",0x44353b)))
            .child(h_flex().px_3().py_1().gap_1().bg(rgb(PANEL))
                .child(self.button("use-ours",if context.ours.is_some(){"Use ours"}else{"Ours: delete"}).ghost().disabled(self.busy()||!context.file.supported).on_click(cx.listener(|this,_,_,cx|this.choose_result(ResultChoice::Ours,cx))))
                .child(self.button("use-theirs",if context.theirs.is_some(){"Use theirs"}else{"Theirs: delete"}).ghost().disabled(self.busy()||!context.file.supported).on_click(cx.listener(|this,_,_,cx|this.choose_result(ResultChoice::Theirs,cx))))
                .child(self.button("use-worktree","Use working file").ghost().disabled(self.busy()||!context.file.supported).on_click(cx.listener(|this,_,_,cx|this.choose_result(ResultChoice::Worktree,cx))))
                .child(self.button("resolve-deletion","Delete result").ghost().disabled(self.busy()||!context.file.supported).on_click(cx.listener(|this,_,_,cx|this.choose_result(ResultChoice::Delete,cx)))))
            .child(h_flex().px_3().py_1().gap_2()
                .child(div().flex_1().text_xs().text_color(rgb(if has_markers {0xe9a3a9}else{MUTED})).child(description))
                .child(self.button("save-conflict","Save & mark resolved").primary().disabled(self.busy()||self.active.conflicts.loading||self.active.conflicts.loading_file||!can_save).on_click(cx.listener(|this,_,w,cx|this.save_conflict(w,cx)))))
            .child(if context.editable && !deletion {
                Editor::new(&self.conflict_editor).aria_label("Conflict result").appearance(false).bordered(false).readonly(self.busy()).h(relative(1.)).flex_1().min_h_0().into_any_element()
            } else {
                div().flex_1().p_3().text_xs().text_color(rgb(MUTED)).child(if deletion {"The file will be removed when you save."}else{"The selected complete version will be saved without text conversion. You can also edit externally, reload, and choose Use working file."}).into_any_element()
            })
            .into_any_element()
    }
}
fn conflict_side(
    title: &'static str,
    side: Option<&git::ConflictSide>,
    id: &'static str,
    background: u32,
) -> AnyElement {
    let lines = side.map(|s| s.lines.clone()).unwrap_or_default();
    v_flex()
        .flex_1()
        .min_w_0()
        .h_full()
        .border_r_1()
        .border_color(rgb(BORDER))
        .child(
            div()
                .px_2()
                .py_1()
                .text_xs()
                .font_weight(FontWeight::SEMIBOLD)
                .bg(rgb(background))
                .child(title),
        )
        .child(
            div()
                .px_2()
                .py_1()
                .text_size(px(10.))
                .text_color(rgb(MUTED))
                .truncate()
                .child(
                    side.map(|s| format!("{} · {}", s.path.display(), s.description))
                        .unwrap_or_else(|| "File absent on this side".into()),
                ),
        )
        .child(
            uniform_list(
                id,
                lines.len().max(1),
                move |range: std::ops::Range<usize>, _, _| {
                    range
                        .map(|i| {
                            h_flex()
                                .h(px(20.))
                                .font_family("Menlo")
                                .text_size(px(11.))
                                .child(
                                    div()
                                        .w(px(32.))
                                        .pr_2()
                                        .text_right()
                                        .text_color(rgb(MUTED))
                                        .child(if lines.is_empty() {
                                            String::new()
                                        } else {
                                            (i + 1).to_string()
                                        }),
                                )
                                .child(
                                    div().whitespace_nowrap().child(
                                        lines
                                            .get(i)
                                            .cloned()
                                            .unwrap_or_else(|| "No text preview".into()),
                                    ),
                                )
                        })
                        .collect::<Vec<_>>()
                },
            )
            .with_horizontal_sizing_behavior(ListHorizontalSizingBehavior::Unconstrained)
            .flex_1()
            .min_h_0(),
        )
        .into_any_element()
}
