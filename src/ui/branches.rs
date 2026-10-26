use super::*;

pub(super) enum Request {
    Rename(String),
    Detach(String),
    DeleteRemote(String),
}
enum Prepared {
    Rename(git::BranchRename),
    Confirm(String, Operation),
}
impl GitBuddy {
    pub(super) fn prepare_branch_action(&mut self, request: Request, cx: &mut Context<Self>) {
        let Some(repo) = self.repo.clone().filter(|_| !self.busy()) else {
            return;
        };
        self.branch_generation += 1;
        let generation = self.branch_generation;
        let tab = self.active.id;
        self.modal = Some(Modal::BranchPreparing("Reviewing branch operation…".into()));
        self.modal_error = None;
        self.branch_focus = true;
        let task = cx.background_executor().spawn(async move {
            match request {
                Request::Rename(name) => repo.prepare_branch_rename(&name).map(Prepared::Rename),
                Request::Detach(revision) => {
                    let expected = repo.prepare_detached_checkout(&revision)?;
                    Ok(Prepared::Confirm(format!("Check out {} at {}?\n\nHEAD will be detached: no local branch will move. Uncommitted changes that conflict with checkout will be refused. Create a branch before leaving if you make commits here.", expected.label, expected.id), Operation::CheckoutDetached(Arc::new(expected))))
                }
                Request::DeleteRemote(name) => {
                    let plan = repo.prepare_remote_branch_delete(&name)?;
                    Ok(Prepared::Confirm(format!("Delete this branch on the remote server?\n\n{}\n\nDeletion is rejected if the remote branch has moved since your last Fetch. Other people's clones are unaffected. Local upstream settings may refer to a now-missing branch.", plan.summary()), Operation::DeleteRemoteBranch(Arc::new(plan))))
                }
            }
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                if this.active.id != tab
                    || this.branch_generation != generation
                    || !matches!(this.modal, Some(Modal::BranchPreparing(_)))
                {
                    return;
                }
                match result {
                    Ok(Prepared::Rename(expected)) => {
                        this.restore_branch_name = Some(expected.name.clone());
                        this.modal = Some(Modal::RenameBranch(Arc::new(expected)));
                    }
                    Ok(Prepared::Confirm(message, operation)) => {
                        this.modal = Some(Modal::Confirm(message, operation))
                    }
                    Err(error) => this.modal_error = Some(format!("{error:#}")),
                }
                this.branch_focus = true;
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
}
