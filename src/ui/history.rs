use super::*;
pub(super) struct Projection {
    query: String,
    pub commits: Vec<usize>,
    graph: Vec<graph::GraphRow>,
    width: f32,
    visible: Arc<Vec<String>>,
    pub files: Vec<usize>,
    pub staged: Vec<usize>,
    pub unstaged: Vec<usize>,
    pub sidebar: Vec<sidebar::Row>,
}
impl RepoTab {
    pub(super) fn projection(&self) -> Arc<Projection> {
        let mut cached = self.history_projection.borrow_mut();
        if let Some(projection) = cached.as_ref().filter(|p| p.query == self.query) {
            return projection.clone();
        }
        let query = self.query.to_lowercase();
        let commits: Vec<_> = self
            .snapshot
            .commits
            .iter()
            .enumerate()
            .filter(|(_, c)| {
                query.is_empty()
                    || [&c.subject, &c.author, &c.id, &c.refs]
                        .iter()
                        .any(|s| s.to_lowercase().contains(&query))
            })
            .map(|(i, _)| i)
            .collect();
        let references: Vec<_> = commits.iter().map(|&i| &self.snapshot.commits[i]).collect();
        let graph = graph::rows(&references, !query.is_empty());
        let width = graph::width(&graph);
        let visible = Arc::new(references.iter().map(|c| c.id.clone()).collect());
        let files = self
            .snapshot
            .files
            .iter()
            .enumerate()
            .filter(|(_, f)| {
                query.is_empty() || f.path.to_string_lossy().to_lowercase().contains(&query)
            })
            .map(|(i, _)| i)
            .collect();
        let staged = self
            .snapshot
            .files
            .iter()
            .enumerate()
            .filter(|(_, f)| f.staged())
            .map(|(i, _)| i)
            .collect();
        let unstaged = self
            .snapshot
            .files
            .iter()
            .enumerate()
            .filter(|(_, f)| f.unstaged())
            .map(|(i, _)| i)
            .collect();
        let projection = Arc::new(Projection {
            query: self.query.clone(),
            commits,
            graph,
            width,
            visible,
            files,
            staged,
            unstaged,
            sidebar: sidebar::rows(&self.snapshot),
        });
        *cached = Some(projection.clone());
        projection
    }
}

impl GitBuddy {
    pub(super) fn history(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let projection = self.active.projection();
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
                    )
                    .child(div().flex_1())
                    .child(
                        self.button(
                            "selected-actions",
                            format!("{} selected…", self.commit_selection.ids.len()),
                        )
                        .ghost()
                        .disabled(self.commit_selection.ids.is_empty())
                        .on_click(cx.listener(|this, _, w, cx| {
                            this.show_modal(Modal::CommitSelection, w, cx)
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
                    .flex_1()
                    .min_h_0()
                    .child(
                        uniform_list(
                            "commit-list",
                            projection.commits.len().max(1),
                            cx.processor(move |this, range: std::ops::Range<usize>, _, cx| {
                                range
                                    .map(|i| {
                                        if let Some(commit) = projection
                                            .commits
                                            .get(i)
                                            .and_then(|&index| this.snapshot.commits.get(index))
                                        {
                                            this.commit_row(
                                                i,
                                                commit,
                                                &projection.graph[i],
                                                projection.width,
                                                projection.visible.clone(),
                                                cx,
                                            )
                                            .into_any_element()
                                        } else {
                                            div()
                                                .h(px(56.))
                                                .px_3()
                                                .text_xs()
                                                .text_color(rgb(MUTED))
                                                .child(if this.query.is_empty() {
                                                    "Your first commit starts here."
                                                } else {
                                                    "No matching commits in loaded history."
                                                })
                                                .into_any_element()
                                        }
                                    })
                                    .collect::<Vec<_>>()
                            }),
                        )
                        .with_horizontal_sizing_behavior(
                            ListHorizontalSizingBehavior::Unconstrained,
                        )
                        .track_scroll(&self.active.scroll.list("commit-list"))
                        .flex_1()
                        .min_h_0(),
                    )
                    .child(
                        div().px_2().py_1().child(
                            self.button("load-more", "Load more history")
                                .ghost()
                                .disabled(
                                    self.busy()
                                        || self.limit >= self.settings.preferences.history_limit
                                        || self.snapshot.commits.len() < self.limit,
                                )
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.active.limit = (this.active.limit + HISTORY_PAGE_SIZE)
                                        .min(this.settings.preferences.history_limit);
                                    this.refresh_with_notice("History loaded".into(), cx);
                                })),
                        ),
                    )
                    .into_any_element()
            } else {
                uniform_list(
                    "file-list",
                    projection.files.len(),
                    cx.processor(move |this, range: std::ops::Range<usize>, _, cx| {
                        range
                            .filter_map(|i| {
                                projection
                                    .files
                                    .get(i)
                                    .and_then(|&index| this.snapshot.files.get(index))
                                    .map(|file| (i, file))
                            })
                            .map(|(i, file)| {
                                let file = file.clone();
                                this.sidebar_button(
                                    ("file-side", i),
                                    format!(
                                        "{}{}  {}",
                                        file.index,
                                        file.worktree,
                                        file.path.display()
                                    ),
                                )
                                .h(px(28.))
                                .ghost()
                                .on_click(cx.listener(
                                    move |this, _, _, cx| {
                                        this.select(
                                            Selection::File(file.clone(), !file.unstaged()),
                                            cx,
                                        )
                                    },
                                ))
                            })
                            .collect::<Vec<_>>()
                    }),
                )
                .track_scroll(&self.active.scroll.list("file-list"))
                .flex_1()
                .min_h_0()
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
                        "{} commits loaded · limit {} · all branches",
                        self.snapshot.commits.len(),
                        self.settings.preferences.history_limit
                    )),
            )
    }

    fn commit_row(
        &self,
        index: usize,
        commit: &Commit,
        graph: &graph::GraphRow,
        graph_width: f32,
        visible: Arc<Vec<String>>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let id = commit.id.clone();
        let toggle_id = id.clone();
        let toggle_visible = visible.clone();
        let selected = self.commit_selection.ids.contains(&commit.id)
            || (self.commit_selection.ids.is_empty()
                && matches!(&self.selection, Selection::Commit(oid) if oid == &commit.id));
        let merge = commit.parents.len() > 1;
        let lane_x = graph::lane_x;
        let line_color =
            |lane: usize| rgb([0x86b7f3, 0xb6a2ec, 0x86c7ad, 0xe2b87b, 0x8cc6d7][lane % 5]);
        let node_x = lane_x(graph.lane);
        div()
            .id(("commit-row", index))
            .min_w(px(graph_width + 180.))
            .h(px(56.))
            .overflow_hidden()
            .relative()
            .flex_shrink_0()
            .pl(px(graph_width))
            .pr(px(30.))
            .py(px(4.))
            .cursor_pointer()
            .bg(rgb(if selected { 0x334152 } else { PANEL }))
            .hover(|s| s.bg(rgb(0x303946)))
            .on_click(cx.listener(move |this, event: &ClickEvent, _, cx| {
                let modifiers = event.modifiers();
                this.select_commit(
                    id.clone(),
                    &visible,
                    modifiers.shift,
                    modifiers.platform || modifiers.control,
                    cx,
                );
            }))
            .child(
                self.button(
                    "toggle-selection",
                    if self.commit_selection.ids.contains(&commit.id) {
                        "☑"
                    } else {
                        "☐"
                    },
                )
                .ghost()
                .absolute()
                .right(px(4.))
                .top(px(4.))
                .w(px(22.))
                .h(px(22.))
                .on_click(cx.listener(move |this, _, _, cx| {
                    cx.stop_propagation();
                    this.select_commit(toggle_id.clone(), &toggle_visible, false, true, cx);
                })),
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
                    .child(
                        div()
                            .truncate()
                            .text_size(px(10.))
                            .line_height(px(12.))
                            .h(px(12.))
                            .text_color(rgb(0xbca9ec))
                            .child(commit.refs.clone()),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .min_w_0()
                            .text_size(px(10.))
                            .line_height(px(14.))
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

#[cfg(test)]
mod tests {
    use super::*;
    #[::core::prelude::v1::test]
    fn large_projection_reuses_graph_and_file_indices_until_query_changes() {
        let mut tab = RepoTab::default();
        for i in 0..50_000 {
            tab.snapshot.commits.push(Commit {
                id: format!("{i:040x}"),
                parents: if i < 49_999 {
                    vec![format!("{:040x}", i + 1)]
                } else {
                    vec![]
                },
                subject: format!("Commit {i}"),
                author: "author".into(),
                date: "today".into(),
                refs: String::new(),
            });
            tab.snapshot.files.push(FileChange {
                path: format!("file-{i}.txt").into(),
                original: None,
                index: 'M',
                worktree: 'M',
            });
        }
        let start = std::time::Instant::now();
        let first = tab.projection();
        let build = start.elapsed();
        let start = std::time::Instant::now();
        for _ in 0..1_000 {
            assert!(Arc::ptr_eq(&first, &tab.projection()));
        }
        let reuse = start.elapsed();
        assert_eq!(first.commits.len(), 50_000);
        assert_eq!(first.graph.len(), 50_000);
        assert_eq!(first.staged.len(), 50_000);
        assert_eq!(first.unstaged.len(), 50_000);
        assert!(first.graph[100].incoming);
        assert!(!first.graph[100].outgoing.is_empty());
        tab.query = "Commit 43210".into();
        let filtered = tab.projection();
        assert_eq!(filtered.commits, vec![43_210]);
        assert_eq!(filtered.graph.len(), 1);
        assert!(filtered.files.is_empty());
        assert!(!Arc::ptr_eq(&first, &filtered));
        eprintln!(
            "50,000 commits + 50,000 files: projection build {build:?}; 1,000 cached accesses {reuse:?} (debug, no GPU/layout timing)"
        );
    }
}
