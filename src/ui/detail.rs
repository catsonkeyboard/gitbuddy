use super::*;

impl GitBuddy {
    pub(super) fn detail(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let title = match &self.selection {
            Selection::Work => "Summary".into(),
            Selection::File(f, staged) => format!(
                "{}  ·  {}",
                f.path.display(),
                if *staged { "Staged" } else { "Working tree" }
            ),
            Selection::Commit(id) => format!("Commit  {}", &id[..8]),
            Selection::Inspect => self.inspection_title(),
            Selection::Conflicts => "Resolve conflicts".into(),
        };
        v_flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .bg(rgb(EDITOR))
            .child(
                h_flex()
                    .h(px(32.))
                    .px_3()
                    .gap_2()
                    .flex_shrink_0()
                    .border_b_1()
                    .border_color(rgb(BORDER))
                    .bg(rgb(PANEL))
                    .child(div().text_color(rgb(ACCENT)).child("▤"))
                    .child(div().text_sm().flex_1().child(title))
                    .when(
                        !matches!(self.selection, Selection::Inspect)
                            && !matches!(self.inspection, inspect::InspectState::Empty),
                        |row| {
                            row.child(
                                self.button("back-inspection", "Back to inspection")
                                    .ghost()
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.select(Selection::Inspect, cx)
                                    })),
                            )
                        },
                    )
                    .when(!matches!(self.selection, Selection::Work), |row| {
                        row.child(self.button("summary", "Summary").ghost().on_click(
                            cx.listener(|this, _, _, cx| this.select(Selection::Work, cx)),
                        ))
                    })
                    .when(matches!(self.selection, Selection::Commit(_)), |row| {
                        row.child(
                            self.button("commit-actions", "Actions…")
                                .on_click(cx.listener(|this, _, w, cx| {
                                    if let Selection::Commit(id) = &this.active.selection {
                                        this.show_modal(Modal::CommitActions(id.clone()), w, cx);
                                    }
                                })),
                        )
                    }),
            )
            .child(if matches!(self.selection, Selection::Conflicts) {
                self.conflicts_view(cx)
            } else if matches!(self.selection, Selection::Inspect) {
                self.inspection_view(cx)
            } else if matches!(self.selection, Selection::Work) {
                self.summary(cx).into_any_element()
            } else if matches!(self.selection, Selection::Commit(_)) {
                self.commit_files_view(cx)
            } else {
                self.diff_view(cx).into_any_element()
            })
    }
    fn summary(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let staged = self.snapshot.files.iter().filter(|f| f.staged()).count();
        v_flex()
            .flex_1()
            .min_h_0()
            .child(
                v_flex()
                    .px_3()
                    .py_2()
                    .gap_1()
                    .border_b_1()
                    .border_color(rgb(BORDER))
                    .child(
                        h_flex()
                            .justify_between()
                            .child(section("COMMIT CHANGES"))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(rgb(MUTED))
                                    .child("⌘ Enter to commit"),
                            ),
                    )
                    .child(Textarea::new(&self.message).h(px(64.)).appearance(false))
                    .child(
                        h_flex()
                            .gap_2()
                            .justify_end()
                            .child(
                                self.button("amend-latest", "Amend…")
                                    .ghost()
                                    .disabled(self.busy() || self.snapshot.head_id.is_none())
                                    .on_click(cx.listener(|this, _, w, cx| {
                                        this.begin_history(HistoryKind::Amend, w, cx)
                                    })),
                            )
                            .child(self.button("stash-work", "Stash…").on_click(
                                cx.listener(|this, _, w, cx| this.show_modal(Modal::Stash, w, cx)),
                            ))
                            .child(
                                self.button(
                                    "commit",
                                    format!(
                                        "Commit {staged} file{}",
                                        if staged == 1 { "" } else { "s" }
                                    ),
                                )
                                .primary()
                                .disabled(
                                    self.busy()
                                        || staged == 0
                                        || self.snapshot.files.iter().any(|f| f.conflict())
                                        || !matches!(
                                            self.snapshot.conflict_operation,
                                            git::ConflictOperation::None
                                                | git::ConflictOperation::Merge
                                        ),
                                )
                                .on_click(cx.listener(|this, _, _, cx| this.commit(cx))),
                            ),
                    ),
            )
            .when(
                self.snapshot.files.iter().any(|f| f.conflict())
                    || self.snapshot.conflict_operation != git::ConflictOperation::None,
                |col| {
                    col.child(
                        h_flex()
                            .px_3()
                            .py_2()
                            .gap_2()
                            .bg(rgb(0x51432d))
                            .child(div().flex_1().text_xs().child(
                                "Review and resolve conflicts, then continue the operation.",
                            ))
                            .child(
                                self.button("resolve-conflicts", "Resolve conflicts…")
                                    .on_click(
                                        cx.listener(|this, _, _, cx| this.open_conflicts(None, cx)),
                                    ),
                            ),
                    )
                },
            )
            .child(
                v_flex()
                    .id("changes-scroll")
                    .track_scroll(&self.active.scroll.area("changes-scroll"))
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(self.file_group(false, cx))
                    .child(self.file_group(true, cx))
                    .when(self.snapshot.files.is_empty(), |col| {
                        col.child(
                            v_flex()
                                .items_center()
                                .justify_center()
                                .py_6()
                                .gap_3()
                                .child(div().text_3xl().text_color(rgb(0x8ec6a5)).child("✓"))
                                .child(
                                    div()
                                        .font_weight(FontWeight::MEDIUM)
                                        .child("Working tree clean"),
                                )
                                .child(
                                    div()
                                        .text_sm()
                                        .text_color(rgb(MUTED))
                                        .child("Everything is committed. A good place to begin."),
                                ),
                        )
                    }),
            )
    }

    fn diff_view(&self, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex().flex_1().min_h_0()
            .when(matches!(self.selection,Selection::File(..)),|col|col.child(h_flex().px_4().py_2().gap_2().border_b_1().border_color(rgb(BORDER))
                .child(div().flex_1().text_xs().text_color(rgb(MUTED)).child("Unified diff · old / new line numbers"))
                .child(self.button("diff-stage",if matches!(self.selection,Selection::File(_,true)){"Unstage file"}else{"Stage file"}).on_click(cx.listener(|this,_,_,cx|{
                    if let Selection::File(file,staged)=&this.active.selection {let mut paths=vec![file.path.clone()];if let Some(old)=&file.original{paths.push(old.clone());}this.perform(if *staged{Operation::Unstage(paths)}else{Operation::Stage(paths)},cx);}
                })))))
            .child(uniform_list("diff-lines",self.diff.len().max(1),cx.processor(|this,range:std::ops::Range<usize>,_,_| {
                range.map(|i|{
                    let line=this.active.diff.get(i);let kind=line.map(|l|l.kind).unwrap_or('h');
                    let (background,foreground)=match kind {'+'=> (0x2b423d,0xb8e5c6),'-'=>(0x48343a,0xedb5b7),'@'=>(0x303e51,0x9fbfe9),_=>(EDITOR,TEXT)};
                    h_flex().w_full().min_h(px(23.)).font_family("Menlo").text_size(px(12.)).bg(rgb(background)).items_start()
                        .child(div().w(px(45.)).flex_shrink_0().text_right().pr_2().text_color(rgb(MUTED)).child(line.map(|l|l.old.clone()).unwrap_or_default()))
                        .child(div().w(px(45.)).flex_shrink_0().text_right().pr_3().text_color(rgb(MUTED)).child(line.map(|l|l.new.clone()).unwrap_or_default()))
                        .child(div().flex_1().min_w_0().whitespace_nowrap().text_color(rgb(foreground)).child(line.map(|l|l.text.replace('\t',"    ")).unwrap_or_else(||"No text diff. The file may be binary, unchanged, or a submodule.".into())))
                }).collect::<Vec<_>>()
            })).with_horizontal_sizing_behavior(ListHorizontalSizingBehavior::Unconstrained).track_scroll(&self.active.scroll.list("diff-lines")).flex_1().min_h_0())
    }
}
