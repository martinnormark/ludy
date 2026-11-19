//! Drawing the window: a sidebar of drives, a toolbar, the drives or the
//! folder in the middle, and a status bar with the live numbers.
//!
//! Folders are drawn with `uniform_list`, so only the rows on screen are
//! built each frame, however many files a folder holds.

use std::ops::Range;
use std::time::{Duration, Instant};

use gpui::{
    anchored, deferred, div, ease_out_quint, point, prelude::*, px, relative, rgb, uniform_list, Animation, AnimationExt as _, AnyElement, ClickEvent, Context,
    Div, FontWeight, MouseButton, MouseDownEvent, SharedString, Stateful, Window,
};
use ludy_core::engine::{DriveView, MountState};

use super::{now_ms, open_path, Entry, LudyApp, Menu, Modal, Perf, Screen, Target};
use crate::theme::*;

const GRID_ROW_H: f32 = 152.;
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
        let link = self.view.server.as_ref().map(|s| {
            let bw = if s.mbps == 0 { "unlimited".to_string() } else { format!("{} Mbit/s", s.mbps) };
            format!("{bw} \u{b7} {} ms (simulated)", s.latency_ms)
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
                this.screen = Screen::Drives;
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
                    .child(icon_button("new-drive", "plus").size(px(24.)).on_click(cx.listener(|this, _, _, cx| {
                        this.modal = Some(Modal::NewDrive { name: String::new() });
                        cx.notify();
                    }))),
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
                            .child(if self.view.online { "Cloud connected" } else { "Cloud unreachable" }),
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
                    this.screen = Screen::Drives;
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
            this.modal = Some(Modal::Share { drive: share_name.clone(), email: String::new(), edit: false });
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
                .child(div().ml(px(4.)).child(primary_button("browser-share", "Share").on_click(share))),
        )
    }

    // ------------------------------------------------------------ status bar

    /// The live numbers, always on screen: streamed, uploads, cache.
    fn status_bar(&self, left: String, cx: &mut Context<Self>) -> impl IntoElement {
        let v = &self.view;
        let pending: usize = v.drives.iter().filter_map(|d| d.mount.as_ref()).map(|m| m.pending.files + m.pending.ops).sum();
        let uploading = v.drives.iter().filter_map(|d| d.mount.as_ref()).find_map(|m| m.pending.uploading.clone());
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
        let detail = format!(
            "{} \u{b7} {}{}",
            fmt_bytes(d.info.bytes),
            plural(d.info.objects as usize, "item"),
            match (&d.info.forked_from, d.info.shares.len()) {
                (Some(f), _) => format!(" \u{b7} fork of {f}"),
                (None, 0) => String::new(),
                (None, n) => format!(" \u{b7} shared with {n}"),
            }
        );
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
        let total: u64 = v.drives.iter().map(|d| d.info.bytes).sum();
        let subtitle = format!("{} in the cloud across {}. {} of it is on this Mac.", fmt_bytes(total), plural(v.drives.len(), "drive"), fmt_bytes(v.cache.bytes));
        div().size_full().flex().flex_col().child(Self::page_header("Drives".into(), subtitle)).child(if v.drives.is_empty() {
            Self::empty_state(
                drive_glyph(false, 1.0),
                if v.online { "No drives yet" } else { "Waiting for the cloud" },
                if v.online { "Create one with New drive." } else { "Start the server with: cargo run -p ludy-cloud" },
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
        let cache = self.engine.cache();
        let local = cache.version_bytes(&e.version);
        if cache.is_pinned(&e.version) {
            (Some(T.accent), "Kept offline".into())
        } else if e.size > 0 && local >= e.size {
            (Some(T.good), "All of it".into())
        } else if local > 0 {
            (Some(T.blue), fmt_bytes(local))
        } else {
            (None, "\u{2013}".into())
        }
    }

    /// What every row and tile share: hover, open on click, the context menu.
    fn entry_base(&self, e: &Entry, cx: &mut Context<Self>) -> Stateful<Div> {
        let (open, menu) = (e.clone(), e.clone());
        div()
            .id(SharedString::from(format!("entry-{}", e.key)))
            .cursor_pointer()
            .hover(|s| s.bg(rgb(T.hover)))
            .active(|s| s.bg(rgb(T.active)))
            .on_click(cx.listener(move |this, ev: &ClickEvent, window, cx| {
                if open.is_dir || ev.click_count() >= 2 {
                    this.menu_action("Open", Target::Entry(open.clone()), window, cx);
                }
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, ev: &MouseDownEvent, _, cx| {
                    this.menu = Some(Menu { at: ev.position, target: Target::Entry(menu.clone()) });
                    cx.notify();
                }),
            )
    }

    fn grid_row(&self, row: usize, cols: usize, cx: &mut Context<Self>) -> Div {
        let start = row * cols;
        let end = (start + cols).min(self.entries.len());
        let mut line = div().h(px(GRID_ROW_H)).w_full().px(px(ROW_INSET)).flex().gap(px(4.));
        for e in &self.entries[start..end] {
            let (dot, _) = self.local_state(e);
            line = line.child(
                self.entry_base(e, cx)
                    .flex_1()
                    .min_w(px(0.))
                    .h(px(GRID_ROW_H - 6.))
                    .px(px(10.))
                    .pt(px(12.))
                    .rounded(px(12.))
                    .flex()
                    .flex_col()
                    .items_center()
                    .child(div().h(px(74.)).flex().items_center().justify_center().child(if e.is_dir { folder_glyph(1.0) } else { file_glyph(1.0, &extension(&e.name)) }))
                    .child(div().max_w_full().pt(px(8.)).truncate().child(e.name.clone()))
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
        // Keep the last row's tiles the same width as the others.
        for _ in end - start..cols {
            line = line.child(div().flex_1().min_w(px(0.)));
        }
        line
    }

    fn list_columns(name: AnyElement, size: String, modified: String, local: AnyElement) -> [AnyElement; 4] {
        [
            div().flex_1().min_w(px(0.)).child(name).into_any_element(),
            div().w(px(100.)).flex_none().truncate().child(size).into_any_element(),
            div().w(px(120.)).flex_none().truncate().child(modified).into_any_element(),
            div().w(px(120.)).flex_none().child(local).into_any_element(),
        ]
    }

    fn list_header() -> Div {
        div().flex_none().px(px(ROW_INSET)).child(
            div()
                .h(px(28.))
                .px(px(10.))
                .flex()
                .items_center()
                .gap(px(12.))
                .border_b_1()
                .border_color(rgb(T.line))
                .text_size(px(12.))
                .text_color(rgb(T.faint))
                .children(Self::list_columns("Name".into_any_element(), "Size".into(), "Modified".into(), "On this Mac".into_any_element())),
        )
    }

    fn list_row(&self, ix: usize, cx: &mut Context<Self>) -> Div {
        let e = &self.entries[ix];
        let (dot, local) = self.local_state(e);
        let name = div()
            .flex()
            .items_center()
            .gap(px(10.))
            .child(div().w(px(22.)).flex().flex_none().justify_center().child(if e.is_dir { folder_glyph(0.32) } else { file_glyph(0.36, "") }))
            .child(div().min_w(px(0.)).truncate().child(e.name.clone()));
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
                .gap(px(12.))
                .rounded(px(7.))
                .text_color(rgb(T.muted))
                .children(Self::list_columns(div().text_color(rgb(T.text)).child(name).into_any_element(), size, modified, local.into_any_element())),
        )
    }

    fn browser_screen(&self, drive: &str, prefix: &str, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let title = prefix.trim_end_matches('/').rsplit('/').next().filter(|s| !s.is_empty()).unwrap_or(drive).to_string();
        let count = self.entries.len();
        let bytes: u64 = self.entries.iter().map(|e| e.size).sum();
        let subtitle = match (count, self.loading) {
            (0, true) => "Loading\u{2026}".to_string(),
            (_, _) if bytes > 0 => format!("{} \u{b7} {}", plural(count, "item"), fmt_bytes(bytes)),
            _ => plural(count, "item"),
        };
        let body = if count == 0 {
            if self.loading {
                div().into_any_element()
            } else {
                Self::empty_state(folder_glyph(1.0), "This folder is empty", "Import files here, or copy them in with the Finder.").into_any_element()
            }
        } else if self.grid {
            let width = f32::from(window.viewport_size().width) - SIDEBAR_W - 2. * ROW_INSET;
            let cols = ((width / TILE_MIN_W) as usize).max(1);
            uniform_list(
                "grid",
                count.div_ceil(cols),
                cx.processor(move |this, range: Range<usize>, _, cx| {
                    let t0 = Instant::now();
                    let rows = range.map(|row| this.grid_row(row, cols, cx)).collect::<Vec<_>>();
                    this.count_build(t0);
                    rows
                }),
            )
            .track_scroll(self.scroll.clone())
            .size_full()
            .into_any_element()
        } else {
            div()
                .size_full()
                .flex()
                .flex_col()
                .child(Self::list_header())
                .child(
                    div().flex_1().min_h(px(0.)).pt(px(4.)).child(
                        uniform_list(
                            "list",
                            count,
                            cx.processor(|this, range: Range<usize>, _, cx| {
                                let t0 = Instant::now();
                                let rows = range.map(|ix| this.list_row(ix, cx)).collect::<Vec<_>>();
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
        div().size_full().flex().flex_col().child(Self::page_header(title, subtitle)).child(div().flex_1().min_h(px(0.)).child(body))
    }

    // ------------------------------------------------------------ menus

    fn menu_items(&self, target: &Target) -> Vec<(&'static str, bool)> {
        match target {
            Target::Drive(name) => {
                let mounted = self.drive(name).is_some_and(|d| d.mount.is_some());
                let mut v = vec![("Open", false), ("Show in Finder", false), ("Open in Terminal", false), ("Copy path", false), ("Rename", false), ("Share drive", false), ("Fork drive", false)];
                v.push((if mounted { "Eject" } else { "Mount" }, false));
                v.push(("Delete drive", true));
                v
            }
            Target::Entry(e) => {
                let mut v = vec![("Open", false), ("Show in Finder", false), ("Open in Terminal", false), ("Copy path", false), ("Rename", false)];
                if !e.is_dir {
                    v.push((if self.engine.cache().is_pinned(&e.version) { "Remove offline copy" } else { "Keep offline" }, false));
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
            Modal::ForkDrive { drive, name } => (
                format!("Fork {drive}"),
                div()
                    .flex()
                    .flex_col()
                    .gap(px(12.))
                    .child(Self::note("A fork is a new drive that starts with the same files. No data is copied; the two drives diverge as they change."))
                    .child(Self::text_field(name, "Name of the fork")),
                Some("Fork"),
                false,
            ),
            Modal::DeleteDrive { drive } => (
                format!("Delete {drive}?"),
                Self::note("The drive and every version of every file in it are removed from the cloud. This cannot be undone."),
                Some("Delete drive"),
                true,
            ),
            Modal::Share { drive, email, edit } => {
                let shares = self.drive(drive).map(|d| d.info.shares.clone()).unwrap_or_default();
                let role = |id: &'static str, label: &'static str, on: bool, value: bool, cx: &mut Context<Self>| {
                    div()
                        .id(id)
                        .px(px(11.))
                        .h(px(26.))
                        .flex()
                        .items_center()
                        .rounded(px(6.))
                        .cursor_pointer()
                        .when(on, |d| d.bg(rgb(T.surface)).shadow_sm())
                        .when(!on, |d| d.text_color(rgb(T.muted)))
                        .child(label)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if let Some(Modal::Share { edit, .. }) = &mut this.modal {
                                *edit = value;
                            }
                            cx.notify();
                        }))
                };
                let drive_for_remove = drive.clone();
                (
                    format!("Share {drive}"),
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(12.))
                        .child(Self::note("Invite someone by email. They get the whole drive without downloading it. (Simulated: the cloud records the share; no mail is sent.)"))
                        .child(Self::text_field(email, "name@example.com"))
                        .child(div().flex().child(
                            div().p(px(2.)).flex().gap(px(2.)).rounded(px(8.)).bg(rgb(T.hover)).child(role("role-view", "Can view", !*edit, false, cx)).child(role("role-edit", "Can edit", *edit, true, cx)),
                        ))
                        .when(!shares.is_empty(), |d| {
                            d.child(div().pt(px(4.)).text_size(px(12.)).text_color(rgb(T.faint)).child("People with access")).children(shares.into_iter().map(|s| {
                                let (drive, email) = (drive_for_remove.clone(), s.email.clone());
                                div()
                                    .flex()
                                    .items_center()
                                    .justify_between()
                                    .child(div().child(s.email.clone()))
                                    .child(
                                        div()
                                            .flex()
                                            .items_center()
                                            .gap(px(12.))
                                            .text_size(px(12.))
                                            .child(div().text_color(rgb(T.muted)).child(if s.role == "edit" { "Can edit" } else { "Can view" }))
                                            .child(
                                                div()
                                                    .id(SharedString::from(format!("unshare-{}", s.email)))
                                                    .text_color(rgb(T.bad))
                                                    .cursor_pointer()
                                                    .child("Remove")
                                                    .on_click(cx.listener(move |this, _, _, cx| {
                                                        let (client, drive, email) = (this.client(), drive.clone(), email.clone());
                                                        this.run("Remove share", cx, async move { client.unshare_drive(&drive, &email).await });
                                                    })),
                                            ),
                                    )
                            }))
                        }),
                    Some("Send invite"),
                    false,
                )
            }
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
                    Some(list) => {
                        let current = list.last().map(|v| v.id.clone());
                        list.iter()
                            .rev()
                            .map(|v| {
                                let is_current = Some(&v.id) == current.as_ref();
                                let (entry, id) = (entry.clone(), v.id.clone());
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
                                            .on_click(cx.listener(move |this, _, _, cx| this.restore(&entry, id.clone(), cx)))
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
