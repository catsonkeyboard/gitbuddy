use super::*;

impl GitBuddy {
    pub(super) fn begin_history(
        &mut self,
        kind: HistoryKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.busy || self.active.repo.is_none() {
            return;
        }
        self.reflog_limit = 100;
        self.active.error = false;
        self.restore_amend_message = None;
        self.amend_message
            .update(cx, |state, cx| state.set_value("", window, cx));
        self.show_modal(Modal::History(kind), window, cx);
        self.request_history(kind, cx);
    }

    fn request_history(&mut self, kind: HistoryKind, cx: &mut Context<Self>) {
        let Some(repo) = self.active.repo.clone() else {
            return;
        };
        let root = repo.root.clone();
        self.history_generation += 1;
        let generation = self.history_generation;
        let limit = self.reflog_limit;
        self.history_data = HistoryData::Loading;
        self.reflog_selected = None;
        let task = cx.background_executor().spawn(async move {
            match kind {
                HistoryKind::Amend | HistoryKind::Undo => repo
                    .commit_edit()
                    .map(|edit| HistoryData::Edit(Arc::new(edit))),
                HistoryKind::Reflog => repo
                    .reflog(limit)
                    .map(|page| HistoryData::Reflog(Arc::new(page))),
            }
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                if this.history_generation != generation
                    || !matches!(this.modal, Some(Modal::History(current)) if current == kind)
                    || this.active.repo.as_ref().map(|repo| &repo.root) != Some(&root)
                {
                    return;
                }
                if kind == HistoryKind::Amend
                    && let Ok(HistoryData::Edit(edit)) = &result
                {
                    this.restore_amend_message = Some(edit.message.clone());
                }
                this.history_data =
                    result.unwrap_or_else(|error| HistoryData::Error(format!("{error:#}")));
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    pub(super) fn history_modal(&self, kind: HistoryKind, cx: &mut Context<Self>) -> AnyElement {
        let title = match kind {
            HistoryKind::Amend => "Amend latest commit",
            HistoryKind::Undo => "Undo latest commit",
            HistoryKind::Reflog => "Reflog / Recover",
        };
        let mut card = v_flex()
            .w(px(760.))
            .p_4()
            .gap_3()
            .rounded_lg()
            .bg(rgb(PANEL))
            .border_1()
            .border_color(rgb(BORDER))
            .shadow_lg()
            .child(
                h_flex()
                    .justify_between()
                    .child(
                        div()
                            .text_lg()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(title),
                    )
                    .child(
                        self.button("history-close", "×")
                            .ghost()
                            .on_click(cx.listener(|this, _, w, cx| {
                                this.modal = None;
                                this.focus.focus(w, cx);
                                cx.notify();
                            })),
                    ),
            );
        match &self.history_data {
            HistoryData::Loading => card = card.child("Loading repository history…"),
            HistoryData::Error(error) => {
                card = card.child(div().text_color(rgb(0xf0a4aa)).child(error.clone()))
            }
            HistoryData::Edit(edit) => {
                let edit = edit.clone();
                card = card
                    .child(div().text_xs().font_family("Menlo").child(format!(
                        "{} · {}",
                        edit.head.reference,
                        edit.head.id.as_deref().unwrap_or_default()
                    )))
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(MUTED))
                            .child(format!("Original author: {}", edit.author)),
                    );
                if kind == HistoryKind::Amend {
                    card = card.child(div().text_sm().child("Replace HEAD using the staged snapshot and message below. Original author and parents are preserved; unstaged files and your commit draft stay unchanged."))
                        .child(Textarea::new(&self.amend_message).h(px(170.)));
                } else {
                    card = card.child(div().text_sm().child(edit.message.lines().next().unwrap_or_default().to_string()))
                        .child(div().text_sm().child(if edit.parents.is_empty() {
                            "Return this branch to an initial, uncommitted state. All files and staged content stay in place. The old commit remains recoverable in HEAD's reflog."
                        } else {
                            "Move HEAD to its first parent. Keep the index and working tree exactly as they are, so the undone commit's changes remain staged together with existing staged edits."
                        }));
                }
                card = card.child(div().text_xs().text_color(rgb(0xe6bc80)).child("This rewrites local history. If already pushed, coordinate before updating the remote; this action does not push."))
                    .when(self.active.error, |col| col.child(div().text_xs().text_color(rgb(0xf0a4aa)).child(self.active.notice.clone())))
                    .child(h_flex().justify_end().gap_2()
                        .child(self.button("history-cancel", "Cancel").on_click(cx.listener(|this,_,w,cx| {this.modal=None;this.focus.focus(w,cx);cx.notify();})))
                        .child(self.button("history-apply", if kind == HistoryKind::Amend { "Amend commit" } else { "Undo commit" }).primary()
                            .disabled(self.busy || (kind == HistoryKind::Amend && self.amend_message.read(cx).value().trim().is_empty())
                                || (kind == HistoryKind::Undo && edit.parents.is_empty() && edit.head.reference == "HEAD"))
                            .on_click(cx.listener(move |this,_,_,cx| {
                                let operation = if kind == HistoryKind::Amend {
                                    Operation::Amend { context: edit.clone(), message: this.amend_message.read(cx).value().to_string() }
                                } else { Operation::UndoLast(edit.head.clone()) };
                                this.perform(operation,cx);
                            }))));
                if kind == HistoryKind::Undo && edit_is_detached_root(&self.history_data) {
                    card = card.child(
                        div()
                            .text_xs()
                            .text_color(rgb(MUTED))
                            .child("Create a branch before undoing a detached initial commit."),
                    );
                }
            }
            HistoryData::Reflog(page) => {
                card = card.child(div().text_xs().text_color(rgb(MUTED)).child("Local HEAD movements, newest first. Select Before or After to recover that exact commit. Expired objects cannot be recovered."))
                    .when(!page.entries.is_empty(), |col| col.child(uniform_list("reflog-entries", page.entries.len(), cx.processor(|this, range: std::ops::Range<usize>, _, cx| {
                        let HistoryData::Reflog(page) = &this.history_data else { return Vec::new(); };
                        range.filter(|&i|i<page.entries.len()).map(|i| {
                            let entry=&page.entries[i];
                            v_flex().id(("reflog-row",i)).w_full().h(px(74.)).px_2().py_1().gap_1().border_b_1().border_color(rgb(BORDER))
                                .child(h_flex().gap_2().text_xs()
                                    .child(div().text_color(rgb(ACCENT)).child(entry.selector.clone()))
                                    .child(div().flex_1().truncate().child(entry.message.clone()))
                                    .child(div().text_color(rgb(MUTED)).child(entry.date.clone())))
                                .child(h_flex().gap_2()
                                    .children([("Before",&entry.before),("After",&entry.after)].into_iter().map(|(label,target)| {
                                        let selected=target.as_ref().is_some_and(|t|this.reflog_selected.as_ref().is_some_and(|s|s.id==t.id));
                                        let target=target.clone();
                                        this.button(label,format!("{label}: {}",target.as_ref().map(|t|&t.id[..8]).unwrap_or("none")))
                                            .ghost().disabled(target.as_ref().is_none_or(|t|!t.available))
                                            .when(selected,|button|button.bg(rgb(0x375776)))
                                            .on_click(cx.listener(move|this,_,_,cx|{this.reflog_selected=target.clone();cx.notify();}))
                                    }))
                                    .child(div().flex_1().truncate().text_xs().text_color(rgb(MUTED)).child(entry.committer.clone())))
                                .child(div().text_size(px(10.)).truncate().text_color(rgb(MUTED))
                                    .child(entry.after.as_ref().or(entry.before.as_ref()).map(|t|t.subject.clone()).unwrap_or_default()))
                        }).collect::<Vec<_>>()
                    })).h(px(296.)).w_full()))
                    .when(page.entries.is_empty(),|col|col.child(div().text_color(rgb(MUTED)).child("No local HEAD reflog entries.")))
                    .child(h_flex().justify_between()
                        .child(div().text_xs().text_color(rgb(MUTED)).child(format!("{} of {} entries",page.entries.len(),page.total)))
                        .child(self.button("reflog-reload",if page.entries.len()<page.total {"Load 100 more"} else {"Refresh"}).ghost()
                            .on_click(cx.listener(|this,_,_,cx| {
                                if let HistoryData::Reflog(page) = &this.history_data
                                    && page.entries.len() < page.total
                                {
                                    this.reflog_limit += 100;
                                }
                                this.request_history(HistoryKind::Reflog,cx);
                            }))))
                    .child(self.recovery_actions(page,cx));
            }
        }
        div()
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .bg(rgba(0x00000099))
            .occlude()
            .child(card)
            .into_any_element()
    }

    fn recovery_actions(&self, page: &git::ReflogPage, cx: &mut Context<Self>) -> AnyElement {
        let Some(target) = self.reflog_selected.clone() else {
            return div()
                .text_xs()
                .text_color(rgb(MUTED))
                .child("Choose a commit above to enable recovery actions.")
                .into_any_element();
        };
        let branch_target = target.clone();
        let expected = page.head.clone();
        v_flex().gap_2()
            .child(div().text_xs().truncate().child(format!("Selected: {} · {}",target.id,target.subject)))
            .child(h_flex().justify_end().gap_2()
                .child(self.button("recover-branch","Create recovery branch…").primary().on_click(cx.listener(move|this,_,w,cx|{
                    this.show_modal(Modal::RecoveryBranch(branch_target.clone()),w,cx);
                    this.form_a.update(cx,|state,cx|state.set_value(format!("recovered/{}",&branch_target.id[..8]),w,cx));
                })))
                .child(self.button("restore-head","Restore current branch…").on_click(cx.listener(move|this,_,w,cx|{
                    this.show_modal(Modal::Confirm(format!("Move {} to {}? This only changes the branch position; index and working tree stay unchanged. Their differences against the recovered commit may appear as staged changes. This rewrites local history and does not push.",expected.reference,target.id),Operation::RestoreReflog { expected:expected.clone(), target:target.clone() }),w,cx);
                })))).into_any_element()
    }
}

fn edit_is_detached_root(data: &HistoryData) -> bool {
    matches!(data,HistoryData::Edit(edit) if edit.parents.is_empty() && edit.head.reference == "HEAD")
}
