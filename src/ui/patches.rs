use super::*;
use gpui_kit::component::scroll::{Scrollbar, ScrollbarMode};
struct LoadPermit(Arc<std::sync::atomic::AtomicUsize>);
impl LoadPermit {
    fn acquire(count: &Arc<std::sync::atomic::AtomicUsize>) -> Option<Self> {
        count
            .fetch_update(
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
                |n| (n < 4).then_some(n + 1),
            )
            .ok()?;
        Some(Self(count.clone()))
    }
}
impl Drop for LoadPermit {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
    }
}
impl gitbuddy::cache::Weight for PatchState {
    fn bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            + match self {
                PatchState::Ready(patch, selection) => {
                    patch.estimated_bytes() + selection.rows.len() * 32
                }
                PatchState::Error(error) => error.capacity(),
                PatchState::Loading => 0,
            }
    }
}

impl GitBuddy {
    pub(super) fn export_repo_patch(&mut self, staged: bool, cx: &mut Context<Self>) {
        if self.busy() || self.modal.is_some() {
            return;
        }
        let Some(repo) = self.active.repo.clone() else {
            return;
        };
        let tab = self.active.id;
        let receiver = cx.prompt_for_new_path(
            &repo.root,
            Some(if staged {
                "staged.patch"
            } else {
                "unstaged.patch"
            }),
        );
        cx.spawn(async move |this, cx| {
            let path = match receiver.await {
                Ok(Ok(Some(p))) => p,
                Ok(Err(error)) => {
                    let _ = this.update(cx, |this, cx| {
                        if this.active.id == tab {
                            this.active.error = true;
                            this.active.notice = format!("{error:#}");
                            cx.notify();
                        }
                    });
                    return;
                }
                _ => return,
            };
            let work = cx.background_executor().spawn(async move {
                use std::io::Write;
                let bytes = repo.export_patch(staged)?;
                let parent = path
                    .parent()
                    .ok_or_else(|| anyhow::anyhow!("Missing destination directory"))?;
                let mut file = tempfile::NamedTempFile::new_in(parent)?;
                file.write_all(&bytes)?;
                file.as_file().sync_all()?;
                file.persist_noclobber(&path).map_err(|e| e.error)?;
                Ok::<_, anyhow::Error>(path)
            });
            let result = work.await;
            let _ = this.update(cx, |this, cx| {
                if this.active.id == tab {
                    match result {
                        Ok(path) => {
                            this.active.notice = format!("Patch exported to {}", path.display());
                            this.active.error = false;
                        }
                        Err(error) => {
                            this.active.notice = format!("Cannot export patch: {error:#}");
                            this.active.error = true;
                        }
                    }
                    cx.notify();
                }
            });
        })
        .detach();
    }
    pub(super) fn choose_patch_file(&mut self, cx: &mut Context<Self>) {
        if self.busy() || self.modal.is_some() {
            return;
        }
        let Some(repo) = self.active.repo.clone() else {
            return;
        };
        let tab = self.active.id;
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Choose a patch file".into()),
        });
        cx.spawn(async move |this, cx| {
            let path = match receiver.await {
                Ok(Ok(Some(paths))) => match paths.into_iter().next() {
                    Some(path) => path,
                    None => return,
                },
                Ok(Err(error)) => {
                    let _ = this.update(cx, |this, cx| {
                        if this.active.id == tab {
                            this.active.notice = format!("{error:#}");
                            this.active.error = true;
                            cx.notify();
                        }
                    });
                    return;
                }
                _ => return,
            };
            let work = cx.background_executor().spawn(async move {
                use std::io::Read;
                let mut bytes = Vec::new();
                std::fs::File::open(path)?
                    .take(2 * 1024 * 1024 + 1)
                    .read_to_end(&mut bytes)?;
                repo.prepare_patch_file(bytes)
            });
            let result = work.await;
            let _ = this.update(cx, |this, cx| {
                if this.active.id != tab || this.busy() || this.modal.is_some() {
                    return;
                }
                match result {
                    Ok(plan) => {
                        this.modal = Some(Modal::Confirm(
                            plan.summary(),
                            Operation::ApplyPatch(Arc::new(plan)),
                        ));
                        this.branch_focus = true;
                    }
                    Err(error) => {
                        this.active.notice = format!("{error:#}");
                        this.active.error = true;
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }
    pub(super) fn toggle_patch(&mut self, source: PatchSource, cx: &mut Context<Self>) {
        let key = source.key();
        if !self.active.expanded.remove(&key) {
            self.active.expanded.insert(key.clone());
            if !matches!(
                self.active.patches.get(&key),
                Some(PatchState::Loading | PatchState::Ready(..))
            ) {
                self.load_patch(source, cx);
            }
        }
        self.active.scroll.invalidate_variables();
        self.active.prune_patch_views();
        cx.notify();
    }

    pub(super) fn load_patch(&mut self, source: PatchSource, cx: &mut Context<Self>) {
        let Some(repo) = self.active.repo.clone() else {
            return;
        };
        let key = source.key();
        if matches!(self.active.patches.get(&key), Some(PatchState::Loading)) {
            return;
        }
        // Bound simultaneous loads as well as retained values (including restored
        // sessions with many expanded files). Opening another file retries it.
        let Some(permit) = LoadPermit::acquire(&self.active.patch_loading) else {
            self.active.patches.insert(
                key,
                PatchState::Error(
                    "Four diffs are loading. Collapse and reopen this file when they finish."
                        .into(),
                ),
            );
            return;
        };
        let generation = self.patch_generation;
        let full = self.active.diff_preferences.full_context;
        let ignore_whitespace = self.active.diff_preferences.ignore_whitespace;
        let previous: HashSet<_> = self
            .active
            .patches
            .iter()
            .map(|(key, _)| key.clone())
            .collect();
        self.active.patches.insert(key.clone(), PatchState::Loading);
        self.active
            .expanded
            .retain(|k| !previous.contains(k) || self.active.patches.contains_key(k));
        let max_bytes = self.settings.preferences.patch_cache_mb * 1024 * 1024;
        let task = cx.background_executor().spawn(async move {
            let _permit = permit;
            let patch = match source {
                PatchSource::Work(file, staged) if ignore_whitespace => repo.diff_context_options(&file, staged, full, true).and_then(|raw| {
                    let mut view = git::FilePatch::read_only_context(&raw, full)?;
                    view.unavailable = Some("Partial actions require the unfiltered diff. Turn off Ignore whitespace to stage, unstage or discard lines.".into());
                    Ok(view)
                }),
                PatchSource::Work(file, staged) => repo.worktree_patch_context(&file, staged, full),
                PatchSource::Commit(id, file) => repo
                    .commit_file_diff_options(&id, &file, full, ignore_whitespace)
                    .and_then(|raw| git::FilePatch::read_only_context(&raw, full)),
                PatchSource::Compare(comparison, file) => repo
                    .comparison_file_diff_options(&comparison, &file, full, ignore_whitespace)
                    .and_then(|raw| git::FilePatch::read_only_context(&raw, full)),
                PatchSource::Stash(section, file) => repo
                    .stash_file_diff_options(&section, &file, full, ignore_whitespace)
                    .and_then(|raw| git::FilePatch::read_only_context(&raw, full)),
            }?;
            anyhow::Ok(Arc::new(patch))
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                // Never attach an old repository/selection's response to the new view.
                if this.patch_generation != generation || !this.active.patches.contains_key(&key) {
                    return;
                }
                let mut selection = match (&result, this.active.patches.get(&key)) {
                    (Ok(next), Some(PatchState::Ready(previous, selection)))
                        if next.partial == previous.partial && next.lines == previous.lines =>
                    {
                        selection.clone()
                    }
                    _ => PatchLineSelection::default(),
                };
                if let Ok(patch) = &result
                    && let Some(saved) = this.active.restored_lines.remove(&key)
                    && let Some((rows, anchor)) = saved.restore(&patch.lines)
                {
                    selection.rows = rows;
                    selection.anchor = anchor;
                }
                let previous: HashSet<_> = this.active.patches.iter().map(|(k, _)| k.clone()).collect();
                this.active.patches.insert(
                    key,
                    match result {
                        Ok(patch) if patch.estimated_bytes() + selection.rows.len() * 32 <= max_bytes.saturating_sub(std::mem::size_of::<PatchState>()) => PatchState::Ready(patch, selection),
                        Ok(_) => PatchState::Error("This diff exceeds the patch cache budget. Increase the budget in Preferences or use compact context.".into()),
                        Err(error) => PatchState::Error(error.to_string()),
                    },
                );
                this.active.expanded.retain(|k| !previous.contains(k) || this.active.patches.contains_key(k));
                this.active.scroll.invalidate_variables();
                this.active.prune_patch_views();
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
                let split = self.active.diff_preferences.side_by_side;
                let count = if split {
                    patch.display.rows.len()
                } else {
                    patch.lines.len()
                };
                let height = (count as f32 * 20.).min(420.);
                let key = key.clone();
                let scroll = self.active.scroll.list(&key.scroll_id());
                let action_key = key.clone();
                let controls_key = key.clone();
                let export_key = key.clone();
                let discard_key = key.clone();
                let clear_key = key.clone();
                let staged = matches!(key, PatchKey::Work(_, true));
                let worktree = matches!(key, PatchKey::Work(_, false))
                    && patch.partial.as_ref().is_some_and(|p| p.can_discard());
                let selected = selection.rows.len();
                let list = if split {
                    h_flex()
                        .h(px(height))
                        .w_full()
                        .min_w_0()
                        .child(self.split_diff_list(&key, patch, false, height, cx))
                        .child(self.split_diff_list(&key, patch, true, height, cx))
                        .into_any_element()
                } else {
                    let list = uniform_list(
                        "patch-lines",
                        count,
                        cx.processor(move |this, range: std::ops::Range<usize>, _, cx| {
                            let Some(PatchState::Ready(patch, _)) = this.active.patches.get(&key)
                            else {
                                return Vec::new();
                            };
                            let patch = patch.clone();
                            range
                                .filter(|&i| i < patch.lines.len())
                                .map(|i| this.diff_row(&key, &patch, i, None, cx))
                                .collect::<Vec<_>>()
                        }),
                    )
                    .with_width_from_item(Some(patch.display.measure_row))
                    .with_horizontal_sizing_behavior(ListHorizontalSizingBehavior::Unconstrained)
                    .track_scroll(&scroll)
                    .h(px(height))
                    .w_full();
                    div()
                        .relative()
                        .h(px(height))
                        .w_full()
                        .child(list)
                        .child(
                            div().absolute().inset_0().child(
                                Scrollbar::new(&scroll)
                                    .id("diff-scrollbar")
                                    .viewport_from_layout()
                                    .mode(ScrollbarMode::Always),
                            ),
                        )
                        .into_any_element()
                };
                v_flex()
                    .id(id)
                    .child(self.diff_controls(&controls_key, cx))
                    .when(split, |col| {
                        col.child(
                            h_flex()
                                .h(px(24.))
                                .bg(rgb(PANEL))
                                .text_xs()
                                .text_color(rgb(MUTED))
                                .child(div().flex_1().px_3().child("Before / index base"))
                                .child(div().flex_1().px_3().child("After / selected version")),
                        )
                    })
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
                                .child(self.button("export-file-patch", "Export patch…").ghost().on_click(cx.listener(move |this, _, _, cx| {
                                    let Some(PatchState::Ready(patch, _)) = this.active.patches.get(&export_key) else { return; };
                                    let Some(partial) = patch.partial.as_ref() else { return; };
                                    let bytes = partial.patch_bytes().to_vec();
                                    let Some(repo) = this.active.repo.as_ref() else { return; };
                                    let tab = this.active.id;
                                    let receiver = cx.prompt_for_new_path(&repo.root, Some("changes.patch"));
                                    cx.spawn(async move |this, cx| {
                                        let path = match receiver.await { Ok(Ok(Some(path))) => path, Ok(Err(error)) => { let _ = this.update(cx, |this, cx| { if this.active.id == tab { this.active.notice = format!("{error:#}"); this.active.error = true; cx.notify(); } }); return; }, _ => return };
                                        let result = cx.background_executor().spawn(async move {
                                            use std::io::Write;
                                            let parent = path.parent().ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "Missing destination directory"))?;
                                            let mut file = tempfile::NamedTempFile::new_in(parent)?;
                                            file.write_all(&bytes)?;
                                            file.as_file().sync_all()?;
                                            file.persist_noclobber(&path).map_err(|e| e.error)?;
                                            Ok::<_, std::io::Error>(path)
                                        }).await;
                                        let _ = this.update(cx, |this, cx| { if this.active.id == tab { match result { Ok(path) => { this.active.notice = format!("Patch exported to {}", path.display()); this.active.error = false; }, Err(error) => { this.active.notice = format!("Cannot export patch: {error}"); this.active.error = true; } } cx.notify(); } });
                                    }).detach();
                                })))
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
                                )
                                .when(worktree, |row| row.child(self.button("discard-line-selection", "Discard selected…")
                                    .ghost().disabled(self.busy() || selected == 0)
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        if let Some(PatchState::Ready(patch, selection)) = this.active.patches.get(&discard_key)
                                            && let Some(partial) = &patch.partial {
                                            let operation = Operation::DiscardPartial { patch: partial.clone(), selection: git::PatchSelection::Lines(selection.rows.iter().copied().collect()) };
                                            this.show_modal(Modal::Confirm("Discard the selected unstaged lines? This changes the working file and cannot be undone. Staged content is preserved.".into(), operation), window, cx);
                                        }
                                    })))),
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
                v_flex()
                    .child(self.diff_controls(key, cx))
                    .child(
                        div()
                            .px_3()
                            .py_2()
                            .text_xs()
                            .text_color(rgb(color))
                            .child(label),
                    )
                    .into_any_element()
            }
        }
    }

    fn split_diff_list(
        &self,
        key: &PatchKey,
        patch: &Arc<git::FilePatch>,
        right: bool,
        height: f32,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let scroll = self.active.scroll.list(&key.scroll_id());
        let key = key.clone();
        let count = patch.display.rows.len();
        let width = (patch.display.columns as f32 * 6.7 + 64.).max(320.);
        // Both virtual lists use the same offset and equal row count / content width.
        // This synchronizes vertical and horizontal scrolling while keeping the divider fixed.
        let list = uniform_list(
            if right { "patch-right" } else { "patch-left" },
            count,
            cx.processor(move |this, range: std::ops::Range<usize>, _, cx| {
                let Some(PatchState::Ready(patch, _)) = this.active.patches.get(&key) else {
                    return Vec::new();
                };
                let patch = patch.clone();
                range
                    .filter(|&i| i < patch.display.rows.len())
                    .map(|i| {
                        let row = &patch.display.rows[i];
                        let source = if right { row.right } else { row.left };
                        div()
                            .h(px(20.))
                            .min_w(px(width))
                            .bg(rgb(EDITOR))
                            .when_some(source, |cell, r| {
                                cell.child(this.diff_row(&key, &patch, r, Some(right), cx))
                            })
                            .into_any_element()
                    })
                    .collect::<Vec<_>>()
            }),
        )
        .with_width_from_item(Some(patch.display.split_measure_row))
        .with_horizontal_sizing_behavior(ListHorizontalSizingBehavior::Unconstrained)
        .track_scroll(&scroll)
        .h(px(height))
        .w_full();
        div()
            .relative()
            .h(px(height))
            .flex_1()
            .min_w_0()
            .when(!right, |col| col.border_r_1().border_color(rgb(BORDER)))
            .child(list)
            .child(
                div().absolute().inset_0().child(
                    Scrollbar::new(&scroll)
                        .id(if right {
                            "right-scrollbar"
                        } else {
                            "left-scrollbar"
                        })
                        .viewport_from_layout()
                        .mode(ScrollbarMode::Always),
                ),
            )
            .into_any_element()
    }
    fn diff_row(
        &self,
        key: &PatchKey,
        patch: &Arc<git::FilePatch>,
        i: usize,
        side: Option<bool>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let line = &patch.lines[i];
        let selected = matches!(self.active.patches.get(key),Some(PatchState::Ready(_,s)) if s.rows.contains(&i));
        if line.kind == '@'
            && let Some(partial) = &patch.partial
        {
            let partial = partial.clone();
            let discard_partial = partial.clone();
            let staged = matches!(key, PatchKey::Work(_, true));
            return h_flex()
                .h(px(20.))
                .bg(rgb(0x2d3b4d))
                .gap_2()
                .child(
                    self.button(
                        ("partial-hunk", i),
                        if staged { "Unstage hunk" } else { "Stage hunk" },
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
                .when(matches!(key, PatchKey::Work(_, false)) && discard_partial.can_discard(), |row| row.child(
                    self.button(("discard-hunk", i), "Discard hunk…").h(px(18.)).ghost()
                        .on_click(cx.listener(move |this, _, window, cx| {
                            cx.stop_propagation();
                            this.show_modal(Modal::Confirm("Discard this unstaged hunk? This changes the working file and cannot be undone. Staged content is preserved.".into(), Operation::DiscardPartial { patch: discard_partial.clone(), selection: git::PatchSelection::Hunk(i) }), window, cx);
                        }))
                ))
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
        let highlights = if self.active.diff_preferences.word_highlight {
            patch.display.highlights[i].as_slice()
        } else {
            &[]
        };
        let matched = self
            .active
            .diff_searches
            .get(key)
            .is_some_and(|(query, _)| {
                !query.is_empty() && line.text.to_lowercase().contains(query)
            });
        let row = patch_line(line, selected, matched, highlights, side);
        if patch.partial.is_none() || !matches!(line.kind, '+' | '-') {
            return row.into_any_element();
        }
        let expected = patch.clone();
        let key = key.clone();
        row.id((
            if side == Some(true) {
                "partial-right"
            } else {
                "partial-line"
            },
            i,
        ))
        .cursor_pointer()
        .on_click(cx.listener(move |this, event: &ClickEvent, _, cx| {
            cx.stop_propagation();
            if this.busy() {
                return;
            }
            if let Some(PatchState::Ready(current, selection)) = this.active.patches.get_mut(&key)
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
        }))
        .into_any_element()
    }
    fn diff_controls(&self, key: &PatchKey, cx: &mut Context<Self>) -> Div {
        let prefs = self.active.diff_preferences;
        let find_key = key.clone();
        let next_key = key.clone();
        h_flex()
            .h(px(28.))
            .px_3()
            .gap_2()
            .bg(rgb(PANEL))
            .child(
                self.button(
                    "diff-whitespace",
                    if prefs.ignore_whitespace {
                        "Ignore whitespace: on"
                    } else {
                        "Ignore whitespace: off"
                    },
                )
                .ghost()
                .on_click(cx.listener(|this, _, _, cx| {
                    if this.busy() {
                        return;
                    }
                    this.active.diff_preferences.ignore_whitespace =
                        !this.active.diff_preferences.ignore_whitespace;
                    this.patch_generation += 1;
                    this.active.patches.clear();
                    this.active.scroll.invalidate_variables();
                    this.active.restored_lines.clear();
                    this.resume_expanded(cx);
                    cx.notify();
                })),
            )
            .child(
                self.button("diff-find", "Find…")
                    .ghost()
                    .on_click(cx.listener(move |this, _, w, cx| {
                        this.show_modal(Modal::DiffSearch(find_key.clone()), w, cx);
                    })),
            )
            .when(self.active.diff_searches.contains_key(key), |row| {
                row.child(
                    self.button("diff-next", "Next match")
                        .ghost()
                        .on_click(cx.listener(move |this, _, _, cx| {
                            let query = this
                                .active
                                .diff_searches
                                .get(&next_key)
                                .map(|s| s.0.clone())
                                .unwrap_or_default();
                            this.search_diff(&next_key, &query, true, cx);
                        })),
                )
            })
            .child(
                self.button(
                    "diff-layout",
                    if prefs.side_by_side {
                        "Side by side"
                    } else {
                        "Unified"
                    },
                )
                .ghost()
                .on_click(cx.listener(|this, _, _, cx| {
                    this.active.diff_preferences.side_by_side =
                        !this.active.diff_preferences.side_by_side;
                    this.active.scroll.invalidate_variables();
                    cx.notify();
                })),
            )
            .child(
                self.button(
                    "diff-words",
                    if prefs.word_highlight {
                        "Word highlight: on"
                    } else {
                        "Word highlight: off"
                    },
                )
                .ghost()
                .on_click(cx.listener(|this, _, _, cx| {
                    this.active.diff_preferences.word_highlight =
                        !this.active.diff_preferences.word_highlight;
                    cx.notify();
                })),
            )
            .child(
                self.button(
                    "diff-context",
                    if prefs.full_context {
                        "Full context"
                    } else {
                        "Context: 3 lines"
                    },
                )
                .ghost()
                .on_click(cx.listener(|this, _, _, cx| {
                    if this.busy() {
                        return;
                    }
                    this.active.diff_preferences.full_context =
                        !this.active.diff_preferences.full_context;
                    this.patch_generation += 1;
                    this.active.patches.clear();
                    this.active.scroll.invalidate_variables();
                    this.active.restored_lines.clear();
                    this.resume_expanded(cx);
                    cx.notify();
                })),
            )
    }
    pub(super) fn search_diff(
        &mut self,
        key: &PatchKey,
        text: &str,
        next: bool,
        cx: &mut Context<Self>,
    ) {
        let query = text.to_lowercase();
        if query.is_empty() {
            self.active.diff_searches.remove(key);
            cx.notify();
            return;
        }
        let Some(PatchState::Ready(patch, _)) = self.active.patches.get(key) else {
            return;
        };
        let matches = patch
            .lines
            .iter()
            .enumerate()
            .filter(|(_, l)| l.text.to_lowercase().contains(&query))
            .map(|(i, _)| i)
            .collect::<Vec<_>>();
        if matches.is_empty() {
            self.active.notice = "No matches in this diff".into();
            self.active.error = false;
            self.active.diff_searches.remove(key);
            cx.notify();
            return;
        }
        let current = self
            .active
            .diff_searches
            .get(key)
            .filter(|(old, _)| old == &query)
            .map(|(_, row)| *row);
        let target = if next {
            current
                .and_then(|row| matches.iter().copied().find(|i| *i > row))
                .unwrap_or(matches[0])
        } else {
            matches[0]
        };
        let view_row = if self.active.diff_preferences.side_by_side {
            patch
                .display
                .rows
                .iter()
                .position(|row| row.left == Some(target) || row.right == Some(target))
                .unwrap_or(0)
        } else {
            target
        };
        self.active
            .scroll
            .list(&key.scroll_id())
            .scroll_to_item(view_row, ScrollStrategy::Nearest);
        self.active
            .diff_searches
            .insert(key.clone(), (query, target));
        self.active.notice = format!(
            "Match {} of {}",
            matches.iter().position(|i| *i == target).unwrap_or(0) + 1,
            matches.len()
        );
        self.active.error = false;
        cx.notify();
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
                    .child(
                        self.button("verify-commit-signature", "Verify signature…")
                            .ghost()
                            .on_click(cx.listener({
                                let id = detail.id.clone();
                                move |this, _, _, cx| this.verify_commit_signature(id.clone(), cx)
                            })),
                    )
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
                        .track_scroll(&self.active.scroll.area("commit-description"))
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
                                this.active.scroll.invalidate_variables();
                                cx.notify();
                            })),
                    ),
            )
            .child(
                list(
                    self.active
                        .scroll
                        .variable("commit-files-scroll", detail.files.len()),
                    cx.processor(|this, index: usize, _, cx| {
                        let Some(detail) = &this.active.commit_detail else {
                            return div().into_any_element();
                        };
                        let Some(file) = detail.files.get(index) else {
                            return div().into_any_element();
                        };
                        this.commit_file_row(index, file, &detail.id, cx)
                            .into_any_element()
                    }),
                )
                .flex_1()
                .min_h_0(),
            )
            .when(detail.files.is_empty(), |col| {
                col.child(
                    div()
                        .p_3()
                        .text_color(rgb(MUTED))
                        .child("No file changes relative to the first parent."),
                )
            })
            .into_any_element()
    }

    fn commit_file_row(
        &self,
        index: usize,
        file: &CommitFile,
        revision: &str,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let source = PatchSource::Commit(revision.to_owned(), file.clone());
        let key = source.key();
        let expanded = self.active.expanded.contains(&key);
        let label = if let Some(old) = &file.original {
            format!("{} → {}", old.display(), file.path.display())
        } else {
            file.path.display().to_string()
        };
        v_flex()
            .w_full()
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
                    .on_click(
                        cx.listener(move |this, _, _, cx| this.toggle_patch(source.clone(), cx)),
                    )
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
                        revision.to_owned(),
                        if file.status == 'D' {
                            None
                        } else {
                            Some((file.path.clone(), revision.to_owned()))
                        },
                        ("commit-file-tools", index),
                        cx,
                    )),
            )
            .when(expanded, |col| {
                col.child(self.patch_body(&key, ("commit-patch", index), cx))
            })
    }

    fn file_group_header(
        &self,
        staged: bool,
        count: usize,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        v_flex().w_full().flex_shrink_0().child(
            h_flex()
                .h(px(32.))
                .px_3()
                .gap_2()
                .bg(rgb(BG))
                .border_b_1()
                .border_color(rgb(BORDER))
                .child(div().text_xs().text_color(rgb(MUTED)).child("▾"))
                .child(
                    div()
                        .flex_1()
                        .text_xs()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(format!(
                            "{}  {}",
                            if staged {
                                "Staged files"
                            } else {
                                "Working directory"
                            },
                            count
                        )),
                )
                .child(
                    self.button(
                        if staged { "unstage-all" } else { "stage-all" },
                        if staged { "Unstage all" } else { "Stage all" },
                    )
                    .ghost()
                    .disabled(
                        self.busy()
                            || count == 0
                            || self.snapshot.files.iter().any(|f| f.conflict()),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.perform(
                            if staged {
                                Operation::UnstageAll
                            } else {
                                Operation::StageAll
                            },
                            cx,
                        )
                    })),
                ),
        )
    }
    fn work_file_row(
        &self,
        index: usize,
        file: &FileChange,
        staged: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let source = PatchSource::Work(file.clone(), staged);
        let key = source.key();
        let expanded = self.active.expanded.contains(&key);
        let action = file.clone();
        let discard = file.clone();
        let open_path = file.path.clone();
        let editor_path = file.path.clone();
        let image_file = file.clone();
        let reveal_path = file.path.clone();
        let status = if file.conflict() {
            '!'
        } else if staged {
            file.index
        } else {
            file.worktree
        };
        v_flex().w_full().flex_shrink_0()
                    .child(h_flex().id((if staged {"staged-row"} else {"work-row"},index)).h(px(32.)).px_3().gap_2().cursor_pointer()
                        .bg(rgb(if expanded {0x333c48} else {PANEL})).hover(|s|s.bg(rgb(0x343e4b)))
                        .border_b_1().border_color(rgb(BORDER))
                        .on_click(cx.listener(move |this, _, _, cx|this.toggle_patch(source.clone(),cx)))
                        .child(Icon::new(if expanded {IconName::ChevronDown} else {IconName::ChevronRight}).size(px(12.)).text_color(rgb(MUTED)))
                        .child(status_badge(status))
                        .child(div().flex_1().min_w_0().truncate().text_size(px(12.)).child(file.path.display().to_string()))
                        .child(self.button((if staged {"open-staged-file"} else {"open-work-file"},index),"Open").ghost().on_click(cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation();
                            this.open_work_file(&open_path, false, cx);
                        })))
                        .child(self.button((if staged {"edit-staged-file"} else {"edit-work-file"},index),"Editor…").ghost().on_click(cx.listener(move |this, _, w, cx| {
                            cx.stop_propagation();
                            this.show_modal(Modal::ExternalEditor(editor_path.clone()), w, cx);
                            let tool = this.settings.external_editor.clone();
                            this.form_a.update(cx, |s, cx| s.set_value(tool.program, w, cx));
                            this.form_b.update(cx, |s, cx| s.set_value(serde_json::to_string(&tool.args).unwrap_or_default(), w, cx));
                        })))
                        .when(file.path.extension().and_then(|s| s.to_str()).is_some_and(|s| ["png","jpg","jpeg"].iter().any(|x| s.eq_ignore_ascii_case(x))), |row| row.child(self.button((if staged {"preview-staged-image"} else {"preview-work-image"},index), "Image…").ghost().on_click(cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation(); this.open_image_preview(image_file.clone(), staged, cx);
                        }))))
                        .child(self.button((if staged {"reveal-staged-file"} else {"reveal-work-file"},index),"Reveal").ghost().on_click(cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation();
                            this.open_work_file(&reveal_path, true, cx);
                        })))
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
    }
    fn open_work_file(&mut self, path: &std::path::Path, reveal: bool, cx: &mut Context<Self>) {
        let Some(repo) = self.active.repo.as_ref() else {
            return;
        };
        if !path
            .components()
            .all(|c| matches!(c, std::path::Component::Normal(_)))
        {
            self.active.notice = "Invalid repository path".into();
            self.active.error = true;
        } else {
            let absolute = repo.root.join(path);
            if !absolute.exists() {
                self.active.notice = format!("Working file does not exist: {}", path.display());
                self.active.error = true;
            } else if reveal {
                cx.reveal_path(&absolute);
            } else {
                cx.open_with_system(&absolute);
            }
        }
        cx.notify();
    }
    pub(super) fn submit_external_editor(
        &mut self,
        path: PathBuf,
        program: String,
        args: String,
        cx: &mut Context<Self>,
    ) {
        let result = (|| -> anyhow::Result<()> {
            let tool = git::ExternalEditor {
                program,
                args: serde_json::from_str(&args)?,
            };
            tool.validate()?;
            self.settings.set_external_editor(tool.clone())?;
            self.active
                .repo
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("Repository is not open"))?
                .launch_editor(&path, &tool)
        })();
        match result {
            Ok(()) => {
                self.modal = None;
                self.active.error = false;
                self.active.notice = "Editor launched".into();
            }
            Err(error) => self.modal_error = Some(format!("{error:#}")),
        }
        cx.notify();
    }
    fn open_image_preview(&mut self, file: FileChange, staged: bool, cx: &mut Context<Self>) {
        if self.busy() || self.modal.is_some() {
            return;
        }
        let Some(repo) = self.active.repo.clone() else {
            return;
        };
        self.image_generation += 1;
        let generation = self.image_generation;
        let tab = self.active.id;
        self.modal = Some(Modal::ImageLoading);
        let work = cx
            .background_executor()
            .spawn(async move { repo.preview_work_images(&file, staged) });
        cx.spawn(async move |this, cx| {
            let result = work.await;
            let _ = this.update(cx, |this, cx| {
                if this.active.id != tab
                    || this.image_generation != generation
                    || !matches!(this.modal, Some(Modal::ImageLoading))
                {
                    return;
                }
                match result {
                    Ok(pair) => {
                        let convert = |data: git::ImageData| {
                            let format = match data.kind {
                                git::ImageKind::Png => ImageFormat::Png,
                                git::ImageKind::Jpeg => ImageFormat::Jpeg,
                            };
                            (
                                Arc::new(Image::from_bytes(format, data.bytes)),
                                data.width,
                                data.height,
                            )
                        };
                        this.modal = Some(Modal::ImagePreview(Arc::new(ImageView {
                            before: pair.before.map(convert),
                            after: pair.after.map(convert),
                        })));
                    }
                    Err(error) => {
                        this.modal = None;
                        this.active.notice = format!("Image preview: {error:#}");
                        this.active.error = true;
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
    pub(super) fn image_preview_modal(&self, modal: Modal, cx: &mut Context<Self>) -> AnyElement {
        let content = match modal {
            Modal::ImageLoading => div().child("Loading image preview…").into_any_element(),
            Modal::ImagePreview(pair) => h_flex()
                .gap_3()
                .children(
                    [
                        ("Before", pair.before.clone()),
                        ("After", pair.after.clone()),
                    ]
                    .into_iter()
                    .map(|(label, data)| {
                        v_flex()
                            .w(px(320.))
                            .gap_2()
                            .child(div().text_sm().child(match &data {
                                Some((_, w, h)) => format!("{label} · {w} × {h}"),
                                None => format!("{label} · missing"),
                            }))
                            .when_some(data, |view, (image, _, _)| {
                                view.child(
                                    img(image)
                                        .w(px(320.))
                                        .h(px(280.))
                                        .object_fit(ObjectFit::Contain),
                                )
                            })
                    }),
                )
                .into_any_element(),
            _ => unreachable!(),
        };
        let card = v_flex()
            .w(px(700.))
            .p_4()
            .gap_3()
            .bg(rgb(PANEL))
            .rounded_lg()
            .child(
                h_flex().child(div().flex_1().child("Image preview")).child(
                    self.button("close-image-preview", "×")
                        .ghost()
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.modal = None;
                            cx.notify();
                        })),
                ),
            )
            .child(content);
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
    pub(super) fn changes_list(&self, cx: &mut Context<Self>) -> AnyElement {
        let projection = self.active.projection();
        let unstaged = projection.unstaged.len();
        let staged = projection.staged.len();
        list(
            self.active
                .scroll
                .variable("changes-scroll", unstaged + staged + 2),
            cx.processor(move |this, index: usize, _, cx| {
                if index == 0 {
                    return this
                        .file_group_header(false, unstaged, cx)
                        .into_any_element();
                }
                if index == unstaged + 1 {
                    return this.file_group_header(true, staged, cx).into_any_element();
                }
                let (staged, row, indices) = if index <= unstaged {
                    (false, index - 1, &projection.unstaged)
                } else {
                    (true, index - unstaged - 2, &projection.staged)
                };
                let Some(file) = indices.get(row).and_then(|&i| this.snapshot.files.get(i)) else {
                    return div().into_any_element();
                };
                this.work_file_row(row, file, staged, cx).into_any_element()
            }),
        )
        .flex_1()
        .min_h_0()
        .into_any_element()
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
fn patch_line(
    line: &DiffLine,
    selected: bool,
    matched: bool,
    highlights: &[std::ops::Range<usize>],
    side: Option<bool>,
) -> Div {
    let (background, foreground, emphasis) = match line.kind {
        '+' => (0x293e38, 0xb8e5c6, 0x39654d),
        '-' => (0x443137, 0xedb5b7, 0x79454d),
        '@' => (0x2d3b4d, 0x9fbfe9, 0x2d3b4d),
        _ => (EDITOR, TEXT, EDITOR),
    };
    let background = if matched && !selected {
        0x5b512d
    } else {
        background
    };
    let text = highlighted_text(&line.text, highlights, emphasis);
    h_flex()
        .h(px(20.))
        .font_family("Menlo")
        .text_size(px(11.))
        .bg(rgb(if selected { 0x375776 } else { background }))
        .child(
            div()
                .w(px(12.))
                .flex_shrink_0()
                .text_color(rgb(ACCENT))
                .child(if selected { "▎" } else { "" }),
        )
        .when(side != Some(true), |row| {
            row.child(
                div()
                    .w(px(38.))
                    .flex_shrink_0()
                    .text_right()
                    .pr_2()
                    .text_color(rgb(MUTED))
                    .child(line.old.clone()),
            )
        })
        .when(side != Some(false), |row| {
            row.child(
                div()
                    .w(px(38.))
                    .flex_shrink_0()
                    .text_right()
                    .pr_2()
                    .text_color(rgb(MUTED))
                    .child(line.new.clone()),
            )
        })
        .child(text.text_color(rgb(foreground)))
}
fn highlighted_text(text: &str, ranges: &[std::ops::Range<usize>], color: u32) -> Div {
    let mut row = h_flex().gap_0().whitespace_nowrap().pr_4();
    let mut start = 0;
    for range in ranges {
        if start < range.start {
            row = row.child(
                div()
                    .flex_shrink_0()
                    .child(text[start..range.start].replace('\t', "    ")),
            );
        }
        row = row.child(
            div()
                .flex_shrink_0()
                .bg(rgb(color))
                .child(text[range.clone()].replace('\t', "    ")),
        );
        start = range.end;
    }
    if start < text.len() {
        row = row.child(
            div()
                .flex_shrink_0()
                .child(text[start..].replace('\t', "    ")),
        );
    }
    row
}

#[cfg(test)]
mod load_tests {
    use super::*;
    #[::core::prelude::v1::test]
    fn load_limit_survives_cache_eviction_and_is_per_repository() {
        let mut tab = RepoTab::default();
        tab.patches.set_limits(1, 1024);
        let mut permits = Vec::new();
        for i in 0..4 {
            permits.push(LoadPermit::acquire(&tab.patch_loading).unwrap());
            tab.patches.insert(
                PatchKey::Work(format!("file-{i}").into(), false),
                PatchState::Loading,
            );
        }
        assert_eq!(tab.patches.len(), 1);
        assert!(LoadPermit::acquire(&tab.patch_loading).is_none());
        assert!(LoadPermit::acquire(&RepoTab::default().patch_loading).is_some());
        permits.pop();
        assert!(LoadPermit::acquire(&tab.patch_loading).is_some());
        drop(permits);
        assert_eq!(
            tab.patch_loading.load(std::sync::atomic::Ordering::Acquire),
            0
        );
    }
}
