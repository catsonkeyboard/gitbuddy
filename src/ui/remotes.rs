use super::*;

#[derive(Clone, Default)]
pub(super) enum Data {
    #[default]
    Loading,
    Ready(Arc<git::RemoteState>),
    Error(String),
}
impl GitBuddy {
    pub(super) fn open_remote_page(
        &mut self,
        modal: Modal,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(repo) = self.repo.clone().filter(|_| !self.busy()) else {
            return;
        };
        self.show_modal(modal.clone(), w, cx);
        self.remote_generation += 1;
        self.remote_data = Data::Loading;
        self.push_preparing = false;
        self.restore_push_target = None;
        if matches!(modal, Modal::PushSettings) {
            self.push_choice = git::PushSelection {
                branch: self
                    .snapshot
                    .branches
                    .iter()
                    .find(|b| b.current && !b.remote)
                    .map(|b| b.name.clone()),
                ..git::PushSelection::default()
            };
        }
        let tab = self.active.id;
        let generation = self.remote_generation;
        let task = cx
            .background_executor()
            .spawn(async move { repo.remote_state() });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                if this.active.id != tab
                    || this.remote_generation != generation
                    || !matches!(
                        this.modal,
                        Some(Modal::Remotes | Modal::PushSettings | Modal::Upstream(_))
                    )
                {
                    return;
                }
                this.remote_data = match result {
                    Ok(state) => {
                        if matches!(this.modal, Some(Modal::PushSettings)) {
                            this.push_choice.remote = state.default_push.remote.clone();
                            this.restore_push_target = Some(state.default_push.target.clone());
                        }
                        Data::Ready(Arc::new(state))
                    }
                    Err(e) => Data::Error(format!("{e:#}")),
                };
                cx.notify();
            });
        })
        .detach();
    }
    fn edit_remote_form(
        &mut self,
        remote: git::RemoteInfo,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.show_modal(Modal::EditRemote(remote.clone()), w, cx);
        self.form_a
            .update(cx, |s, cx| s.set_value(remote.url, w, cx));
        self.form_b.update(cx, |s, cx| {
            s.set_value(remote.push_url.unwrap_or_default(), w, cx)
        });
    }
    fn prepare_selected_push(&mut self, cx: &mut Context<Self>) {
        if self.busy() || self.push_preparing || !matches!(self.remote_data, Data::Ready(_)) {
            return;
        }
        let Some(repo) = self.repo.clone() else {
            return;
        };
        let mut selection = self.push_choice.clone();
        selection.target = self.form_a.read(cx).value().trim().to_string();
        self.push_preparing = true;
        self.modal_error = None;
        let generation = self.remote_generation;
        let tab = self.active.id;
        let task = cx
            .background_executor()
            .spawn(async move { repo.prepare_push(selection) });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                if this.active.id != tab || this.remote_generation != generation
                    || !matches!(this.modal, Some(Modal::PushSettings)) { return; }
                this.push_preparing = false;
                match result {
                    Ok(plan) => {
                        let message = format!("{}\n\n{}",
                            plan.summary(),
                            if plan.has_lease() {
                                "Force-with-lease may rewrite branch history. The remote must still match this exact expected commit."
                            } else {
                                "Push these refs? Existing tags are never force-replaced."
                            });
                        this.modal = Some(Modal::Confirm(message, Operation::PushTo(Arc::new(plan))));
                    }
                    Err(e) => this.modal_error = Some(format!("{e:#}")),
                }
                cx.notify();
            });
        }).detach();
        cx.notify();
    }
    pub(super) fn remote_modal(&self, modal: Modal, cx: &mut Context<Self>) -> AnyElement {
        let title = match &modal {
            Modal::Remotes => "Remote management",
            Modal::Upstream(_) => "Set upstream",
            _ => "Push options / Tags",
        };
        let mut card = v_flex()
            .w(px(680.))
            .max_h(px(700.))
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
                        self.button("remote-close", "×")
                            .ghost()
                            .disabled(false)
                            .on_click(cx.listener(|this, _, w, cx| {
                                this.modal = None;
                                this.restore_push_target = None;
                                this.focus.focus(w, cx);
                                cx.notify();
                            })),
                    ),
            );
        card = match &self.remote_data {
            Data::Loading => card.child(div().text_color(rgb(MUTED)).child("Loading…")),
            Data::Error(error) => card
                .child(div().text_color(rgb(0xf0a4aa)).child(error.clone()))
                .child(self.button("reload-remotes", "Reload").on_click(
                    cx.listener(move |this, _, w, cx| this.open_remote_page(modal.clone(), w, cx)),
                )),
            Data::Ready(state) => card.child(match &modal {
                Modal::Remotes => self.remotes_list(state, cx),
                Modal::Upstream(branch) => self.upstream_choices(state, branch, cx),
                _ => self.push_choices(state, cx),
            }),
        };
        if let Some(error) = &self.modal_error {
            card = card.child(
                div()
                    .text_sm()
                    .text_color(rgb(0xf0a4aa))
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
    fn remotes_list(&self, state: &git::RemoteState, cx: &mut Context<Self>) -> AnyElement {
        v_flex().gap_2()
            .child(self.button("add-remote", "Add remote…").on_click(cx.listener(|this, _, w, cx| this.show_modal(Modal::Remote, w, cx))))
            .child(v_flex().id("remote-list").max_h(px(470.)).overflow_y_scroll().gap_2()
                .children(state.remotes.iter().enumerate().map(|(i, remote)| {
                    let edit = remote.clone();
                    let rename = remote.clone();
                    let delete = remote.clone();
                    v_flex().p_3().gap_1().rounded_md().bg(rgb(BG))
                        .child(div().text_color(rgb(ACCENT)).font_weight(FontWeight::SEMIBOLD).child(remote.name.clone()))
                        .child(div().text_xs().child(format!("Fetch: {}", remote.url)))
                        .child(div().text_xs().text_color(rgb(MUTED)).child(format!("Push: {}", remote.destination())))
                        .child(h_flex().gap_2()
                            .child(self.button(("remote-edit", i), "Edit URLs…").on_click(cx.listener(move |this, _, w, cx| this.edit_remote_form(edit.clone(), w, cx))))
                            .child(self.button(("remote-rename", i), "Rename…").on_click(cx.listener(move |this, _, w, cx| {
                                this.show_modal(Modal::RenameRemote(rename.clone()), w, cx);
                                this.form_a.update(cx, |s, cx| s.set_value(rename.name.clone(), w, cx));
                            })))
                            .child(self.button(("remote-delete", i), "Delete…").on_click(cx.listener(move |this, _, w, cx| {
                                this.show_modal(Modal::Confirm(format!("Remove {} and its remote-tracking refs? Local branches and the remote server stay unchanged.", delete.name), Operation::DeleteRemote(delete.clone())), w, cx);
                            }))))
                }))
                .when(state.remotes.is_empty(), |v| v.child(div().text_color(rgb(MUTED)).child("No remotes configured."))))
            .into_any_element()
    }
    fn upstream_choices(
        &self,
        state: &git::RemoteState,
        branch: &str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let current = state
            .upstreams
            .iter()
            .find(|(name, _)| name == branch)
            .map(|(_, upstream)| upstream.as_str())
            .unwrap_or_default();
        let clear = branch.to_owned();
        v_flex().gap_2()
            .child(div().text_sm().child(format!("{branch} → {}", if current.is_empty() { "(none)" } else { current })))
            .child(div().text_xs().text_color(rgb(MUTED)).child("Choose a fetched remote branch or a local branch. Fetch first if the target is missing."))
            .child(self.button("clear-upstream", "Clear upstream").on_click(cx.listener(move |this, _, _, cx| this.perform(Operation::SetUpstream { branch: clear.clone(), upstream: None }, cx))))
            .child(v_flex().id("upstream-choices").max_h(px(400.)).overflow_y_scroll().gap_1()
                .children(self.snapshot.branches.iter().filter(|b| b.name != branch || b.remote).enumerate().map(|(i, b)| {
                    let source = branch.to_owned();
                    let target = format!("refs/{}/{}", if b.remote { "remotes" } else { "heads" }, b.name);
                    self.sidebar_button(("upstream-choice", i), format!("{}  {}", if b.remote { "Remote" } else { "Local" }, b.name))
                        .ghost().on_click(cx.listener(move |this, _, _, cx| this.perform(Operation::SetUpstream { branch: source.clone(), upstream: Some(target.clone()) }, cx)))
                })))
            .into_any_element()
    }
    fn push_choices(&self, state: &git::RemoteState, cx: &mut Context<Self>) -> AnyElement {
        let selected = &self.push_choice;
        let branch_enabled = selected.branch.is_some();
        v_flex().id("push-options-scroll").max_h(px(530.)).overflow_y_scroll().gap_2()
            .child(section("REMOTE"))
            .child(h_flex().flex_wrap().gap_1().children(state.remotes.iter().enumerate().map(|(i, r)| {
                let name = r.name.clone();
                self.button(("push-remote", i), r.name.clone()).ghost()
                    .when(selected.remote == r.name, |b| b.bg(rgb(0x2b3543)).text_color(rgb(ACCENT)))
                    .tooltip(r.destination().to_string()).on_click(cx.listener(move |this, _, _, cx| { this.push_choice.remote = name.clone(); cx.notify(); }))
            })))
            .when(state.remotes.is_empty(), |v| v.child(div().text_color(rgb(MUTED)).child("Add a remote before pushing.")))
            .child(section("LOCAL SOURCE BRANCH"))
            .child(h_flex().flex_wrap().gap_1()
                .child(self.button("tags-only", "Tags only").ghost()
                    .when(!branch_enabled, |b| b.bg(rgb(0x2b3543)).text_color(rgb(ACCENT)))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.push_choice.branch = None; this.push_choice.set_upstream = false; this.push_choice.force_with_lease = false; cx.notify();
                    })))
                .children(self.snapshot.branches.iter().filter(|b| !b.remote).enumerate().map(|(i, b)| {
                    let name = b.name.clone();
                    self.button(("push-source", i), b.name.clone()).ghost()
                        .when(selected.branch.as_ref() == Some(&b.name), |b| b.bg(rgb(0x2b3543)).text_color(rgb(ACCENT)))
                        .on_click(cx.listener(move |this, _, w, cx| {
                            this.push_choice.branch = Some(name.clone());
                            this.form_a.update(cx, |s, cx| s.set_value(name.clone(), w, cx));
                            cx.notify();
                        }))
                })))
            .when(branch_enabled, |v| v
                .child(div().text_xs().text_color(rgb(MUTED)).child("Destination branch (may differ from local name)"))
                .child(Input::new(&self.form_a))
                .child(h_flex().gap_2()
                    .child(self.button("push-upstream", format!("{} Set upstream after success", if selected.set_upstream { "☑" } else { "☐" })).ghost()
                        .on_click(cx.listener(|this, _, _, cx| { this.push_choice.set_upstream = !this.push_choice.set_upstream; cx.notify(); })))
                    .child(self.button("push-lease", format!("{} Force-with-lease", if selected.force_with_lease { "☑" } else { "☐" })).ghost()
                        .when(selected.force_with_lease, |b| b.text_color(rgb(0xf0a4aa)))
                        .on_click(cx.listener(|this, _, _, cx| { this.push_choice.force_with_lease = !this.push_choice.force_with_lease; cx.notify(); }))))
                .when(selected.force_with_lease, |v| v.child(div().text_xs().text_color(rgb(MUTED)).child("Uses the current fetched tracking ID, then pins it for confirmation. No implicit Fetch. An unknown target must not exist remotely."))))
            .child(h_flex().justify_between().child(section("TAGS"))
                .child(h_flex().gap_1()
                    .child(self.button("push-all-tags", "Select all").ghost().on_click(cx.listener(|this, _, _, cx| { this.push_choice.tags = this.snapshot.tags.clone(); cx.notify(); })))
                    .child(self.button("push-no-tags", "Clear").ghost().on_click(cx.listener(|this, _, _, cx| { this.push_choice.tags.clear(); cx.notify(); })))))
            .child(v_flex().id("push-tags").max_h(px(160.)).overflow_y_scroll().gap_1()
                .children(self.snapshot.tags.iter().enumerate().map(|(i, tag)| {
                    let tag = tag.clone();
                    self.sidebar_button(("push-tag-choice", i), format!("{}  {tag}", if selected.tags.contains(&tag) { "☑" } else { "☐" })).ghost()
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if let Some(i) = this.push_choice.tags.iter().position(|t| t == &tag) { this.push_choice.tags.remove(i); } else { this.push_choice.tags.push(tag.clone()); }
                            cx.notify();
                        }))
                })))
            .child(div().text_xs().text_color(rgb(MUTED)).child("Tags use ordinary push protection. A multi-ref push may partially succeed if the server rejects some refs."))
            .child(self.button("prepare-push", if self.push_preparing { "Preparing…" } else { "Review push…" }).primary()
                .disabled(self.busy() || self.push_preparing || selected.remote.is_empty() || (!branch_enabled && selected.tags.is_empty()))
                .on_click(cx.listener(|this, _, _, cx| this.prepare_selected_push(cx))))
            .into_any_element()
    }
}
