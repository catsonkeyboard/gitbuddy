//! Explicit slate palette; system appearance applies to both components and custom views.
use super::*;
use gitbuddy::settings::Appearance;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
static LIGHT: AtomicBool = AtomicBool::new(false);
static MODE: AtomicU8 = AtomicU8::new(0);

fn set(light: bool, window: &mut Window, cx: &mut App) {
    LIGHT.store(light, Ordering::Relaxed);
    Theme::change(
        if light {
            ThemeMode::Light
        } else {
            ThemeMode::Dark
        },
        Some(window),
        cx,
    );
    cx.refresh_windows();
}
pub(super) fn apply(appearance: Appearance, window: &mut Window, cx: &mut App) {
    MODE.store(
        match appearance {
            Appearance::Dark => 0,
            Appearance::Light => 1,
            Appearance::System => 2,
        },
        Ordering::Relaxed,
    );
    cx.set_window_appearance(match appearance {
        Appearance::Dark => Some(WindowAppearance::Dark),
        Appearance::Light => Some(WindowAppearance::Light),
        Appearance::System => None,
    });
    let light = match appearance {
        Appearance::Dark => false,
        Appearance::Light => true,
        Appearance::System => matches!(
            window.appearance(),
            WindowAppearance::Light | WindowAppearance::VibrantLight
        ),
    };
    set(light, window, cx);
}
pub(super) fn sync_system(window: &mut Window, cx: &mut App) {
    if MODE.load(Ordering::Relaxed) == 2 {
        set(
            matches!(
                window.appearance(),
                WindowAppearance::Light | WindowAppearance::VibrantLight
            ),
            window,
            cx,
        );
    }
}
pub(super) fn rgb(value: u32) -> Rgba {
    if !LIGHT.load(Ordering::Relaxed) {
        return gpui_kit::rgb(value);
    }
    let converted = match value {
        BG => 0xf0f2f5,
        PANEL => 0xffffff,
        EDITOR => 0xf7f9fc,
        BORDER => 0xc7cfd9,
        MUTED => 0x596779,
        TEXT => 0x202b3a,
        ACCENT => 0x245bb5,
        0x334152 | 0x344251 | 0x354151 => 0xd9e8fb,
        0x2b423d | 0x2c4237 => 0xe0f2e6,
        0x44353b => 0xffe6eb,
        0x303846 => 0xe4ebf7,
        0xb8e5c6 => 0x256a3e,
        0xedb5b7 | 0xe9a3a9 | 0xf0a4aa => 0xa62d3d,
        0xe6bc80 => 0x78551b,
        other => {
            let r = (other >> 16) & 255;
            let g = (other >> 8) & 255;
            let b = other & 255;
            if r.max(g).max(b) < 120 {
                ((226 + r / 5) << 16) | ((226 + g / 5) << 8) | (226 + b / 5)
            } else {
                ((r * 3 / 5) << 16) | ((g * 3 / 5) << 8) | (b * 3 / 5)
            }
        }
    };
    gpui_kit::rgb(converted)
}
