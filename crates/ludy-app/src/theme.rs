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

/// A moment as this Mac's clock shows it: `19 Nov 2026 at 14:05`.
pub fn fmt_moment(unix_ms: u64) -> String {
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let secs = (unix_ms / 1000) as libc::time_t;
    // SAFETY: `localtime_r` writes into the `tm` it is given and keeps neither pointer.
    let tm = unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&secs, &mut tm).is_null() {
            return String::new();
        }
        tm
    };
    format!("{} {} {} at {:02}:{:02}", tm.tm_mday, MONTHS[tm.tm_mon.rem_euclid(12) as usize], tm.tm_year + 1900, tm.tm_hour, tm.tm_min)
}

/// A whole number with its thousands apart: `1,572,864`.
pub fn fmt_count(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// How a folder's width is shared between tiles and the details pane.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GridFit {
    /// Tiles in a row when the pane is closed, and when it is open.
    pub cols: usize,
    pub cols_beside_pane: usize,
    /// A tile's width, in whole points. It is the same with the pane open
    /// and closed, so that opening the pane moves no tile sideways.
    pub tile_w: f32,
    /// The pane's width: a whole number of tiles and their gaps.
    pub pane_w: f32,
}

/// Fit tiles no narrower than `min_tile`, `gap` apart, into `full`, and a
/// pane no narrower than `min_pane` that takes the place of whole columns.
/// The columns left of the pane keep their size and their place.
pub fn grid_fit(full: f32, min_tile: f32, gap: f32, min_pane: f32) -> GridFit {
    let cols = ((full / min_tile) as usize).max(1);
    let tile_w = ((full - gap * (cols - 1) as f32) / cols as f32).floor();
    let pitch = tile_w + gap;
    let taken = (min_pane / pitch).ceil() as usize;
    if taken >= cols {
        // No room for a tile of that size beside the pane: one narrower
        // column. The window's smallest size keeps this from happening.
        let pane_w = min_pane.min(full - gap);
        return GridFit { cols, cols_beside_pane: 1, tile_w: tile_w.min(full - pane_w).max(1.), pane_w };
    }
    GridFit { cols, cols_beside_pane: cols - taken, tile_w, pane_w: taken as f32 * pitch }
}

/// `chars[from..]` in one line no wider than `max`: all of it when it fits,
/// else its start, an ellipsis, and its end in a little under half the
/// line, so that the end of a name, where the extension is, can be read.
fn middle(chars: &[char], x: &[f32], from: usize, ellipsis: f32, max: f32) -> String {
    let n = chars.len();
    let text = |a: usize, b: usize| chars[a..b].iter().collect::<String>();
    let width = |a: usize, b: usize| x[b] - x[a];
    if width(from, n) <= max {
        return text(from, n);
    }
    let tail = (1..n - from).take_while(|&t| width(n - t, n) <= max * 0.45).last().unwrap_or(1);
    let room = max - ellipsis - width(n - tail, n);
    let head = (0..n - from - tail).take_while(|&h| width(from, from + h) <= room).last().unwrap_or(0);
    format!("{}\u{2026}{}", text(from, from + head).trim_end(), text(n - tail, n))
}

/// A name in one line no wider than `max`, its middle left out when it is
/// longer. `x[i]` is where character `i` starts and `x[chars.len()]` where
/// the name ends, in the same unit as `max` and the width of an `ellipsis`.
pub fn name_line(chars: &[char], x: &[f32], ellipsis: f32, max: f32) -> String {
    middle(chars, x, 0, ellipsis, max)
}

/// A name under a tile: one line when it fits in `max`, else two, and when
/// two are not enough the middle of the second is left out (see `name_line`).
pub fn name_lines(chars: &[char], x: &[f32], ellipsis: f32, max: f32) -> (String, Option<String>) {
    let n = chars.len();
    let text = |a: usize, b: usize| chars[a..b].iter().collect::<String>();
    let width = |a: usize, b: usize| x[b] - x[a];
    if n == 0 || width(0, n) <= max {
        return (text(0, n), None);
    }
    // The first line: what fits, ending after a space, hyphen or underscore
    // or before a dot when there is one in its last two fifths, so that a
    // word or an extension is not cut in two where it need not be.
    let mut cut = (1..n).take_while(|&k| width(0, k) <= max).last().unwrap_or(1);
    let breaks_at = |j: usize| matches!(chars[j - 1], ' ' | '-' | '_') || (j < n && chars[j] == '.');
    if let Some(at) = (cut * 3 / 5 + 1..=cut).rev().find(|&j| breaks_at(j)) {
        cut = at;
    }
    let first = text(0, cut).trim_end().to_string();
    match (cut..n).find(|&i| chars[i] != ' ') {
        Some(from) => (first, Some(middle(chars, x, from, ellipsis, max))),
        None => (first, None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `name_lines` with every character 7 wide and ten to a line.
    fn lines(name: &str) -> (String, Option<String>) {
        let chars: Vec<char> = name.chars().collect();
        let x: Vec<f32> = (0..=chars.len()).map(|i| i as f32 * 7.).collect();
        name_lines(&chars, &x, 7., 70.)
    }

    #[test]
    fn names_keep_their_end() {
        let two = |a: &str, b: &str| (a.to_string(), Some(b.to_string()));
        assert_eq!(lines(""), (String::new(), None));
        assert_eq!(lines("short.txt"), ("short.txt".to_string(), None));
        // Nothing to break at: cut where the line is full.
        assert_eq!(lines("zEnytxQHuS3FqELQ.mp4"), two("zEnytxQHuS", "3FqELQ.mp4"));
        // A space near the end of the first line: break there, and drop it.
        assert_eq!(lines("a very long.mov"), two("a very", "long.mov"));
        // Too long for two lines: the middle goes, the extension stays.
        assert_eq!(lines("a very long holiday video.mov"), two("a very", "long\u{2026}.mov"));
        assert_eq!(lines("0123456789abcdefghijklmnopqrstuvwxyz"), two("0123456789", "abcde\u{2026}wxyz"));
        // Many dots, and no extension at all.
        assert_eq!(lines("archive.2026.11.23.final.tar.gz"), two("archive", ".2026\u{2026}r.gz"));
        // A dot near the end of the first line: the extension goes to the second whole.
        assert_eq!(lines("holiday8.jpeg"), two("holiday8", ".jpeg"));
        assert_eq!(lines("clip-0001.mp4"), two("clip-0001", ".mp4"));
        assert_eq!(lines("README for the whole project"), two("README", "for t\u{2026}ject"));
        // Characters of more than one byte are cut between, never inside.
        assert_eq!(lines("caf\u{e9} d\u{e9}j\u{e0} vu \u{2013} \u{e9}t\u{e9} \u{e0} la plage.jpeg"), two("caf\u{e9} d\u{e9}j\u{e0}", "vu \u{2013}\u{2026}jpeg"));
        assert_eq!(lines("\u{65e5}\u{672c}\u{8a9e}\u{306e}\u{30d5}\u{30a1}\u{30a4}\u{30eb}\u{540d}\u{524d}\u{3067}\u{3059}.mov").1.as_deref(), Some("\u{3067}\u{3059}.mov"));
        // One character wider than a tile still gives a line.
        assert_eq!(name_lines(&['W'], &[0., 90.], 7., 70.), ("W".to_string(), None));
        assert_eq!(name_line(&['W'], &[0., 90.], 7., 70.), "\u{2026}W");

        // In a list a name has one line.
        let one = |name: &str| {
            let chars: Vec<char> = name.chars().collect();
            let x: Vec<f32> = (0..=chars.len()).map(|i| i as f32 * 7.).collect();
            name_line(&chars, &x, 7., 70.)
        };
        assert_eq!(one("short.txt"), "short.txt");
        assert_eq!(one("a very long holiday video.mov"), "a ver\u{2026}.mov");
        assert_eq!(one("caf\u{e9} d\u{e9}j\u{e0} vu \u{2013} \u{e9}t\u{e9}.jpeg"), "caf\u{e9}\u{2026}jpeg");
    }

    #[test]
    fn opening_the_pane_moves_no_tile() {
        // Every folder width from the smallest window's to a very wide one.
        for full in 568..3200 {
            let full = full as f32;
            let fit = grid_fit(full, 148., 4., 260.);
            assert_eq!(fit.tile_w, fit.tile_w.floor(), "{full}: whole points");
            assert!(fit.tile_w >= 144., "{full}: {fit:?}");
            assert!(fit.cols_beside_pane >= 1 && fit.cols_beside_pane < fit.cols, "{full}: {fit:?}");
            // The pane is wide enough, takes whole columns, and no more of them than it needs.
            let pitch = fit.tile_w + 4.;
            assert!(fit.pane_w >= 260. && fit.pane_w < 260. + pitch, "{full}: {fit:?}");
            assert_eq!(fit.pane_w, (fit.cols - fit.cols_beside_pane) as f32 * pitch, "{full}");
            // Both rows fit where they are drawn.
            let row = |cols: usize| cols as f32 * pitch - 4.;
            assert!(row(fit.cols) <= full, "{full}: {fit:?}");
            assert!(row(fit.cols_beside_pane) <= full - fit.pane_w, "{full}: {fit:?}");
        }
        // Too narrow for a tile beside the pane: one column, and no panic.
        let fit = grid_fit(300., 148., 4., 260.);
        assert_eq!((fit.cols, fit.cols_beside_pane), (2, 1));
        assert!(fit.tile_w >= 1. && fit.pane_w <= 300.);
    }

    #[test]
    fn counts_and_moments() {
        assert_eq!(fmt_count(0), "0");
        assert_eq!(fmt_count(999), "999");
        assert_eq!(fmt_count(1_572_864), "1,572,864");
        assert_eq!(fmt_count(12_000_000_000), "12,000,000,000");
        // 2026-11-19 00:00 UTC is the 18th or the 19th of November everywhere.
        let moment = fmt_moment(1_795_046_400_000);
        assert!(moment.contains(" Nov 2026 at "), "{moment}");
    }

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
