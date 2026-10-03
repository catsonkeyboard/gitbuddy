use super::*;

impl GitBuddy {
    pub(super) fn history(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let filtered: Vec<_> = self
            .snapshot
            .commits
            .iter()
            .filter(|c| {
                self.query.is_empty()
                    || format!("{} {} {} {}", c.subject, c.author, c.id, c.refs)
                        .to_lowercase()
                        .contains(&self.query)
            })
            .collect();
        let graph = graph::rows(&filtered, !self.query.is_empty());
        let graph_width = graph
            .iter()
            .flat_map(|row| {
                row.through
                    .iter()
                    .chain(row.outgoing.iter())
                    .chain(std::iter::once(&row.lane))
            })
            .copied()
            .max()
            .map_or(36., |lane| ((lane + 1) * 14 + 20).max(36) as f32)
            .min(130.);
        v_flex()
            .w(px(340.))
            .min_w(px(270.))
            .h_full()
            .bg(rgb(PANEL))
            .border_r_1()
            .border_color(rgb(BORDER))
            .child(
                h_flex()
                    .h(px(32.))
                    .px_3()
                    .gap_2()
                    .border_b_1()
                    .border_color(rgb(BORDER))
                    .child(
                        self.button("history-tab", "Commits")
                            .ghost()
                            .when(self.history_tab == 0, |b| b.text_color(rgb(ACCENT)))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.active.history_tab = 0;
                                cx.notify();
                            })),
                    )
                    .child(
                        self.button("files-tab", "Files")
                            .ghost()
                            .when(self.history_tab == 1, |b| b.text_color(rgb(ACCENT)))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.active.history_tab = 1;
                                cx.notify();
                            })),
                    ),
            )
            .child(div().px_2().py_1().child(Input::new(&self.search).small()))
            .child(
                div()
                    .id("work-summary")
                    .mx_2()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .bg(rgb(
                        if matches!(self.selection, Selection::Work | Selection::File(..)) {
                            0x354151
                        } else {
                            0x2b3038
                        },
                    ))
                    .cursor_pointer()
                    .on_click(cx.listener(|this, _, _, cx| this.select(Selection::Work, cx)))
                    .child(
                        h_flex()
                            .gap_2()
                            .child(div().text_color(rgb(ACCENT)).child("●"))
                            .child(
                                div()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child("Working directory"),
                            ),
                    )
                    .child(
                        div()
                            .pl_5()
                            .mt_1()
                            .text_xs()
                            .text_color(rgb(MUTED))
                            .child(format!(
                                "{} staged · {} unstaged",
                                self.snapshot.files.iter().filter(|f| f.staged()).count(),
                                self.snapshot.files.iter().filter(|f| f.unstaged()).count()
                            )),
                    ),
            )
            .child(if self.history_tab == 0 {
                v_flex()
                    .id("commit-list")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .mt_1()
                    .children(
                        filtered.iter().enumerate().map(|(i, commit)| {
                            self.commit_row(i, commit, &graph[i], graph_width, cx)
                        }),
                    )
                    .when(filtered.is_empty(), |col| {
                        col.child(div().p_5().text_sm().text_color(rgb(MUTED)).child(
                            if self.query.is_empty() {
                                "Your first commit starts here."
                            } else {
                                "No matching commits in loaded history."
                            },
                        ))
                    })
                    .child(
                        div().px_2().py_1().child(
                            self.button("load-more", "Load more history")
                                .ghost()
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.active.limit += HISTORY_PAGE_SIZE;
                                    this.refresh_with_notice("History loaded".into(), cx);
                                })),
                        ),
                    )
                    .into_any_element()
            } else {
                v_flex()
                    .id("file-list")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .mt_1()
                    .children(
                        self.snapshot
                            .files
                            .iter()
                            .filter(|f| {
                                f.path
                                    .to_string_lossy()
                                    .to_lowercase()
                                    .contains(&self.query)
                            })
                            .enumerate()
                            .map(|(i, file)| {
                                let file = file.clone();
                                self.button(
                                    ("file-side", i),
                                    format!(
                                        "{}{}  {}",
                                        file.index,
                                        file.worktree,
                                        file.path.display()
                                    ),
                                )
                                .ghost()
                                .on_click(cx.listener(
                                    move |this, _, _, cx| {
                                        this.select(
                                            Selection::File(file.clone(), !file.unstaged()),
                                            cx,
                                        )
                                    },
                                ))
                            }),
                    )
                    .into_any_element()
            })
            .child(
                div()
                    .h(px(24.))
                    .px_3()
                    .py_1()
                    .text_xs()
                    .text_color(rgb(MUTED))
                    .border_t_1()
                    .border_color(rgb(BORDER))
                    .child(format!(
                        "{} commits loaded · all branches",
                        self.snapshot.commits.len()
                    )),
            )
    }

    fn commit_row(
        &self,
        index: usize,
        commit: &Commit,
        graph: &graph::GraphRow,
        graph_width: f32,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let id = commit.id.clone();
        let selected = matches!(&self.selection, Selection::Commit(oid) if oid == &commit.id);
        let merge = commit.parents.len() > 1;
        let lane_x = |lane: usize| 16. + lane as f32 * 14.;
        let line_color =
            |lane: usize| rgb([0x86b7f3, 0xb6a2ec, 0x86c7ad, 0xe2b87b, 0x8cc6d7][lane % 5]);
        let node_x = lane_x(graph.lane);
        div()
            .id(("commit-row", index))
            .relative()
            .flex_shrink_0()
            .pl(px(graph_width))
            .pr_2()
            .py(px(6.))
            .cursor_pointer()
            .bg(rgb(if selected { 0x334152 } else { PANEL }))
            .hover(|s| s.bg(rgb(0x303946)))
            .on_click(
                cx.listener(move |this, _, _, cx| this.select(Selection::Commit(id.clone()), cx)),
            )
            .children(graph.through.clone().into_iter().map(|lane| {
                div()
                    .absolute()
                    .left(px(lane_x(lane)))
                    .top_0()
                    .bottom_0()
                    .w(px(2.))
                    .bg(line_color(lane))
            }))
            .when(graph.incoming, |row| {
                row.child(
                    div()
                        .absolute()
                        .left(px(node_x))
                        .top_0()
                        .h(px(16.))
                        .w(px(2.))
                        .bg(line_color(graph.lane)),
                )
            })
            .children(
                graph
                    .outgoing
                    .clone()
                    .into_iter()
                    .filter(|lane| !graph.through.contains(lane))
                    .map(|lane| {
                        div()
                            .absolute()
                            .left(px(lane_x(lane)))
                            .top(px(16.))
                            .bottom_0()
                            .w(px(2.))
                            .bg(line_color(lane))
                    }),
            )
            .children(graph.links.clone().into_iter().map(|(from, to)| {
                let left = lane_x(from.min(to));
                let right = lane_x(from.max(to));
                div()
                    .absolute()
                    .left(px(left))
                    .top(px(15.))
                    .w(px(right - left + 2.))
                    .h(px(2.))
                    .bg(line_color(to))
            }))
            .child(
                div()
                    .absolute()
                    .left(px(node_x - 4.))
                    .top(px(11.))
                    .size(px(9.))
                    .bg(line_color(graph.lane))
                    .when(merge, |node| node.rounded_sm())
                    .when(!merge, |node| node.rounded_full()),
            )
            .child(
                v_flex()
                    .min_w_0()
                    .gap(px(2.))
                    .child(
                        div()
                            .truncate()
                            .text_size(px(12.))
                            .line_height(px(18.))
                            .font_weight(FontWeight::MEDIUM)
                            .child(commit.subject.clone()),
                    )
                    .when(!commit.refs.is_empty(), |col| {
                        col.child(
                            div()
                                .truncate()
                                .text_size(px(10.))
                                .line_height(px(15.))
                                .text_color(rgb(0xbca9ec))
                                .child(commit.refs.clone()),
                        )
                    })
                    .child(
                        h_flex()
                            .gap_2()
                            .min_w_0()
                            .text_size(px(10.))
                            .line_height(px(15.))
                            .text_color(rgb(MUTED))
                            .child(
                                div()
                                    .font_family("Menlo")
                                    .text_color(rgb(0x8498ae))
                                    .child(commit.id.chars().take(7).collect::<String>()),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .child(commit.author.clone()),
                            )
                            .child(commit.date.clone()),
                    ),
            )
    }
}
