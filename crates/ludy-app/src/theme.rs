//! Ludy's look: dark graphite with an amber accent.

use gpui::{div, prelude::*, px, rgb, rgba, Div, FontWeight, Rgba, SharedString, Stateful};

pub const BG: u32 = 0x101114;
pub const SIDEBAR: u32 = 0x0b0c0e;
pub const PANEL: u32 = 0x181a1f;
pub const PANEL_HI: u32 = 0x20232b;
pub const BORDER: u32 = 0x2a2e37;
pub const TEXT: u32 = 0xeceef2;
pub const MUTED: u32 = 0x8a909c;
pub const FAINT: u32 = 0x5c626e;
pub const ACCENT: u32 = 0xffb224;
pub const ACCENT_HI: u32 = 0xffc655;
pub const ON_ACCENT: u32 = 0x1a1300;
pub const GOOD: u32 = 0x46d08a;
pub const BAD: u32 = 0xff6b5e;
pub const BLUE: u32 = 0x6aa9ff;

pub fn scrim() -> Rgba {
    rgba(0x000000aa)
}

/// Filled amber button: the one obvious thing to press.
pub fn primary_button(id: impl Into<SharedString>, label: impl Into<SharedString>) -> Stateful<Div> {
    div()
        .id(id.into())
        .px_4()
        .h(px(32.))
        .flex()
        .items_center()
        .justify_center()
        .rounded_md()
        .bg(rgb(ACCENT))
        .text_color(rgb(ON_ACCENT))
        .text_sm()
        .font_weight(FontWeight::SEMIBOLD)
        .cursor_pointer()
        .hover(|s| s.bg(rgb(ACCENT_HI)))
        .active(|s| s.opacity(0.85))
        .child(label.into())
}

/// Outlined button for secondary actions.
pub fn button(id: impl Into<SharedString>, label: impl Into<SharedString>) -> Stateful<Div> {
    div()
        .id(id.into())
        .px_3()
        .h(px(32.))
        .flex()
        .items_center()
        .justify_center()
        .rounded_md()
        .border_1()
        .border_color(rgb(BORDER))
        .text_color(rgb(TEXT))
        .text_sm()
        .cursor_pointer()
        .hover(|s| s.bg(rgb(PANEL_HI)))
        .active(|s| s.opacity(0.85))
        .child(label.into())
}

/// Small square button holding a glyph.
pub fn icon_button(id: impl Into<SharedString>, glyph: impl Into<SharedString>) -> Stateful<Div> {
    div()
        .id(id.into())
        .size(px(28.))
        .flex()
        .items_center()
        .justify_center()
        .rounded_md()
        .text_color(rgb(MUTED))
        .cursor_pointer()
        .hover(|s| s.bg(rgb(PANEL_HI)).text_color(rgb(TEXT)))
        .child(glyph.into())
}

/// The drive glyph: three stacked platters with an activity light.
pub fn drive_glyph(lit: bool, scale: f32) -> Div {
    let w = 64. * scale;
    let platter = |tone: u32| div().w(px(w)).h(px(13. * scale)).rounded(px(4. * scale)).bg(rgb(tone)).border_1().border_color(rgb(BORDER));
    div()
        .flex()
        .flex_col()
        .gap(px(3. * scale))
        .child(platter(0x262a33))
        .child(platter(0x2d323d))
        .child(
            platter(0x363c49).flex().items_center().justify_end().pr(px(6. * scale)).child(
                div().size(px(5. * scale)).rounded_full().bg(rgb(if lit { GOOD } else { FAINT })),
            ),
        )
}

pub fn folder_glyph(scale: f32) -> Div {
    div()
        .flex()
        .flex_col()
        .child(div().w(px(22. * scale)).h(px(7. * scale)).rounded_t(px(3. * scale)).bg(rgb(0xb98a2e)))
        .child(div().w(px(52. * scale)).h(px(34. * scale)).rounded(px(4. * scale)).rounded_tl(px(0.)).bg(rgb(0xe0a83a)))
}

pub fn file_glyph(scale: f32, tint: u32) -> Div {
    div()
        .w(px(38. * scale))
        .h(px(46. * scale))
        .rounded(px(4. * scale))
        .bg(rgb(0x2b303a))
        .border_1()
        .border_color(rgb(0x3a404c))
        .flex()
        .flex_col()
        .justify_end()
        .child(div().h(px(8. * scale)).rounded_b(px(3. * scale)).bg(rgb(tint)))
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
    fn dates() {
        let now = 1_795_046_400_000; // 2026-11-19
        assert_eq!(fmt_time(now - 5_000, now), "just now");
        assert_eq!(fmt_time(now - 7_200_000, now), "2 h ago");
        assert_eq!(fmt_time(1_785_888_000_000, now), "2026-08-05");
    }
}
