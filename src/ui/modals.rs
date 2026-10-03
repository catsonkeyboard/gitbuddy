use super::*;

impl GitBuddy {
    pub(super) fn submit_modal(&mut self, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let a = self.form_a.read(cx).value().to_string();
        let b = self.form_b.read(cx).value().to_string();
        let a = a.trim().to_owned();
        let b = b.trim().to_owned();
        let Some(modal) = self.modal.clone() else {
            return;
        };
        match modal {
            Modal::Open | Modal::Init | Modal::Clone => {
                if a.is_empty() || (matches!(modal, Modal::Clone) && b.is_empty()) {
                    self.active.notice = "请填写完整的仓库地址 / 路径".into();
                    self.active.error = true;
                    cx.notify();
                    return;
                }
                self.modal = None;
                let label = if self.active.repo.is_some() {
                    ""
                } else {
                    "Preparing repository…"
                };
                self.dispatch(
                    label,
                    move |progress| {
                        let repo = match modal {
                            Modal::Open => Repository::open(expand_path(&a))?,
                            Modal::Init => Repository::init(&expand_path(&a))?,
                            _ => Repository::clone_repo_with_progress(
                                &a,
                                &expand_path(&b),
                                progress,
                            )?,
                        };
                        Loaded::read(
                            repo,
                            Selection::Work,
                            HISTORY_PAGE_SIZE,
                            "Repository ready".into(),
                            false,
                            false,
                            true,
                        )
                    },
                    cx,
                );
            }
            Modal::Branch => self.perform(Operation::CreateBranch(a), cx),
            Modal::Tag => self.perform(Operation::Tag(a), cx),
            Modal::Remote => self.perform(Operation::AddRemote(a, b), cx),
            Modal::Stash => self.perform(Operation::Stash(a), cx),
            Modal::Identity => self.perform(Operation::SetIdentity(a, b), cx),
            Modal::Confirm(_, operation) => self.perform(operation, cx),
            Modal::RecoveryBranch(target) => {
                self.perform(Operation::RecoverBranch { target, name: a }, cx)
            }
            _ => {}
        }
    }

    pub(super) fn modal_view(&self, modal: Modal, cx: &mut Context<Self>) -> AnyElement {
        if let Modal::History(kind) = modal {
            return self.history_modal(kind, cx);
        }
        let (title, label_a, label_b, submit) = match &modal {
            Modal::Open => ("Open repository", "Local repository path", None, "Open"),
            Modal::Init => (
                "Initialize repository",
                "New directory path",
                None,
                "Initialize",
            ),
            Modal::Clone => (
                "Clone repository",
                "Remote URL (HTTPS / SSH)",
                Some("Destination directory"),
                "Clone",
            ),
            Modal::Branch => (
                "Create branch",
                "New branch name (from HEAD)",
                None,
                "Create & switch",
            ),
            Modal::Tag => ("Create tag", "Tag name (at HEAD)", None, "Create tag"),
            Modal::Remote => (
                "Add remote",
                "Remote name, e.g. origin",
                Some("Remote URL"),
                "Add remote",
            ),
            Modal::Stash => (
                "Stash changes",
                "Description (optional; includes untracked files)",
                None,
                "Stash",
            ),
            Modal::Identity => (
                "Commit identity",
                "Author name (this repository only)",
                Some("Author email"),
                "Save identity",
            ),
            Modal::Confirm(..) => ("Confirm operation", "", None, "Confirm"),
            Modal::BranchActions(..) => ("Branch actions", "", None, ""),
            Modal::StashActions(..) => ("Stash actions", "", None, ""),
            Modal::CommitActions(..) => ("Commit actions", "", None, ""),
            Modal::TagActions(..) => ("Tag actions", "", None, ""),
            Modal::RecoveryBranch(..) => (
                "Create recovery branch",
                "New branch name (current branch stays unchanged)",
                None,
                "Create branch",
            ),
            Modal::History(_) => unreachable!(),
        };
        let mut card = v_flex()
            .w(px(540.))
            .p_6()
            .gap_4()
            .rounded_lg()
            .bg(rgb(PANEL))
            .border_1()
            .border_color(rgb(BORDER))
            .shadow_lg()
            .child(
                h_flex()
                    .justify_between()
                    .child(
                        div()
                            .text_lg()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(title),
                    )
                    .child(
                        self.button("modal-close", "×")
                            .ghost()
                            .on_click(cx.listener(|this, _, w, cx| {
                                this.modal = None;
                                this.focus.focus(w, cx);
                                cx.notify();
                            })),
                    ),
            );
        if !label_a.is_empty() {
            card = card
                .child(div().text_sm().text_color(rgb(MUTED)).child(label_a))
                .child(Input::new(&self.form_a));
        }
        if let Some(label) = label_b {
            card = card
                .child(div().text_sm().text_color(rgb(MUTED)).child(label))
                .child(Input::new(&self.form_b));
        }
        if matches!(modal, Modal::Open) {
            card = card.child(
                self.button("browse", "Browse folders…")
                    .on_click(cx.listener(|this, _, _, cx| this.browse(cx))),
            );
        }
        match modal {
            Modal::Confirm(ref message, _) => {
                card = card.child(div().text_sm().child(message.clone()));
            }
            Modal::BranchActions(name, remote) => {
                let checkout = name.clone();
                let merge = name.clone();
                let delete = name.clone();
                card = card.child(div().text_color(rgb(ACCENT)).child(name));
                if !remote {
                    card = card.child(self.button("branch-switch", "Switch to branch").on_click(
                        cx.listener(move |this, _, _, cx| {
                            this.perform(Operation::Checkout(checkout.clone()), cx)
                        }),
                    ));
                } else {
                    card = card.child(
                        self.button("branch-track", "Create local tracking branch & switch")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.perform(Operation::TrackBranch(checkout.clone()), cx)
                            })),
                    );
                }
                card = card.child(
                    self.button("branch-merge", "Merge into current branch…")
                        .on_click(cx.listener(move |this, _, w, cx| {
                            this.show_modal(
                                Modal::Confirm(
                                    format!("Merge {merge} into {}?", this.snapshot.branch),
                                    Operation::Merge(merge.clone()),
                                ),
                                w,
                                cx,
                            )
                        })),
                );
                if !remote {
                    card = card.child(self.button("branch-delete","Delete merged branch…").on_click(cx.listener(move|this,_,w,cx|this.show_modal(Modal::Confirm(format!("Delete local branch {delete}? Git will reject unmerged branches."),Operation::DeleteBranch(delete.clone())),w,cx))));
                }
            }
            Modal::StashActions(id) => {
                let apply = id.clone();
                let drop = id.clone();
                card =
                    card.child(id)
                        .child(self.button("stash-apply", "Apply (keep stash)").on_click(
                            cx.listener(move |this, _, _, cx| {
                                this.perform(Operation::ApplyStash(apply.clone()), cx)
                            }),
                        ))
                        .child(
                            self.button("stash-drop", "Delete stash…")
                                .on_click(cx.listener(move |this, _, w, cx| {
                                    this.show_modal(
                                        Modal::Confirm(
                                            format!("Permanently delete {drop}?"),
                                            Operation::DropStash(drop.clone()),
                                        ),
                                        w,
                                        cx,
                                    )
                                })),
                        );
            }
            Modal::CommitActions(id) => {
                if self.snapshot.head_id.as_ref() == Some(&id) {
                    card = card
                        .child(
                            self.button("amend-head", "Amend this HEAD commit…")
                                .on_click(cx.listener(|this, _, w, cx| {
                                    this.begin_history(HistoryKind::Amend, w, cx)
                                })),
                        )
                        .child(self.button("undo-head", "Undo this HEAD commit…").on_click(
                            cx.listener(|this, _, w, cx| {
                                this.begin_history(HistoryKind::Undo, w, cx)
                            }),
                        ));
                }
                let revert = id.clone();
                let cherry = id.clone();
                let copy = id.clone();
                card =
                    card.child(div().font_family("Menlo").text_xs().child(id))
                        .child(
                            self.button("copy-id", "Copy commit ID")
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    cx.write_to_clipboard(ClipboardItem::new_string(copy.clone()));
                                    this.modal = None;
                                    this.active.notice = "Commit ID copied".into();
                                    cx.notify();
                                })),
                        )
                        .child(
                            self.button("cherry-pick", "Cherry-pick into current branch…")
                                .on_click(cx.listener(move |this, _, w, cx| {
                                    this.show_modal(
                                        Modal::Confirm(
                                            "Apply this commit to the current branch?".into(),
                                            Operation::CherryPick(cherry.clone()),
                                        ),
                                        w,
                                        cx,
                                    )
                                })),
                        )
                        .child(self.button("revert", "Revert with a new commit…").on_click(
                            cx.listener(move |this, _, w, cx| {
                                this.show_modal(
                                    Modal::Confirm(
                                        "Create a new commit that reverses this commit?".into(),
                                        Operation::Revert(revert.clone()),
                                    ),
                                    w,
                                    cx,
                                )
                            }),
                        ))
                        .child(self.button("abort-cherry", "Abort cherry-pick…").on_click(
                            cx.listener(|this, _, w, cx| {
                                this.show_modal(
                                    Modal::Confirm(
                                        "Abort the current cherry-pick?".into(),
                                        Operation::AbortCherryPick,
                                    ),
                                    w,
                                    cx,
                                )
                            }),
                        ))
                        .child(
                            self.button("continue-cherry", "Continue cherry-pick after resolving")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.perform(Operation::ContinueCherryPick, cx)
                                })),
                        )
                        .child(
                            self.button("continue-revert", "Continue revert after resolving")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.perform(Operation::ContinueRevert, cx)
                                })),
                        )
                        .child(
                            self.button("abort-revert", "Abort revert…")
                                .on_click(cx.listener(|this, _, w, cx| {
                                    this.show_modal(
                                        Modal::Confirm(
                                            "Abort the current revert?".into(),
                                            Operation::AbortRevert,
                                        ),
                                        w,
                                        cx,
                                    )
                                })),
                        );
            }
            Modal::TagActions(name) => {
                let delete = name.clone();
                card = card.child(name).child(
                    self.button("delete-tag", "Delete local tag…")
                        .on_click(cx.listener(move |this, _, w, cx| {
                            this.show_modal(
                                Modal::Confirm(
                                    format!("Delete local tag {delete}?"),
                                    Operation::DeleteTag(delete.clone()),
                                ),
                                w,
                                cx,
                            )
                        })),
                );
            }
            _ => {}
        }
        if !submit.is_empty() {
            card = card.child(
                h_flex()
                    .justify_end()
                    .gap_2()
                    .mt_2()
                    .child(self.button("cancel", "Cancel").on_click(cx.listener(
                        |this, _, w, cx| {
                            this.modal = None;
                            this.focus.focus(w, cx);
                            cx.notify();
                        },
                    )))
                    .child(
                        self.button("submit", submit)
                            .primary()
                            .on_click(cx.listener(|this, _, _, cx| this.submit_modal(cx))),
                    ),
            );
        }
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
}
