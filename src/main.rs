mod ui;
use gpui_kit::{
    component::{Root, Theme, ThemeMode},
    *,
};
#[expect(
    dead_code,
    reason = "Keep the window close observer active for the app lifetime"
)]
struct CloseWindowSubscription(Subscription);
impl Global for CloseWindowSubscription {}
fn main() {
    let path = std::env::args_os().nth(1).map(std::path::PathBuf::from);
    gpui_kit::application()
        .with_assets(gpui_kit::assets::Assets)
        .run(move |cx| {
            gpui_kit::init(cx);
            Theme::change(ThemeMode::Dark, None, cx);
            cx.set_window_appearance(Some(WindowAppearance::Dark));
            cx.bind_keys([
                KeyBinding::new("secondary-o", ui::OpenRepository, None),
                KeyBinding::new("secondary-r", ui::Refresh, None),
                KeyBinding::new("secondary-enter", ui::CommitChanges, None),
                KeyBinding::new("secondary-q", ui::Quit, None),
                KeyBinding::new("escape", ui::CloseModal, None),
            ]);
            let close_subscription = cx.on_window_closed(|cx, _| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            });
            cx.set_global(CloseWindowSubscription(close_subscription));
            let bounds = WindowBounds::centered(size(px(1440.), px(900.)), cx);
            cx.spawn(async move |cx| {
                cx.open_window(
                    WindowOptions {
                        window_bounds: Some(bounds),
                        window_min_size: Some(size(px(1000.), px(650.))),
                        titlebar: Some(TitlebarOptions {
                            title: Some("GitBuddy".into()),
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                    |window, cx| {
                        let view = cx.new(|cx| ui::GitBuddy::new(path, window, cx));
                        let quit_view = view.downgrade();
                        cx.on_action(move |_: &ui::Quit, cx| {
                            let _ = quit_view.update(cx, |view, cx| {
                                view.request_quit(cx);
                            });
                        });
                        let close_view = view.downgrade();
                        window.on_window_should_close(cx, move |_, cx| {
                            close_view
                                .update(cx, |view, cx| view.request_quit(cx))
                                .unwrap_or(true)
                        });
                        cx.new(|cx| Root::new(view, window, cx))
                    },
                )
                .expect("Cannot open GitBuddy window");
                cx.update(|cx| cx.activate(true));
            })
            .detach();
        });
}
