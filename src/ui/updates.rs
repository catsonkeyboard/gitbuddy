use super::*;
use gitbuddy::updates::{DownloadCancellation, UpdateAsset, UpdateCheck};
pub(super) enum State {
    Loading,
    Checked(UpdateCheck),
    Error(String),
    Downloading(DownloadCancellation),
    Downloaded(PathBuf, String),
}
impl GitBuddy {
    pub(super) fn cancel_update_download(&self) {
        if let State::Downloading(cancellation) = &self.update_state {
            cancellation.cancel();
        }
    }
    pub(super) fn open_updates(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.cancel_update_download();
        self.modal = Some(Modal::Updates);
        self.update_state = State::Loading;
        self.update_generation += 1;
        self.focus.focus(window, cx);
        let generation = self.update_generation;
        let task = cx
            .background_executor()
            .spawn(async { gitbuddy::updates::check_updates() });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                if this.update_generation != generation
                    || !matches!(this.modal, Some(Modal::Updates))
                {
                    return;
                }
                this.update_state = match result {
                    Ok(info) => State::Checked(info),
                    Err(error) => State::Error(format!("{error:#}")),
                };
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
    fn download_release(&mut self, asset: UpdateAsset, cx: &mut Context<Self>) {
        self.cancel_update_download();
        self.update_generation += 1;
        let directory = self
            .repo
            .as_ref()
            .map(|r| r.root.clone())
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| std::env::temp_dir()));
        let receiver = cx.prompt_for_new_path(&directory, Some(&asset.name));
        let generation = self.update_generation;
        cx.spawn(async move |this, cx| {
            let path = match receiver.await {
                Ok(Ok(Some(path))) => path,
                _ => return,
            };
            let cancellation = DownloadCancellation::default();
            let active = this
                .update(cx, |this, cx| {
                    if this.update_generation != generation
                        || !matches!(this.modal, Some(Modal::Updates))
                    {
                        return false;
                    }
                    this.update_state = State::Downloading(cancellation.clone());
                    cx.notify();
                    true
                })
                .unwrap_or(false);
            if !active {
                return;
            }
            let result = cx
                .background_executor()
                .spawn(async move {
                    gitbuddy::updates::download_update_with_cancel(&asset, &path, &cancellation)
                        .map(|_| (path, asset.sha256))
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.update_generation == generation
                    && matches!(this.modal, Some(Modal::Updates))
                {
                    this.update_state = match result {
                        Ok((path, sha256)) => State::Downloaded(path, sha256),
                        Err(error) => State::Error(format!("{error:#}")),
                    };
                    cx.notify();
                }
            });
        })
        .detach();
    }
    pub(super) fn updates_modal(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut card = v_flex()
            .w(px(600.))
            .p_4()
            .gap_3()
            .rounded_lg()
            .bg(rgb(PANEL))
            .border_1()
            .border_color(rgb(BORDER))
            .child(
                h_flex()
                    .child(
                        div()
                            .flex_1()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(format!("GitBuddy {}", env!("CARGO_PKG_VERSION"))),
                    )
                    .child(
                        self.button(
                            "close-updates",
                            if matches!(self.update_state, State::Downloading(_)) {
                                "Cancel download / Close"
                            } else {
                                "Close"
                            },
                        )
                        .ghost()
                        .on_click(cx.listener(|this, _, w, cx| {
                            this.cancel_update_download();
                            this.update_generation += 1;
                            this.modal = None;
                            this.focus.focus(w, cx);
                            cx.notify();
                        })),
                    ),
            );
        card=match &self.update_state {
            State::Loading=>card.child("Checking published releases…"),
            State::Downloading(_)=>card.child("Downloading and verifying size / SHA-256…").child(div().text_xs().text_color(rgb(MUTED)).child("Closing this dialog cancels the download. A pending network read can take up to 30 seconds to return. Publication starts only after verification.")),
            State::Error(error)=>card.child(div().text_xs().text_color(rgb(0xe9a3a9)).child(error.clone())),
            State::Downloaded(path,sha256)=>card.child(format!("Verified download saved to {}",path.display())).child(div().font_family("Menlo").text_xs().child(format!("SHA-256: {sha256}"))).child(div().text_xs().text_color(rgb(MUTED)).child("Quit GitBuddy before installing. The package includes INSTALL.md and install_local.py for installation with a backup of the previous version.")),
            State::Checked(UpdateCheck::NoReleases)=>card.child("No published releases are available yet."),
            State::Checked(UpdateCheck::Current)=>card.child("This version matches or is newer than the latest stable release."),
            State::Checked(UpdateCheck::Available(update))=>{
                let url=update.release_url.clone();
                let mut result=card.child(format!("Version {} is available",update.version))
                    .child(self.button("view-release","View release notes").on_click(move|_,_,cx|cx.open_url(&url)));
                if let Some(asset)=&update.asset {
                    let download=asset.clone();result=result.child(div().font_family("Menlo").text_xs().child(format!("SHA-256: {}",asset.sha256))).child(self.button("download-update","Download verified package…").primary().on_click(cx.listener(move|this,_,_,cx|this.download_release(download.clone(),cx))));
                } else {result=result.child(div().text_xs().text_color(rgb(MUTED)).child("No package with a verified digest is available for this platform. See the release page for installation options."));}
                result
            }
        };
        card = card.child(
            self.button("all-releases", "Open published releases")
                .ghost()
                .on_click(|_, _, cx| {
                    cx.open_url("https://github.com/catsonkeyboard/gitbuddy/releases")
                }),
        );
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
