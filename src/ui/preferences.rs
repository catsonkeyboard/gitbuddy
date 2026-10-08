use super::*;
use gitbuddy::settings::{Preferences, SHORTCUTS};

const KEY_SOURCE: KeyBindingMetaIndex = KeyBindingMetaIndex(0x4742);
fn action(name: &str) -> Box<dyn Action> {
    match name {
        "open" => Box::new(OpenRepository),
        "clone" => Box::new(CloneRepository),
        "init" => Box::new(InitRepository),
        "refresh" => Box::new(Refresh),
        "commit" => Box::new(CommitChanges),
        "amend" => Box::new(AmendCommit),
        "reflog" => Box::new(OpenReflog),
        "compare" => Box::new(CompareRevisions),
        "conflicts" => Box::new(OpenConflicts),
        "fetch" => Box::new(Fetch),
        "pull" => Box::new(Pull),
        "push" => Box::new(Push),
        "next_tab" => Box::new(NextTab),
        "previous_tab" => Box::new(PreviousTab),
        "close_tab" => Box::new(CloseTab),
        "cancel_task" => Box::new(CancelTask),
        "task_log" => Box::new(OpenTaskLog),
        "preferences" => Box::new(OpenPreferences),
        "quit" => Box::new(Quit),
        _ => Box::new(CloseModal),
    }
}
fn bindings(preferences: &Preferences) -> anyhow::Result<Vec<KeyBinding>> {
    preferences.validate()?;
    let mut result: Vec<KeyBinding> = Vec::new();
    for &(name, _, _) in SHORTCUTS {
        let text = preferences.binding(name).trim();
        if text.is_empty() {
            continue;
        }
        let strokes = text
            .split_whitespace()
            .map(Keystroke::parse)
            .collect::<Result<Vec<_>, _>>()?;
        anyhow::ensure!(
            !strokes.is_empty() && strokes.len() <= 2,
            "{name}: use one key or a two-key chord"
        );
        let first = &strokes[0];
        for stroke in &strokes {
            anyhow::ensure!(
                stroke.key.chars().count() == 1
                    || matches!(
                        stroke.key.as_str(),
                        "enter"
                            | "tab"
                            | "escape"
                            | "space"
                            | "backspace"
                            | "delete"
                            | "home"
                            | "end"
                            | "pageup"
                            | "pagedown"
                            | "up"
                            | "down"
                            | "left"
                            | "right"
                            | "f1"
                            | "f2"
                            | "f3"
                            | "f4"
                            | "f5"
                            | "f6"
                            | "f7"
                            | "f8"
                            | "f9"
                            | "f10"
                            | "f11"
                            | "f12"
                    ),
                "{name}: unknown key {}",
                stroke.key
            );
        }
        anyhow::ensure!(
            first.modifiers.platform
                || first.modifiers.control
                || first.modifiers.alt
                || (name == "close_modal" && first.key == "escape"),
            "{name}: use a Ctrl, Cmd/secondary or Alt modifier to preserve typing"
        );
        let context = Some(std::rc::Rc::new(KeyBindingContextPredicate::parse(
            "GitBuddy",
        )?));
        let binding = KeyBinding::load(
            text,
            action(name),
            context,
            false,
            None,
            &DummyKeyboardMapper,
        )?
        .with_meta(KEY_SOURCE);
        for previous in &result {
            let left = previous.keystrokes();
            let right = binding.keystrokes();
            let prefix = left
                .iter()
                .zip(right)
                .all(|(a, b)| a.as_keystroke() == b.as_keystroke());
            anyhow::ensure!(!prefix, "Shortcut conflict or chord prefix: {text}");
        }
        result.push(binding);
    }
    Ok(result)
}
pub(super) fn install_keys(preferences: &Preferences, cx: &mut App) {
    let keys = bindings(preferences).unwrap_or_else(|error| {
        eprintln!("Ignoring invalid shortcuts: {error}");
        bindings(&Preferences::default()).expect("Valid default shortcuts")
    });
    // Remove only this app's source. Component editor/navigation bindings survive
    // reloads; repeated saves do not accumulate stale shortcut entries.
    let other: Vec<_> = cx
        .key_bindings()
        .borrow()
        .bindings()
        .filter(|b| b.meta() != Some(KEY_SOURCE))
        .cloned()
        .collect();
    cx.clear_key_bindings();
    cx.bind_keys(other);
    cx.bind_keys(keys);
}
impl RepoTab {
    pub(super) fn apply_cache_limits(&mut self, preferences: &Preferences) {
        let before: HashSet<_> = self.patches.iter().map(|(key, _)| key.clone()).collect();
        self.commit_cache.set_limits(
            preferences.commit_cache_entries,
            preferences.commit_cache_mb * 1024 * 1024,
        );
        self.patches.set_limits(
            preferences.patch_cache_entries,
            preferences.patch_cache_mb * 1024 * 1024,
        );
        self.limit = self
            .limit
            .clamp(HISTORY_PAGE_SIZE, preferences.history_limit);
        if self.snapshot.commits.len() > self.limit {
            self.snapshot.commits.truncate(self.limit);
            self.history_projection.borrow_mut().take();
        }
        self.expanded
            .retain(|key| !before.contains(key) || self.patches.contains_key(key));
        self.prune_patch_views();
        self.scroll.invalidate_variables();
    }
    pub(super) fn prune_patch_views(&self) {
        let keep = self
            .patches
            .iter()
            .map(|(key, _)| key.scroll_id())
            .chain(self.expanded.iter().map(PatchKey::scroll_id))
            .collect();
        self.scroll.prune_patch_handles(&keep);
    }
}
impl GitBuddy {
    pub(super) fn cycle_tab(&mut self, direction: isize, cx: &mut Context<Self>) {
        if self.modal.is_some() || self.tabs.is_empty() {
            return;
        }
        let index = (self.active_index.unwrap_or(0) as isize + direction)
            .rem_euclid(self.tabs.len() as isize) as usize;
        self.activate_tab(index, cx);
    }
    fn fill_preferences(
        &mut self,
        preferences: &Preferences,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut values = vec![
            preferences.history_limit.to_string(),
            preferences.commit_cache_entries.to_string(),
            preferences.commit_cache_mb.to_string(),
            preferences.patch_cache_entries.to_string(),
            preferences.patch_cache_mb.to_string(),
        ];
        values.extend(
            SHORTCUTS
                .iter()
                .map(|(name, _, _)| preferences.binding(name).to_owned()),
        );
        for (field, value) in self.preference_fields.iter().zip(values) {
            field.update(cx, |s, cx| s.set_value(value, window, cx));
        }
    }
    pub(super) fn open_preferences(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.modal.is_some() {
            return;
        }
        self.fill_preferences(&self.settings.preferences.clone(), window, cx);
        self.modal = Some(Modal::Preferences);
        self.modal_error = None;
        self.preference_fields[0]
            .read(cx)
            .focus_handle(cx)
            .focus(window, cx);
        cx.notify();
    }
    fn save_preferences(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let result = (|| {
            let number = |index: usize| -> anyhow::Result<usize> {
                self.preference_fields[index]
                    .read(cx)
                    .value()
                    .trim()
                    .parse()
                    .map_err(|_| anyhow::anyhow!("Performance limits must be whole numbers"))
            };
            let next = Preferences {
                history_limit: number(0)?,
                commit_cache_entries: number(1)?,
                commit_cache_mb: number(2)?,
                patch_cache_entries: number(3)?,
                patch_cache_mb: number(4)?,
                shortcuts: SHORTCUTS
                    .iter()
                    .enumerate()
                    .map(|(i, (name, _, _))| {
                        (
                            name.to_string(),
                            self.preference_fields[i + 5]
                                .read(cx)
                                .value()
                                .trim()
                                .to_owned(),
                        )
                    })
                    .collect(),
            };
            bindings(&next)?;
            self.settings.set_preferences(next.clone())?;
            install_keys(&next, cx);
            self.active.apply_cache_limits(&next);
            for tab in self.tabs.iter_mut().flatten() {
                tab.apply_cache_limits(&next);
            }
            anyhow::Ok(())
        })();
        match result {
            Ok(()) => {
                self.modal = None;
                self.focus.focus(window, cx);
                self.active.notice = "Preferences saved and applied".into();
            }
            Err(error) => self.modal_error = Some(format!("{error:#}")),
        }
        cx.notify();
    }
    pub(super) fn preferences_modal(&self, cx: &mut Context<Self>) -> AnyElement {
        let performance = [
            ("Loaded history", "300–100,000 commits"),
            ("Commit cache entries", "1–256 entries"),
            ("Commit cache budget", "1–128 MB"),
            ("Diff cache entries", "1–64 entries"),
            ("Diff cache budget", "16–512 MB"),
        ];
        div().absolute().inset_0().occlude().bg(rgba(0x00000088)).flex().items_center().justify_center()
            .child(v_flex().w(px(720.)).max_h(px(620.)).p_4().gap_2().bg(rgb(PANEL)).border_1().border_color(rgb(BORDER)).rounded_lg()
                .child(h_flex().child(div().flex_1().font_weight(FontWeight::SEMIBOLD).child("Preferences · Performance & Shortcuts"))
                    .child(self.button("preferences-close", "×").disabled(false).ghost().on_click(cx.listener(|this, _, w, cx| { this.modal = None; this.focus.focus(w, cx); cx.notify(); }))))
                .child(div().text_xs().text_color(rgb(MUTED)).child("Limits apply per repository. Active documents and worker allocations are separate."))
                .child(v_flex().id("preferences-scroll").h(px(430.)).overflow_y_scroll().gap_2()
                    .child(section("PERFORMANCE"))
                    .children(performance.iter().enumerate().map(|(i, (label, range))| h_flex().gap_3()
                        .child(div().w(px(185.)).text_xs().child(*label))
                        .child(div().w(px(110.)).child(Input::new(&self.preference_fields[i]).small()))
                        .child(div().text_xs().text_color(rgb(MUTED)).child(*range))))
                    .child(section("SHORTCUTS"))
                    .child(div().text_xs().text_color(rgb(MUTED)).child("secondary = Cmd on macOS / Ctrl elsewhere. Example: secondary-r or ctrl-k ctrl-o. Leave empty to disable."))
                    .children(SHORTCUTS.iter().enumerate().map(|(i, (_, label, _))| h_flex().gap_3()
                        .child(div().w(px(185.)).text_xs().child(*label))
                        .child(div().flex_1().child(Input::new(&self.preference_fields[i + 5]).small())))))
                .when_some(self.modal_error.clone(), |col, error| col.child(div().text_xs().text_color(rgb(0xf0a4aa)).child(error)))
                .child(h_flex().gap_2()
                    .child(self.button("reset-preferences", "Load defaults").disabled(false).on_click(cx.listener(|this, _, w, cx| {
                        this.fill_preferences(&Preferences::default(), w, cx); this.modal_error = None; cx.notify();
                    })))
                    .child(div().flex_1())
                    .child(self.button("save-preferences", "Save & apply").disabled(false).primary().on_click(cx.listener(|this, _, w, cx| this.save_preferences(w, cx))))))
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[::core::prelude::v1::test]
    fn defaults_alias_conflicts_chords_and_disabled_bindings() {
        assert!(bindings(&Preferences::default()).is_ok());
        let mut p = Preferences::default();
        p.shortcuts.insert("open".into(), "ctrl-k ctrl-o".into());
        assert!(bindings(&p).is_ok());
        p.shortcuts.insert("clone".into(), "ctrl-k".into());
        assert!(bindings(&p).is_err());
        p.shortcuts.insert("clone".into(), "".into());
        p.shortcuts.insert("open".into(), "o".into());
        assert!(bindings(&p).is_err());
        p.shortcuts.insert("open".into(), "secondary-r".into());
        assert!(bindings(&p).is_err());
        p.shortcuts.insert("refresh".into(), "".into());
        assert!(bindings(&p).is_ok());
        p.shortcuts
            .insert("clone".into(), "ctrl-unknown-key-name".into());
        assert!(bindings(&p).is_err());
    }
    #[::core::prelude::v1::test]
    fn lowering_cache_limits_keeps_drafts_and_pending_expansions() {
        let mut tab = RepoTab {
            message: "draft".into(),
            limit: 900,
            ..Default::default()
        };
        let pending = PatchKey::Work("not-yet-loaded".into(), false);
        tab.expanded.insert(pending.clone());
        for i in 0..10 {
            let key = PatchKey::Work(format!("file-{i}").into(), false);
            tab.expanded.insert(key.clone());
            tab.patches.insert(key, PatchState::Error("diff".into()));
        }
        let preferences = Preferences {
            history_limit: 300,
            patch_cache_entries: 2,
            ..Default::default()
        };
        tab.apply_cache_limits(&preferences);
        assert_eq!(tab.patches.len(), 2);
        assert_eq!(tab.expanded.len(), 3);
        assert!(tab.expanded.contains(&pending));
        assert_eq!(tab.message, "draft");
        assert_eq!(tab.limit, 300);
    }
}
