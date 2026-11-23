//! Drawing the window: a sidebar of drives, a toolbar, the drives or the
//! folder in the middle, and a status bar with the live numbers.
//!
//! Folders are drawn with `uniform_list`, so only the rows on screen are
//! built each frame, however many files a folder holds.

use std::ops::Range;
use std::time::{Duration, Instant};

use gpui::{
    anchored, deferred, div, ease_out_quint, point, prelude::*, px, relative, rgb, uniform_list, Animation, AnimationExt as _, AnyElement, ClickEvent, Context,
    Div, FontWeight, MouseButton, MouseDownEvent, ScrollStrategy, SharedString, Stateful, TextRun, Window,
};
use ludy_core::engine::{DriveView, MountState};

use super::{now_ms, open_path, Entry, LudyApp, Menu, Modal, Perf, Screen, Target};
use crate::theme::*;

/// Every row of the grid is this high, with room for a name of two lines.
const GRID_ROW_H: f32 = 168.;
/// A tile's name: its size, and the height of one of its lines.
const NAME_SIZE: f32 = 13.;
const NAME_LINE_H: f32 = 17.;
/// The gap between tiles, and a tile's padding on each side.
const TILE_GAP: f32 = 4.;
const TILE_PAD: f32 = 10.;
/// The details pane beside a folder is at least this wide (see `grid_fit`).
const INFO_MIN_W: f32 = 260.;
/// The list's columns after the name, and the gap between columns.
const LIST_SIZE_W: f32 = 100.;
const LIST_WIDE_W: f32 = 120.;
const LIST_GAP: f32 = 12.;
const LIST_ROW_H: f32 = 34.;
/// Narrowest a grid tile may get before a column is dropped.
const TILE_MIN_W: f32 = 148.;
/// Horizontal inset of rows; with a tile's own padding it lines content up with `PAD`.
const ROW_INSET: f32 = PAD - 10.;

fn plural(n: usize, one: &str) -> String {
    format!("{n} {one}{}", if n == 1 { "" } else { "s" })
}

fn home_relative(path: &std::path::Path) -> String {
    path.display().to_string().replacen(&std::env::var("HOME").unwrap_or_default(), "~", 1)
}

impl LudyApp {
    // ------------------------------------------------------------ sidebar

    fn nav_row(id: SharedString, icon_name: &'static str, label: String, active: bool) -> Stateful<Div> {
        div()
            .id(id)
            .h(px(30.))
            .pl(px(8.))
            .pr(px(4.))
            .flex()
            .items_center()
            .gap(px(8.))
            .rounded(px(7.))
            .cursor_pointer()
            .when(active, |d| d.bg(rgb(T.active)))
            .when(!active, |d| d.hover(|s| s.bg(rgb(T.hover))))
            .child(icon(icon_name, 16., if active { T.text } else { T.muted }))
            .child(div().flex_1().min_w(px(0.)).truncate().child(label))
    }

    fn sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let on_drives = self.screen == Screen::Drives;
        let mut drives: Vec<Stateful<Div>> = Vec::with_capacity(self.view.drives.len());
        for d in &self.view.drives {
            let name = d.info.name.clone();
            let active = matches!(&self.screen, Screen::Browser { drive, .. } if *drive == name);
            let mut row = Self::nav_row(SharedString::from(format!("nav-{name}")), "drive", name.clone(), active);
            if d.mount.is_some() {
                let eject = name.clone();
                row = row.child(
                    div()
                        .id(SharedString::from(format!("nav-eject-{name}")))
                        .size(px(22.))
                        .flex()
                        .flex_none()
                        .items_center()
                        .justify_center()
                        .rounded(px(6.))
                        .hover(|s| s.bg(rgb(T.line)))
                        .child(icon("eject", 14., T.muted))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation();
                            this.engine.eject(&eject);
                        })),
                );
            }
            drives.push(row.on_click(cx.listener(move |this, _, _, cx| this.open_drive(&name, cx))));
        }
        let address = self.view.server_url.trim_start_matches("https://").trim_start_matches("http://").to_string();
        let link = Some(match &self.view.server_name {
            Some(name) => format!("{name} \u{b7} {address}"),
            None => address,
        });
        div()
            .w(px(SIDEBAR_W))
            .h_full()
            .flex_none()
            .bg(rgb(T.sidebar))
            .border_r_1()
            .border_color(rgb(T.line))
            .flex()
            .flex_col()
            .px(px(10.))
            .pb(px(12.))
            // room for the traffic lights
            .child(div().h(px(52.)).flex_none())
            .child(
                div()
                    .h(px(30.))
                    .px(px(8.))
                    .mb(px(10.))
                    .flex()
                    .items_center()
                    .gap(px(7.))
                    .child(div().size(px(9.)).rounded_full().bg(rgb(T.accent)))
                    .child(div().text_size(px(16.)).font_weight(FontWeight::SEMIBOLD).child("ludy")),
            )
            .child(Self::nav_row("nav-drives".into(), "drives", "All drives".into(), on_drives).on_click(cx.listener(|this, _, _, cx| {
                this.show_drives();
                cx.notify();
            })))
            .child(
                div()
                    .h(px(30.))
                    .pl(px(8.))
                    .mt(px(14.))
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(div().text_size(px(11.)).font_weight(FontWeight::MEDIUM).text_color(rgb(T.faint)).child("Drives"))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .child(icon_button("add-drive", "down").size(px(24.)).on_click(cx.listener(|this, _, _, cx| {
                                this.modal = Some(Modal::AddDrive { key: String::new() });
                                cx.notify();
                            })))
                            .child(icon_button("new-drive", "plus").size(px(24.)).on_click(cx.listener(|this, _, _, cx| {
                                this.modal = Some(Modal::NewDrive { name: String::new() });
                                cx.notify();
                            }))),
                    ),
            )
            .child(div().id("nav-list").flex_1().min_h(px(0.)).overflow_y_scroll().flex().flex_col().gap(px(1.)).children(drives))
            .child(
                div()
                    .px(px(8.))
                    .pt(px(10.))
                    .flex()
                    .flex_col()
                    .gap(px(3.))
                    .text_size(px(12.))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(7.))
                            .text_color(rgb(T.muted))
                            .child(div().size(px(7.)).rounded_full().bg(rgb(if self.view.online { T.good } else { T.bad })))
                            .child(if self.view.online { "Server connected" } else { "Server unreachable" }),
                    )
                    .when_some(link, |d, link| d.child(div().truncate().text_color(rgb(T.faint)).child(link))),
            )
    }

    // ------------------------------------------------------------ toolbar

    /// A click handler that opens a folder of the current drive, or the
    /// drives screen for `None`.
    fn go_to(target: Option<String>, cx: &mut Context<Self>) -> impl Fn(&ClickEvent, &mut Window, &mut gpui::App) + 'static {
        let view = cx.weak_entity();
        move |_, _, cx| {
            let _ = view.update(cx, |this, cx| match &target {
                Some(prefix) => this.navigate(prefix.clone(), cx),
                None => {
                    this.show_drives();
                    cx.notify();
                }
            });
        }
    }

    fn toolbar(&self, cx: &mut Context<Self>) -> Div {
        let bar = div().h(px(52.)).flex_none().pl(px(PAD - 8.)).pr(px(PAD - 8.)).flex().items_center().justify_between().gap(px(16.));
        let Screen::Browser { drive, prefix } = &self.screen else {
            return bar.child(div()).child(
                button("toolbar-new-drive", "New drive").on_click(cx.listener(|this, _, _, cx| {
                    this.modal = Some(Modal::NewDrive { name: String::new() });
                    cx.notify();
                })),
            );
        };
        let mounted = self.drive(drive).is_some_and(|d| d.mount.is_some());

        // Drives > drive > folder > folder
        let mut crumbs: Vec<(String, Option<String>)> = vec![("Drives".into(), None), (drive.clone(), Some(String::new()))];
        let mut acc = String::new();
        for part in prefix.split('/').filter(|p| !p.is_empty()) {
            acc.push_str(part);
            acc.push('/');
            crumbs.push((part.to_string(), Some(acc.clone())));
        }
        let parent = crumbs[crumbs.len() - 2].1.clone();
        let last = crumbs.len() - 1;
        let go = Self::go_to;
        let mut trail = div().flex().items_center().gap(px(2.)).min_w(px(0.)).overflow_hidden();
        for (i, (label, target)) in crumbs.into_iter().enumerate() {
            if i > 0 {
                trail = trail.child(icon("chevron-right", 13., T.faint));
            }
            trail = trail.child(
                div()
                    .id(SharedString::from(format!("crumb-{i}")))
                    .h(px(26.))
                    .px(px(6.))
                    .flex()
                    .flex_none()
                    .items_center()
                    .rounded(px(6.))
                    .text_color(rgb(if i == last { T.text } else { T.muted }))
                    .when(i == last, |d| d.font_weight(FontWeight::MEDIUM))
                    .cursor_pointer()
                    .hover(|s| s.bg(rgb(T.hover)))
                    .child(label)
                    .on_click(go(target, cx)),
            );
        }

        let view_toggle = |id: &'static str, name: &'static str, on: bool, grid: bool, cx: &mut Context<Self>| {
            div()
                .id(id)
                .size(px(26.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(6.))
                .cursor_pointer()
                .when(on, |d| d.bg(rgb(T.surface)).shadow_sm())
                .child(icon(name, 16., if on { T.text } else { T.muted }))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.grid = grid;
                    cx.notify();
                }))
        };
        let drive_name = drive.clone();
        let share_name = drive.clone();
        let share = cx.listener(move |this: &mut Self, _: &ClickEvent, _, cx| {
            this.modal = Some(Modal::DriveKey { drive: share_name.clone() });
            cx.notify();
        });
        bar.child(div().flex().items_center().gap(px(6.)).min_w(px(0.)).child(icon_button("back", "chevron-left").on_click(go(parent, cx))).child(trail)).child(
            div()
                .flex()
                .flex_none()
                .items_center()
                .gap(px(4.))
                .child(icon_button("import", "import").on_click(cx.listener(|this, _, _, cx| this.import(cx))))
                .child(icon_button("new-folder", "folder-plus").on_click(cx.listener(|this, _, _, cx| {
                    this.modal = Some(Modal::NewFolder { name: String::new() });
                    cx.notify();
                })))
                .child(icon_button("refresh", "refresh").on_click(cx.listener(|this, _, _, cx| this.reload(cx))))
                .child(
                    div()
                        .mx(px(6.))
                        .p(px(2.))
                        .flex()
                        .gap(px(2.))
                        .rounded(px(8.))
                        .bg(rgb(T.hover))
                        .child(view_toggle("view-grid", "grid", self.grid, true, cx))
                        .child(view_toggle("view-list", "list", !self.grid, false, cx)),
                )
                .child(if mounted {
                    button("browser-finder", "Show in Finder")
                        .on_click(cx.listener(move |this, _, _, _| {
                            let Screen::Browser { drive, prefix } = this.screen.clone() else { return };
                            match this.local_path(&drive, &prefix) {
                                Some(p) => open_path(&[], &p),
                                None => this.show_in_finder(&drive, ""),
                            }
                        }))
                        .into_any_element()
                } else {
                    button("browser-mount", "Mount").on_click(cx.listener(move |this, _, _, _| this.engine.mount(&drive_name))).into_any_element()
                })
                .child(div().ml(px(4.)).child(primary_button("browser-share", "Drive key").on_click(share))),
        )
    }

    // ------------------------------------------------------------ status bar

    /// The live numbers, always on screen: streamed, uploads, cache.
    fn status_bar(&self, left: String, cx: &mut Context<Self>) -> impl IntoElement {
        let v = &self.view;
        let pending: usize = v.drives.iter().filter_map(|d| d.pending.as_ref()).map(|p| p.files + p.ops).sum();
        let uploading = v.drives.iter().filter_map(|d| d.pending.as_ref()).find_map(|p| p.uploading.clone());
        let down = match (v.stats.down_rate >= 1.0, v.stats.range_requests) {
            (true, n) => format!("{} \u{b7} {} in {}", fmt_rate(v.stats.down_rate), fmt_bytes(v.stats.bytes_down), plural(n as usize, "range")),
            (false, 0) => "Nothing streamed yet".into(),
            (false, n) => format!("{} streamed in {}", fmt_bytes(v.stats.bytes_down), plural(n as usize, "range")),
        };
        let up = match (&uploading, pending) {
            (Some(name), _) => format!("Uploading {name} \u{b7} {}", fmt_rate(v.stats.up_rate)),
            (None, 0) if v.stats.bytes_up > 0 => format!("{} uploaded", fmt_bytes(v.stats.bytes_up)),
            (None, 0) => "Up to date".into(),
            (None, n) => format!("{} waiting", plural(n, "change")),
        };
        let fill = if v.cache.cap > 0 { (v.cache.bytes as f32 / v.cache.cap as f32).clamp(0.0, 1.0) } else { 0.0 };
        let item = |name: &'static str, text: String| div().flex().flex_none().items_center().gap(px(5.)).child(icon(name, 13., T.faint)).child(text);
        div()
            .h(px(30.))
            .flex_none()
            .px(px(PAD - 8.))
            .flex()
            .items_center()
            .justify_between()
            .gap(px(16.))
            .border_t_1()
            .border_color(rgb(T.line))
            .text_size(px(12.))
            .text_color(rgb(T.muted))
            .child(div().flex_1().min_w(px(0.)).truncate().child(left))
            .child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap(px(18.))
                    .child(item("down", down))
                    .child(item("up", up))
                    .child(
                        div()
                            .flex()
                            .flex_none()
                            .items_center()
                            .gap(px(7.))
                            .child("Cache")
                            .child(div().w(px(52.)).h(px(4.)).rounded_full().bg(rgb(T.line)).child(div().h_full().w(relative(fill.max(0.02))).rounded_full().bg(rgb(T.accent))))
                            .child(format!("{} of {}", fmt_bytes(v.cache.bytes), fmt_bytes(v.cache.cap)))
                            .child(
                                div()
                                    .id("clear-cache")
                                    .px(px(5.))
                                    .rounded(px(5.))
                                    .cursor_pointer()
                                    .text_color(rgb(T.text))
                                    .hover(|s| s.bg(rgb(T.hover)))
                                    .child("Clear")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.engine.cache().clear();
                                        cx.notify();
                                    })),
                            ),
                    ),
            )
    }

    // ------------------------------------------------------------ drives

    /// A click handler that runs a drive action. Built by hand (rather than
    /// with `cx.listener`) so it can be returned from a function.
    fn drive_action(label: &'static str, name: &str, cx: &mut Context<Self>) -> impl Fn(&ClickEvent, &mut Window, &mut gpui::App) + 'static {
        let name = name.to_string();
        let view = cx.weak_entity();
        move |_, window, cx| {
            let _ = view.update(cx, |this, cx| this.menu_action(label, Target::Drive(name.clone()), window, cx));
        }
    }

    fn drive_card(&self, d: &DriveView, cx: &mut Context<Self>) -> impl IntoElement {
        let name = d.info.name.clone();
        let mounted = d.mount.is_some();
        let stuck = d.mount.as_ref().is_some_and(|m| m.state == MountState::Stuck);
        let place = match (&d.busy, &d.mount) {
            (Some(b), _) => b.clone(),
            (None, Some(m)) if stuck => "Eject did not finish".into(),
            (None, Some(m)) => home_relative(&m.mountpoint),
            (None, None) => "Not mounted".into(),
        };
        // A drive's size is in its tree, which only this Mac can read, and
        // only once the drive has been opened.
        let detail = match d.info.totals {
            Some((files, bytes)) => format!("{} \u{b7} {}", fmt_bytes(bytes), plural(files as usize, "file")),
            None => "End-to-end encrypted".to_string(),
        };
        let act = Self::drive_action;
        let (menu_name, more_name) = (name.clone(), name.clone());
        div()
            .id(SharedString::from(format!("card-{name}")))
            .w(px(236.))
            .rounded(px(16.))
            .bg(rgb(T.surface))
            .border_1()
            .border_color(rgb(T.line))
            .when(mounted, |d| d.shadow_md())
            .px(px(16.))
            .pt(px(10.))
            .pb(px(18.))
            .flex()
            .flex_col()
            .items_center()
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, ev: &MouseDownEvent, _, cx| {
                    this.menu = Some(Menu { at: ev.position, target: Target::Drive(menu_name.clone()) });
                    cx.notify();
                }),
            )
            .child(
                div()
                    .w_full()
                    .h(px(30.))
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(icon_button(format!("more-{name}"), "more").on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, ev: &MouseDownEvent, _, cx| {
                            this.menu = Some(Menu { at: ev.position, target: Target::Drive(more_name.clone()) });
                            cx.stop_propagation();
                            cx.notify();
                        }),
                    ))
                    .when(mounted, |row| row.child(icon_button(format!("eject-{name}"), "eject").on_click(act("Eject", &name, cx)))),
            )
            .child(div().pt(px(4.)).pb(px(14.)).child(drive_glyph(mounted, 1.0)))
            .child(div().max_w_full().truncate().text_size(px(15.)).font_weight(FontWeight::SEMIBOLD).child(name.clone()))
            .child(
                div()
                    .max_w_full()
                    .pt(px(3.))
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .text_size(px(12.))
                    .text_color(rgb(if stuck { T.bad } else { T.muted }))
                    .when(mounted && !stuck && d.busy.is_none(), |row| row.child(div().size(px(6.)).flex_none().rounded_full().bg(rgb(T.good))))
                    .child(div().min_w(px(0.)).truncate().child(place)),
            )
            .child(div().max_w_full().truncate().pt(px(2.)).text_size(px(12.)).text_color(rgb(T.faint)).child(detail))
            .child(div().flex().gap(px(8.)).pt(px(16.)).children(if d.busy.is_some() {
                vec![button(format!("busy-{name}"), "Working\u{2026}").opacity(0.6)]
            } else if mounted {
                vec![
                    primary_button(format!("open-{name}"), "Open").on_click(act("Open", &name, cx)),
                    button(format!("finder-{name}"), "Show in Finder").on_click(act("Show in Finder", &name, cx)),
                ]
            } else {
                vec![
                    primary_button(format!("mount-{name}"), "Mount").on_click(act("Mount", &name, cx)),
                    button(format!("open-{name}"), "Open").on_click(act("Open", &name, cx)),
                ]
            }))
    }

    fn page_header(title: String, subtitle: String) -> Div {
        div()
            .flex_none()
            .px(px(PAD))
            .pt(px(4.))
            .pb(px(18.))
            .flex()
            .flex_col()
            .gap(px(3.))
            .child(div().truncate().text_size(px(24.)).font_weight(FontWeight::SEMIBOLD).child(title))
            .child(div().truncate().text_color(rgb(T.muted)).child(subtitle))
    }

    fn empty_state(glyph: Div, title: &'static str, hint: &'static str) -> Div {
        div()
            .size_full()
            .pb(px(60.))
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(px(6.))
            .child(div().pb(px(10.)).opacity(0.75).child(glyph))
            .child(div().text_size(px(14.)).font_weight(FontWeight::MEDIUM).child(title))
            .child(div().text_color(rgb(T.muted)).child(hint))
    }

    fn drives_screen(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let v = &self.view;
        let subtitle = format!("{} on {}. {} cached on this Mac.", plural(v.drives.len(), "drive"), v.server_url, fmt_bytes(v.cache.bytes));
        div().size_full().flex().flex_col().child(Self::page_header("Drives".into(), subtitle)).child(if v.drives.is_empty() {
            Self::empty_state(
                drive_glyph(false, 1.0),
                if v.online { "No drives yet" } else { "Waiting for the server" },
                if v.online { "Create one with +, or add one from its key." } else { "Start it with: docker compose up -d --wait" },
            )
            .into_any_element()
        } else {
            div()
                .id("drives-scroll")
                .flex_1()
                .min_h(px(0.))
                .overflow_y_scroll()
                .px(px(PAD))
                .pb(px(PAD))
                .child(div().flex().flex_wrap().gap(px(16.)).children(v.drives.iter().map(|d| self.drive_card(d, cx))))
                .into_any_element()
        })
    }

    // ------------------------------------------------------------ folder

    /// How much of a file this Mac holds: a dot colour and a phrase.
    fn local_state(&self, e: &Entry) -> (Option<u32>, String) {
        if e.is_dir {
            return (None, String::new());
        }
        let local = e.local_bytes;
        if e.size > 0 && local >= e.size {
            (Some(T.good), "All of it".into())
        } else if local > 0 {
            (Some(T.blue), fmt_bytes(local))
        } else {
            (None, "\u{2013}".into())
        }
    }

    /// A name cut to fit `max`, measured in the font it is drawn in: in a
    /// tile's two lines (see `name_lines`), or in a list row's one.
    fn fit_name(name: &str, max: f32, two_lines: bool, window: &Window) -> (String, Option<String>) {
        // A name may hold a line break; a shaped line may not.
        let name = name.replace(['\n', '\r'], " ");
        let style = window.text_style();
        let shape = |text: &str| {
            let run = TextRun { len: text.len(), font: style.font(), color: style.color, background_color: None, underline: None, strikethrough: None };
            window.text_system().shape_line(SharedString::from(text.to_string()), px(NAME_SIZE), &[run], None)
        };
        let line = shape(&name);
        if f32::from(line.width) <= max {
            return (name, None);
        }
        let chars: Vec<char> = name.chars().collect();
        let mut x: Vec<f32> = name.char_indices().map(|(at, _)| f32::from(line.x_for_index(at))).collect();
        x.push(f32::from(line.width));
        let ellipsis = f32::from(shape("\u{2026}").width);
        if two_lines {
            name_lines(&chars, &x, ellipsis, max)
        } else {
            (name_line(&chars, &x, ellipsis, max), None)
        }
    }

    /// What every row and tile share: hover, the selection, what a click
    /// does, the context menu.
    fn entry_base(&self, e: &Entry, cx: &mut Context<Self>) -> Stateful<Div> {
        let (clicked, menu) = (e.clone(), e.clone());
        let selected = self.selected.as_deref() == Some(e.key.as_str());
        div()
            .id(SharedString::from(format!("entry-{}", e.key)))
            .cursor_pointer()
            .when(selected, |d| d.bg(rgb(T.active)))
            .when(!selected, |d| d.hover(|s| s.bg(rgb(T.hover))))
            .active(|s| s.bg(rgb(T.active)))
            .on_click(cx.listener(move |this, ev: &ClickEvent, window, cx| this.click_entry(&clicked, ev, window, cx)))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, ev: &MouseDownEvent, _, cx| {
                    this.menu = Some(Menu { at: ev.position, target: Target::Entry(menu.clone()) });
                    cx.notify();
                }),
            )
    }

    /// One row of tiles, each `tile_w` wide. The width is given, not shared
    /// out of the row's, so that a tile is where it was whatever the row holds.
    fn grid_row(&self, row: usize, cols: usize, tile_w: f32, window: &Window, cx: &mut Context<Self>) -> Div {
        let name_w = tile_w - 2. * TILE_PAD - 1.;
        let start = row * cols;
        let end = (start + cols).min(self.entries.len());
        let mut line = div().h(px(GRID_ROW_H)).w_full().px(px(ROW_INSET)).flex().gap(px(TILE_GAP));
        for e in &self.entries[start..end] {
            let (dot, _) = self.local_state(e);
            let (first, second) = Self::fit_name(&e.name, name_w, true, window);
            let name_line = |text: String| div().max_w_full().overflow_hidden().whitespace_nowrap().child(text);
            line = line.child(
                self.entry_base(e, cx)
                    .w(px(tile_w))
                    .flex_none()
                    .h(px(GRID_ROW_H - 6.))
                    .px(px(TILE_PAD))
                    .pt(px(12.))
                    .rounded(px(12.))
                    .flex()
                    .flex_col()
                    .items_center()
                    .child(div().h(px(74.)).flex().items_center().justify_center().child(if e.is_dir { folder_glyph(1.0) } else { file_glyph(1.0, &extension(&e.name)) }))
                    .child(
                        div()
                            .max_w_full()
                            .pt(px(8.))
                            .flex()
                            .flex_col()
                            .items_center()
                            .text_size(px(NAME_SIZE))
                            .line_height(px(NAME_LINE_H))
                            .child(name_line(first))
                            .children(second.map(name_line)),
                    )
                    .child(
                        div()
                            .max_w_full()
                            .pt(px(2.))
                            .flex()
                            .items_center()
                            .gap(px(5.))
                            .text_size(px(12.))
                            .text_color(rgb(T.faint))
                            .when_some(dot, |d, c| d.child(div().size(px(6.)).flex_none().rounded_full().bg(rgb(c))))
                            .child(if e.is_dir { "Folder".to_string() } else { fmt_bytes(e.size) }),
                    ),
            );
        }
        line
    }

    /// The columns of the list. `slim` leaves out the two that the details
    /// pane shows, so that a name keeps its room while the pane is open.
    fn list_columns(name: AnyElement, size: String, modified: String, local: AnyElement, slim: bool) -> Vec<AnyElement> {
        let mut columns = vec![div().flex_1().min_w(px(0.)).child(name).into_any_element(), div().w(px(LIST_SIZE_W)).flex_none().truncate().child(size).into_any_element()];
        if !slim {
            columns.push(div().w(px(LIST_WIDE_W)).flex_none().truncate().child(modified).into_any_element());
            columns.push(div().w(px(LIST_WIDE_W)).flex_none().child(local).into_any_element());
        }
        columns
    }

    /// The width a row of the list has for a name, in a list `width` across.
    fn list_name_w(width: f32, slim: bool) -> f32 {
        let others = if slim { LIST_SIZE_W + LIST_GAP } else { LIST_SIZE_W + 2. * LIST_WIDE_W + 3. * LIST_GAP };
        // Less the row's padding, the other columns and the glyph before the name.
        (width - 2. * 10. - others - (22. + 10.) - 1.).max(40.)
    }

    fn list_header(slim: bool) -> Div {
        div().flex_none().px(px(ROW_INSET)).child(
            div()
                .h(px(28.))
                .px(px(10.))
                .flex()
                .items_center()
                .gap(px(LIST_GAP))
                .border_b_1()
                .border_color(rgb(T.line))
                .text_size(px(12.))
                .text_color(rgb(T.faint))
                .children(Self::list_columns("Name".into_any_element(), "Size".into(), "Modified".into(), "On this Mac".into_any_element(), slim)),
        )
    }

    /// One row of the list. `name_w` is the width it has for the name.
    fn list_row(&self, ix: usize, name_w: f32, slim: bool, window: &Window, cx: &mut Context<Self>) -> Div {
        let e = &self.entries[ix];
        let (dot, local) = self.local_state(e);
        let name = div()
            .flex()
            .items_center()
            .gap(px(10.))
            .child(div().w(px(22.)).flex().flex_none().justify_center().child(if e.is_dir { folder_glyph(0.32) } else { file_glyph(0.36, "") }))
            .child(div().min_w(px(0.)).overflow_hidden().whitespace_nowrap().child(Self::fit_name(&e.name, name_w, false, window).0));
        let local = div()
            .flex()
            .items_center()
            .gap(px(6.))
            .when_some(dot, |d, c| d.child(div().size(px(6.)).flex_none().rounded_full().bg(rgb(c))))
            .child(div().truncate().child(local));
        let (size, modified) = if e.is_dir { ("\u{2013}".to_string(), String::new()) } else { (fmt_bytes(e.size), fmt_time(e.mtime_ms, now_ms())) };
        div().h(px(LIST_ROW_H)).w_full().px(px(ROW_INSET)).child(
            self.entry_base(e, cx)
                .h(px(LIST_ROW_H - 2.))
                .px(px(10.))
                .flex()
                .items_center()
                .gap(px(LIST_GAP))
                .rounded(px(7.))
                .text_color(rgb(T.muted))
                .children(Self::list_columns(div().text_color(rgb(T.text)).child(name).into_any_element(), size, modified, local.into_any_element(), slim)),
        )
    }

    fn browser_screen(&mut self, drive: &str, prefix: &str, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The row to keep in view now that the rows are a different width.
        let reveal = self.reveal.take().and_then(|key| self.entries.iter().position(|e| e.key == key));
        let title = prefix.trim_end_matches('/').rsplit('/').next().filter(|s| !s.is_empty()).unwrap_or(drive).to_string();
        let selected = self.selected_entry().cloned();
        let count = self.entries.len();
        let bytes: u64 = self.entries.iter().map(|e| e.size).sum();
        let subtitle = match (count, self.loading) {
            (0, true) => "Loading\u{2026}".to_string(),
            (_, _) if bytes > 0 => format!("{} \u{b7} {}", plural(count, "item"), fmt_bytes(bytes)),
            _ => plural(count, "item"),
        };
        // What the folder has across, beside the sidebar and the details pane.
        let full = f32::from(window.viewport_size().width) - SIDEBAR_W - 2. * ROW_INSET;
        let fit = grid_fit(full, TILE_MIN_W, TILE_GAP, INFO_MIN_W);
        let pane_w = if selected.is_some() { fit.pane_w } else { 0. };
        let width = full - pane_w;
        let body = if count == 0 {
            if self.loading {
                div().into_any_element()
            } else {
                Self::empty_state(folder_glyph(1.0), "This folder is empty", "Import files here, or copy them in with the Finder.").into_any_element()
            }
        } else if self.grid {
            let (cols, tile_w) = (if selected.is_some() { fit.cols_beside_pane } else { fit.cols }, fit.tile_w);
            if let Some(ix) = reveal {
                self.scroll.scroll_to_item(ix / cols, ScrollStrategy::Center);
            }
            uniform_list(
                "grid",
                count.div_ceil(cols),
                cx.processor(move |this, range: Range<usize>, window, cx| {
                    let t0 = Instant::now();
                    let rows = range.map(|row| this.grid_row(row, cols, tile_w, window, cx)).collect::<Vec<_>>();
                    this.count_build(t0);
                    rows
                }),
            )
            .track_scroll(self.scroll.clone())
            .size_full()
            .into_any_element()
        } else {
            let slim = selected.is_some();
            let name_w = Self::list_name_w(width, slim);
            if let Some(ix) = reveal {
                self.scroll.scroll_to_item(ix, ScrollStrategy::Center);
            }
            div()
                .size_full()
                .flex()
                .flex_col()
                .child(Self::list_header(slim))
                .child(
                    div().flex_1().min_h(px(0.)).pt(px(4.)).child(
                        uniform_list(
                            "list",
                            count,
                            cx.processor(move |this, range: Range<usize>, window, cx| {
                                let t0 = Instant::now();
                                let rows = range.map(|ix| this.list_row(ix, name_w, slim, window, cx)).collect::<Vec<_>>();
                                this.count_build(t0);
                                rows
                            }),
                        )
                        .track_scroll(self.scroll.clone())
                        .size_full(),
                    ),
                )
                .into_any_element()
        };
        let folder = div().flex_1().min_w(px(0.)).h_full().flex().flex_col().child(Self::page_header(title, subtitle)).child(div().flex_1().min_h(px(0.)).child(body));
        div().size_full().flex().child(folder).when_some(selected, |d, e| d.child(self.info_pane(&e, drive, pane_w, cx)))
    }

    /// A click handler that does to an entry what the context menu's `label` does.
    fn entry_action(label: &'static str, e: &Entry, cx: &mut Context<Self>) -> impl Fn(&ClickEvent, &mut Window, &mut gpui::App) + 'static {
        let e = e.clone();
        let view = cx.weak_entity();
        move |ev, window, cx| {
            let _ = view.update(cx, |this, cx| {
                // The pane's own handler opens the file (see `info_pane`).
                if !this.is_second_click(ev) {
                    this.menu_action(label, Target::Entry(e.clone()), window, cx);
                }
            });
        }
    }

    /// The details of the selected file, beside the folder: its whole name,
    /// what it is, how much of it this Mac holds, where it is, and what
    /// can be done with it.
    fn info_pane(&self, e: &Entry, drive: &str, width: f32, cx: &mut Context<Self>) -> impl IntoElement {
        let ext = extension(&e.name);
        let kind = if ext.is_empty() { "File".to_string() } else { format!("{ext} file") };
        let size = if e.size < 1000 { fmt_bytes(e.size) } else { format!("{} ({} bytes)", fmt_bytes(e.size), fmt_count(e.size)) };
        let modified = if e.mtime_ms == 0 { "\u{2013}".to_string() } else { fmt_moment(e.mtime_ms) };
        let (dot, _) = self.local_state(e);
        let here = if e.size == 0 {
            "Nothing to fetch".to_string()
        } else if e.local_bytes >= e.size {
            "All of it".to_string()
        } else if e.local_bytes > 0 {
            format!("{} of {}", fmt_bytes(e.local_bytes), fmt_bytes(e.size))
        } else {
            // The app learns how much is cached from the file's manifest,
            // which it has only after a read in this session.
            "Not known until the file is read".to_string()
        };
        let place = match e.key.rsplit_once('/') {
            Some((folder, _)) => format!("{drive}/{folder}"),
            None => drive.to_string(),
        };
        let finder = match self.local_path(drive, &e.key) {
            Some(path) => home_relative(&path),
            None => "Not mounted".to_string(),
        };
        // A label over its value. Values wrap: a path or a name is shown whole.
        let fact = |label: &'static str, value: AnyElement| {
            div().flex().flex_col().gap(px(2.)).child(div().text_size(px(12.)).text_color(rgb(T.faint)).child(label)).child(div().w_full().child(value))
        };
        let words = |text: String| div().w_full().child(text).into_any_element();
        let act = |label: &'static str, cx: &mut Context<Self>| Self::entry_action(label, e, cx);
        // Three parts, top to bottom: the cross, what the file is (which
        // scrolls when it is long), and the buttons. The buttons are at the
        // bottom so that they are in one place whatever the name's length.
        div()
            .id("info")
            .w(px(width))
            .h_full()
            .flex_none()
            .flex()
            .flex_col()
            .border_l_1()
            .border_color(rgb(T.line))
            // The second click of the double-click that opened this pane can land on it.
            .on_click(cx.listener(|this, ev: &ClickEvent, window, cx| {
                if ev.click_count() >= 2 {
                    this.second_click(None, window, cx);
                }
            }))
            .child(
                div().h(px(38.)).flex_none().px(px(8.)).flex().items_center().justify_end().child(icon_button("info-close", "close").size(px(26.)).on_click(cx.listener(
                    |this, ev: &ClickEvent, _, cx| {
                        if !this.is_second_click(ev) {
                            this.close_details();
                            cx.notify();
                        }
                    },
                ))),
            )
            .child(
                div()
                    .id("info-facts")
                    .flex_1()
                    .min_h(px(0.))
                    .overflow_y_scroll()
                    .px(px(20.))
                    .pb(px(20.))
                    .flex()
                    .flex_col()
                    .gap(px(14.))
                    .child(div().h(px(92.)).flex_none().flex().items_center().justify_center().child(file_glyph(1.4, &ext)))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(3.))
                            .child(div().w_full().text_size(px(15.)).font_weight(FontWeight::SEMIBOLD).child(e.name.replace(['\n', '\r'], " ")))
                            .child(div().text_color(rgb(T.muted)).child(kind)),
                    )
                    .child(div().h(px(1.)).flex_none().bg(rgb(T.line)))
                    .child(fact("Size", words(size)))
                    .child(fact("Modified", words(modified)))
                    .child(fact(
                        "On this Mac",
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .when_some(dot, |d, c| d.child(div().size(px(6.)).flex_none().rounded_full().bg(rgb(c))))
                            .child(div().flex_1().min_w(px(0.)).child(here))
                            .into_any_element(),
                    ))
                    .child(fact("In the drive", words(place)))
                    .child(fact("In the Finder", words(finder))),
            )
            .child(
                div()
                    .flex_none()
                    .px(px(20.))
                    .py(px(14.))
                    .border_t_1()
                    .border_color(rgb(T.line))
                    .flex()
                    .flex_wrap()
                    .gap(px(8.))
                    .child(primary_button("info-open", "Open").on_click(act("Open", cx)))
                    .child(button("info-finder", "Show in Finder").on_click(act("Show in Finder", cx)))
                    .child(button("info-versions", "Versions").on_click(act("Versions", cx))),
            )
    }

    // ------------------------------------------------------------ menus

    fn menu_items(&self, target: &Target) -> Vec<(&'static str, bool)> {
        match target {
            Target::Drive(name) => {
                let mounted = self.drive(name).is_some_and(|d| d.mount.is_some());
                let mut v = vec![("Open", false), ("Show in Finder", false), ("Open in Terminal", false), ("Copy path", false), ("Rename", false), ("Copy drive key", false)];
                v.push((if mounted { "Eject" } else { "Mount" }, false));
                v.push(("Remove drive", true));
                v
            }
            Target::Entry(e) => {
                let mut v = vec![("Open", false), ("Show in Finder", false), ("Open in Terminal", false), ("Copy path", false), ("Rename", false)];
                if !e.is_dir {
                    v.push(("Versions", false));
                }
                v.push(("Delete", true));
                v
            }
        }
    }

    fn fade_in(id: &'static str) -> (SharedString, Animation) {
        (id.into(), Animation::new(Duration::from_millis(120)).with_easing(ease_out_quint()))
    }

    fn render_menu(&self, menu: &Menu, cx: &mut Context<Self>) -> AnyElement {
        let items = self.menu_items(&menu.target);
        let target = menu.target.clone();
        let mut list = div()
            .id("context-menu")
            .occlude()
            .w(px(204.))
            .p(px(5.))
            .rounded(px(10.))
            .bg(rgb(T.surface))
            .border_1()
            .border_color(rgb(T.line))
            .shadow_lg()
            .flex()
            .flex_col()
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                this.menu = None;
                cx.notify();
            }));
        for (label, danger) in items {
            let target = target.clone();
            if danger {
                list = list.child(div().h(px(1.)).mx(px(6.)).my(px(4.)).bg(rgb(T.line)));
            }
            list = list.child(
                div()
                    .id(SharedString::from(format!("menu-{label}")))
                    .h(px(28.))
                    .px(px(10.))
                    .flex()
                    .items_center()
                    .rounded(px(6.))
                    .text_color(rgb(if danger { T.bad } else { T.text }))
                    .cursor_pointer()
                    .hover(|s| s.bg(rgb(T.hover)))
                    .child(label)
                    .on_click(cx.listener(move |this, _, window, cx| this.menu_action(label, target.clone(), window, cx))),
            );
        }
        let (id, anim) = Self::fade_in("menu-in");
        deferred(anchored().position(menu.at).snap_to_window_with_margin(px(8.)).child(list.with_animation(id, anim, |el, t| el.opacity(t)))).with_priority(1).into_any_element()
    }

    // ------------------------------------------------------------ dialogs

    fn text_field(value: &str, placeholder: &'static str) -> Div {
        div()
            .h(px(36.))
            .px(px(11.))
            .flex()
            .items_center()
            .rounded(px(9.))
            .bg(rgb(T.surface))
            .border_1()
            .border_color(rgb(T.blue))
            .when(!value.is_empty(), |d| d.child(div().child(value.to_string())))
            .child(div().w(px(1.5)).h(px(16.)).mx(px(1.)).bg(rgb(T.blue)))
            .when(value.is_empty(), |d| d.child(div().text_color(rgb(T.faint)).child(placeholder)))
    }

    fn note(text: &'static str) -> Div {
        div().text_color(rgb(T.muted)).line_height(px(19.)).child(text)
    }

    fn render_modal(&self, modal: &Modal, cx: &mut Context<Self>) -> AnyElement {
        let (title, body, confirm, danger): (String, Div, Option<&'static str>, bool) = match modal {
            Modal::NewDrive { name } => ("New drive".into(), div().child(Self::text_field(name, "Drive name")), Some("Create"), false),
            Modal::RenameDrive { drive, name } => (format!("Rename {drive}"), div().child(Self::text_field(name, "New name")), Some("Rename"), false),
            Modal::AddDrive { key } => (
                "Add a drive from its key".into(),
                div()
                    .flex()
                    .flex_col()
                    .gap(px(12.))
                    .child(Self::note("Paste the key of a drive that lives on this server. The key is what opens the drive: copy it on the Mac that has the drive, from the drive's menu or with ludy key."))
                    .child(Self::text_field(if key.is_empty() { "" } else { "Key pasted" }, "Paste the key (\u{2318}V)")),
                Some("Add drive"),
                false,
            ),
            Modal::RemoveDrive { drive } => (
                format!("Remove {drive} from this Mac?"),
                Self::note("This Mac forgets the drive's key. The drive's sealed content stays on the server, and without the key nobody can read it, this Mac included. Copy the key first if the drive should ever be opened again."),
                Some("Remove drive"),
                true,
            ),
            Modal::DriveKey { drive } => (
                format!("The key of {drive}"),
                Self::note("A drive is opened by its key. Whoever has the key and can reach the server can read and change everything in the drive. There are no accounts and no read-only access yet. To open the drive on another Mac, copy the key and add it there."),
                Some("Copy key"),
                false,
            ),
            Modal::NewFolder { name } => ("New folder".into(), div().child(Self::text_field(name, "Folder name")), Some("Create"), false),
            Modal::RenameEntry { entry, name } => (format!("Rename {}", entry.name), div().child(Self::text_field(name, "New name")), Some("Rename"), false),
            Modal::DeleteEntry { entry } => (
                format!("Delete {}?", entry.name),
                Self::note(if entry.is_dir { "The folder and everything in it are removed from the drive." } else { "The file and its versions are removed from the drive." }),
                Some("Delete"),
                true,
            ),
            Modal::Versions { entry, versions } => (
                format!("Versions of {}", entry.name),
                div().flex().flex_col().children(match versions {
                    None => vec![div().text_color(rgb(T.muted)).child("Loading\u{2026}").into_any_element()],
                    // Newest first; the first one is what the file holds now.
                    Some(list) => {
                        let current = list.first().map(|v| v.id);
                        list.iter()
                            .map(|v| {
                                let is_current = Some(v.id) == current;
                                let (entry, id) = (entry.clone(), v.id);
                                div()
                                    .h(px(34.))
                                    .flex()
                                    .items_center()
                                    .justify_between()
                                    .gap(px(12.))
                                    .border_b_1()
                                    .border_color(rgb(T.line))
                                    .child(div().flex_1().child(fmt_time(v.mtime_ms, now_ms())))
                                    .child(div().w(px(90.)).text_size(px(12.)).text_color(rgb(T.muted)).child(fmt_bytes(v.size)))
                                    .child(if is_current {
                                        div().w(px(70.)).text_size(px(12.)).text_color(rgb(T.good)).child("Current").into_any_element()
                                    } else {
                                        div()
                                            .id(SharedString::from(format!("restore-{}", v.id)))
                                            .w(px(70.))
                                            .text_size(px(12.))
                                            .text_color(rgb(T.blue))
                                            .cursor_pointer()
                                            .child("Restore")
                                            .on_click(cx.listener(move |this, _, _, cx| this.restore(&entry, id, cx)))
                                            .into_any_element()
                                    })
                                    .into_any_element()
                            })
                            .collect()
                    }
                }),
                None,
                false,
            ),
        };
        let card = div()
            .w(px(420.))
            .rounded(px(16.))
            .bg(rgb(T.bg))
            .border_1()
            .border_color(rgb(T.line))
            .shadow_lg()
            .p(px(22.))
            .flex()
            .flex_col()
            .gap(px(14.))
            .child(div().text_size(px(16.)).font_weight(FontWeight::SEMIBOLD).child(title))
            .child(body)
            .child(
                div()
                    .flex()
                    .justify_end()
                    .gap(px(8.))
                    .pt(px(6.))
                    .child(button("modal-cancel", if confirm.is_some() { "Cancel" } else { "Close" }).on_click(cx.listener(|this, _, _, cx| {
                        this.modal = None;
                        cx.notify();
                    })))
                    .when_some(confirm, |d, label| {
                        let b = primary_button("modal-ok", label).on_click(cx.listener(|this, _, _, cx| this.submit_modal(cx)));
                        d.child(if danger { b.bg(rgb(T.bad)).hover(|s| s.bg(rgb(0xe2574b))) } else { b })
                    }),
            );
        let (id, anim) = Self::fade_in("modal-in");
        div()
            .id("modal-scrim")
            .occlude()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .bg(scrim())
            .flex()
            .items_center()
            .justify_center()
            .child(card.with_animation(id, anim, |el, t| el.opacity(t)))
            .into_any_element()
    }

    // ------------------------------------------------------------ frame pacing

    fn count_build(&mut self, since: Instant) {
        if let Some(perf) = &mut self.perf {
            perf.build += since.elapsed();
        }
    }

    /// `--perf`: scroll the open folder up and down by itself for a few
    /// seconds, record the time between frames, print the spread and quit.
    fn perf_step(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.perf_wanted || self.entries.is_empty() {
            return;
        }
        let now = Instant::now();
        let perf = self.perf.get_or_insert_with(|| Perf { started: now, last: now, intervals_ms: Vec::new(), build: Duration::ZERO });
        let frame = now.duration_since(perf.last).as_secs_f32() * 1000.0;
        perf.last = now;
        let t = now.duration_since(perf.started).as_secs_f32();
        if t > 1.0 {
            perf.intervals_ms.push(frame);
        }
        let handle = self.scroll.0.borrow().base_handle.clone();
        let max = f32::from(handle.max_offset().height);
        if max > 0.0 {
            // A triangle wave over the scroll range at 2400 points a second.
            let travelled = (t * 2400.0) % (2.0 * max);
            let y = if travelled > max { 2.0 * max - travelled } else { travelled };
            handle.set_offset(point(px(0.), px(-y)));
        }
        if t < 7.0 {
            window.request_animation_frame();
            return;
        }
        if max <= 0.0 {
            eprintln!("perf: the folder fits in the window, so nothing scrolled; open a bigger folder (scripts/seed-stress.sh makes one)");
        }
        let build_ms = perf.build.as_secs_f32() * 1000.0 / perf.intervals_ms.len().max(1) as f32;
        let mut v = std::mem::take(&mut perf.intervals_ms);
        v.sort_by(|a, b| a.total_cmp(b));
        let at = |q: f32| v.get(((v.len() as f32 - 1.0) * q) as usize).copied().unwrap_or(0.0);
        let mean = v.iter().sum::<f32>() / v.len().max(1) as f32;
        let slow = v.iter().filter(|ms| **ms > at(0.5) * 1.5).count();
        eprintln!(
            "perf: {} entries, {} view, scroll range {:.0} pt, {} frames in 6 s = {:.1} fps; frame time p50 {:.2} ms, p95 {:.2}, p99 {:.2}, max {:.2}; {} frames over 1.5x the median; {:.2} ms a frame spent building elements",
            self.entries.len(),
            if self.grid { "grid" } else { "list" },
            max,
            v.len(),
            1000.0 / mean,
            at(0.5),
            at(0.95),
            at(0.99),
            at(1.0),
            slow,
            build_ms
        );
        self.perf_wanted = false;
        cx.quit();
    }
}

impl Render for LudyApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.perf_step(window, cx);
        let build_started = Instant::now();
        let (main, left) = match self.screen.clone() {
            Screen::Drives => {
                let mounted = self.view.drives.iter().filter(|d| d.mount.is_some()).count();
                (self.drives_screen(cx).into_any_element(), format!("{}, {} mounted", plural(self.view.drives.len(), "drive"), mounted))
            }
            Screen::Browser { drive, prefix } => {
                let place = match self.local_path(&drive, &prefix) {
                    Some(p) => home_relative(&p),
                    None => "Not mounted".into(),
                };
                (self.browser_screen(&drive, &prefix, window, cx).into_any_element(), place)
            }
        };
        let notice = self.view.notice.clone();
        let root = div()
            .size_full()
            .relative()
            .flex()
            .bg(rgb(T.bg))
            .text_color(rgb(T.text))
            .text_size(px(13.))
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::on_key))
            .child(self.sidebar(cx))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .h_full()
                    .flex()
                    .flex_col()
                    .child(self.toolbar(cx))
                    .child(div().flex_1().min_h(px(0.)).child(main))
                    .when_some(notice, |d, text| {
                        d.child(
                            div()
                                .flex_none()
                                .mx(px(PAD - 8.))
                                .mb(px(10.))
                                .pl(px(14.))
                                .pr(px(6.))
                                .py(px(5.))
                                .rounded(px(10.))
                                .bg(rgb(0xfdf3dc))
                                .border_1()
                                .border_color(rgb(0xf1dba6))
                                .flex()
                                .items_center()
                                .justify_between()
                                .gap(px(12.))
                                .child(div().flex_1().min_w(px(0.)).text_color(rgb(0x6b4e0e)).child(text))
                                .child(icon_button("dismiss-notice", "close").size(px(24.)).on_click(cx.listener(|this, _, _, cx| {
                                    this.engine.clear_notice();
                                    cx.notify();
                                }))),
                        )
                    })
                    .child(self.status_bar(left, cx)),
            )
            .when_some(self.menu.as_ref(), |d, menu| d.child(self.render_menu(menu, cx)))
            .when_some(self.modal.as_ref(), |d, modal| d.child(self.render_modal(modal, cx)));
        self.count_build(build_started);
        root
    }
}
