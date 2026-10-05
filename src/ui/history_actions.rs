use super::*;

#[derive(Clone, Default)]
pub(super) struct CommitSelection {
    pub ids: BTreeSet<String>,
    pub anchor: Option<String>,
}
impl CommitSelection {
    pub fn select(&mut self, visible: &[String], id: &str, extend: bool, additive: bool) {
        if !visible.iter().any(|v| v == id) {
            return;
        }
        if extend {
            let a = self
                .anchor
                .as_ref()
                .and_then(|a| visible.iter().position(|v| v == a));
            let b = visible.iter().position(|v| v == id).unwrap();
            let a = a.unwrap_or(b);
            if !additive {
                self.ids.clear();
            }
            self.ids
                .extend(visible[a.min(b)..=a.max(b)].iter().cloned());
        } else {
            if additive {
                if !self.ids.remove(id) {
                    self.ids.insert(id.into());
                }
            } else {
                self.ids.clear();
                self.ids.insert(id.into());
            }
            self.anchor = Some(id.into());
        }
    }
}
impl GitBuddy {
    pub(super) fn select_commit(
        &mut self,
        id: String,
        visible: &[String],
        extend: bool,
        additive: bool,
        cx: &mut Context<Self>,
    ) {
        if self.busy() {
            return;
        }
        self.active
            .commit_selection
            .select(visible, &id, extend, additive);
        self.select(Selection::Commit(id), cx);
        cx.notify();
    }
    pub(super) fn begin_reset(&mut self, id: String, w: &mut Window, cx: &mut Context<Self>) {
        let Some(repo) = self.repo.clone() else {
            return;
        };
        self.show_modal(Modal::ResetHistory(None), w, cx);
        self.history_generation += 1;
        let generation = self.history_generation;
        let tab = self.active.id;
        let task = cx
            .background_executor()
            .spawn(async move { repo.prepare_reset(&id) });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                if this.active.id != tab
                    || this.history_generation != generation
                    || !matches!(this.modal, Some(Modal::ResetHistory(_)))
                {
                    return;
                }
                match result {
                    Ok(context) => this.modal = Some(Modal::ResetHistory(Some(Arc::new(context)))),
                    Err(e) => this.modal_error = Some(format!("{e:#}")),
                }
                cx.notify();
            });
        })
        .detach();
    }
    pub(super) fn preview_history_rewrite(
        &mut self,
        ids: Vec<String>,
        action: git::RebaseAction,
        cx: &mut Context<Self>,
    ) {
        if self.busy() {
            return;
        }
        let Some(repo) = self.repo.clone() else {
            return;
        };
        self.modal = Some(Modal::RebasePlan);
        self.management_data = management::Data::Loading;
        self.management_generation += 1;
        let generation = self.management_generation;
        let tab = self.active.id;
        let task = cx
            .background_executor()
            .spawn(async move { repo.history_rewrite_preview(&ids, action) });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                if this.active.id != tab
                    || this.management_generation != generation
                    || !matches!(this.modal, Some(Modal::RebasePlan))
                {
                    return;
                }
                this.management_data = match result {
                    Ok((preview, steps)) => management::Data::Rebase(Arc::new(preview), steps),
                    Err(e) => management::Data::Error(format!("{e:#}")),
                };
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
    pub(super) fn history_actions_modal(&self, modal: Modal, cx: &mut Context<Self>) -> AnyElement {
        let mut card = v_flex()
            .w(px(620.))
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
                            .child(if matches!(modal, Modal::CommitSelection) {
                                "Selected commits"
                            } else {
                                "Reset current HEAD"
                            }),
                    )
                    .child(
                        self.button("history-actions-close", "×")
                            .ghost()
                            .disabled(false)
                            .on_click(cx.listener(|this, _, w, cx| {
                                this.modal = None;
                                this.focus.focus(w, cx);
                                cx.notify();
                            })),
                    ),
            );
        match modal {
            Modal::ResetHistory(Some(context)) => {
                card = card.child(div().text_sm().child(format!("{} → {}",context.head.reference.trim_start_matches("refs/heads/"),context.target)))
                    .child(div().text_xs().text_color(rgb(MUTED)).child("Choose the effect on the index and working files. Original commits remain recoverable; discarded uncommitted edits are not included in the backup."));
                for mode in [
                    git::ResetMode::Soft,
                    git::ResetMode::Mixed,
                    git::ResetMode::Hard,
                ] {
                    let context = context.clone();
                    card = card.child(v_flex().gap_1().p_2().bg(rgb(BG)).rounded_sm()
                        .child(self.button(mode.label(),format!("{} reset…",mode.label())).on_click(cx.listener(move |this,_,w,cx| {
                            this.show_modal(Modal::Confirm(format!("{} reset {} to {}?\n{}\nOriginal commits will be kept in a backup reference and reflog.",mode.label(),context.head.reference,context.target,mode.impact()),Operation::Reset{context:context.clone(),mode}),w,cx);
                        }))).child(div().text_xs().text_color(rgb(if mode == git::ResetMode::Hard {0xe6bc80}else{MUTED})).child(mode.impact())));
                }
            }
            Modal::ResetHistory(None) => {
                card = card.child(
                    div().child(
                        self.modal_error
                            .clone()
                            .unwrap_or_else(|| "Loading Reset preview…".into()),
                    ),
                );
            }
            Modal::CommitSelection => {
                // Use displayed history order rather than lexicographic object IDs.
                let ids: Vec<_> = self
                    .snapshot
                    .commits
                    .iter()
                    .filter(|c| self.commit_selection.ids.contains(&c.id))
                    .map(|c| c.id.clone())
                    .collect();
                let complete = ids.len() == self.commit_selection.ids.len();
                card=card.child(div().text_sm().child(format!("{} commits selected · {} available in loaded history",self.commit_selection.ids.len(),ids.len())))
                    .child(div().text_xs().text_color(rgb(MUTED)).child("Shift selects a visible range; ⌘ / Ctrl toggles a commit. Rewrite plans replay descendants too and require a clean working tree. Squash requires adjacent commits in the current branch."));
                let copy = ids.clone();
                card = card.child(
                    self.button("copy-selected", "Copy selected commit IDs")
                        .disabled(ids.is_empty() || !complete)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(copy.join("\n")));
                            this.modal = None;
                            cx.notify();
                        })),
                );
                let compare = ids.clone();
                card = card.child(
                    self.button("compare-selected", "Compare the two selected commits…")
                        .disabled(ids.len() != 2 || !complete)
                        .on_click(cx.listener(move |this, _, w, cx| {
                            if compare.len() == 2 {
                                this.open_compare(
                                    Some(compare[1].clone()),
                                    Some(compare[0].clone()),
                                    w,
                                    cx,
                                );
                            }
                        })),
                );
                for action in [
                    git::RebaseAction::Squash,
                    git::RebaseAction::Drop,
                    git::RebaseAction::Edit,
                    git::RebaseAction::Split,
                ] {
                    let ids = ids.clone();
                    card = card.child(
                        self.button(
                            action.label(),
                            format!("{} selected commits…", action.label()),
                        )
                        .disabled(
                            ids.is_empty()
                                || !complete
                                || (action == git::RebaseAction::Squash && ids.len() < 2),
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.preview_history_rewrite(ids.clone(), action, cx)
                        })),
                    );
                }
                card = card.child(
                    self.button("clear-selected", "Clear selection")
                        .ghost()
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.active.commit_selection = CommitSelection::default();
                            this.modal = None;
                            cx.notify();
                        })),
                );
            }
            _ => unreachable!(),
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
}

#[cfg(test)]
mod tests {
    use super::CommitSelection;
    #[test]
    fn commit_ranges_follow_visible_order_and_toggle_independently() {
        let ids = ["c", "a", "b", "d"].map(String::from);
        let mut s = CommitSelection::default();
        s.select(&ids, "a", false, false);
        s.select(&ids, "d", true, false);
        assert_eq!(s.ids, ["a", "b", "d"].map(String::from).into());
        s.select(&ids, "b", false, true);
        assert_eq!(s.ids, ["a", "d"].map(String::from).into());
        s.select(&ids, "c", true, true);
        assert_eq!(s.ids, ids.into());
        s.select(&["d".into()], "d", true, false);
        assert_eq!(s.ids, ["d".into()].into());
    }
}
