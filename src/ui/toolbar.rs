use super::*;

impl GitBuddy {
    pub(super) fn toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let name = self
            .repo
            .as_ref()
            .map(|r| &r.root)
            .or(self.active.preparing.as_ref())
            .and_then(|path| path.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "GitBuddy".into());
        let focus = self.focus.clone();
        let has_repo = self.repo.is_some();
        let busy = self.busy();
        let has_head = self.snapshot.head_id.is_some();
        let has_conflicts = self.snapshot.files.iter().any(|f| f.conflict())
            || self.snapshot.conflict_operation != git::ConflictOperation::None;
        h_flex()
            .h(px(44.))
            .px_3()
            .gap_2()
            .flex_shrink_0()
            .bg(rgb(0x23272e))
            .border_b_1()
            .border_color(rgb(BORDER))
            .child(
                h_flex()
                    .w(px(157.))
                    .min_w_0()
                    .gap_2()
                    .child(
                        Icon::new(IconName::FolderOpen)
                            .size(px(16.))
                            .text_color(rgb(ACCENT)),
                    )
                    .child(
                        div()
                            .truncate()
                            .text_size(px(13.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(name),
                    ),
            )
            .child(
                self.button("repository-menu", "Repository")
                    .disabled(false)
                    .ghost()
                    .icon(IconName::ChevronDown)
                    .dropdown_menu(move |menu, _, _| {
                        menu.action_context(focus.clone())
                            .menu_with_icon(
                                "Open repository…",
                                IconName::FolderOpen,
                                Box::new(OpenRepository),
                            )
                            .menu_with_icon(
                                "Clone repository…",
                                IconName::Copy,
                                Box::new(CloneRepository),
                            )
                            .menu_with_icon(
                                "Initialize repository…",
                                IconName::Plus,
                                Box::new(InitRepository),
                            )
                            .separator()
                            .menu_with_icon(
                                "Preferences / Shortcuts…",
                                IconName::Settings,
                                Box::new(OpenPreferences),
                            )
                            .menu_with_icon("Task log…", IconName::FileText, Box::new(OpenTaskLog))
                            .separator()
                            .menu_with_icon_and_disabled(
                                "Resolve conflicts…",
                                IconName::FileText,
                                Box::new(OpenConflicts),
                                busy || !has_conflicts,
                            )
                            .menu_with_icon_and_disabled(
                                "File history / Blame…",
                                IconName::FileText,
                                Box::new(OpenFileTools),
                                busy || !has_head,
                            )
                            .menu_with_icon_and_disabled(
                                "Compare commits / branches…",
                                IconName::Copy,
                                Box::new(CompareRevisions),
                                busy || !has_head,
                            )
                            .separator()
                            .menu_with_icon_and_disabled(
                                "Amend latest commit…",
                                IconName::FileText,
                                Box::new(AmendCommit),
                                busy || !has_head,
                            )
                            .menu_with_icon_and_disabled(
                                "Undo latest commit…",
                                IconName::ArrowLeft,
                                Box::new(UndoCommit),
                                busy || !has_head,
                            )
                            .menu_with_icon_and_disabled(
                                "Reflog / Recover…",
                                IconName::RotateCw,
                                Box::new(OpenReflog),
                                busy || !has_repo,
                            )
                            .separator()
                            .menu_with_icon_and_disabled(
                                "Interactive rebase / Squash…",
                                IconName::RotateCw,
                                Box::new(OpenRebase),
                                busy || !has_head,
                            )
                            .menu_with_icon_and_disabled(
                                "Worktrees…",
                                IconName::FolderOpen,
                                Box::new(OpenWorktrees),
                                busy || !has_repo,
                            )
                            .menu_with_icon_and_disabled(
                                "Submodules…",
                                IconName::Network,
                                Box::new(OpenSubmodules),
                                busy || !has_repo,
                            )
                            .menu_with_icon_and_disabled(
                                "Git LFS…",
                                IconName::FileText,
                                Box::new(OpenLfs),
                                busy || !has_repo,
                            )
                            .separator()
                            .menu_with_icon_and_disabled(
                                "Manage remotes…",
                                IconName::Network,
                                Box::new(OpenRemotes),
                                busy || !has_repo,
                            )
                            .menu_with_icon_and_disabled(
                                "Push options / Tags…",
                                IconName::ArrowUp,
                                Box::new(OpenPushOptions),
                                busy || !has_repo,
                            )
                            .menu_with_icon_and_disabled(
                                "Commit identity…",
                                IconName::User,
                                Box::new(EditIdentity),
                                busy || !has_repo,
                            )
                    }),
            )
            .child(
                self.button("refresh", "")
                    .ghost()
                    .icon(IconName::RotateCw)
                    .tooltip(format!(
                        "Refresh · {}",
                        self.settings.preferences.binding("refresh")
                    ))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.refresh_with_notice("Refreshed".into(), cx)
                    })),
            )
            .when(has_repo, |row| {
                row.child(div().mx_1().w(px(1.)).h(px(18.)).bg(rgb(BORDER)))
                    .child(
                        h_flex()
                            .gap_2()
                            .px_2()
                            .h(px(26.))
                            .rounded_sm()
                            .bg(rgb(0x2d3745))
                            .child(
                                Icon::new(IconName::Network)
                                    .size(px(13.))
                                    .text_color(rgb(ACCENT)),
                            )
                            .child(
                                div()
                                    .text_size(px(12.))
                                    .text_color(rgb(ACCENT))
                                    .child(self.snapshot.branch.clone()),
                            ),
                    )
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(rgb(MUTED))
                            .child(format!(
                                "↑{}  ↓{}",
                                self.snapshot.ahead, self.snapshot.behind
                            )),
                    )
            })
            .child(div().flex_1())
            .when(has_repo, |row| {
                row.child(
                    h_flex()
                        .gap_1()
                        .p(px(2.))
                        .rounded_md()
                        .border_1()
                        .border_color(rgb(BORDER))
                        .bg(rgb(BG))
                        .child(
                            self.button("fetch", "Fetch")
                                .ghost()
                                .icon(IconName::RotateCw)
                                .on_click(
                                    cx.listener(|this, _, _, cx| {
                                        this.perform(Operation::Fetch, cx)
                                    }),
                                ),
                        )
                        .child(div().w(px(1.)).h(px(16.)).bg(rgb(BORDER)))
                        .child(
                            self.button("pull", "Pull")
                                .ghost()
                                .icon(IconName::ArrowDown)
                                .on_click(
                                    cx.listener(|this, _, _, cx| this.perform(Operation::Pull, cx)),
                                ),
                        )
                        .child(
                            self.button("push", "Push")
                                .ghost()
                                .icon(IconName::ArrowUp)
                                .text_color(rgb(ACCENT))
                                .on_click(
                                    cx.listener(|this, _, _, cx| this.perform(Operation::Push, cx)),
                                ),
                        )
                        .child(
                            self.button("push-options", "▾")
                                .ghost()
                                .tooltip("Select push target / Force-with-lease / Tags")
                                .on_click(cx.listener(|this, _, w, cx| {
                                    this.open_remote_page(Modal::PushSettings, w, cx)
                                })),
                        ),
                )
                .child(
                    self.button("identity", "")
                        .ghost()
                        .icon(IconName::Settings)
                        .tooltip("Repository identity")
                        .on_click(
                            cx.listener(|this, _, w, cx| this.show_modal(Modal::Identity, w, cx)),
                        ),
                )
            })
    }

    pub(super) fn repository_tabs(&self, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .h(px(32.))
            .flex_shrink_0()
            .bg(rgb(0x1d2025))
            .border_b_1()
            .border_color(rgb(BORDER))
            .child(
                h_flex()
                    .id("repository-tabs")
                    .track_scroll(&self.tab_scroll.area("repository-tabs"))
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .overflow_x_scroll()
                    .children(self.tabs.iter().enumerate().filter_map(|(index, slot)| {
                        // The active tab's data lives in `active`; render a
                        // lightweight header from `active` itself.
                        let tab = slot.as_ref().map(Some).unwrap_or_else(|| {
                            (self.active_index == Some(index)).then_some(&self.active)
                        })?;
                        let active = self.active_index == Some(index);
                        let name = tab
                            .repo
                            .as_ref()
                            .map(|r| &r.root)
                            .or(tab.preparing.as_ref())?
                            .file_name()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .into_owned();
                        let tooltip = tab
                            .repo
                            .as_ref()
                            .map(|r| &r.root)
                            .or(tab.preparing.as_ref())
                            .map(|path| format!("{}\n{}", path.display(), tab.notice))
                            .unwrap_or_default();
                        Some(
                            h_flex()
                                .h_full()
                                .min_w(px(120.))
                                .max_w(px(220.))
                                .px_1()
                                .gap_1()
                                .flex_shrink_0()
                                .bg(rgb(if active { PANEL } else { 0x202328 }))
                                .border_r_1()
                                .border_color(rgb(BORDER))
                                .when(active, |tab| tab.border_t_2().border_color(rgb(ACCENT)))
                                .child(
                                    self.button(("repo-tab", index), name)
                                        .disabled(false)
                                        .ghost()
                                        .flex_1()
                                        .min_w_0()
                                        .tooltip(tooltip)
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.activate_tab(index, cx)
                                        })),
                                )
                                .when(
                                    self.tasks.get(tab.id).is_some() || tab.task_finished,
                                    |row| {
                                        row.child(
                                            div()
                                                .text_xs()
                                                .text_color(rgb(if tab.error {
                                                    0xf0a4aa
                                                } else {
                                                    ACCENT
                                                }))
                                                .child(if self.tasks.get(tab.id).is_some() {
                                                    "◌"
                                                } else if tab.error {
                                                    "!"
                                                } else {
                                                    "✓"
                                                }),
                                        )
                                    },
                                )
                                .child(
                                    self.button(("close-repo-tab", index), "×")
                                        .disabled(self.tasks.get(tab.id).is_some())
                                        .ghost()
                                        .tooltip("Close repository tab")
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.close_tab(index, cx)
                                        })),
                                ),
                        )
                    })),
            )
            .child(
                self.button("add-repo-tab", "+")
                    .disabled(false)
                    .ghost()
                    .tooltip(format!(
                        "Open another repository · {}",
                        self.settings.preferences.binding("open")
                    ))
                    .on_click(
                        cx.listener(|this, _, window, cx| this.show_modal(Modal::Open, window, cx)),
                    ),
            )
    }

    pub(super) fn welcome(&self, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .flex_1()
            .items_center()
            .justify_center()
            .gap_5()
            .bg(rgb(BG))
            .child(div().text_size(px(48.)).text_color(rgb(ACCENT)).child("⑂"))
            .child(
                div()
                    .text_3xl()
                    .font_weight(FontWeight::BOLD)
                    .child("A clearer view of your code."),
            )
            .child(
                div()
                    .text_color(rgb(MUTED))
                    .child("GitBuddy  /  Your native Git workspace"),
            )
            .child(
                h_flex()
                    .gap_3()
                    .mt_4()
                    .child(
                        self.button("welcome-open", "Open repository")
                            .primary()
                            .on_click(
                                cx.listener(|this, _, w, cx| this.show_modal(Modal::Open, w, cx)),
                            ),
                    )
                    .child(self.button("welcome-clone", "Clone repository").on_click(
                        cx.listener(|this, _, w, cx| this.show_modal(Modal::Clone, w, cx)),
                    ))
                    .child(self.button("welcome-init", "Initialize new").on_click(
                        cx.listener(|this, _, w, cx| this.show_modal(Modal::Init, w, cx)),
                    )),
            )
            .when(!self.settings.recent.is_empty(), |col| {
                col.child(
                    v_flex()
                        .w(px(480.))
                        .mt_6()
                        .gap_2()
                        .child(section("RECENT REPOSITORIES"))
                        .children(self.settings.recent.iter().enumerate().map(|(i, path)| {
                            let path = path.clone();
                            self.button(("recent", i), path.display().to_string())
                                .ghost()
                                .on_click(
                                    cx.listener(move |this, _, _, cx| this.open(path.clone(), cx)),
                                )
                        })),
                )
            })
    }
}
