use super::*;

pub(super) use gitbuddy::session::Inspection as InspectRequest;
#[derive(Clone, Debug)]
pub(super) enum InspectResult {
    History(Arc<git::FileHistory>),
    Blame(Arc<git::BlameView>),
    Compare(Arc<git::Comparison>),
    Stash(Arc<git::StashPreview>),
}
#[derive(Clone, Debug, Default)]
pub(super) enum InspectState {
    #[default]
    Empty,
    Loading(InspectRequest),
    Ready(InspectResult),
    Error(InspectRequest, String),
}

impl GitBuddy {
    pub(super) fn begin_inspection(&mut self, request: InspectRequest, cx: &mut Context<Self>) {
        self.load_inspection(request, false, cx);
    }
    pub(super) fn load_inspection(
        &mut self,
        request: InspectRequest,
        restoring: bool,
        cx: &mut Context<Self>,
    ) {
        if self.busy() {
            return;
        }
        let Some(repo) = self.active.repo.clone() else {
            return;
        };
        self.modal = None;
        self.active.selection = Selection::Inspect;
        self.active.commit_detail = None;
        self.active.inspection = InspectState::Loading(request.clone());
        if !restoring {
            self.active.expanded.clear();
            self.active.patches.clear();
        }
        self.active.error = false;
        self.patch_generation += 1;
        self.selection_generation += 1;
        let generation = self.selection_generation;
        let root = repo.root.clone();
        let worker_request = request.clone();
        let task = cx.background_executor().spawn(async move {
            match worker_request {
                InspectRequest::History(path, revision, limit) => repo
                    .file_history(&path, &revision, limit)
                    .map(|v| InspectResult::History(Arc::new(v))),
                InspectRequest::Blame(path, revision) => repo
                    .blame(&path, &revision)
                    .map(|v| InspectResult::Blame(Arc::new(v))),
                InspectRequest::Compare(base, target) => repo
                    .compare(&base, &target)
                    .map(|v| InspectResult::Compare(Arc::new(v))),
                InspectRequest::Stash(id) => repo
                    .stash_preview(&id)
                    .map(|v| InspectResult::Stash(Arc::new(v))),
            }
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                if this.selection_generation != generation
                    || this.active.repo.as_ref().map(|r| &r.root) != Some(&root)
                    || !matches!(this.active.selection, Selection::Inspect)
                {
                    return;
                }
                this.active.inspection = match result {
                    Ok(result) => InspectState::Ready(result),
                    Err(error) => InspectState::Error(request, format!("{error:#}")),
                };
                this.resume_expanded(cx);
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    pub(super) fn inspection_title(&self) -> String {
        match &self.active.inspection {
            InspectState::Ready(InspectResult::History(v)) => {
                format!("File history · {}", v.path.display())
            }
            InspectState::Ready(InspectResult::Blame(v)) => format!("Blame · {}", v.path.display()),
            InspectState::Ready(InspectResult::Compare(_)) => "Compare revisions".into(),
            InspectState::Ready(InspectResult::Stash(v)) => format!("Stash · {}", &v.id[..8]),
            _ => "Repository inspection".into(),
        }
    }

    pub(super) fn inspection_sources(&self) -> Vec<PatchSource> {
        match &self.active.inspection {
            InspectState::Ready(InspectResult::Stash(v)) => v
                .sections
                .iter()
                .flat_map(|section| {
                    let section = Arc::new(section.clone());
                    section
                        .files
                        .clone()
                        .into_iter()
                        .map(move |f| PatchSource::Stash(section.clone(), f))
                })
                .collect(),
            InspectState::Ready(InspectResult::Compare(v)) => v
                .files
                .iter()
                .cloned()
                .map(|f| PatchSource::Compare(v.clone(), f))
                .collect(),
            InspectState::Ready(InspectResult::History(v)) => v
                .entries
                .iter()
                .map(|e| PatchSource::Commit(e.commit.id.clone(), e.file.clone()))
                .collect(),
            _ => Vec::new(),
        }
    }

    pub(super) fn open_compare(
        &mut self,
        base: Option<String>,
        target: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.busy() || self.repo.is_none() {
            return;
        }
        let base = base
            .or_else(|| self.comparison_base.clone())
            .unwrap_or_else(|| match &self.selection {
                Selection::Commit(id) => id.clone(),
                _ => self
                    .snapshot
                    .commits
                    .iter()
                    .find(|c| Some(&c.id) == self.snapshot.head_id.as_ref())
                    .and_then(|c| c.parents.first().cloned())
                    .unwrap_or_else(|| "HEAD".into()),
            });
        self.show_modal(Modal::Compare, window, cx);
        self.form_a
            .update(cx, |s, cx| s.set_value(base, window, cx));
        self.form_b.update(cx, |s, cx| {
            s.set_value(target.unwrap_or_else(|| "HEAD".into()), window, cx)
        });
    }

    pub(super) fn compare_choices(&self, cx: &mut Context<Self>) -> AnyElement {
        v_flex().gap_2()
            .child(div().text_xs().text_color(rgb(MUTED)).child("Direct tree comparison: Base → Target. Use full refs to disambiguate a branch and tag with the same name."))
            .child(h_flex().gap_2()
                .child(self.button("swap-compare", "Swap direction").ghost().on_click(cx.listener(|this,_,w,cx| {
                    let a=this.form_a.read(cx).value().to_string(); let b=this.form_b.read(cx).value().to_string();
                    this.form_a.update(cx,|s,cx|s.set_value(b,w,cx)); this.form_b.update(cx,|s,cx|s.set_value(a,w,cx));
                })))
                .child(self.button("compare-head", "Target: HEAD").ghost().on_click(cx.listener(|this,_,w,cx|this.form_b.update(cx,|s,cx|s.set_value("HEAD",w,cx))))))
            .child(v_flex().id("compare-branches").max_h(px(130.)).overflow_y_scroll().children(self.snapshot.branches.iter().enumerate().map(|(i,branch)| {
                let full=format!("refs/{}/{}",if branch.remote {"remotes"} else {"heads"},branch.name);
                let base=full.clone();
                h_flex().gap_2().child(div().flex_1().text_xs().truncate().child(branch.name.clone()))
                    .child(self.button(("compare-base",i),"Base").ghost().on_click(cx.listener(move|this,_,w,cx|this.form_a.update(cx,|s,cx|s.set_value(base.clone(),w,cx)))))
                    .child(self.button(("compare-target",i),"Target").ghost().on_click(cx.listener(move|this,_,w,cx|this.form_b.update(cx,|s,cx|s.set_value(full.clone(),w,cx)))))
            }))).into_any_element()
    }

    pub(super) fn open_file_tools(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy() || self.repo.is_none() {
            return;
        }
        let spec = match &self.selection {
            Selection::Commit(id) => id.clone(),
            _ => "HEAD".into(),
        };
        self.show_modal(Modal::FileTools, window, cx);
        self.form_b
            .update(cx, |s, cx| s.set_value(spec.clone(), window, cx));
        self.request_tool_files(spec, cx);
    }

    fn request_tool_files(&mut self, spec: String, cx: &mut Context<Self>) {
        let Some(repo) = self.repo.clone() else {
            return;
        };
        self.tool_generation += 1;
        let generation = self.tool_generation;
        let root = repo.root.clone();
        self.tool_files = None;
        let task = cx
            .background_executor()
            .spawn(async move { repo.revision_files(&spec) });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                if generation != this.tool_generation
                    || !matches!(this.modal, Some(Modal::FileTools))
                    || this.repo.as_ref().map(|r| &r.root) != Some(&root)
                {
                    return;
                }
                this.tool_files = Some(result.map(Arc::new).map_err(|e| format!("{e:#}")));
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    pub(super) fn file_tools_modal(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut card = v_flex()
            .w(px(640.))
            .p_4()
            .gap_2()
            .rounded_lg()
            .bg(rgb(PANEL))
            .border_1()
            .border_color(rgb(BORDER))
            .shadow_lg()
            .child(
                h_flex()
                    .justify_between()
                    .child(div().text_lg().child("File history / Blame"))
                    .child(
                        self.button("file-tools-close", "×")
                            .ghost()
                            .on_click(cx.listener(|this, _, w, cx| {
                                this.modal = None;
                                this.focus.focus(w, cx);
                                cx.notify();
                            })),
                    ),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(rgb(MUTED))
                    .child("Repository-relative file path / filter"),
            )
            .child(Input::new(&self.form_a))
            .child(
                div()
                    .text_xs()
                    .text_color(rgb(MUTED))
                    .child("Revision: branch, tag or commit ID"),
            )
            .child(Input::new(&self.form_b))
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        self.button("list-revision-files", "Load files at revision")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.request_tool_files(
                                    this.form_b.read(cx).value().to_string(),
                                    cx,
                                )
                            })),
                    )
                    .child(
                        self.button("manual-history", "File history")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.begin_inspection(
                                    InspectRequest::History(
                                        PathBuf::from(this.form_a.read(cx).value().as_str()),
                                        this.form_b.read(cx).value().to_string(),
                                        100,
                                    ),
                                    cx,
                                )
                            })),
                    )
                    .child(self.button("manual-blame", "Blame").on_click(cx.listener(
                        |this, _, _, cx| {
                            this.begin_inspection(
                                InspectRequest::Blame(
                                    PathBuf::from(this.form_a.read(cx).value().as_str()),
                                    this.form_b.read(cx).value().to_string(),
                                ),
                                cx,
                            )
                        },
                    ))),
            );
        card = match &self.tool_files {
            None => card.child("Loading files…"),
            Some(Err(error)) => card.child(div().text_color(rgb(0xe9a3a9)).child(error.clone())),
            Some(Ok(files)) => {
                let filter = self.form_a.read(cx).value().to_lowercase();
                let revision = files.revision.id.clone();
                let matching = files
                    .paths
                    .iter()
                    .filter(|p| p.to_string_lossy().to_lowercase().contains(&filter))
                    .count();
                card.child(div().text_xs().text_color(rgb(MUTED)).child(format!(
                    "Files at {} · {}{}",
                    &revision[..8],
                    files.paths.len(),
                    if files.truncated {
                        " (first 5,000)"
                    } else {
                        ""
                    }
                )))
                .when(matching == 0, |card| card.child(div().text_xs().text_color(rgb(MUTED)).child("No files match this filter. Enter an exact path to inspect a deleted file.")))
                .child(
                    uniform_list(
                        "tracked-file-choices",
                        matching,
                        cx.processor(|this, range: std::ops::Range<usize>, _, cx| {
                            let Some(Ok(files)) = &this.tool_files else {
                                return Vec::new();
                            };
                            let filter = this.form_a.read(cx).value().to_lowercase();
                            let filtered: Vec<_> = files
                                .paths
                                .iter()
                                .filter(|p| p.to_string_lossy().to_lowercase().contains(&filter))
                                .collect();
                            range
                                .filter_map(|i| {
                                    filtered.get(i).map(|path| {
                                        let history = InspectRequest::History(
                                            (*path).clone(),
                                            files.revision.id.clone(),
                                            100,
                                        );
                                        let blame = InspectRequest::Blame(
                                            (*path).clone(),
                                            files.revision.id.clone(),
                                        );
                                        h_flex()
                                            .id(("tracked-choice", i))
                                            .w_full()
                                            .h(px(28.))
                                            .gap_2()
                                            .border_b_1()
                                            .border_color(rgb(BORDER))
                                            .child(
                                                div()
                                                    .flex_1()
                                                    .min_w_0()
                                                    .truncate()
                                                    .text_xs()
                                                    .child(path.display().to_string()),
                                            )
                                            .child(
                                                this.button("history", "History").ghost().on_click(
                                                    cx.listener(move |this, _, _, cx| {
                                                        this.begin_inspection(history.clone(), cx)
                                                    }),
                                                ),
                                            )
                                            .child(this.button("blame", "Blame").ghost().on_click(
                                                cx.listener(move |this, _, _, cx| {
                                                    this.begin_inspection(blame.clone(), cx)
                                                }),
                                            ))
                                    })
                                })
                                .collect::<Vec<_>>()
                        }),
                    )
                    .h(px(224.))
                    .w_full(),
                )
            }
        };
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

    pub(super) fn inspection_view(&self, cx: &mut Context<Self>) -> AnyElement {
        match &self.inspection {
            InspectState::Empty => div()
                .p_3()
                .child("Choose a file or two revisions to inspect.")
                .into_any_element(),
            InspectState::Loading(_) => div()
                .p_3()
                .child("Loading repository inspection…")
                .into_any_element(),
            InspectState::Error(request, error) => {
                let request = request.clone();
                v_flex()
                    .p_3()
                    .gap_2()
                    .child(div().text_color(rgb(0xe9a3a9)).child(error.clone()))
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                self.button("retry-inspection", "Retry")
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.begin_inspection(request.clone(), cx)
                                    })),
                            )
                            .child(self.button("inspect-file-tools", "Choose file…").on_click(
                                cx.listener(|this, _, w, cx| this.open_file_tools(w, cx)),
                            ))
                            .child(self.button("inspect-compare-tools", "Compare…").on_click(
                                cx.listener(|this, _, w, cx| this.open_compare(None, None, w, cx)),
                            )),
                    )
                    .into_any_element()
            }
            InspectState::Ready(InspectResult::History(history)) => {
                self.file_history_view(history, cx)
            }
            InspectState::Ready(InspectResult::Blame(blame)) => self.blame_view(blame, cx),
            InspectState::Ready(InspectResult::Compare(comparison)) => {
                self.comparison_view(comparison, cx)
            }
            InspectState::Ready(InspectResult::Stash(preview)) => {
                self.stash_preview_view(preview, cx)
            }
        }
    }

    fn file_history_view(
        &self,
        history: &Arc<git::FileHistory>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let start_blame = InspectRequest::Blame(history.path.clone(), history.revision.id.clone());
        let more = InspectRequest::History(
            history.path.clone(),
            history.revision.id.clone(),
            (history.entries.len() + 100).min(2000),
        );
        v_flex().flex_1().min_h_0()
            .child(h_flex().px_3().py_2().gap_2().bg(rgb(PANEL))
                .child(div().flex_1().min_w_0().truncate().text_xs().text_color(rgb(MUTED)).child(format!("At {} · {} · follows renames across parents",&history.revision.id[..8],history.entries.len())))
                .child(self.button("history-blame","Blame at revision").ghost().on_click(cx.listener(move|this,_,_,cx|this.begin_inspection(start_blame.clone(),cx)))))
                .child(v_flex().id("file-history-scroll").track_scroll(&self.active.scroll.area("file-history-scroll")).flex_1().min_h_0().overflow_y_scroll()
                .children(history.entries.iter().enumerate().map(|(i,entry)| {
                    let source=PatchSource::Commit(entry.commit.id.clone(),entry.file.clone());
                    let key=source.key(); let open=self.expanded.contains(&key);
                    let commit=entry.commit.id.clone();
                    let blame=InspectRequest::Blame(entry.file.path.clone(),commit.clone());
                    v_flex().flex_shrink_0()
                        .child(h_flex().id(("file-history-row",i)).px_3().py_1().gap_2().cursor_pointer().bg(rgb(if open {0x333c48} else {PANEL})).border_b_1().border_color(rgb(BORDER))
                            .on_click(cx.listener(move|this,_,_,cx|this.toggle_patch(source.clone(),cx)))
                            .child(div().text_color(rgb(ACCENT)).child(if open {"▾"} else {"▸"}))
                            .child(v_flex().flex_1().min_w_0().gap_1()
                                .child(div().truncate().text_sm().child(entry.commit.subject.clone()))
                                .child(div().truncate().text_xs().text_color(rgb(MUTED)).child(format!("{} · {} · {} · {}{}",&entry.commit.id[..8],entry.commit.author,entry.commit.date,entry.file.path.display(),entry.file.original.as_ref().map(|p|format!(" (from {})",p.display())).unwrap_or_default()))))
                            .child(self.button("open-commit","Commit").ghost().on_click(cx.listener(move|this,_,_,cx|{cx.stop_propagation();this.select(Selection::Commit(commit.clone()),cx);})))
                            .child(self.button("entry-blame","Blame").ghost().disabled(entry.file.status=='D').on_click(cx.listener(move|this,_,_,cx|{cx.stop_propagation();this.begin_inspection(blame.clone(),cx);}))))
                        .when(open,|col|col.child(self.patch_body(&key,("file-history-patch",i),cx)))
                }))
                .when(history.entries.is_empty(),|col|col.child(div().p_3().text_color(rgb(MUTED)).child("No history for this path at the selected revision.")))
                .when(history.has_more && history.entries.len()<2000,|col|col.child(self.button("more-file-history","Load 100 more").ghost().on_click(cx.listener(move|this,_,_,cx|this.begin_inspection(more.clone(),cx)))))
                .when(history.has_more && history.entries.len()>=2000,|col|col.child(div().p_3().text_xs().text_color(rgb(MUTED)).child("Showing the first 2,000 entries. Choose an older revision to continue.")))
                .when(history.scan_limited,|col|col.child(div().p_3().text_xs().text_color(rgb(MUTED)).child("History scan limited to 50,000 commits. Choose an older revision to continue."))))
            .into_any_element()
    }

    fn blame_view(&self, blame: &Arc<git::BlameView>, cx: &mut Context<Self>) -> AnyElement {
        let history = InspectRequest::History(blame.path.clone(), blame.revision.id.clone(), 100);
        v_flex().flex_1().min_h_0()
            .child(h_flex().px_3().py_2().gap_2().bg(rgb(PANEL))
                .child(div().flex_1().text_xs().text_color(rgb(MUTED)).child(format!("Committed content at {} · {} lines{} · click a commit ID for details",&blame.revision.id[..8],blame.lines.len(),if blame.truncated {" (preview limited)"} else {""})))
                .child(self.button("blame-history","File history").ghost().on_click(cx.listener(move|this,_,_,cx|this.begin_inspection(history.clone(),cx)))))
            .when(blame.lines.is_empty(),|col|col.child(div().p_3().child("Empty file at this revision.")))
            .child(uniform_list("blame-lines",blame.lines.len(),cx.processor(|this,range:std::ops::Range<usize>,_,cx| {
                let InspectState::Ready(InspectResult::Blame(blame))=&this.inspection else {return Vec::new();};
                range.filter_map(|i|blame.lines.get(i).map(|line| {
                    let id=line.commit_id.clone(); let boundary=i==0 || blame.lines[i-1].commit_id!=line.commit_id;
                    h_flex().id(("blame-row",i)).h(px(24.)).font_family("Menlo").text_size(px(11.)).bg(rgb(if boundary {0x303846} else {EDITOR})).gap_2()
                        .child(this.button("blame-commit",&id[..8]).ghost().w(px(82.)).h(px(23.)).tooltip(format!("{}\n{} · {}:{}",line.subject,line.author,line.original_path.display(),line.original_line))
                            .on_click(cx.listener(move|this,_,_,cx|this.select(Selection::Commit(id.clone()),cx))))
                        .child(div().w(px(120.)).flex_shrink_0().truncate().text_color(rgb(MUTED)).child(line.author.clone()))
                        .child(div().w(px(112.)).flex_shrink_0().text_color(rgb(MUTED)).child(line.date.split(' ').next().unwrap_or_default().to_string()))
                        .child(div().w(px(40.)).flex_shrink_0().text_right().text_color(rgb(MUTED)).child(line.number.to_string()))
                        .child(div().whitespace_nowrap().child(line.text.replace('\t',"    ")))
                })).collect::<Vec<_>>()
            })).with_horizontal_sizing_behavior(ListHorizontalSizingBehavior::Unconstrained).track_scroll(&self.active.scroll.list("blame-lines")).flex_1().min_h_0()).into_any_element()
    }

    fn comparison_view(
        &self,
        comparison: &Arc<git::Comparison>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let base = comparison.base.id.clone();
        let target = comparison.target.id.clone();
        let change_base = comparison.base.label.clone();
        let change_target = comparison.target.label.clone();
        let revision_label = |revision: &git::Revision| {
            if revision.label == revision.id {
                revision.id[..8].to_string()
            } else {
                format!("{} ({})", revision.label, &revision.id[..8])
            }
        };
        v_flex()
            .flex_1()
            .min_h_0()
            .child(
                v_flex()
                    .px_3()
                    .py_2()
                    .gap_1()
                    .bg(rgb(PANEL))
                    .child(div().truncate().text_sm().child(format!(
                        "{} → {}",
                        revision_label(&comparison.base),
                        revision_label(&comparison.target)
                    )))
                    .child(div().text_xs().text_color(rgb(MUTED)).child(format!(
                        "{} files · +{} −{} · direct tree diff",
                        comparison.files.len(),
                        comparison.insertions,
                        comparison.deletions
                    )))
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                self.button("compare-swap-results", "Swap")
                                    .ghost()
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.begin_inspection(
                                            InspectRequest::Compare(target.clone(), base.clone()),
                                            cx,
                                        )
                                    })),
                            )
                            .child(
                                self.button("change-comparison", "Choose revisions…")
                                    .ghost()
                                    .on_click(cx.listener(move |this, _, w, cx| {
                                        this.open_compare(
                                            Some(change_base.clone()),
                                            Some(change_target.clone()),
                                            w,
                                            cx,
                                        )
                                    })),
                            )
                            .child(
                                self.button("collapse-comparison", "Collapse all")
                                    .ghost()
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.expanded.clear();
                                        cx.notify();
                                    })),
                            ),
                    ),
            )
            .child(
                v_flex()
                    .id("comparison-scroll")
                    .track_scroll(&self.active.scroll.area("comparison-scroll"))
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .children(comparison.files.iter().enumerate().map(|(i, file)| {
                        let source = PatchSource::Compare(comparison.clone(), file.clone());
                        let key = source.key();
                        let open = self.expanded.contains(&key);
                        let path = file.path.clone();
                        let revision = comparison.target.id.clone();
                        let blame = if file.status == 'D' {
                            Some((file.path.clone(), comparison.base.id.clone()))
                        } else {
                            Some((file.path.clone(), revision.clone()))
                        };
                        v_flex()
                            .flex_shrink_0()
                            .child(
                                h_flex()
                                    .id(("compare-file", i))
                                    .h(px(32.))
                                    .px_3()
                                    .gap_2()
                                    .cursor_pointer()
                                    .bg(rgb(if open { 0x333c48 } else { PANEL }))
                                    .border_b_1()
                                    .border_color(rgb(BORDER))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.toggle_patch(source.clone(), cx)
                                    }))
                                    .child(div().text_color(rgb(ACCENT)).child(if open {
                                        "▾"
                                    } else {
                                        "▸"
                                    }))
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(rgb(ACCENT))
                                            .child(file.status.to_string()),
                                    )
                                    .child(
                                        div().flex_1().min_w_0().truncate().text_sm().child(
                                            file.original
                                                .as_ref()
                                                .map(|p| {
                                                    format!(
                                                        "{} → {}",
                                                        p.display(),
                                                        file.path.display()
                                                    )
                                                })
                                                .unwrap_or_else(|| file.path.display().to_string()),
                                        ),
                                    )
                                    .child(self.file_inspect_buttons(
                                        path,
                                        revision,
                                        blame,
                                        ("compare-tools", i),
                                        cx,
                                    )),
                            )
                            .when(open, |col| {
                                col.child(self.patch_body(&key, ("comparison-patch", i), cx))
                            })
                    }))
                    .when(comparison.files.is_empty(), |col| {
                        col.child(
                            div()
                                .p_3()
                                .child("No file differences between these revisions."),
                        )
                    }),
            )
            .into_any_element()
    }

    pub(super) fn file_inspect_buttons(
        &self,
        path: PathBuf,
        revision: String,
        blame: Option<(PathBuf, String)>,
        id: impl Into<ElementId>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let history = InspectRequest::History(path.clone(), revision.clone(), 100);
        let blame = blame.map(|(path, revision)| InspectRequest::Blame(path, revision));
        let blame_disabled = self.busy() || blame.is_none();
        h_flex()
            .id(id)
            .gap_1()
            .child(
                self.button("file-history", "History")
                    .ghost()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        this.begin_inspection(history.clone(), cx);
                    })),
            )
            .child(
                self.button("file-blame", "Blame")
                    .ghost()
                    .disabled(blame_disabled)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        if let Some(request) = &blame {
                            this.begin_inspection(request.clone(), cx);
                        }
                    })),
            )
            .into_any_element()
    }
}
