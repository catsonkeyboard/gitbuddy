use super::*;

impl GitBuddy {
    pub(super) fn sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut sidebar = v_flex()
            .id("sidebar")
            .w(px(180.))
            .h_full()
            .flex_shrink_0()
            .overflow_y_scroll()
            .bg(rgb(BG))
            .border_r_1()
            .border_color(rgb(BORDER))
            .px_2()
            .py_1()
            .gap_1();
        sidebar = sidebar
            .child(section("WORKSPACE"))
            .child(
                self.sidebar_button(
                    "changes",
                    format!("◉  Working tree       {}", self.snapshot.files.len()),
                )
                .ghost()
                .on_click(cx.listener(|this, _, _, cx| {
                    this.active.history_tab = 0;
                    this.select(Selection::Work, cx);
                })),
            )
            .child(
                h_flex()
                    .mt_2()
                    .justify_between()
                    .child(section("BRANCHES"))
                    .child(
                        self.button("new-branch", "+")
                            .ghost()
                            .justify_start()
                            .on_click(
                                cx.listener(|this, _, w, cx| this.show_modal(Modal::Branch, w, cx)),
                            ),
                    ),
            );
        for (i, branch) in self
            .snapshot
            .branches
            .iter()
            .filter(|b| !b.remote)
            .enumerate()
        {
            let name = branch.name.clone();
            sidebar = sidebar.child(
                self.sidebar_button(
                    ("branch", i),
                    format!(
                        "{}  {}",
                        if branch.current { "●" } else { "⑂" },
                        branch.name
                    ),
                )
                .ghost()
                .when(branch.current, |b| {
                    b.text_color(rgb(ACCENT)).bg(rgb(0x2b3543))
                })
                .on_click(cx.listener(move |this, _, w, cx| {
                    this.show_modal(Modal::BranchActions(name.clone(), false), w, cx)
                })),
            );
        }
        sidebar = sidebar.child(
            h_flex()
                .mt_2()
                .justify_between()
                .child(section("REMOTES"))
                .child(
                    self.button("remote-add", "+")
                        .ghost()
                        .justify_start()
                        .on_click(
                            cx.listener(|this, _, w, cx| this.show_modal(Modal::Remote, w, cx)),
                        ),
                ),
        );
        for (i, branch) in self
            .snapshot
            .branches
            .iter()
            .filter(|b| b.remote)
            .enumerate()
        {
            let name = branch.name.clone();
            sidebar = sidebar.child(
                self.sidebar_button(("remote", i), format!("⑂  {name}"))
                    .ghost()
                    .on_click(cx.listener(move |this, _, w, cx| {
                        this.show_modal(Modal::BranchActions(name.clone(), true), w, cx)
                    })),
            );
        }
        if self.snapshot.remotes.is_empty() {
            sidebar = sidebar.child(
                div()
                    .text_xs()
                    .text_color(rgb(MUTED))
                    .px_2()
                    .child("No remotes configured"),
            );
        }
        sidebar = sidebar.child(
            h_flex()
                .mt_2()
                .justify_between()
                .child(section("STASHES"))
                .child(
                    self.button("stash-new", "+")
                        .ghost()
                        .justify_start()
                        .on_click(
                            cx.listener(|this, _, w, cx| this.show_modal(Modal::Stash, w, cx)),
                        ),
                ),
        );
        for (i, (id, summary)) in self.snapshot.stashes.iter().enumerate() {
            let id = id.clone();
            sidebar = sidebar.child(
                self.sidebar_button(("stash", i), summary.clone())
                    .ghost()
                    .tooltip(summary.clone())
                    .on_click(cx.listener(move |this, _, w, cx| {
                        this.show_modal(Modal::StashActions(id.clone()), w, cx)
                    })),
            );
        }
        if self.snapshot.stashes.is_empty() {
            sidebar = sidebar.child(
                div()
                    .text_xs()
                    .text_color(rgb(MUTED))
                    .px_2()
                    .child("No stashed changes"),
            );
        }
        sidebar = sidebar.child(
            h_flex()
                .mt_2()
                .justify_between()
                .child(section("TAGS"))
                .child(
                    self.button("tag-new", "+")
                        .ghost()
                        .justify_start()
                        .on_click(cx.listener(|this, _, w, cx| this.show_modal(Modal::Tag, w, cx))),
                ),
        );
        for (i, tag) in self.snapshot.tags.iter().enumerate() {
            let tag = tag.clone();
            sidebar = sidebar.child(
                self.sidebar_button(("tag", i), format!("◇  {tag}"))
                    .ghost()
                    .on_click(cx.listener(move |this, _, w, cx| {
                        this.show_modal(Modal::TagActions(tag.clone()), w, cx)
                    })),
            );
        }
        sidebar
            .child(div().h(px(12.)).flex_shrink_0())
            .child(section("RECENT REPOSITORIES"))
            .children(
                self.settings
                    .recent
                    .iter()
                    .take(6)
                    .enumerate()
                    .map(|(i, path)| {
                        let path = path.clone();
                        self.sidebar_button(
                            ("recent-side", i),
                            path.file_name()
                                .unwrap_or_default()
                                .to_string_lossy()
                                .into_owned(),
                        )
                        .ghost()
                        .tooltip(path.display().to_string())
                        .on_click(cx.listener(move |this, _, _, cx| this.open(path.clone(), cx)))
                    }),
            )
    }
}
