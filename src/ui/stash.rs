use super::*;

#[derive(Clone, Default)]
pub(super) enum Data {
    #[default]
    Loading,
    Ready(Arc<git::StashContext>),
    Error(String),
}
impl GitBuddy {
    pub(super) fn load_stash_modal(&mut self, modal: Modal, cx: &mut Context<Self>) {
        let Some(repo) = self.active.repo.clone() else {
            return;
        };
        self.stash_generation += 1;
        let generation = self.stash_generation;
        let root = repo.root.clone();
        self.stash_data = Data::Loading;
        self.stash_restore_index = false;
        self.stash_selected.clear();
        let save = matches!(modal, Modal::Stash);
        let task = cx
            .background_executor()
            .spawn(async move { repo.stash_context().map(Arc::new) });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                if this.stash_generation != generation
                    || this.active.repo.as_ref().map(|r| &r.root) != Some(&root)
                    || !matches!(this.modal, Some(Modal::Stash | Modal::StashActions(_)))
                {
                    return;
                }
                this.stash_data = match result {
                    Ok(context) => {
                        if save {
                            this.stash_selected =
                                context.files.iter().map(|f| f.path.clone()).collect();
                        }
                        Data::Ready(context)
                    }
                    Err(error) => Data::Error(format!("{error:#}")),
                };
                cx.notify();
            });
        })
        .detach();
    }
    pub(super) fn submit_stash(&mut self, message: String, cx: &mut Context<Self>) {
        let Data::Ready(context) = &self.stash_data else {
            return;
        };
        let paths = context
            .files
            .iter()
            .filter(|f| self.stash_selected.contains(&f.path))
            .map(|f| f.path.clone())
            .collect::<Vec<_>>();
        if paths.is_empty() {
            self.modal_error = Some("Select at least one changed file".into());
            cx.notify();
            return;
        }
        self.perform(
            Operation::SaveStash {
                context: context.clone(),
                paths,
                message,
            },
            cx,
        );
    }
    pub(super) fn stash_modal(&self, modal: Modal, cx: &mut Context<Self>) -> AnyElement {
        let save = matches!(modal, Modal::Stash);
        let mut card = v_flex()
            .w(px(640.))
            .max_h(px(640.))
            .p_4()
            .gap_2()
            .rounded_lg()
            .bg(rgb(PANEL))
            .border_1()
            .border_color(rgb(BORDER))
            .child(
                h_flex()
                    .justify_between()
                    .child(div().font_weight(FontWeight::SEMIBOLD).child(if save {
                        "Stash selected files"
                    } else {
                        "Restore Stash"
                    }))
                    .child(
                        self.button("stash-close", "×")
                            .ghost()
                            .disabled(false)
                            .on_click(cx.listener(|this, _, w, cx| {
                                this.modal = None;
                                this.focus.focus(w, cx);
                                cx.notify();
                            })),
                    ),
            );
        match &self.stash_data {
            Data::Loading => {
                card = card.child("Loading current repository state…");
            }
            Data::Error(error) => {
                card = card.child(
                    div()
                        .text_sm()
                        .text_color(rgb(0xe9a3a9))
                        .child(error.clone()),
                );
            }
            Data::Ready(context) if save => {
                card = card.child(div().text_xs().text_color(rgb(MUTED)).child("Description (optional)"))
                    .child(Input::new(&self.form_a))
                    .child(div().text_xs().text_color(rgb(MUTED)).child("Selected files include their staged, unstaged and untracked contents. Other files keep their current contents and index state."))
                    .child(h_flex().gap_2().child(div().flex_1().text_sm().child(format!("{} / {} files selected", self.stash_selected.len(), context.files.len())))
                        .child(self.button("stash-select-all", "All").ghost().on_click(cx.listener(|this, _, _, cx| {
                            if let Data::Ready(context) = &this.stash_data { this.stash_selected = context.files.iter().map(|f| f.path.clone()).collect(); } cx.notify();
                        })))
                        .child(self.button("stash-select-none", "None").ghost().on_click(cx.listener(|this, _, _, cx| { this.stash_selected.clear(); cx.notify(); }))))
                    .child(v_flex().id("stash-file-picker").max_h(px(340.)).overflow_y_scroll()
                        .children(context.files.iter().enumerate().map(|(i, file)| {
                            let path = file.path.clone();
                            let label = format!("{}  {}{}  {}", if self.stash_selected.contains(&path) { "☑" } else { "☐" }, file.index, file.worktree, path.display());
                            self.sidebar_button(("stash-select-file", i), label).ghost().on_click(cx.listener(move |this, _, _, cx| {
                                if !this.stash_selected.remove(&path) { this.stash_selected.insert(path.clone()); } cx.notify();
                            }))
                        }))
                        .when(context.files.is_empty(), |col| col.child(div().p_2().text_color(rgb(MUTED)).child("No changed files to stash."))))
                    .child(h_flex().justify_end().child(self.button("stash-save-selected", "Stash selected files").primary().disabled(self.busy() || self.stash_selected.is_empty())
                        .on_click(cx.listener(|this, _, _, cx| { this.submit_stash(this.form_a.read(cx).value().to_string(), cx); }))));
            }
            Data::Ready(context) => {
                if let Modal::StashActions(id) = modal {
                    let preview = id.clone();
                    let apply = id.clone();
                    let pop = id.clone();
                    let drop_id = id.clone();
                    let branch_id = id.clone();
                    let apply_context = context.clone();
                    let pop_context = context.clone();
                    let branch_context = context.clone();
                    let reinstate = self.stash_restore_index;
                    card = card.child(div().text_xs().text_color(rgb(MUTED)).child(format!("Stash {}", &id[..8])))
                        .child(self.button("stash-preview", "Preview contents").on_click(cx.listener(move |this, _, _, cx| { this.begin_inspection(inspect::InspectRequest::Stash(preview.clone()), cx); })))
                        .child(self.sidebar_button("stash-reinstate-index", format!("{}  Restore saved index (staged / unstaged split)", if reinstate { "☑" } else { "☐" })).ghost().on_click(cx.listener(|this, _, _, cx| { this.stash_restore_index = !this.stash_restore_index; cx.notify(); })))
                        .child(div().text_xs().text_color(rgb(MUTED)).child("Apply retains the stash. Pop deletes it only after successful restoration. A failed restoration retains the stash; check for restored untracked files before retrying."))
                        .child(h_flex().gap_2()
                            .child(self.button("stash-apply-index", "Apply (keep stash)").on_click(cx.listener(move |this, _, _, cx| { this.perform(Operation::RestoreStash { context: apply_context.clone(), id: apply.clone(), action: git::StashAction::Apply, reinstate_index: reinstate }, cx); })))
                            .child(self.button("stash-pop", "Pop").primary().on_click(cx.listener(move |this, _, _, cx| { this.perform(Operation::RestoreStash { context: pop_context.clone(), id: pop.clone(), action: git::StashAction::Pop, reinstate_index: reinstate }, cx); }))))
                        .child(self.button("stash-create-branch", "Create branch from Stash…").disabled(self.busy() || !context.files.is_empty()).on_click(cx.listener(move |this, _, w, cx| {
                            this.show_modal(Modal::StashBranch(branch_context.clone(), branch_id.clone()), w, cx);
                        })))
                        .when(!context.files.is_empty(), |col| col.child(div().text_xs().text_color(rgb(MUTED)).child("Creating a branch requires a clean working tree and index.")))
                        .child(self.button("stash-delete", "Delete stash…").ghost().on_click(cx.listener(move |this, _, w, cx| {
                            this.show_modal(Modal::Confirm(format!("Permanently delete stash {}?", &drop_id[..8]), Operation::DropStash(drop_id.clone())), w, cx);
                        })));
                }
            }
        }
        if let Some(error) = &self.modal_error {
            card = card.child(
                div()
                    .text_sm()
                    .text_color(rgb(0xe9a3a9))
                    .child(error.clone()),
            );
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
    pub(super) fn stash_preview_view(
        &self,
        preview: &Arc<git::StashPreview>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = preview.id.clone();
        v_flex()
            .flex_1()
            .min_h_0()
            .child(
                v_flex()
                    .px_3()
                    .py_2()
                    .gap_1()
                    .bg(rgb(PANEL))
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                div()
                                    .flex_1()
                                    .truncate()
                                    .text_sm()
                                    .child(preview.message.clone()),
                            )
                            .child(self.button("stash-preview-actions", "Actions…").on_click(
                                cx.listener(move |this, _, w, cx| {
                                    this.show_modal(Modal::StashActions(id.clone()), w, cx);
                                }),
                            )),
                    )
                    .child(div().text_xs().text_color(rgb(MUTED)).child(format!(
                        "{} · base {} · click a file to expand its diff",
                        &preview.id[..8],
                        &preview.base[..8]
                    ))),
            )
            .child(
                list(
                    self.active.scroll.variable(
                        "stash-preview-scroll",
                        preview
                            .sections
                            .iter()
                            .map(|section| 1 + section.files.len().max(1))
                            .sum(),
                    ),
                    cx.processor(|this, mut index: usize, _, cx| {
                        let inspect::InspectState::Ready(inspect::InspectResult::Stash(preview)) =
                            &this.active.inspection
                        else {
                            return div().into_any_element();
                        };
                        for (s, section) in preview.sections.iter().enumerate() {
                            if index == 0 {
                                return div()
                                    .w_full()
                                    .h(px(32.))
                                    .px_3()
                                    .bg(rgb(PANEL))
                                    .text_xs()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(format!(
                                        "{} · {} files · +{} −{}",
                                        section.kind,
                                        section.files.len(),
                                        section.insertions,
                                        section.deletions
                                    ))
                                    .into_any_element();
                            }
                            let count = section.files.len().max(1);
                            if index <= count {
                                let i = index - 1;
                                return section.files.get(i).map_or_else(
                                    || {
                                        div()
                                            .h(px(30.))
                                            .px_3()
                                            .text_xs()
                                            .text_color(rgb(MUTED))
                                            .child("No changes in this section.")
                                            .into_any_element()
                                    },
                                    |file| {
                                        this.stash_file_row(section, s, i, file, cx)
                                            .into_any_element()
                                    },
                                );
                            }
                            index -= count + 1;
                        }
                        div().into_any_element()
                    }),
                )
                .flex_1()
                .min_h_0(),
            )
            .into_any_element()
    }
    fn stash_file_row(
        &self,
        section: &Arc<git::StashSection>,
        s: usize,
        i: usize,
        file: &CommitFile,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let source = PatchSource::Stash(section.clone(), file.clone());
        let key = source.key();
        let open = self.expanded.contains(&key);
        let file_name = file
            .original
            .as_ref()
            .map(|old| format!("{} → {}", old.display(), file.path.display()))
            .unwrap_or_else(|| file.path.display().to_string());
        v_flex()
            .w_full()
            .flex_shrink_0()
            .child(
                h_flex()
                    .id(("stash-file-row", s * 100000 + i))
                    .cursor_pointer()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, _, cx| {
                            this.toggle_patch(source.clone(), cx);
                        }),
                    )
                    .px_3()
                    .h(px(30.))
                    .gap_2()
                    .bg(rgb(EDITOR))
                    .border_b_1()
                    .border_color(rgb(BORDER))
                    .child(div().w(px(20.)).text_color(rgb(MUTED)).child(if open {
                        "▾"
                    } else {
                        "▸"
                    }))
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(ACCENT))
                            .child(file.status.to_string()),
                    )
                    .child(div().flex_1().truncate().text_sm().child(file_name)),
            )
            .when(open, |col| {
                col.child(self.patch_body(&key, ("stash-patch", s * 100000 + i), cx))
            })
    }
}
