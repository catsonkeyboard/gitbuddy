use super::*;

#[derive(Clone, Default)]
pub(super) enum Data {
    #[default]
    Loading,
    Error(String),
    Rebase(Arc<git::RebasePreview>, Vec<git::RebaseStep>),
    Progress(git::RebaseStatus),
    Worktrees(Vec<git::WorktreeInfo>),
    Submodules(Vec<git::SubmoduleInfo>),
    Lfs(git::LfsStatus),
    Locks(String, git::LfsLocks),
    Maintenance(Arc<git::LfsMaintenance>),
}
pub(super) enum Inspection {
    Locks(String),
    Maintenance,
}
impl GitBuddy {
    pub(super) fn inspect_lfs(&mut self, request: Inspection, cx: &mut Context<Self>) {
        if self.busy() {
            return;
        }
        let Some(repo) = self.repo.clone() else {
            return;
        };
        let selection = self.selection.clone();
        let limit = self.limit;
        self.modal = None;
        self.dispatch(
            "Inspect LFS",
            true,
            move |control| {
                let result = match request {
                    Inspection::Locks(remote) => repo
                        .lfs_locks(&remote, control)
                        .map(|locks| Data::Locks(remote, locks)),
                    Inspection::Maintenance => repo
                        .lfs_maintenance(control)
                        .map(|report| Data::Maintenance(Arc::new(report))),
                };
                let (data, error, notice) = match result {
                    Ok(data) => (data, false, "LFS inspection complete".to_owned()),
                    Err(error) => {
                        let message = format!("{error:#}");
                        (
                            Data::Error(message.clone()),
                            !git::is_cancelled(&error),
                            message,
                        )
                    }
                };
                let mut loaded = Loaded::read(repo, selection, limit, notice, error, false, false)?;
                loaded.management_result = Some(data);
                Ok(loaded)
            },
            cx,
        );
    }
    pub(super) fn open_management(&mut self, modal: Modal, w: &mut Window, cx: &mut Context<Self>) {
        if self.busy() || self.repo.is_none() {
            return;
        }
        // An Open/Clone dialog may have been shown during a network query.
        // Keep its inputs; deliver that query's report when LFS is next opened.
        if matches!(modal, Modal::Lfs)
            && let Some(data) = self.active.management_result.take()
        {
            self.show_modal(Modal::LfsReport, w, cx);
            self.management_data = data;
            cx.notify();
            return;
        }
        self.management_generation += 1;
        self.management_data = Data::Loading;
        if matches!(modal, Modal::RebaseSetup)
            && self.snapshot.conflict_operation != git::ConflictOperation::Rebase
        {
            self.show_modal(modal, w, cx);
            let default = if self.snapshot.upstream.is_empty() {
                "HEAD~1".into()
            } else {
                self.snapshot.upstream.clone()
            };
            self.form_a.update(cx, |s, cx| s.set_value(default, w, cx));
            return;
        }
        let request = modal.clone();
        self.show_modal(
            if matches!(modal, Modal::RebaseSetup) {
                Modal::RebasePlan
            } else {
                modal
            },
            w,
            cx,
        );
        let repo = self.repo.clone().unwrap();
        let generation = self.management_generation;
        let tab = self.active.id;
        let task = cx.background_executor().spawn(async move {
            match request {
                Modal::RebaseSetup => repo
                    .rebase_status()?
                    .map(Data::Progress)
                    .ok_or_else(|| anyhow::anyhow!("Rebase ended; reopen the action")),
                Modal::Worktrees => repo.worktrees().map(Data::Worktrees),
                Modal::Submodules => repo.submodules_recursive().map(Data::Submodules),
                Modal::Lfs => repo.lfs_status().map(Data::Lfs),
                _ => unreachable!(),
            }
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                if this.active.id != tab
                    || this.management_generation != generation
                    || this.modal.is_none()
                {
                    return;
                }
                this.management_data = result.unwrap_or_else(|e| Data::Error(format!("{e:#}")));
                cx.notify();
            });
        })
        .detach();
    }
    pub(super) fn preview_rebase(
        &mut self,
        upstream: String,
        onto: String,
        cx: &mut Context<Self>,
    ) {
        let Some(repo) = self.repo.clone() else {
            return;
        };
        self.modal = Some(Modal::RebasePlan);
        self.management_data = Data::Loading;
        self.management_generation += 1;
        let generation = self.management_generation;
        let tab = self.active.id;
        let task = cx
            .background_executor()
            .spawn(async move { repo.rebase_preview_onto(&upstream, Some(&onto)) });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                if this.active.id != tab
                    || this.management_generation != generation
                    || !matches!(this.modal, Some(Modal::RebasePlan))
                {
                    return;
                }
                this.management_data = match result {
                    Ok(plan) => Data::Rebase(Arc::new(plan.clone()), plan.steps),
                    Err(e) => Data::Error(format!("{e:#}")),
                };
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
    pub(super) fn management_modal(&self, modal: Modal, cx: &mut Context<Self>) -> AnyElement {
        let title = match modal {
            Modal::RebasePlan => "Interactive rebase / Squash",
            Modal::RebaseMessage(_) => "Edit commit message",
            Modal::Worktrees => "Worktrees",
            Modal::Submodules => "Submodules",
            _ => "Git LFS",
        };
        let mut card = v_flex()
            .w(px(820.))
            .max_h(px(680.))
            .p_4()
            .gap_3()
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
                        self.button("management-close", "×")
                            .ghost()
                            .disabled(false)
                            .on_click(cx.listener(|this, _, w, cx| {
                                this.modal = None;
                                this.focus.focus(w, cx);
                                cx.notify();
                            })),
                    ),
            );
        if let Modal::RebaseMessage(index) = modal {
            card = card
                .child(Textarea::new(&self.amend_message).h(px(220.)))
                .child(
                    h_flex()
                        .justify_end()
                        .gap_2()
                        .child(self.button("message-back", "Cancel").on_click(cx.listener(
                            |this, _, _, cx| {
                                this.modal = Some(Modal::RebasePlan);
                                cx.notify();
                            },
                        )))
                        .child(
                            self.button("message-save", "Use message")
                                .primary()
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    let message = this.amend_message.read(cx).value().to_string();
                                    if !message.trim().is_empty()
                                        && let Data::Rebase(_, steps) = &mut this.management_data
                                        && let Some(step) = steps.get_mut(index)
                                    {
                                        step.message = message;
                                        if step.action == git::RebaseAction::Pick {
                                            step.action = git::RebaseAction::Reword;
                                        }
                                        this.modal = Some(Modal::RebasePlan);
                                        cx.notify();
                                    }
                                })),
                        ),
                );
        } else {
            card = card.child(match &self.management_data {
                Data::Loading => div()
                    .text_color(rgb(MUTED))
                    .child("Loading…")
                    .into_any_element(),
                Data::Error(error) => v_flex()
                    .gap_3()
                    .child(div().text_color(rgb(0xf0a4aa)).child(error.clone()))
                    .child(
                        self.button("management-retry", "Back / Reload")
                            .on_click(cx.listener(move |this, _, w, cx| {
                                let request = if matches!(modal, Modal::RebasePlan) {
                                    Modal::RebaseSetup
                                } else if matches!(modal, Modal::LfsReport) {
                                    Modal::Lfs
                                } else {
                                    modal.clone()
                                };
                                this.open_management(request, w, cx);
                            })),
                    )
                    .into_any_element(),
                Data::Rebase(preview, steps) => self.rebase_plan(preview, steps, cx),
                Data::Progress(status) => self.rebase_progress(status, cx),
                Data::Worktrees(trees) => self.worktree_list(trees, cx),
                Data::Submodules(modules) => self.submodule_list(modules, cx),
                Data::Lfs(status) => self.lfs_tools(status, cx),
                Data::Locks(remote, locks) => self.lfs_locks_view(remote, locks, cx),
                Data::Maintenance(report) => self.lfs_maintenance_view(report, cx),
            });
        }
        if let Some(error) = &self.modal_error {
            card = card.child(
                div()
                    .text_sm()
                    .text_color(rgb(0xf0a4aa))
                    .child(error.clone()),
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
    fn rebase_plan(
        &self,
        preview: &Arc<git::RebasePreview>,
        steps: &[git::RebaseStep],
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let plan = preview.clone();
        let submitted = steps.to_vec();
        v_flex().gap_3().min_h_0()
            .child(div().text_xs().text_color(rgb(MUTED)).child(format!("{} → {} · oldest first · {} commits",preview.head.reference.trim_start_matches("refs/heads/"),&preview.onto[..8],steps.len())))
            .child(div().text_xs().text_color(rgb(0xe6bc80)).child("Rewrites local history. Original commits remain in a backup reference and reflog. Merge commits are not supported in this plan."))
            .child(h_flex().gap_2()
                .child(self.button("autosquash","Autosquash").on_click(cx.listener(|this,_,_,cx| {
                    if let Data::Rebase(_,steps)=&mut this.management_data {
                        match git::autosquash(steps) { Ok(plan)=>{*steps=plan;this.modal_error=None;},Err(error)=>this.modal_error=Some(format!("{error:#}")) }
                    } cx.notify();
                })))
                .child(self.button("squash-all","Squash all into one").on_click(cx.listener(|this,_,_,cx|{
                    if let Data::Rebase(_,steps)=&mut this.management_data {for (i,step) in steps.iter_mut().enumerate(){step.action=if i==0{git::RebaseAction::Pick}else{git::RebaseAction::Squash};}}
                    cx.notify();
                })))
                .child(self.button("reset-plan","Reset plan").ghost().on_click(cx.listener(|this,_,_,cx|{if let Data::Rebase(preview,steps)=&mut this.management_data{*steps=preview.steps.clone();}cx.notify();}))))
            .child(v_flex().id("rebase-plan-scroll").max_h(px(380.)).overflow_y_scroll().gap_1()
                .children(steps.iter().enumerate().map(|(index,step)| {
                    let message=step.message.clone();
                    v_flex().id(("rebase-step",index)).p_2().gap_1().bg(rgb(BG)).rounded_sm()
                        .child(h_flex().gap_2()
                            .child(div().text_xs().text_color(rgb(ACCENT)).child(step.id[..8].to_string()))
                            .child(div().flex_1().truncate().text_sm().child(step.message.lines().next().unwrap_or_default().to_string()))
                            .child(self.button("up","↑").ghost().disabled(index==0).on_click(cx.listener(move|this,_,_,cx|{if let Data::Rebase(_,steps)=&mut this.management_data {steps.swap(index,index-1);}cx.notify();})))
                            .child(self.button("down","↓").ghost().disabled(index+1==steps.len()).on_click(cx.listener(move|this,_,_,cx|{if let Data::Rebase(_,steps)=&mut this.management_data{steps.swap(index,index+1);}cx.notify();})))
                            .child(self.button("edit","Message…").ghost().on_click(cx.listener(move|this,_,w,cx|{
                                this.modal=Some(Modal::RebaseMessage(index));this.amend_message.update(cx,|s,cx|s.set_value(message.clone(),w,cx));cx.notify();
                            }))))
                        .child(h_flex().gap_1().children([git::RebaseAction::Pick,git::RebaseAction::Reword,git::RebaseAction::Squash,git::RebaseAction::Fixup,git::RebaseAction::Drop,git::RebaseAction::Edit,git::RebaseAction::Split,git::RebaseAction::Break].into_iter().map(|action|{
                            self.button(action.label(),action.label()).ghost().when(step.action==action,|b|b.bg(rgb(0x375776)))
                                .on_click(cx.listener(move|this,_,_,cx|{if let Data::Rebase(_,steps)=&mut this.management_data{steps[index].action=action;}cx.notify();}))
                        })))
                })))
            .child(h_flex().justify_end().gap_2()
                .child(self.button("rebase-back","Change base…").on_click(cx.listener(|this,_,w,cx|this.open_management(Modal::RebaseSetup,w,cx))))
                .child(self.button("rebase-start","Start rebase…").primary().on_click(cx.listener(move|this,_,w,cx| {
                    this.show_modal(Modal::Confirm(format!("Replay {} commits onto {}? The current branch history will be rewritten. No push is performed.",submitted.len(),plan.onto),Operation::Rebase {context:plan.clone(),steps:submitted.clone()}),w,cx);
                })))).into_any_element()
    }
    fn rebase_progress(&self, status: &git::RebaseStatus, cx: &mut Context<Self>) -> AnyElement {
        v_flex().gap_3()
            .child(div().child(format!("{} · {} / {} completed",status.branch.trim_start_matches("refs/heads/"),status.completed,status.total)))
            .child(div().text_xs().text_color(rgb(MUTED)).child(format!("Original {} · onto {}",&status.original[..8],&status.onto[..8])))
            .when_some(status.current.clone(),|v,s|v.child(div().text_sm().child(format!("{} {} · {}",s.action.label(),&s.id[..8],s.message.lines().next().unwrap_or_default()))))
            .when(status.editing,|v|v.child(div().text_sm().text_color(rgb(ACCENT)).child(format!("Paused for editing / splitting · {} replacement commits. Edit files, stage hunks / lines and commit each part. Commit all remaining changes before Continue.",status.replacement_commits))))
            .when(status.paused,|v|v.child(div().text_sm().text_color(rgb(ACCENT)).child("Break: the preceding commit has been replayed. Continue from a clean working tree.")))
            .when(status.interrupted,|v|v.child(div().text_sm().text_color(rgb(0xe6bc80)).child("Interrupted apply: Continue cannot verify the result. Retry saves working files, index and journal before replaying this step. Existing index locks are never removed automatically.")))
            .when(status.recoverable,|v| {
                let retry=status.current.as_ref().unwrap().id.clone();let skip=retry.clone();
                v.child(h_flex().gap_2()
                    .child(self.button("rebase-retry","Retry current step…").on_click(cx.listener(move|this,_,w,cx|this.show_modal(Modal::Confirm("Save recovery copies, reset tracked files to the preceding replayed commit and retry this step? Current resolutions are retained in the recovery directory.".into(),Operation::RecoverRebaseStep {expected:retry.clone(),skip:false}),w,cx))))
                    .child(self.button("rebase-skip","Skip current step…").on_click(cx.listener(move|this,_,w,cx|this.show_modal(Modal::Confirm("Save recovery copies and discard this step from the rewritten history? Its working changes will be reset; original commits remain in the backup reference.".into(),Operation::RecoverRebaseStep {expected:skip.clone(),skip:true}),w,cx)))))
            })
            .child(h_flex().gap_2()
                .child(self.button("rebase-resolve",if status.editing {"Edit working files…"}else{"Resolve conflicts…"}).on_click(cx.listener(|this,_,_,cx|{this.modal=None;if this.snapshot.rebase_editing {this.select(Selection::Work,cx);}else{this.open_conflicts(None,cx);}})))
                .when(status.editing, |v| v.child(self.button("rebase-use-message","Use original message").ghost().on_click(cx.listener({let message=status.current.as_ref().map(|s|s.message.clone()).unwrap_or_default();move |this,_,w,cx|{this.message.update(cx,|s,cx|s.set_value(message.clone(),w,cx));this.modal=None;this.select(Selection::Work,cx);}}))))
                .child(self.button("rebase-continue","Continue rebase…").primary().disabled(status.interrupted).on_click(cx.listener(|this,_,w,cx|this.show_modal(Modal::Confirm(if this.snapshot.rebase_editing { "Continue after committing all parts? Remaining commits will be replayed." } else { "Continue rebase using all staged resolutions?" }.into(),Operation::ContinueRebase),w,cx))))
                .child(self.button("rebase-abort","Abort rebase…").on_click(cx.listener(|this,_,w,cx|this.show_modal(Modal::Confirm("Restore the original branch and tracked files? Resolutions and tracked edits made during this rebase will be discarded.".into(),Operation::AbortRebase),w,cx))))).into_any_element()
    }
    fn worktree_list(&self, trees: &[git::WorktreeInfo], cx: &mut Context<Self>) -> AnyElement {
        v_flex().gap_3()
            .child(div().text_xs().text_color(rgb(MUTED)).child("Linked worktrees share Git history and refs, with independent working files and index. The main checkout is not listed here."))
            .child(h_flex().gap_2()
                .child(self.button("create-worktree","Create worktree…").primary().on_click(cx.listener(|this,_,w,cx|this.show_modal(Modal::WorktreeCreate,w,cx))))
                .child(self.button("reload-worktrees","Refresh").ghost().on_click(cx.listener(|this,_,w,cx|this.open_management(Modal::Worktrees,w,cx)))))
            .child(v_flex().id("worktrees-scroll").max_h(px(420.)).overflow_y_scroll().gap_2()
                .children(trees.iter().enumerate().map(|(i,tree)|{
                    let path=tree.path.clone();let name=tree.name.clone();let locked=tree.locked;let remove=tree.clone();let prepare=tree.clone();
                    v_flex().id(("worktree",i)).p_2().gap_1().bg(rgb(BG)).rounded_sm()
                        .child(div().text_sm().child(format!("{} · {}{}{}",tree.name,tree.branch,if locked{" · locked"}else{""},if tree.dirty && tree.valid{" · changes"}else{""})))
                        .child(div().text_xs().text_color(rgb(MUTED)).child(tree.path.display().to_string()))
                        .child(h_flex().gap_2()
                            .child(self.button("open","Open in tab").ghost().disabled(!tree.valid).on_click(cx.listener(move|this,_,_,cx|this.open(path.clone(),cx))))
                            .child(self.button("prepare","Prepare submodules / LFS").ghost().disabled(!tree.valid||tree.dirty).on_click(cx.listener(move|this,_,_,cx|this.perform(Operation::PrepareWorktree(prepare.clone()),cx))))
                            .child(self.button("lock",if locked{"Unlock"}else{"Lock"}).ghost().on_click(cx.listener(move|this,_,_,cx|this.perform(Operation::LockWorktree {name:name.clone(),locked:!locked},cx))))
                            .child(self.button("remove",if tree.valid{"Remove…"}else{"Prune missing…"}).ghost().disabled(locked||(tree.valid&&tree.dirty)).on_click(cx.listener(move|this,_,w,cx|this.show_modal(Modal::Confirm(format!("Remove worktree {}? Only a clean, unlocked linked worktree can be removed. The branch is retained.",remove.path.display()),Operation::RemoveWorktree(remove.clone())),w,cx)))))
                })))
            .when(trees.is_empty(),|v|v.child(div().text_color(rgb(MUTED)).child("No linked worktrees."))).into_any_element()
    }
    fn submodule_list(&self, modules: &[git::SubmoduleInfo], cx: &mut Context<Self>) -> AnyElement {
        let root = self.repo.as_ref().unwrap().root.clone();
        v_flex().gap_3()
            .child(div().text_xs().text_color(rgb(MUTED)).child("Update checks out the commit recorded by the parent index. It preserves dirty submodules by refusing their update; cancellation may leave completed modules updated."))
            .child(h_flex().gap_2()
                .child(self.button("submodule-add","Add…").primary().on_click(cx.listener(|this,_,w,cx|this.show_modal(Modal::SubmoduleAdd,w,cx))))
                .child(self.button("submodule-all","Initialize / Update recursively").on_click(cx.listener(|this,_,_,cx|this.perform(Operation::UpdateSubmodules {names:vec![],recursive:true},cx))))
                .child(self.button("submodule-sync","Sync URLs").ghost().on_click(cx.listener(|this,_,_,cx|this.perform(Operation::SyncSubmodules,cx))))
                .child(self.button("submodule-refresh","Refresh").ghost().on_click(cx.listener(|this,_,w,cx|this.open_management(Modal::Submodules,w,cx)))))
            .child(v_flex().id("submodules-scroll").max_h(px(420.)).overflow_y_scroll().gap_2()
                .children(modules.iter().enumerate().map(|(i,module)|{
                    let path=root.join(&module.path);let update=module.clone();let stage=module.clone();
                    v_flex().id(("submodule",i)).p_2().gap_1().bg(rgb(BG)).rounded_sm()
                        .child(div().text_sm().child(format!("{}{}{}",module.path.display(),if module.initialized{""}else{" · not initialized"},if module.dirty{" · changes"}else{""})))
                        .child(div().truncate().text_xs().text_color(rgb(MUTED)).child(module.url.clone()))
                        .child(div().text_xs().text_color(rgb(MUTED)).child(format!("Index {} · worktree {}",module.index.as_deref().map(|s|&s[..8]).unwrap_or("none"),module.workdir.as_deref().map(|s|&s[..8]).unwrap_or("none"))))
                        .child(h_flex().gap_2()
                            .child(self.button("open","Open in tab").ghost().disabled(!module.initialized).on_click(cx.listener(move|this,_,_,cx|this.open(path.clone(),cx))))
                            .child(self.button("update","Initialize / Update").ghost().disabled(module.dirty).on_click(cx.listener(move|this,_,_,cx|this.perform(Operation::UpdateSubmoduleAt(update.clone()),cx))))
                            .child(self.button("stage","Stage current commit").ghost().disabled(!module.initialized).on_click(cx.listener(move|this,_,_,cx|this.perform(Operation::StageSubmoduleAt(stage.clone()),cx)))))
                })))
            .when(modules.is_empty(),|v|v.child(div().text_color(rgb(MUTED)).child("No submodules."))).into_any_element()
    }
    fn lfs_tools(&self, status: &git::LfsStatus, cx: &mut Context<Self>) -> AnyElement {
        let available = status.tool.is_some();
        v_flex().gap_3()
            .child(div().text_xs().text_color(rgb(MUTED)).child(status.tool.clone().unwrap_or_else(||"git-lfs is not installed. Native local tracking / staging works; install git-lfs for network transfers.".into())))
            .child(div().text_xs().text_color(rgb(MUTED)).child("Stage stores SHA-256 objects locally and commits LFS pointers. Fetch / Push below transfer LFS objects only; ordinary Push uploads LFS objects before updating Git refs. Existing history is not migrated."))
            .child(h_flex().gap_2()
                .child(self.button("lfs-track","Track pattern…").primary().on_click(cx.listener(|this,_,w,cx|this.show_modal(Modal::LfsPattern(true),w,cx))))
                .child(self.button("lfs-maintenance","Verify / Prune preview…").ghost().on_click(cx.listener(|this,_,_,cx|this.inspect_lfs(Inspection::Maintenance,cx))))
                .child(self.button("lfs-untrack","Untrack pattern…").on_click(cx.listener(|this,_,w,cx|this.show_modal(Modal::LfsPattern(false),w,cx))))
                .child(self.button("lfs-checkout","Checkout local objects").ghost().on_click(cx.listener(|this,_,_,cx|this.perform(Operation::LfsCheckout,cx))))
                .child(self.button("lfs-refresh","Refresh").ghost().on_click(cx.listener(|this,_,w,cx|this.open_management(Modal::Lfs,w,cx)))))
            .when(!self.snapshot.remotes.is_empty(), |v| v.child(div().text_xs().text_color(rgb(MUTED)).child("Choose a remote to transfer HEAD objects:")))
            .child(h_flex().flex_wrap().gap_2().children(self.snapshot.remotes.iter().enumerate().map(|(i,name)|{
                let name=name.clone();let push=name.clone();let locks=name.clone();
                h_flex().id(("lfs-remote",i)).gap_1()
                    .child(self.button("fetch",format!("Fetch {name}")).ghost().disabled(!available).on_click(cx.listener(move|this,_,_,cx|this.perform(Operation::LfsFetch(name.clone()),cx))))
                    .child(self.button("push",format!("Push {push}")).ghost().disabled(!available).on_click(cx.listener(move|this,_,_,cx|this.perform(Operation::LfsPush(push.clone()),cx))))
                    .child(self.button("locks",format!("Locks {locks}…")).ghost().disabled(!available).on_click(cx.listener(move|this,_,_,cx|this.inspect_lfs(Inspection::Locks(locks.clone()),cx))))
            })))
            .child(v_flex().id("lfs-scroll").max_h(px(350.)).overflow_y_scroll().gap_1()
                .children(status.patterns.iter().map(|s|div().text_xs().text_color(rgb(ACCENT)).child(s.clone())))
                .children(status.files.iter().map(|file|v_flex().p_2().bg(rgb(BG)).rounded_sm()
                    .child(div().text_sm().child(file.path.display().to_string()))
                    .child(div().text_xs().text_color(rgb(MUTED)).child(format!("{} bytes · {} · {} · {}",file.size,&file.oid[..12],if file.available{"cached"}else{"missing object"},if file.hydrated{"content"}else{"pointer"}))))))
            .when(status.files.is_empty(),|v|v.child(div().text_color(rgb(MUTED)).child("No LFS pointers in the index.")))
            .when(self.snapshot.remotes.is_empty(),|v|v.child(div().text_xs().text_color(rgb(MUTED)).child("Configure a remote such as origin before network transfers.")))
            .into_any_element()
    }
    fn lfs_locks_view(
        &self,
        remote: &str,
        locks: &git::LfsLocks,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let create = remote.to_owned();
        let refresh = create.clone();
        v_flex().gap_3()
            .child(div().text_sm().child(format!("Verified locks · {remote} · {} yours / {} others",locks.ours.len(),locks.theirs.len())))
            .child(div().text_xs().text_color(rgb(MUTED)).child("The server must support LFS locking. A configured lfs.url overrides remote selection. Cancellation after a server write can leave it completed; refresh to verify."))
            .child(h_flex().gap_2()
                .child(self.button("lock-create","Lock file…").primary().on_click(cx.listener(move|this,_,w,cx|this.show_modal(Modal::LfsLock(create.clone()),w,cx))))
                .child(self.button("lock-refresh","Refresh").ghost().on_click(cx.listener(move|this,_,_,cx|this.inspect_lfs(Inspection::Locks(refresh.clone()),cx))))
                .child(self.button("lock-back","Back to LFS").ghost().on_click(cx.listener(|this,_,w,cx|this.open_management(Modal::Lfs,w,cx)))))
            .child(v_flex().id("lfs-locks-scroll").max_h(px(400.)).overflow_y_scroll().gap_2()
                .children(locks.ours.iter().map(|lock|(lock,false)).chain(locks.theirs.iter().map(|lock|(lock,true))).enumerate().map(|(i,(lock,force))|{
                    let expected=lock.clone();let remote=remote.to_owned();
                    v_flex().id(("lfs-lock",i)).p_2().gap_1().bg(rgb(BG)).rounded_sm()
                        .child(div().text_sm().child(lock.path.clone()))
                        .child(div().text_xs().text_color(rgb(MUTED)).child(format!("{} · {} · {}",lock.owner.as_ref().map(|o|o.name.as_str()).unwrap_or("Unknown owner"),lock.locked_at,if force{"other owner"}else{"yours"})))
                        .child(self.button("unlock",if force{"Force unlock…"}else{"Unlock…"}).ghost().on_click(cx.listener(move|this,_,w,cx|this.show_modal(Modal::Confirm(format!("{} lock {} on {}? {}",if force{"Force remove another user's"}else{"Remove your"},expected.id,expected.path,if force{"Coordinate with its owner before editing this file."}else{""}),Operation::LfsUnlock {remote:remote.clone(),lock:expected.clone(),force}),w,cx))))
                })))
            .into_any_element()
    }
    fn lfs_maintenance_view(
        &self,
        report: &Arc<git::LfsMaintenance>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let expected = report.clone();
        let purge = report.clone();
        v_flex().gap_3()
            .child(div().id("lfs-maintenance-scroll").max_h(px(380.)).overflow_y_scroll().text_sm().child(report.report()))
            .child(h_flex().gap_2()
                .child(self.button("lfs-prune","Quarantine unreferenced objects…").primary().disabled(report.candidates.is_empty()).on_click(cx.listener(move|this,_,w,cx|this.show_modal(Modal::Confirm(format!("Move {} unreferenced LFS objects to a recovery quarantine? History and cache are checked again before any move. This is reversible and retains disk space.",expected.candidates.len()),Operation::LfsQuarantine(expected.clone())),w,cx))))
                .child(self.button("lfs-restore-cache","Restore quarantine").ghost().on_click(cx.listener(|this,_,_,cx|this.perform(Operation::LfsRestoreQuarantine,cx))))
                .child(self.button("lfs-purge-cache","Delete quarantine…").ghost().disabled(report.purge.is_empty()).on_click(cx.listener(move|this,_,w,cx|this.show_modal(Modal::Confirm(format!("Permanently delete {} unreferenced quarantine objects ({} bytes)? This cannot be undone. Referenced objects are checked again and retained.",purge.purge.len(),purge.purge_bytes),Operation::LfsPurgeQuarantine(purge.clone())),w,cx))))
                .child(self.button("lfs-maintenance-refresh","Refresh").ghost().on_click(cx.listener(|this,_,_,cx|this.inspect_lfs(Inspection::Maintenance,cx))))
                .child(self.button("lfs-maintenance-back","Back").ghost().on_click(cx.listener(|this,_,w,cx|this.open_management(Modal::Lfs,w,cx)))))
            .into_any_element()
    }
}
