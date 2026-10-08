use super::*;
impl GitBuddy {
    pub(super) fn task_log_modal(&self, cx: &mut Context<Self>) -> AnyElement {
        let root = self
            .repo
            .as_ref()
            .map(|r| r.root.as_path())
            .or(self.preparing.as_deref());
        let records = Arc::new(self.task_log.records(if self.log_all_repositories {
            None
        } else {
            root
        }));
        let copy = records.clone();
        div().absolute().inset_0().occlude().bg(rgba(0x00000088)).flex().items_center().justify_center()
            .child(v_flex().w(px(880.)).h(px(560.)).p_4().gap_2().bg(rgb(PANEL)).border_1().border_color(rgb(BORDER)).rounded_lg()
                .child(h_flex().gap_2().child(div().flex_1().font_weight(FontWeight::SEMIBOLD).child("Task log"))
                    .child(self.button("log-scope", if self.log_all_repositories { "All repositories" } else { "Current repository" }).disabled(false).ghost().on_click(cx.listener(|this, _, _, cx| { this.log_all_repositories = !this.log_all_repositories; cx.notify(); })))
                    .child(self.button("log-copy", "Copy JSON").disabled(false).ghost().on_click(cx.listener(move |_, _, _, cx| { cx.write_to_clipboard(ClipboardItem::new_string(serde_json::to_string_pretty(copy.as_ref()).unwrap())); })))
                    .child(self.button("log-clear", "Clear finished").disabled(false).ghost().on_click(cx.listener(|this, _, _, cx| {
                        let root = this.repo.as_ref().map(|r| r.root.as_path()).or(this.preparing.as_deref());
                        if let Err(error) = this.task_log.clear_finished(if this.log_all_repositories { None } else { root }) { this.modal_error = Some(format!("{error:#}")); } cx.notify();
                    })))
                    .child(self.button("log-close", "×").disabled(false).ghost().on_click(cx.listener(|this, _, w, cx| { this.modal = None; this.focus.focus(w, cx); cx.notify(); }))))
                .child(div().text_xs().text_color(rgb(MUTED)).child("Latest 100 tasks · 128 events per task · 1 MB total. Finished history persists across restarts. Progress is sampled; dropped events are counted."))
                .when_some(self.modal_error.clone(), |col, error| col.child(div().text_xs().text_color(rgb(0xf0a4aa)).child(error)))
                .child(uniform_list("task-log-list", records.len().max(1), cx.processor(move |_, range: std::ops::Range<usize>, _, _| {
                    range.map(|i| {
                        if let Some(record) = records.get(i) {
                            let text = record.events.iter().map(|e| format!("{}  {}", e.at, e.text)).collect::<Vec<_>>().join("\n");
                            v_flex().w_full().h(px(190.)).gap_1().p_2().border_b_1().border_color(rgb(BORDER))
                                .child(div().text_xs().child(format!("{} · {:?} · {}", record.label, record.outcome, record.started)))
                                .child(div().text_xs().text_color(rgb(MUTED)).truncate().child(record.repository.display().to_string()))
                                .child(div().id(("task-events", record.id as usize)).flex_1().min_h_0().overflow_y_scroll().text_xs().font_family("Menlo").child(text))
                                .child(div().text_xs().text_color(rgb(MUTED)).child(format!("{} earlier events omitted", record.dropped)))
                                .into_any_element()
                        } else { div().h(px(190.)).p_3().child("No tasks recorded yet.").into_any_element() }
                    }).collect::<Vec<_>>()
                })).track_scroll(&self.active.scroll.list("task-log-list")).flex_1().min_h_0()))
            .into_any_element()
    }
}
