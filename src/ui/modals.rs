use super::*;

impl GitBuddy {
    pub(super) fn submit_modal(&mut self, cx: &mut Context<Self>) {
        if self.busy() && !matches!(self.modal, Some(Modal::Open | Modal::Init | Modal::Clone)) {
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
            Modal::RebaseSetup => self.preview_rebase(a, cx),
            Modal::WorktreeCreate => self.perform(
                Operation::CreateWorktree {
                    name: a,
                    path: expand_path(&b),
                },
                cx,
            ),
            Modal::SubmoduleAdd => self.perform(
                Operation::AddSubmodule {
                    url: a,
                    path: b.into(),
                },
                cx,
            ),
            Modal::LfsPattern(track) => self.perform(Operation::LfsTrack { pattern: a, track }, cx),
            Modal::Open | Modal::Init | Modal::Clone => {
                if a.is_empty() || (matches!(modal, Modal::Clone) && b.is_empty()) {
                    self.modal_error = Some("请填写完整的仓库地址 / 路径".into());
                    cx.notify();
                    return;
                }
                self.modal_error = None;
                let preparation = match modal {
                    Modal::Open => tasks::Preparation::Open(expand_path(&a)),
                    Modal::Init => tasks::Preparation::Init(expand_path(&a)),
                    _ => tasks::Preparation::Clone {
                        url: a,
                        destination: expand_path(&b),
                    },
                };
                self.prepare_repository(preparation, cx);
            }
            Modal::Branch => self.perform(Operation::CreateBranch(a), cx),
            Modal::BranchAt(target) => {
                self.perform(Operation::CreateBranchAt { name: a, target }, cx)
            }
            Modal::Tag => self.perform(Operation::Tag(a), cx),
            Modal::Remote => self.perform(Operation::AddRemote(a, b), cx),
            Modal::EditRemote(expected) => self.perform(
                Operation::EditRemote {
                    expected,
                    url: a,
                    push_url: if b.is_empty() { None } else { Some(b) },
                },
                cx,
            ),
            Modal::RenameRemote(expected) => {
                self.perform(Operation::RenameRemote { expected, name: a }, cx)
            }
            Modal::Stash => self.perform(Operation::Stash(a), cx),
            Modal::Identity => self.perform(Operation::SetIdentity(a, b), cx),
            Modal::Confirm(_, operation) => self.perform(operation, cx),
            Modal::RecoveryBranch(target) => {
                self.perform(Operation::RecoverBranch { target, name: a }, cx)
            }
            Modal::Compare => self.begin_inspection(inspect::InspectRequest::Compare(a, b), cx),
            _ => {}
        }
    }

    pub(super) fn modal_view(&self, modal: Modal, cx: &mut Context<Self>) -> AnyElement {
        if matches!(modal, Modal::ResetHistory(_) | Modal::CommitSelection) {
            return self.history_actions_modal(modal, cx);
        }
        if matches!(
            modal,
            Modal::Remotes | Modal::PushSettings | Modal::Upstream(_)
        ) {
            return self.remote_modal(modal, cx);
        }
        if matches!(
            modal,
            Modal::RebasePlan
                | Modal::Worktrees
                | Modal::Submodules
                | Modal::Lfs
                | Modal::RebaseMessage(_)
        ) {
            return self.management_modal(modal, cx);
        }
        let preparation = matches!(modal, Modal::Open | Modal::Init | Modal::Clone);
        if matches!(modal, Modal::FileTools) {
            return self.file_tools_modal(cx);
        }
        if let Modal::History(kind) = modal {
            return self.history_modal(kind, cx);
        }
        let (title, label_a, label_b, submit) = match &modal {
            Modal::RebaseSetup => (
                "Interactive rebase",
                "Upstream / new base: branch, tag or commit",
                None,
                "Build plan",
            ),
            Modal::WorktreeCreate => (
                "Create worktree",
                "New branch / worktree name",
                Some("Absolute destination directory"),
                "Create",
            ),
            Modal::SubmoduleAdd => (
                "Add submodule",
                "Repository URL",
                Some("Relative destination path"),
                "Add",
            ),
            Modal::LfsPattern(track) => (
                if *track {
                    "Track with LFS"
                } else {
                    "Untrack LFS pattern"
                },
                "Pattern, e.g. *.psd (changes .gitattributes)",
                None,
                "Apply",
            ),
            Modal::RebasePlan
            | Modal::Worktrees
            | Modal::Submodules
            | Modal::Lfs
            | Modal::RebaseMessage(_) => unreachable!(),
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
            Modal::BranchAt(_) => (
                "Create branch from commit",
                "New branch name (current checkout stays unchanged)",
                None,
                "Create branch",
            ),
            Modal::ResetHistory(_) | Modal::CommitSelection => unreachable!(),
            Modal::Tag => ("Create tag", "Tag name (at HEAD)", None, "Create tag"),
            Modal::Remote => (
                "Add remote",
                "Remote name, e.g. origin",
                Some("Remote URL"),
                "Add remote",
            ),
            Modal::EditRemote(_) => (
                "Edit remote",
                "Fetch URL",
                Some("Push URL (blank uses fetch URL)"),
                "Save",
            ),
            Modal::RenameRemote(_) => ("Rename remote", "New remote name", None, "Rename"),
            Modal::Remotes | Modal::PushSettings | Modal::Upstream(_) => unreachable!(),
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
            Modal::FileTools => unreachable!(),
            Modal::Compare => (
                "Compare commits / branches",
                "Base: branch, tag or commit ID",
                Some("Target: branch, tag or commit ID"),
                "Compare",
            ),
        };
        let mut card = v_flex()
            .id("modal-card-scroll")
            .max_h(px(700.))
            .overflow_y_scroll()
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
                            .disabled(false)
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
                    .disabled(false)
                    .on_click(cx.listener(|this, _, _, cx| this.browse(cx))),
            );
        }
        if matches!(modal, Modal::RebaseSetup) {
            card=card.child(div().text_xs().text_color(rgb(MUTED)).child("The chosen upstream becomes the new base. To edit / squash the last N commits on this branch, use HEAD~N. Start with a clean working tree; merge commits in the range are rejected."));
        }
        if let Some(error) = &self.modal_error {
            card = card.child(
                div()
                    .text_sm()
                    .text_color(rgb(0xf0a4aa))
                    .child(error.clone()),
            );
        }
        let is_compare = matches!(modal, Modal::Compare);
        match modal {
            Modal::Confirm(ref message, _) => {
                card = card.child(
                    div()
                        .id("confirmation-message")
                        .max_h(px(340.))
                        .overflow_y_scroll()
                        .text_sm()
                        .child(message.clone()),
                );
            }
            Modal::BranchActions(name, remote) => {
                let compare = format!("refs/{}/{}", if remote { "remotes" } else { "heads" }, name);
                card = card.child(
                    self.button("branch-compare", "Compare HEAD → this branch…")
                        .on_click(cx.listener(move |this, _, w, cx| {
                            this.open_compare(Some("HEAD".into()), Some(compare.clone()), w, cx)
                        })),
                );
                let checkout = name.clone();
                let merge = format!("refs/{}/{}", if remote { "remotes" } else { "heads" }, name);
                let delete = name.clone();
                card = card.child(div().text_color(rgb(ACCENT)).child(name.clone()));
                if !remote {
                    let upstream_branch = name.clone();
                    card = card.child(
                        self.button("branch-upstream", "Set / clear upstream…")
                            .on_click(cx.listener(move |this, _, w, cx| {
                                this.open_remote_page(
                                    Modal::Upstream(upstream_branch.clone()),
                                    w,
                                    cx,
                                )
                            })),
                    );
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
                let branch_id = id.clone();
                let reset_id = id.clone();
                let edit_id = id.clone();
                let split_id = id.clone();
                card = card
                    .child(
                        self.button("branch-at", "Create branch from this commit…")
                            .on_click(cx.listener(move |this, _, w, cx| {
                                this.show_modal(Modal::BranchAt(branch_id.clone()), w, cx)
                            })),
                    )
                    .child(
                        self.button("reset-to", "Reset current branch to this commit…")
                            .on_click(cx.listener(move |this, _, w, cx| {
                                this.begin_reset(reset_id.clone(), w, cx)
                            })),
                    )
                    .child(
                        self.button("edit-old", "Edit this commit…")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.preview_history_rewrite(
                                    vec![edit_id.clone()],
                                    git::RebaseAction::Edit,
                                    cx,
                                )
                            })),
                    )
                    .child(
                        self.button("split-old", "Split this commit…")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.preview_history_rewrite(
                                    vec![split_id.clone()],
                                    git::RebaseAction::Split,
                                    cx,
                                )
                            })),
                    );
                let base = id.clone();
                let target = id.clone();
                let against = self
                    .comparison_base
                    .clone()
                    .unwrap_or_else(|| "HEAD".into());
                card = card
                    .child(
                        self.button("set-compare-base", "Set as comparison base")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.active.comparison_base = Some(base.clone());
                                this.active.notice = format!("Comparison base: {}", &base[..8]);
                                this.modal = None;
                                cx.notify();
                            })),
                    )
                    .child(
                        self.button("compare-commit", "Compare base / HEAD → this commit…")
                            .on_click(cx.listener(move |this, _, w, cx| {
                                this.open_compare(
                                    Some(against.clone()),
                                    Some(target.clone()),
                                    w,
                                    cx,
                                )
                            })),
                    );
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
                let push_tag = name.clone();
                card = card.child(
                    self.button("push-tag", "Push this tag…")
                        .on_click(cx.listener(move |this, _, w, cx| {
                            this.open_remote_page(Modal::PushSettings, w, cx);
                            this.push_choice.tags = vec![push_tag.clone()];
                            this.push_choice.branch = None;
                        })),
                );
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
        if is_compare {
            card = card.child(self.compare_choices(cx));
        }
        if !submit.is_empty() {
            card =
                card.child(
                    h_flex()
                        .justify_end()
                        .gap_2()
                        .mt_2()
                        .child(self.button("cancel", "Cancel").disabled(false).on_click(
                            cx.listener(|this, _, w, cx| {
                                this.modal = None;
                                this.focus.focus(w, cx);
                                cx.notify();
                            }),
                        ))
                        .child(
                            self.button("submit", submit)
                                .disabled(self.busy() && !preparation)
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
