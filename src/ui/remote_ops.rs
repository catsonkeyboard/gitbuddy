use super::*;
use anyhow::Context as _;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Purpose {
    Fetch,
    Pull,
    Tags,
}
#[derive(Clone, Default)]
pub(super) enum Data {
    #[default]
    Loading,
    Choices(Purpose, Vec<git::RemoteInfo>),
    Refs(Purpose, Arc<git::RemoteRefs>),
    Error(String),
}
impl GitBuddy {
    pub(super) fn present_remote_result(&mut self) {
        if self.modal.is_none()
            && let Some(data) = self.active.remote_result.take()
        {
            self.remote_ops_data = data;
            self.modal = Some(Modal::RemoteOperations);
        }
    }
    pub(super) fn open_remote_operations(
        &mut self,
        purpose: Purpose,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(repo) = self.repo.clone().filter(|_| !self.busy()) else {
            return;
        };
        if let Some(data) = self.active.remote_result.take() {
            self.remote_ops_data = data;
            self.show_modal(Modal::RemoteOperations, w, cx);
            return;
        }
        self.remote_ops_generation += 1;
        let generation = self.remote_ops_generation;
        let tab = self.active.id;
        self.remote_ops_data = Data::Loading;
        self.show_modal(Modal::RemoteOperations, w, cx);
        self.focus.focus(w, cx);
        let work = cx
            .background_executor()
            .spawn(async move { repo.remote_info() });
        cx.spawn(async move |this, cx| {
            let result = work.await;
            let _ = this.update(cx, |this, cx| {
                if this.active.id != tab
                    || this.remote_ops_generation != generation
                    || !matches!(this.modal, Some(Modal::RemoteOperations))
                {
                    return;
                }
                this.remote_ops_data = result
                    .map(|remotes| Data::Choices(purpose, remotes))
                    .unwrap_or_else(|e| Data::Error(format!("{e:#}")));
                cx.notify();
            });
        })
        .detach();
    }
    fn query_remote(&mut self, purpose: Purpose, remote: git::RemoteInfo, cx: &mut Context<Self>) {
        if self.busy() {
            return;
        }
        let Some(repo) = self.repo.clone() else {
            return;
        };
        let selection = self.selection.clone();
        let limit = self.limit;
        self.modal = None;
        self.dispatch(
            "Query remote branches / tags",
            true,
            move |control| {
                let result = repo.remote_refs(&remote, control);
                let (data, notice, error, cancelled) = match result {
                    Ok(refs) => (
                        Data::Refs(purpose, Arc::new(refs)),
                        "Remote references queried; local refs unchanged".to_owned(),
                        false,
                        false,
                    ),
                    Err(e) => {
                        let cancelled = git::is_cancelled(&e);
                        let text = format!("{e:#}");
                        (Data::Error(text.clone()), text, !cancelled, cancelled)
                    }
                };
                let mut loaded = Loaded::read(repo, selection, limit, notice, error, false, false)?;
                loaded.remote_result = Some(data);
                loaded.cancelled = cancelled;
                Ok(loaded)
            },
            cx,
        );
    }
    fn fetch_source(
        &mut self,
        remote: &git::RemoteInfo,
        branch: Option<&str>,
        cx: &mut Context<Self>,
    ) {
        let result = self
            .repo
            .as_ref()
            .context("Open a repository")
            .and_then(|r| r.prepare_fetch(remote, branch));
        match result {
            Ok(plan) => self.perform(Operation::FetchFrom(Arc::new(plan)), cx),
            Err(e) => {
                self.modal_error = Some(format!("{e:#}"));
                cx.notify();
            }
        }
    }
    pub(super) fn remote_operations_modal(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut card = v_flex()
            .w(px(700.))
            .p_4()
            .gap_3()
            .bg(rgb(PANEL))
            .rounded_lg()
            .border_1()
            .border_color(rgb(BORDER))
            .child(
                h_flex()
                    .child(
                        div()
                            .flex_1()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("Remote operations"),
                    )
                    .child(
                        self.button("remote-ops-close", "×")
                            .ghost()
                            .disabled(false)
                            .on_click(cx.listener(|this, _, w, cx| {
                                this.modal = None;
                                this.focus.focus(w, cx);
                                cx.notify();
                            })),
                    ),
            );
        card = card.child(
            h_flex().gap_2().children(
                [
                    (Purpose::Fetch, "Fetch source…"),
                    (Purpose::Pull, "Pull source…"),
                    (Purpose::Tags, "Remote tags…"),
                ]
                .into_iter()
                .enumerate()
                .map(|(i, (purpose, label))| {
                    self.button(("remote-purpose", i), label)
                        .on_click(cx.listener(move |this, _, w, cx| {
                            this.open_remote_operations(purpose, w, cx)
                        }))
                }),
            ),
        );
        match &self.remote_ops_data {
            Data::Loading => card = card.child("Loading remote configuration…"),
            Data::Error(error) => {
                card = card.child(
                    div()
                        .text_sm()
                        .text_color(rgb(0xf0a4aa))
                        .child(error.clone()),
                )
            }
            Data::Choices(purpose, remotes) => {
                let purpose = *purpose;
                card = card.child(div().text_sm().child(match purpose { Purpose::Fetch => "Choose a remote to fetch, or query its branches.", Purpose::Pull => "Choose a remote, query its branches, then select the source to integrate into the current branch.", Purpose::Tags => "Choose a remote to query the server's tags. Local tags are unchanged." }));
                if purpose == Purpose::Fetch {
                    card = card.child(
                        self.button("fetch-all-remotes", "Fetch all remotes")
                            .on_click(
                                cx.listener(|this, _, _, cx| this.perform(Operation::Fetch, cx)),
                            ),
                    );
                }
                if purpose == Purpose::Pull {
                    card = card.child(
                        self.button("pull-configured", "Use configured upstream…")
                            .on_click(
                                cx.listener(|this, _, w, cx| this.open_integration(None, w, cx)),
                            ),
                    );
                }
                if remotes.is_empty() {
                    card = card.child("No remotes configured. Add one in Remote management.");
                }
                for (i, remote) in remotes.iter().enumerate() {
                    let query = remote.clone();
                    let fetch = remote.clone();
                    card = card.child(
                        h_flex()
                            .gap_2()
                            .child(div().flex_1().min_w_0().text_sm().truncate().child(format!(
                                "{} · {}",
                                remote.name,
                                gitbuddy::task_log::sanitize(&remote.url)
                            )))
                            .when(purpose == Purpose::Fetch, |row| {
                                row.child(
                                    self.button(("fetch-remote", i), "Fetch remote").on_click(
                                        cx.listener(move |this, _, _, cx| {
                                            this.fetch_source(&fetch, None, cx)
                                        }),
                                    ),
                                )
                            })
                            .child(
                                self.button(
                                    ("query-remote", i),
                                    if purpose == Purpose::Tags {
                                        "Query tags"
                                    } else {
                                        "Query branches"
                                    },
                                )
                                .on_click(cx.listener(
                                    move |this, _, _, cx| {
                                        this.query_remote(purpose, query.clone(), cx)
                                    },
                                )),
                            ),
                    );
                }
            }
            Data::Refs(purpose, refs) => {
                let purpose = *purpose;
                let refresh = refs.remote.clone();
                card = card.child(
                    h_flex()
                        .gap_2()
                        .child(div().flex_1().min_w_0().truncate().text_sm().child(format!(
                            "{} · {}",
                            refs.remote.name,
                            gitbuddy::task_log::sanitize(&refs.remote.url)
                        )))
                        .child(self.button("refresh-remote-refs", "Query again").on_click(
                            cx.listener(move |this, _, _, cx| {
                                this.query_remote(purpose, refresh.clone(), cx)
                            }),
                        )),
                );
                card = card.child(div().text_xs().text_color(rgb(MUTED)).child(if purpose==Purpose::Tags {"Server-advertised tag objects. Delete affects the remote only; the queried ID must still match."} else {"One-off source; upstream configuration stays unchanged. Unmapped branches fetch to FETCH_HEAD only. Select a branch below."}));
                let count = if purpose == Purpose::Tags {
                    refs.tags.len()
                } else {
                    refs.branches.len()
                };
                card = card.child(div().text_xs().child(format!(
                    "{count} {}",
                    if purpose == Purpose::Tags {
                        "remote tags"
                    } else {
                        "remote branches"
                    }
                )));
                card=card.child(uniform_list("remote-ref-list",count,cx.processor(|this,range:std::ops::Range<usize>,_,cx| {
                    let Data::Refs(purpose,refs)=&this.remote_ops_data else {return Vec::new();};
                    let purpose=*purpose;
                    let rows=if purpose==Purpose::Tags {&refs.tags} else {&refs.branches};
                    range.filter_map(|i| rows.get(i).map(|r| {
                        let name=r.name.clone(); let remote=refs.remote.clone(); let listing=refs.clone();
                        h_flex().w_full().h(px(36.)).gap_2().px_2().border_b_1().border_color(rgb(BORDER))
                            .child(div().flex_1().min_w_0().text_sm().truncate().child(format!("{}{}",r.name,if r.peeled.is_some() {" · annotated"} else {""})))
                            .child(div().text_xs().font_family("Menlo").text_color(rgb(MUTED)).child(r.id[..8].to_owned()))
                            .child(this.button(("remote-ref-action",i),match purpose {Purpose::Fetch=>"Fetch branch",Purpose::Pull=>"Pull branch…",Purpose::Tags=>"Delete remote tag…"}).on_click(cx.listener(move|this,_,w,cx|match purpose {
                                Purpose::Fetch=>this.fetch_source(&remote,Some(&name),cx),
                                Purpose::Pull=>this.open_pull_source(remote.clone(),name.clone(),w,cx),
                                Purpose::Tags=> {
                                    let result=this.repo.as_ref().context("Open a repository").and_then(|repo|repo.prepare_remote_tag_delete(&listing,&name));
                                    match result {Ok(plan)=>this.show_modal(Modal::Confirm(format!("Delete remote tag {} / {}?\n\nEndpoint: {}\nExpected tag object: {}\nLocal tags are preserved. The server must still have this exact object.",plan.remote.name,plan.name,gitbuddy::task_log::sanitize(plan.remote.destination()),plan.id),Operation::DeleteRemoteTag(Arc::new(plan))),w,cx),Err(e)=>{this.modal_error=Some(format!("{e:#}"));cx.notify();}}
                                }
                            }))).into_any_element()
                    })).collect::<Vec<_>>()
                })).w_full().h(px(count.clamp(1, 8) as f32 * 36.)));
            }
        }
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
            .occlude()
            .bg(rgba(0x00000099))
            .flex()
            .items_center()
            .justify_center()
            .child(card)
            .into_any_element()
    }
}
