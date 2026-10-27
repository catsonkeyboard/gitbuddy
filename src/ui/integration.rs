use super::*;

#[derive(Default)]
pub(super) enum Data {
    #[default]
    Loading,
    Error(String),
    Merge(Arc<git::MergePreview>),
    Pull(Arc<git::PullPreview>),
}
impl GitBuddy {
    pub(super) fn open_integration(
        &mut self,
        branch: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.prepare_integration(branch, None, window, cx);
    }
    pub(super) fn open_pull_source(
        &mut self,
        remote: git::RemoteInfo,
        branch: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.prepare_integration(None, Some((remote, branch)), window, cx);
    }
    fn prepare_integration(
        &mut self,
        branch: Option<String>,
        source: Option<(git::RemoteInfo, String)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(repo) = self.repo.clone().filter(|_| !self.busy()) else {
            return;
        };
        self.integration_generation += 1;
        let generation = self.integration_generation;
        let tab = self.active.id;
        self.integration_data = Data::Loading;
        self.merge_mode = git::MergeMode::Auto;
        self.pull_strategy = git::PullStrategy::FastForwardOnly;
        self.show_modal(Modal::Integration, window, cx);
        self.focus.focus(window, cx);
        let task = cx.background_executor().spawn(async move {
            if let Some((remote, branch)) = source {
                return repo
                    .prepare_pull_from(&remote, &branch)
                    .map(|p| Data::Pull(Arc::new(p)));
            }
            match branch {
                Some(branch) => repo
                    .prepare_merge(&branch)
                    .map(|p| Data::Merge(Arc::new(p))),
                None => repo.prepare_pull().map(|p| Data::Pull(Arc::new(p))),
            }
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                if this.active.id != tab
                    || this.integration_generation != generation
                    || !matches!(this.modal, Some(Modal::Integration))
                {
                    return;
                }
                this.integration_data = result.unwrap_or_else(|e| Data::Error(format!("{e:#}")));
                cx.notify();
            });
        })
        .detach();
    }
    pub(super) fn integration_modal(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut card = v_flex()
            .w(px(640.))
            .max_h(px(620.))
            .p_4()
            .gap_3()
            .bg(rgb(PANEL))
            .rounded_lg()
            .border_1()
            .border_color(rgb(BORDER))
            .child(
                h_flex()
                    .gap_2()
                    .child(div().flex_1().font_weight(FontWeight::SEMIBOLD).child(
                        match &self.integration_data {
                            Data::Pull(_) => "Pull strategy",
                            Data::Merge(_) => "Merge strategy",
                            _ => "Merge / Pull options",
                        },
                    ))
                    .child(
                        self.button("integration-close", "×")
                            .disabled(false)
                            .ghost()
                            .on_click(cx.listener(|this, _, w, cx| {
                                this.modal = None;
                                this.focus.focus(w, cx);
                                cx.notify();
                            })),
                    ),
            );
        match &self.integration_data {
            Data::Loading => card = card.child("Preparing operation preview…"),
            Data::Error(error) => {
                card = card.child(
                    div()
                        .text_sm()
                        .text_color(rgb(0xf0a4aa))
                        .child(error.clone()),
                )
            }
            Data::Merge(preview) => {
                card = card
                    .child(
                        div()
                            .text_sm()
                            .child(format!("{} → {}", preview.label, self.snapshot.branch)),
                    )
                    .child(
                        div()
                            .text_xs()
                            .font_family("Menlo")
                            .text_color(rgb(MUTED))
                            .child(format!("Target {}", preview.target)),
                    )
                    .child(
                        h_flex().gap_2().children(
                            [
                                git::MergeMode::Auto,
                                git::MergeMode::NoFastForward,
                                git::MergeMode::Squash,
                            ]
                            .into_iter()
                            .enumerate()
                            .map(|(i, mode)| {
                                self.button(("merge-policy", i), mode.label())
                                    .when(self.merge_mode == mode, |b| {
                                        b.bg(rgb(0x354151)).text_color(rgb(ACCENT))
                                    })
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.merge_mode = mode;
                                        cx.notify();
                                    }))
                            }),
                        ),
                    )
                    .child(div().text_sm().child(self.merge_mode.impact()));
                let context = preview.clone();
                let mode = self.merge_mode;
                card = card.child(
                    h_flex().justify_end().child(
                        self.button(
                            "apply-merge",
                            if mode == git::MergeMode::Squash {
                                "Stage squash changes"
                            } else {
                                "Merge"
                            },
                        )
                        .primary()
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.perform(
                                Operation::MergeWith {
                                    context: context.clone(),
                                    mode,
                                },
                                cx,
                            )
                        })),
                    ),
                );
            }
            Data::Pull(preview) => {
                let destination = preview
                    .remote
                    .as_ref()
                    .map(|r| format!("{} · {}", r.name, gitbuddy::task_log::sanitize(&r.url)))
                    .unwrap_or_else(|| "Local upstream (no network fetch)".into());
                card = card.child(div().text_sm().child(format!("{} ← {}", self.snapshot.branch, preview.upstream)))
                    .child(div().text_xs().text_color(rgb(MUTED)).child(destination))
                    .child(h_flex().gap_2().children([git::PullStrategy::FastForwardOnly, git::PullStrategy::Merge, git::PullStrategy::Rebase].into_iter().enumerate().map(|(i, strategy)| {
                        self.button(("pull-policy", i), strategy.label()).when(self.pull_strategy == strategy, |b| b.bg(rgb(0x354151)).text_color(rgb(ACCENT))).on_click(cx.listener(move |this, _, _, cx| { this.pull_strategy = strategy; cx.notify(); }))
                    })))
                    .child(div().text_sm().child(self.pull_strategy.impact()))
                    .child(div().text_xs().text_color(rgb(MUTED)).child("Uses the upstream tip fetched by this operation. Conflicts pause for review; rebase can be continued or aborted after restart."));
                let context = preview.clone();
                let strategy = self.pull_strategy;
                card = card.child(
                    h_flex().justify_end().child(
                        self.button("apply-pull", "Fetch & pull")
                            .primary()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.perform(
                                    Operation::PullWith {
                                        context: context.clone(),
                                        strategy,
                                    },
                                    cx,
                                )
                            })),
                    ),
                );
            }
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
