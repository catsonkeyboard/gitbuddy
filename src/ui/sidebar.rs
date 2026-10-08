use super::*;

pub(super) enum Row {
    Section(&'static str),
    Work,
    Branch(usize),
    Remote(usize),
    Stash(usize),
    Tag(usize),
    Empty(&'static str),
}
pub(super) fn rows(snapshot: &Snapshot) -> Vec<Row> {
    let mut rows = vec![
        Row::Section("WORKSPACE"),
        Row::Work,
        Row::Section("BRANCHES"),
    ];
    rows.extend(
        snapshot
            .branches
            .iter()
            .enumerate()
            .filter(|(_, b)| !b.remote)
            .map(|(i, _)| Row::Branch(i)),
    );
    rows.push(Row::Section("REMOTES"));
    rows.extend((0..snapshot.remotes.len()).map(Row::Remote));
    rows.extend(
        snapshot
            .branches
            .iter()
            .enumerate()
            .filter(|(_, b)| b.remote)
            .map(|(i, _)| Row::Branch(i)),
    );
    if snapshot.remotes.is_empty() {
        rows.push(Row::Empty("No remotes configured"));
    }
    rows.push(Row::Section("STASHES"));
    rows.extend((0..snapshot.stashes.len()).map(Row::Stash));
    if snapshot.stashes.is_empty() {
        rows.push(Row::Empty("No stashed changes"));
    }
    rows.push(Row::Section("TAGS"));
    rows.extend((0..snapshot.tags.len()).map(Row::Tag));
    rows
}
impl GitBuddy {
    pub(super) fn sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let projection = self.active.projection();
        let count = projection.sidebar.len();
        v_flex()
            .w(px(180.))
            .h_full()
            .flex_shrink_0()
            .bg(rgb(BG))
            .border_r_1()
            .border_color(rgb(BORDER))
            .px_2()
            .py_1()
            .child(
                uniform_list(
                    "sidebar",
                    count + self.settings.recent.len().min(6) + 1,
                    cx.processor(move |this, range: std::ops::Range<usize>, _, cx| {
                        range
                            .map(|i| {
                                let row = if let Some(row) = projection.sidebar.get(i) {
                                    this.sidebar_row(row, cx)
                                } else if i == count {
                                    section("RECENT REPOSITORIES").into_any_element()
                                } else if let Some(path) = this.settings.recent.get(i - count - 1) {
                                    let path = path.clone();
                                    this.sidebar_button(
                                        ("recent-side", i),
                                        path.file_name()
                                            .unwrap_or_default()
                                            .to_string_lossy()
                                            .into_owned(),
                                    )
                                    .disabled(false)
                                    .ghost()
                                    .tooltip(path.display().to_string())
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.open(path.clone(), cx)
                                    }))
                                    .into_any_element()
                                } else {
                                    div().into_any_element()
                                };
                                div().h(px(28.)).w_full().child(row)
                            })
                            .collect::<Vec<_>>()
                    }),
                )
                .track_scroll(&self.active.scroll.list("sidebar"))
                .flex_1()
                .min_h_0(),
            )
    }
    fn sidebar_row(&self, row: &Row, cx: &mut Context<Self>) -> AnyElement {
        match row {
            Row::Section(label) => {
                let label = *label;
                h_flex()
                    .w_full()
                    .h(px(28.))
                    .child(section(label))
                    .child(div().flex_1())
                    .when(label == "REMOTES", |row| {
                        row.child(
                            self.button("remote-manage", "…")
                                .ghost()
                                .tooltip("Manage remotes")
                                .on_click(cx.listener(|this, _, w, cx| {
                                    this.open_remote_page(Modal::Remotes, w, cx)
                                })),
                        )
                    })
                    .when(label != "WORKSPACE", |row| {
                        row.child(
                            self.button(SharedString::from(format!("section-add-{label}")), "+")
                                .ghost()
                                .on_click(cx.listener(move |this, _, w, cx| {
                                    this.show_modal(
                                        match label {
                                            "BRANCHES" => Modal::Branch,
                                            "REMOTES" => Modal::Remote,
                                            "STASHES" => Modal::Stash,
                                            _ => Modal::Tag,
                                        },
                                        w,
                                        cx,
                                    )
                                })),
                        )
                    })
                    .into_any_element()
            }
            Row::Work => self
                .sidebar_button(
                    "changes",
                    format!("◉  Working tree       {}", self.snapshot.files.len()),
                )
                .ghost()
                .on_click(cx.listener(|this, _, _, cx| {
                    this.active.history_tab = 0;
                    this.select(Selection::Work, cx);
                }))
                .into_any_element(),
            Row::Branch(i) => {
                let Some(branch) = self.snapshot.branches.get(*i) else {
                    return div().into_any_element();
                };
                let name = branch.name.clone();
                let remote = branch.remote;
                self.sidebar_button(
                    ("branch", *i),
                    format!("{}  {}", if branch.current { "●" } else { "⑂" }, name),
                )
                .ghost()
                .when(branch.current, |b| {
                    b.text_color(rgb(ACCENT)).bg(rgb(0x2b3543))
                })
                .on_click(cx.listener(move |this, _, w, cx| {
                    this.show_modal(Modal::BranchActions(name.clone(), remote), w, cx)
                }))
                .into_any_element()
            }
            Row::Remote(i) => {
                let Some(name) = self.snapshot.remotes.get(*i).cloned() else {
                    return div().into_any_element();
                };
                self.sidebar_button(("remote-config", *i), format!("▸  {name}"))
                    .ghost()
                    .text_color(rgb(ACCENT))
                    .tooltip("Edit / rename / delete remote")
                    .on_click(cx.listener(move |this, _, w, cx| {
                        this.open_remote_page(Modal::Remotes, w, cx);
                        this.active.notice = format!("Remote: {name}");
                    }))
                    .into_any_element()
            }
            Row::Stash(i) => {
                let Some((id, summary)) = self.snapshot.stashes.get(*i) else {
                    return div().into_any_element();
                };
                let id = id.clone();
                self.sidebar_button(("stash", *i), summary.clone())
                    .ghost()
                    .tooltip(summary.clone())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.begin_inspection(inspect::InspectRequest::Stash(id.clone()), cx)
                    }))
                    .into_any_element()
            }
            Row::Tag(i) => {
                let Some(tag) = self.snapshot.tags.get(*i).cloned() else {
                    return div().into_any_element();
                };
                self.sidebar_button(("tag", *i), format!("◇  {tag}"))
                    .ghost()
                    .on_click(cx.listener(move |this, _, w, cx| {
                        this.show_modal(Modal::TagActions(tag.clone()), w, cx)
                    }))
                    .into_any_element()
            }
            Row::Empty(text) => div()
                .text_xs()
                .text_color(rgb(MUTED))
                .px_2()
                .child(*text)
                .into_any_element(),
        }
    }
}
