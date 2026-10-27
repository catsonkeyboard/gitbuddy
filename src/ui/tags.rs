use super::*;

#[derive(Default)]
pub(super) enum Data {
    #[default]
    Loading,
    Ready(Arc<git::TagDefaults>, Option<Arc<git::TagTarget>>),
    Error(String),
}
impl GitBuddy {
    pub(super) fn open_tag_creator(
        &mut self,
        revision: String,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(repo) = self.repo.clone().filter(|_| !self.busy()) else {
            return;
        };
        self.tag_generation += 1;
        let generation = self.tag_generation;
        let tab = self.active.id;
        self.tag_data = Data::Loading;
        self.tag_kind = git::TagKind::Lightweight;
        self.tag_preparing = false;
        self.show_modal(Modal::Tag, w, cx);
        self.form_b
            .update(cx, |s, cx| s.set_value(revision.clone(), w, cx));
        self.tag_message.update(cx, |s, cx| s.set_value("", w, cx));
        self.form_a.read(cx).focus_handle(cx).focus(w, cx);
        let work = cx.background_executor().spawn(async move {
            Ok::<_, anyhow::Error>((
                repo.tag_defaults()?,
                repo.prepare_tag_target(&revision).ok(),
            ))
        });
        cx.spawn(async move |this, cx| {
            let result = work.await;
            let _ = this.update(cx, |this, cx| {
                if this.active.id != tab
                    || this.tag_generation != generation
                    || !matches!(this.modal, Some(Modal::Tag))
                {
                    return;
                }
                this.tag_data = match result {
                    Ok((defaults, target)) => {
                        this.tag_restore = true;
                        Data::Ready(Arc::new(defaults), target.map(Arc::new))
                    }
                    Err(e) => Data::Error(format!("{e:#}")),
                };
                cx.notify();
            });
        })
        .detach();
    }
    pub(super) fn restore_tag_form(&mut self, w: &mut Window, cx: &mut Context<Self>) {
        if !self.tag_restore {
            return;
        }
        self.tag_restore = false;
        if !matches!(self.modal, Some(Modal::Tag)) {
            return;
        }
        if let Data::Ready(defaults, _) = &self.tag_data {
            self.tag_kind = if defaults.signed {
                git::TagKind::Signed
            } else {
                git::TagKind::Lightweight
            };
            self.tag_format = defaults.format;
            self.tag_format_selected = defaults.notice.is_none();
            self.tag_key
                .update(cx, |s, cx| s.set_value(defaults.key.clone(), w, cx));
            self.tag_program.update(cx, |s, cx| {
                s.set_value(defaults.program(defaults.format), w, cx)
            });
        }
    }
    pub(super) fn review_tag(&mut self, cx: &mut Context<Self>) {
        if self.busy() || self.tag_preparing || !matches!(self.tag_data, Data::Ready(..)) {
            return;
        }
        let Some(repo) = self.repo.clone() else {
            return;
        };
        let kind = self.tag_kind;
        if kind == git::TagKind::Signed && !self.tag_format_selected {
            self.modal_error = Some(
                "Configured signature format is unsupported. Explicitly choose OpenPGP or SSH."
                    .into(),
            );
            cx.notify();
            return;
        }
        let spec = git::TagSpec {
            name: self.form_a.read(cx).value().trim().into(),
            kind,
            message: if kind == git::TagKind::Lightweight {
                String::new()
            } else {
                self.tag_message.read(cx).value().to_string()
            },
            signer: (kind == git::TagKind::Signed).then(|| git::TagSigner {
                format: self.tag_format,
                program: self.tag_program.read(cx).value().trim().into(),
                key: self.tag_key.read(cx).value().trim().into(),
            }),
        };
        let revision = self.form_b.read(cx).value().trim().to_owned();
        let generation = self.tag_generation;
        let tab = self.active.id;
        self.tag_preparing = true;
        self.modal_error = None;
        let work = cx.background_executor().spawn(async move {
            let target = repo.prepare_tag_target(&revision)?;
            repo.prepare_tag(&target, spec)
        });
        cx.spawn(async move |this, cx| {
            let result = work.await;
            let _ = this.update(cx, |this, cx| {
                if this.active.id != tab
                    || this.tag_generation != generation
                    || !matches!(this.modal, Some(Modal::Tag))
                {
                    return;
                }
                this.tag_preparing = false;
                match result {
                    Ok(plan) => {
                        this.modal = Some(Modal::Confirm(
                            plan.summary(),
                            Operation::CreateTag(Arc::new(plan)),
                        ));
                        this.branch_focus = true;
                    }
                    Err(e) => this.modal_error = Some(format!("{e:#}")),
                };
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
    pub(super) fn tag_creator_modal(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut card = v_flex()
            .id("tag-creator")
            .w(px(680.))
            .max_h(px(600.))
            .overflow_y_scroll()
            .p_4()
            .gap_2()
            .bg(rgb(PANEL))
            .rounded_lg()
            .border_1()
            .border_color(rgb(BORDER))
            .child(
                h_flex()
                    .child(
                        div()
                            .flex_1()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("Create tag"),
                    )
                    .child(
                        self.button("tag-close", "×")
                            .ghost()
                            .disabled(false)
                            .on_click(cx.listener(|this, _, w, cx| {
                                this.modal = None;
                                this.focus.focus(w, cx);
                                cx.notify();
                            })),
                    ),
            );
        match &self.tag_data {
            Data::Loading => card = card.child("Resolving target and signing configuration…"),
            Data::Error(e) => {
                card = card.child(div().text_sm().text_color(rgb(0xf0a4aa)).child(e.clone()))
            }
            Data::Ready(defaults, target) => {
                card = card
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(MUTED))
                            .child("Name (existing tags are never replaced)"),
                    )
                    .child(Input::new(&self.form_a).disabled(self.tag_preparing))
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(MUTED))
                            .child("Target commit / branch / tag / revision"),
                    )
                    .child(Input::new(&self.form_b).disabled(self.tag_preparing))
                    .child(div().text_xs().text_color(rgb(MUTED)).child(format!(
                            "Initially resolved: {}. Review resolves and pins the entered target.",
                            target
                                .as_ref()
                                .map(|t| t.id.as_str())
                                .unwrap_or("No HEAD yet; enter an existing commit / revision")
                        )))
                    .child(
                        h_flex().gap_2().children(
                            [
                                git::TagKind::Lightweight,
                                git::TagKind::Annotated,
                                git::TagKind::Signed,
                            ]
                            .into_iter()
                            .enumerate()
                            .map(|(i, kind)| {
                                self.button(("tag-kind", i), kind.label())
                                    .disabled(self.tag_preparing)
                                    .when(self.tag_kind == kind, |b| {
                                        b.bg(rgb(0x354151)).text_color(rgb(ACCENT))
                                    })
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.tag_kind = kind;
                                        cx.notify();
                                    }))
                            }),
                        ),
                    );
                if self.tag_kind != git::TagKind::Lightweight {
                    card = card
                        .child(
                            div()
                                .text_xs()
                                .text_color(rgb(MUTED))
                                .child("Annotation (multiline)"),
                        )
                        .child(Textarea::new(&self.tag_message).h(px(115.)));
                }
                if self.tag_kind == git::TagKind::Signed {
                    card=card.child(h_flex().gap_2().children([git::TagSigningFormat::OpenPgp,git::TagSigningFormat::Ssh].into_iter().enumerate().map(|(i,format)|self.button(("tag-sign-format",i),format.label()).disabled(self.tag_preparing).when(self.tag_format==format,|b|b.bg(rgb(0x354151)).text_color(rgb(ACCENT))).on_click(cx.listener(move|this,_,w,cx|{
                        this.tag_format=format;this.tag_format_selected=true;if let Data::Ready(defaults,_)=&this.tag_data {this.tag_program.update(cx,|s,cx|s.set_value(defaults.program(format),w,cx));}cx.notify();
                    })))))
                        .child(div().text_xs().text_color(rgb(MUTED)).child("Signing key: OpenPGP ID / identity, or SSH key path / key::public-key"))
                        .child(Input::new(&self.tag_key).disabled(self.tag_preparing))
                        .child(div().text_xs().text_color(rgb(MUTED)).child("Signing executable (path only; uses agent / pinentry for unlocking)"))
                        .child(Input::new(&self.tag_program).disabled(self.tag_preparing))
                        .child(div().text_xs().text_color(rgb(MUTED)).child("The signer runs directly. Failed or cancelled signing creates no tag reference. Private keys and passphrases stay with the signing tool / agent."));
                    if let Some(notice) = &defaults.notice {
                        card = card.child(
                            div()
                                .text_xs()
                                .text_color(rgb(0xe3ba77))
                                .child(notice.clone()),
                        );
                    }
                }
                card = card.child(
                    h_flex().justify_end().child(
                        self.button(
                            "tag-review",
                            if self.tag_preparing {
                                "Reviewing…"
                            } else {
                                "Review tag…"
                            },
                        )
                        .primary()
                        .disabled(self.tag_preparing)
                        .on_click(cx.listener(|this, _, _, cx| this.review_tag(cx))),
                    ),
                );
            }
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
            .occlude()
            .bg(rgba(0x00000099))
            .flex()
            .items_center()
            .justify_center()
            .child(card)
            .into_any_element()
    }
}
