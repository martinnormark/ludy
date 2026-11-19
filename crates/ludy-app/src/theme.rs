//! Ludy's look: a quiet, light file explorer. Colours live in one `Theme`
//! so a dark variant is a second constant, not a rewrite.

use gpui::{div, prelude::*, px, rgb, rgba, svg, Div, FontWeight, Rgba, SharedString, Stateful, Svg};

pub struct Theme {
    pub bg: u32,
    pub sidebar: u32,
    pub surface: u32,
    pub line: u32,
    pub hover: u32,
    pub active: u32,
    pub text: u32,
    pub muted: u32,
    pub faint: u32,
    /// Fill of the one primary button on screen, and its label colour.
    pub ink: u32,
    pub on_ink: u32,
    pub ink_hover: u32,
    pub accent: u32,
    pub good: u32,
    pub bad: u32,
    pub blue: u32,
    pub folder_back: u32,
    pub folder_front: u32,
    pub scrim: u32,
}

pub const LIGHT: Theme = Theme {
    bg: 0xfbfbfa,
    sidebar: 0xf4f4f2,
    surface: 0xffffff,
    line: 0xe7e7e3,
    hover: 0xefefec,
    active: 0xe6e6e2,
    text: 0x1d1d1f,
    muted: 0x6e6e73,
    faint: 0xa3a3a8,
    ink: 0x1d1d1f,
    on_ink: 0xffffff,
    ink_hover: 0x3a3a3d,
    accent: 0xf2a41c,
    good: 0x2fad66,
    bad: 0xd6392c,
    blue: 0x2f7cf6,
    folder_back: 0xe9bd52,
    folder_front: 0xf6d57a,
    scrim: 0x00000038,
};

pub const T: Theme = LIGHT;

pub const SIDEBAR_W: f32 = 216.;
pub const PAD: f32 = 28.;

pub fn scrim() -> Rgba {
    rgba(T.scrim)
}

/// An icon from `assets.rs`, tinted with `color`.
pub fn icon(name: &'static str, size: f32, color: u32) -> Svg {
    svg().path(SharedString::from(format!("icons/{name}.svg"))).size(px(size)).flex_none().text_color(rgb(color))
}

/// Filled pill: the one obvious thing to press.
pub fn primary_button(id: impl Into<SharedString>, label: impl Into<SharedString>) -> Stateful<Div> {
    div()
        .id(id.into())
        .px(px(14.))
        .h(px(30.))
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .rounded_full()
        .bg(rgb(T.ink))
        .text_color(rgb(T.on_ink))
        .text_size(px(13.))
        .font_weight(FontWeight::MEDIUM)
        .cursor_pointer()
        .hover(|s| s.bg(rgb(T.ink_hover)))
        .active(|s| s.opacity(0.85))
        .child(label.into())
}

/// Outlined pill for secondary actions.
pub fn button(id: impl Into<SharedString>, label: impl Into<SharedString>) -> Stateful<Div> {
    div()
        .id(id.into())
        .px(px(13.))
        .h(px(30.))
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .gap(px(6.))
        .rounded_full()
        .bg(rgb(T.surface))
        .border_1()
        .border_color(rgb(T.line))
        .text_color(rgb(T.text))
        .text_size(px(13.))
        .cursor_pointer()
        .hover(|s| s.bg(rgb(T.hover)))
        .active(|s| s.bg(rgb(T.active)))
        .child(label.into())
}

/// Small square button holding an icon.
pub fn icon_button(id: impl Into<SharedString>, name: &'static str) -> Stateful<Div> {
    div()
        .id(id.into())
        .size(px(30.))
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .rounded(px(8.))
        .cursor_pointer()
        .hover(|s| s.bg(rgb(T.hover)))
        .active(|s| s.bg(rgb(T.active)))
        .child(icon(name, 17., T.muted))
}

/// The drive glyph: a slab with a slot and an activity light.
pub fn drive_glyph(lit: bool, scale: f32) -> Div {
    div()
        .w(px(76. * scale))
        .h(px(50. * scale))
        .flex_none()
        .rounded(px(11. * scale))
        .bg(rgb(0xf0f0ed))
        .border_1()
        .border_color(rgb(0xdfdfda))
        .flex()
        .flex_col()
        .justify_end()
        .child(
            div()
                .h(px(17. * scale))
                .px(px(10. * scale))
                .flex()
                .items_center()
                .justify_between()
                .border_t_1()
                .border_color(rgb(0xe3e3de))
                .child(div().w(px(22. * scale)).h(px(3. * scale)).rounded_full().bg(rgb(0xcdcdc7)))
                .child(div().size(px(6. * scale)).rounded_full().bg(rgb(if lit { T.good } else { 0xc9c9c3 }))),
        )
}

pub fn folder_glyph(scale: f32) -> Div {
    div()
        .w(px(64. * scale))
        .h(px(50. * scale))
        .flex_none()
        .relative()
        .child(div().absolute().top_0().left_0().w(px(27. * scale)).h(px(14. * scale)).rounded(px(5. * scale)).bg(rgb(T.folder_back)))
        .child(div().absolute().top(px(6. * scale)).left_0().w_full().h(px(44. * scale)).rounded(px(6. * scale)).bg(rgb(T.folder_back)))
        .child(div().absolute().top(px(13. * scale)).left_0().w_full().h(px(37. * scale)).rounded(px(6. * scale)).bg(rgb(T.folder_front)))
}

/// A sheet of paper with the file's extension on it.
pub fn file_glyph(scale: f32, ext: &str) -> Div {
    let sheet = div()
        .w(px(44. * scale))
        .h(px(56. * scale))
        .flex_none()
        .relative()
        .rounded(px(6. * scale))
        .bg(rgb(T.surface))
        .border_1()
        .border_color(rgb(0xd9d9d4));
    if scale < 0.6 || ext.is_empty() {
        return sheet;
    }
    sheet.child(
        div()
            .absolute()
            .bottom(px(7. * scale))
            .left_0()
            .w_full()
            .flex()
            .justify_center()
            .text_size(px(if ext.len() > 4 { 8. } else { 9.5 } * scale))
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(rgb(T.faint))
            .child(ext.to_string()),
    )
}

/// The extension shown on a file's glyph: up to six letters, upper case.
pub fn extension(name: &str) -> String {
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() && (1..=6).contains(&ext.len()) && ext.chars().all(|c| c.is_ascii_alphanumeric()) => ext.to_ascii_uppercase(),
        _ => String::new(),
    }
}

/// Decimal units, the way the Finder shows sizes.
pub fn fmt_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["bytes", "KB", "MB", "GB", "TB"];
    if n == 0 {
        return "Zero bytes".into();
    }
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1000.0 && u < UNITS.len() - 1 {
        v /= 1000.0;
        u += 1;
    }
    match u {
        0 => format!("{n} bytes"),
        _ if v >= 100.0 => format!("{v:.0} {}", UNITS[u]),
        _ if v >= 10.0 => format!("{v:.1} {}", UNITS[u]),
        _ => format!("{v:.2} {}", UNITS[u]),
    }
}

pub fn fmt_rate(bytes_per_sec: f64) -> String {
    if bytes_per_sec < 1.0 {
        return "idle".into();
    }
    format!("{}/s", fmt_bytes(bytes_per_sec as u64))
}

/// "today 14:03", "3 days ago", or a date.
pub fn fmt_time(unix_ms: u64, now_ms: u64) -> String {
    let age = now_ms.saturating_sub(unix_ms) / 1000;
    match age {
        0..=59 => "just now".into(),
        60..=3599 => format!("{} min ago", age / 60),
        3600..=86_399 => format!("{} h ago", age / 3600),
        86_400..=2_591_999 => format!("{} days ago", age / 86_400),
        _ => {
            // Civil date from days since the epoch (proleptic Gregorian).
            let z = (unix_ms / 86_400_000) as i64 + 719_468;
            let era = z.div_euclid(146_097);
            let doe = z.rem_euclid(146_097);
            let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
            let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
            let mp = (5 * doy + 2) / 153;
            let d = doy - (153 * mp + 2) / 5 + 1;
            let m = if mp < 10 { mp + 3 } else { mp - 9 };
            let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
            format!("{y}-{m:02}-{d:02}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_read_like_the_finder() {
        assert_eq!(fmt_bytes(0), "Zero bytes");
        assert_eq!(fmt_bytes(999), "999 bytes");
        assert_eq!(fmt_bytes(95_099_984), "95.1 MB");
        assert_eq!(fmt_bytes(53_687_091_200), "53.7 GB");
        assert_eq!(fmt_bytes(1_500), "1.50 KB");
        assert_eq!(fmt_bytes(648_000_000_000), "648 GB");
    }

    #[test]
    fn extensions() {
        assert_eq!(extension("demo.mp4"), "MP4");
        assert_eq!(extension("take-01.braw"), "BRAW");
        assert_eq!(extension("README"), "");
        assert_eq!(extension(".DS_Store"), "");
        assert_eq!(extension("edit.prproj"), "PRPROJ");
        assert_eq!(extension("archive.tar.backups"), "");
    }

    #[test]
    fn dates() {
        let now = 1_795_046_400_000; // 2026-11-19
        assert_eq!(fmt_time(now - 5_000, now), "just now");
        assert_eq!(fmt_time(now - 7_200_000, now), "2 h ago");
        assert_eq!(fmt_time(1_785_888_000_000, now), "2026-08-05");
    }
}
