use super::*;

impl GitBuddy {
    pub(super) fn toggle_patch(&mut self, source: PatchSource, cx: &mut Context<Self>) {
        let key = source.key();
        if !self.active.expanded.remove(&key) {
            self.active.expanded.insert(key.clone());
            if !self.active.patches.contains_key(&key) {
                self.load_patch(source, cx);
            }
        }
        cx.notify();
    }

    pub(super) fn load_patch(&mut self, source: PatchSource, cx: &mut Context<Self>) {
        let Some(repo) = self.active.repo.clone() else {
            return;
        };
        let key = source.key();
        let generation = self.patch_generation;
        self.active
            .patches
            .entry(key.clone())
            .or_insert(PatchState::Loading);
        let task = cx.background_executor().spawn(async move {
            let patch = match source {
                PatchSource::Work(file, staged) => repo.worktree_patch(&file, staged),
                PatchSource::Commit(id, file) => repo
                    .commit_file_diff(&id, &file)
                    .map(|raw| git::FilePatch::read_only(&raw)),
                PatchSource::Compare(comparison, file) => repo
                    .comparison_file_diff(&comparison, &file)
                    .map(|raw| git::FilePatch::read_only(&raw)),
            }?;
            anyhow::Ok(Arc::new(patch))
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                // Never attach an old repository/selection's response to the new view.
                if this.patch_generation != generation {
                    return;
                }
                let selection = match (&result, this.active.patches.get(&key)) {
                    (Ok(next), Some(PatchState::Ready(previous, selection)))
                        if next.partial == previous.partial && next.lines == previous.lines =>
                    {
                        selection.clone()
                    }
                    _ => PatchLineSelection::default(),
                };
                this.active.patches.insert(
                    key,
                    match result {
                        Ok(patch) => PatchState::Ready(patch, selection),
                        Err(error) => PatchState::Error(error.to_string()),
                    },
                );
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn patch_body(
        &self,
        key: &PatchKey,
        id: impl Into<ElementId>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = id.into();
        match self.active.patches.get(key) {
            Some(PatchState::Ready(patch, selection)) if !patch.lines.is_empty() => {
                let height = (patch.lines.len() as f32 * 20.).min(420.);
                let key = key.clone();
                let action_key = key.clone();
                let clear_key = key.clone();
                let staged = matches!(key, PatchKey::Work(_, true));
                let selected = selection.rows.len();
                // Independent horizontal/vertical scrolling keeps other file headers reachable.
                let list = uniform_list(
                    "patch-lines",
                    patch.lines.len(),
                    cx.processor(move |this, range: std::ops::Range<usize>, _, cx| {
                        let Some(PatchState::Ready(patch, selection)) =
                            this.active.patches.get(&key)
                        else {
                            return Vec::new();
                        };
                        range
                            .filter(|&i| i < patch.lines.len())
                            .map(|i| {
                                let line = &patch.lines[i];
                                if let Some(partial) = &patch.partial {
                                    if line.kind == '@' {
                                        let partial = partial.clone();
                                        return h_flex()
                                            .h(px(20.))
                                            .bg(rgb(0x2d3b4d))
                                            .gap_2()
                                            .child(
                                                this.button(
                                                    ("partial-hunk", i),
                                                    if staged {
                                                        "Unstage hunk"
                                                    } else {
                                                        "Stage hunk"
                                                    },
                                                )
                                                .h(px(18.))
                                                .ghost()
                                                .on_click(cx.listener(move |this, _, _, cx| {
                                                    cx.stop_propagation();
                                                    this.perform(
                                                        Operation::ApplyPartial {
                                                            patch: partial.clone(),
                                                            selection: git::PatchSelection::Hunk(i),
                                                        },
                                                        cx,
                                                    );
                                                })),
                                            )
                                            .child(
                                                div()
                                                    .font_family("Menlo")
                                                    .text_size(px(11.))
                                                    .text_color(rgb(0x9fbfe9))
                                                    .whitespace_nowrap()
                                                    .child(line.text.clone()),
                                            )
                                            .into_any_element();
                                    }
                                    if matches!(line.kind, '+' | '-') {
                                        let key = key.clone();
                                        let expected = patch.clone();
                                        return patch_line(line, selection.rows.contains(&i))
                                            .id(("partial-line", i))
                                            .cursor_pointer()
                                            .on_click(cx.listener(
                                                move |this, event: &ClickEvent, _, cx| {
                                                    cx.stop_propagation();
                                                    if this.busy() {
                                                        return;
                                                    }
                                                    if let Some(PatchState::Ready(
                                                        current,
                                                        selection,
                                                    )) = this.active.patches.get_mut(&key)
                                                        && Arc::ptr_eq(current, &expected)
                                                    {
                                                        let modifiers = event.modifiers();
                                                        selection.select(
                                                            &current.lines,
                                                            i,
                                                            modifiers.shift,
                                                            modifiers.platform || modifiers.control,
                                                        );
                                                        cx.notify();
                                                    }
                                                },
                                            ))
                                            .into_any_element();
                                    }
                                }
                                patch_line(line, false).into_any_element()
                            })
                            .collect::<Vec<_>>()
                    }),
                )
                .with_horizontal_sizing_behavior(ListHorizontalSizingBehavior::Unconstrained)
                .h(px(height))
                .w_full();
                v_flex()
                    .id(id)
                    .when(patch.partial.is_some(), |col| {
                        col.child(
                            h_flex()
                                .h(px(28.))
                                .px_3()
                                .gap_2()
                                .bg(rgb(PANEL))
                                .child(
                                    div()
                                        .flex_1()
                                        .text_size(px(10.))
                                        .text_color(rgb(MUTED))
                                        .child(if selected == 0 {
                                            "Click ± lines · Shift range · ⌘/Ctrl multi-select"
                                                .into()
                                        } else {
                                            format!("{selected} changed lines selected")
                                        }),
                                )
                                .child(
                                    self.button("clear-line-selection", "Clear")
                                        .ghost()
                                        .disabled(self.busy() || selected == 0)
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            if let Some(PatchState::Ready(_, selection)) =
                                                this.active.patches.get_mut(&clear_key)
                                            {
                                                *selection = PatchLineSelection::default();
                                                cx.notify();
                                            }
                                        })),
                                )
                                .child(
                                    self.button(
                                        "apply-line-selection",
                                        if staged {
                                            "Unstage selected"
                                        } else {
                                            "Stage selected"
                                        },
                                    )
                                    .disabled(self.busy() || selected == 0)
                                    .on_click(cx.listener(
                                        move |this, _, _, cx| {
                                            if let Some(PatchState::Ready(patch, selection)) =
                                                this.active.patches.get(&action_key)
                                                && let Some(partial) = &patch.partial
                                            {
                                                let operation = Operation::ApplyPartial {
                                                    patch: partial.clone(),
                                                    selection: git::PatchSelection::Lines(
                                                        selection.rows.iter().copied().collect(),
                                                    ),
                                                };
                                                this.perform(operation, cx);
                                            }
                                        },
                                    )),
                                ),
                        )
                    })
                    .when_some(patch.unavailable.clone(), |col, reason| {
                        col.child(
                            div()
                                .px_3()
                                .py_1()
                                .text_size(px(10.))
                                .text_color(rgb(MUTED))
                                .child(reason),
                        )
                    })
                    .child(list)
                    .into_any_element()
            }
            state => {
                let (label, color) = match state {
                    Some(PatchState::Error(error)) => (error.clone(), 0xe9a3a9),
                    Some(PatchState::Ready(_, _)) => (
                        "No text changes · empty file or metadata-only change".into(),
                        MUTED,
                    ),
                    _ => ("Loading diff…".into(), MUTED),
                };
                div()
                    .px_3()
                    .py_2()
                    .text_xs()
                    .text_color(rgb(color))
                    .child(label)
                    .into_any_element()
            }
        }
    }

    pub(super) fn commit_files_view(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(detail) = &self.commit_detail else {
            return div().p_3().child("Loading commit…").into_any_element();
        };
        let body_open = self.show_commit_body;
        let metadata = v_flex()
            .px_3()
            .py_2()
            .gap_1()
            .bg(rgb(PANEL))
            .border_b_1()
            .border_color(rgb(BORDER))
            .child(
                div()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_sm()
                    .child(detail.subject.clone()),
            )
            .child(
                h_flex()
                    .gap_3()
                    .text_xs()
                    .text_color(rgb(MUTED))
                    .child(detail.author.clone())
                    .child(detail.date.clone())
                    .child(div().flex_1())
                    .when(!detail.body.is_empty(), |row| {
                        row.child(
                            self.button(
                                "commit-body",
                                if body_open {
                                    "Hide description"
                                } else {
                                    "Description"
                                },
                            )
                            .ghost()
                            .icon(if body_open {
                                IconName::ChevronDown
                            } else {
                                IconName::ChevronRight
                            })
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.show_commit_body = !this.show_commit_body;
                                cx.notify();
                            })),
                        )
                    }),
            )
            .when(body_open, |col| {
                col.child(
                    div()
                        .id("commit-description")
                        .max_h(px(160.))
                        .overflow_y_scroll()
                        .text_xs()
                        .py_2()
                        .child(detail.body.clone()),
                )
            });
        v_flex()
            .flex_1()
            .min_h_0()
            .child(metadata)
            .child(
                h_flex()
                    .h(px(30.))
                    .px_3()
                    .gap_2()
                    .bg(rgb(BG))
                    .border_b_1()
                    .border_color(rgb(BORDER))
                    .child(
                        Icon::new(IconName::FileText)
                            .size(px(13.))
                            .text_color(rgb(MUTED)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .text_xs()
                            .text_color(rgb(MUTED))
                            .child(format!(
                                "{} changed files · click a file to expand",
                                detail.files.len()
                            )),
                    )
                    .child(
                        self.button("collapse-files", "Collapse all")
                            .ghost()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.expanded.clear();
                                cx.notify();
                            })),
                    ),
            )
            .child(
                v_flex()
                    .id("commit-files-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .children(detail.files.iter().enumerate().map(|(index, file)| {
                        let source = PatchSource::Commit(detail.id.clone(), file.clone());
                        let key = source.key();
                        let expanded = self.active.expanded.contains(&key);
                        let label = if let Some(old) = &file.original {
                            format!("{} → {}", old.display(), file.path.display())
                        } else {
                            file.path.display().to_string()
                        };
                        v_flex()
                            .flex_shrink_0()
                            .child(
                                h_flex()
                                    .id(("commit-file", index))
                                    .h(px(32.))
                                    .px_3()
                                    .gap_2()
                                    .cursor_pointer()
                                    .bg(rgb(if expanded { 0x333c48 } else { PANEL }))
                                    .hover(|s| s.bg(rgb(0x343e4b)))
                                    .border_b_1()
                                    .border_color(rgb(BORDER))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.toggle_patch(source.clone(), cx)
                                    }))
                                    .child(
                                        Icon::new(if expanded {
                                            IconName::ChevronDown
                                        } else {
                                            IconName::ChevronRight
                                        })
                                        .size(px(12.))
                                        .text_color(rgb(MUTED)),
                                    )
                                    .child(status_badge(file.status))
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .truncate()
                                            .text_size(px(12.))
                                            .child(label),
                                    )
                                    .child(patch_stat(self.active.patches.get(&key)))
                                    .child(self.file_inspect_buttons(
                                        file.path.clone(),
                                        detail.id.clone(),
                                        if file.status == 'D' {
                                            None
                                        } else {
                                            Some((file.path.clone(), detail.id.clone()))
                                        },
                                        ("commit-file-tools", index),
                                        cx,
                                    )),
                            )
                            .when(expanded, |col| {
                                col.child(self.patch_body(&key, ("commit-patch", index), cx))
                            })
                    }))
                    .when(detail.files.is_empty(), |col| {
                        col.child(
                            div()
                                .p_3()
                                .text_color(rgb(MUTED))
                                .child("No file changes relative to the first parent."),
                        )
                    }),
            )
            .into_any_element()
    }

    pub(super) fn file_group(&self, staged: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let files: Vec<_> = self
            .snapshot
            .files
            .iter()
            .filter(|f| if staged { f.staged() } else { f.unstaged() })
            .collect();
        v_flex().flex_shrink_0()
            .child(h_flex().h(px(32.)).px_3().gap_2().bg(rgb(BG)).border_b_1().border_color(rgb(BORDER))
                .child(div().text_xs().text_color(rgb(MUTED)).child("▾"))
                .child(div().flex_1().text_xs().font_weight(FontWeight::SEMIBOLD).child(format!("{}  {}", if staged {"Staged files"} else {"Working directory"},files.len())))
                .child(self.button(if staged {"unstage-all"} else {"stage-all"}, if staged {"Unstage all"} else {"Stage all"}).ghost().disabled(self.busy() || files.is_empty() || self.snapshot.files.iter().any(|f|f.conflict()))
                    .on_click(cx.listener(move |this, _, _, cx|this.perform(if staged {Operation::UnstageAll} else {Operation::StageAll},cx)))))
            .children(files.into_iter().enumerate().map(|(index, file)| {
                let source = PatchSource::Work(file.clone(),staged);
                let key = source.key();
                let expanded = self.active.expanded.contains(&key);
                let action = file.clone(); let discard = file.clone();
                let status = if file.conflict() {'!'} else if staged {file.index} else {file.worktree};
                v_flex().flex_shrink_0()
                    .child(h_flex().id((if staged {"staged-row"} else {"work-row"},index)).h(px(32.)).px_3().gap_2().cursor_pointer()
                        .bg(rgb(if expanded {0x333c48} else {PANEL})).hover(|s|s.bg(rgb(0x343e4b)))
                        .border_b_1().border_color(rgb(BORDER))
                        .on_click(cx.listener(move |this, _, _, cx|this.toggle_patch(source.clone(),cx)))
                        .child(Icon::new(if expanded {IconName::ChevronDown} else {IconName::ChevronRight}).size(px(12.)).text_color(rgb(MUTED)))
                        .child(status_badge(status))
                        .child(div().flex_1().min_w_0().truncate().text_size(px(12.)).child(file.path.display().to_string()))
                        .when_some(self.snapshot.head_id.clone(), |row, revision| {
                            let path = file.original.clone().unwrap_or_else(|| file.path.clone());
                            let blame = if file.index == 'A' || file.index == '?' { None } else { Some((path.clone(), revision.clone())) };
                            row.child(self.file_inspect_buttons(path, revision, blame, (if staged {"staged-file-tools"} else {"work-file-tools"},index),cx))
                        })
                         .when(!staged && file.index != '?' && !file.conflict(), |row|row.child(self.button(("discard",index),"Discard…").ghost()
                            .on_click(cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();this.show_modal(Modal::Confirm(format!("Discard unstaged changes to {}? This cannot be undone. Staged content is preserved.",discard.path.display()),Operation::Discard(discard.path.clone())),window,cx);
                             }))))
                        .when(file.conflict(), |row| { let path = file.path.clone(); row.child(self.button(("resolve-file",index),"Resolve…").ghost().on_click(cx.listener(move|this,_,_,cx| {cx.stop_propagation();this.open_conflicts(Some(path.clone()),cx);}))) })
                        .when(!file.conflict(), |row|row.child(self.button((if staged {"unstage"} else {"stage"},index),if staged {"Unstage"} else {"Stage"}).ghost()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();let mut paths=vec![action.path.clone()];if let Some(old)=&action.original {paths.push(old.clone());}
                                this.perform(if staged {Operation::Unstage(paths)} else {Operation::Stage(paths)},cx);
                            })))))
                    .when(expanded, |col|col.child(self.patch_body(&key,(if staged {"staged-patch"} else {"work-patch"},index),cx)))
            }))
    }
}

fn status_badge(status: char) -> impl IntoElement {
    let color = match status {
        'A' | '?' => 0x9dcbb1,
        'D' | '!' | 'U' => 0xe8a0aa,
        'R' => 0xbba6e7,
        _ => ACCENT,
    };
    div()
        .w(px(17.))
        .flex_shrink_0()
        .text_center()
        .text_size(px(10.))
        .font_weight(FontWeight::BOLD)
        .text_color(rgb(color))
        .child(status.to_string())
}
fn patch_stat(state: Option<&PatchState>) -> impl IntoElement {
    let text = if let Some(PatchState::Ready(patch, _)) = state {
        format!(
            "+{}  −{}",
            patch.lines.iter().filter(|l| l.kind == '+').count(),
            patch.lines.iter().filter(|l| l.kind == '-').count()
        )
    } else {
        String::new()
    };
    div().text_size(px(10.)).text_color(rgb(MUTED)).child(text)
}
fn patch_line(line: &DiffLine, selected: bool) -> Div {
    let (background, foreground) = match line.kind {
        '+' => (0x293e38, 0xb8e5c6),
        '-' => (0x443137, 0xedb5b7),
        '@' => (0x2d3b4d, 0x9fbfe9),
        _ => (EDITOR, TEXT),
    };
    h_flex()
        .h(px(20.))
        .font_family("Menlo")
        .text_size(px(11.))
        .bg(rgb(if selected { 0x375776 } else { background }))
        .child(
            div()
                .w(px(18.))
                .flex_shrink_0()
                .text_color(rgb(ACCENT))
                .child(if selected { "▎" } else { "" }),
        )
        .child(
            div()
                .w(px(38.))
                .flex_shrink_0()
                .text_right()
                .pr_2()
                .text_color(rgb(MUTED))
                .child(line.old.clone()),
        )
        .child(
            div()
                .w(px(38.))
                .flex_shrink_0()
                .text_right()
                .pr_2()
                .text_color(rgb(MUTED))
                .child(line.new.clone()),
        )
        .child(
            div()
                .whitespace_nowrap()
                .pr_4()
                .text_color(rgb(foreground))
                .child(line.text.replace('\t', "    ")),
        )
}
