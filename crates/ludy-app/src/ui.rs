//! The Ludy window: a sidebar, the drives screen, the file browser, and the
//! menus and dialogs that hang off them.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use gpui::{
    anchored, deferred, div, prelude::*, px, rgb, AnyElement, ClipboardItem, Context, Div, FocusHandle, FontWeight, KeyDownEvent,
    MouseButton, MouseDownEvent, PathPromptOptions, Pixels, Point, SharedString, Stateful, Window,
};
use ludy_core::client::{CloudClient, Version};
use ludy_core::engine::{DriveView, Engine, MountState, View};

use crate::theme::*;

/// One row of the in-app browser.
#[derive(Clone, Debug, PartialEq)]
struct Entry {
    name: String,
    key: String,
    is_dir: bool,
    size: u64,
    mtime_ms: u64,
    version: String,
    versions: usize,
    synthetic: bool,
}

#[derive(Clone, Debug, PartialEq)]
enum Screen {
    Drives,
    Browser { drive: String, prefix: String },
}

#[derive(Clone, Debug, PartialEq)]
enum Target {
    Drive(String),
    Entry(Entry),
}

struct Menu {
    at: Point<Pixels>,
    target: Target,
}

enum Modal {
    NewDrive { name: String },
    RenameDrive { drive: String, name: String },
    ForkDrive { drive: String, name: String },
    DeleteDrive { drive: String },
    Share { drive: String, email: String, edit: bool },
    NewFolder { name: String },
    RenameEntry { entry: Entry, name: String },
    DeleteEntry { entry: Entry },
    Versions { entry: Entry, versions: Option<Vec<Version>> },
}

impl Modal {
    fn text_mut(&mut self) -> Option<&mut String> {
        match self {
            Modal::NewDrive { name } | Modal::RenameDrive { name, .. } | Modal::ForkDrive { name, .. } | Modal::NewFolder { name } => Some(name),
            Modal::RenameEntry { name, .. } => Some(name),
            Modal::Share { email, .. } => Some(email),
            Modal::DeleteDrive { .. } | Modal::DeleteEntry { .. } | Modal::Versions { .. } => None,
        }
    }
}

pub struct LudyApp {
    engine: Arc<Engine>,
    view: Arc<View>,
    screen: Screen,
    entries: Vec<Entry>,
    loading: bool,
    /// Drive sequence number the browser listing was loaded at.
    loaded_seq: u64,
    grid: bool,
    menu: Option<Menu>,
    modal: Option<Modal>,
    focus: FocusHandle,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

fn open_path(args: &[&str], path: &std::path::Path) {
    let _ = std::process::Command::new("/usr/bin/open").args(args).arg(path).spawn();
}

impl LudyApp {
    pub fn new(engine: Arc<Engine>, open: Option<String>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus = cx.focus_handle();
        window.focus(&focus);
        // Poll the engine's status a few times a second; repaint on change.
        cx.spawn(async move |this, cx| loop {
            cx.background_executor().timer(Duration::from_millis(400)).await;
            if this.update(cx, |this, cx| this.tick(cx)).is_err() {
                break;
            }
        })
        .detach();
        let view = engine.view();
        let mut app = LudyApp {
            engine,
            view,
            screen: Screen::Drives,
            entries: Vec::new(),
            loading: false,
            loaded_seq: 0,
            grid: true,
            menu: None,
            modal: None,
            focus,
        };
        if let Some(target) = open {
            let (drive, folder) = target.split_once('/').unwrap_or((&target, ""));
            let prefix = if folder.is_empty() { String::new() } else { format!("{}/", folder.trim_end_matches('/')) };
            app.screen = Screen::Browser { drive: drive.to_string(), prefix };
            app.reload(cx);
        }
        app
    }

    fn tick(&mut self, cx: &mut Context<Self>) {
        let view = self.engine.view();
        if *view != *self.view {
            self.view = view;
            cx.notify();
        }
        // A commit on the open drive (from anywhere) reloads the listing.
        if let Screen::Browser { drive, .. } = &self.screen {
            let seq = self.drive(drive).map(|d| d.info.seq).unwrap_or(0);
            if seq != self.loaded_seq && !self.loading {
                self.reload(cx);
            }
        }
    }

    fn drive(&self, name: &str) -> Option<&DriveView> {
        self.view.drives.iter().find(|d| d.info.name == name)
    }

    fn client(&self) -> CloudClient {
        self.engine.client()
    }

    /// The path of a drive (or something in it) in the Finder, when mounted.
    fn local_path(&self, drive: &str, key: &str) -> Option<PathBuf> {
        let mount = self.drive(drive)?.mount.as_ref()?;
        Some(if key.is_empty() { mount.mountpoint.clone() } else { mount.mountpoint.join(key.trim_end_matches('/')) })
    }

    fn open_drive(&mut self, drive: &str, cx: &mut Context<Self>) {
        self.screen = Screen::Browser { drive: drive.to_string(), prefix: String::new() };
        self.entries.clear();
        self.reload(cx);
    }

    fn navigate(&mut self, prefix: String, cx: &mut Context<Self>) {
        if let Screen::Browser { prefix: p, .. } = &mut self.screen {
            *p = prefix;
            self.entries.clear();
            self.reload(cx);
        }
    }

    /// Load the current folder from the cloud.
    fn reload(&mut self, cx: &mut Context<Self>) {
        let Screen::Browser { drive, prefix } = self.screen.clone() else { return };
        self.loading = true;
        let seq = self.drive(&drive).map(|d| d.info.seq).unwrap_or(0);
        let client = self.client();
        let (d, p) = (drive.clone(), prefix.clone());
        let task = self.engine.spawn(async move { client.list_folder(&d, &p).await });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                if this.screen != (Screen::Browser { drive, prefix: prefix.clone() }) {
                    return;
                }
                this.loading = false;
                match result {
                    Ok(Ok(listing)) => {
                        let mut entries: Vec<Entry> = listing
                            .prefixes
                            .iter()
                            .map(|p| Entry {
                                name: p[prefix.len()..].trim_end_matches('/').to_string(),
                                key: p.clone(),
                                is_dir: true,
                                size: 0,
                                mtime_ms: 0,
                                version: String::new(),
                                versions: 0,
                                synthetic: false,
                            })
                            .collect();
                        entries.extend(listing.objects.iter().filter(|o| o.key != prefix).map(|o| Entry {
                            name: o.key[prefix.len()..].to_string(),
                            key: o.key.clone(),
                            is_dir: false,
                            size: o.size,
                            mtime_ms: o.mtime_ms,
                            version: o.version.clone(),
                            versions: o.versions,
                            synthetic: o.synthetic,
                        }));
                        this.entries = entries;
                        this.loaded_seq = listing.seq.max(seq);
                    }
                    Ok(Err(e)) => this.engine.set_notice(format!("Could not list folder: {e:#}")),
                    Err(e) => this.engine.set_notice(format!("Could not list folder: {e}")),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Run a cloud operation, then refresh whatever is on screen.
    fn run<F>(&mut self, what: &'static str, cx: &mut Context<Self>, op: F)
    where
        F: std::future::Future<Output = anyhow::Result<()>> + Send + 'static,
    {
        let task = self.engine.spawn(op);
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => this.engine.set_notice(format!("{what} failed: {e:#}")),
                    Err(e) => this.engine.set_notice(format!("{what} failed: {e}")),
                }
                this.engine.refresh_drives();
                this.reload(cx);
                cx.notify();
            });
        })
        .detach();
    }

    // ------------------------------------------------------------ actions

    fn show_in_finder(&mut self, drive: &str, key: &str) {
        match self.local_path(drive, key) {
            Some(path) if key.is_empty() => open_path(&[], &path),
            Some(path) => open_path(&["-R"], &path),
            None => self.engine.set_notice(format!("Mount {drive} to see it in the Finder.")),
        }
    }

    fn menu_action(&mut self, action: &'static str, target: Target, window: &mut Window, cx: &mut Context<Self>) {
        self.menu = None;
        let current_drive = match &self.screen {
            Screen::Browser { drive, .. } => drive.clone(),
            Screen::Drives => String::new(),
        };
        match (action, target) {
            ("Mount", Target::Drive(d)) => self.engine.mount(&d),
            ("Eject", Target::Drive(d)) => self.engine.eject(&d),
            ("Open", Target::Drive(d)) => self.open_drive(&d, cx),
            ("Show in Finder", Target::Drive(d)) => self.show_in_finder(&d, ""),
            ("Open in Terminal", Target::Drive(d)) => match self.local_path(&d, "") {
                Some(path) => open_path(&["-a", "Terminal"], &path),
                None => self.engine.set_notice(format!("Mount {d} to open it in Terminal.")),
            },
            ("Copy path", Target::Drive(d)) => {
                let path = self.engine.mountpoint(&d);
                cx.write_to_clipboard(ClipboardItem::new_string(path.display().to_string()));
            }
            ("Rename", Target::Drive(d)) => self.modal = Some(Modal::RenameDrive { name: d.clone(), drive: d }),
            ("Share drive", Target::Drive(d)) => self.modal = Some(Modal::Share { drive: d, email: String::new(), edit: false }),
            ("Fork drive", Target::Drive(d)) => self.modal = Some(Modal::ForkDrive { name: format!("{d}-fork"), drive: d }),
            ("Delete drive", Target::Drive(d)) => self.modal = Some(Modal::DeleteDrive { drive: d }),

            ("Open", Target::Entry(e)) if e.is_dir => self.navigate(e.key, cx),
            ("Open", Target::Entry(e)) => match self.local_path(&current_drive, &e.key) {
                Some(path) => open_path(&[], &path),
                None => self.engine.set_notice(format!("Mount {current_drive} to open files in other apps.")),
            },
            ("Show in Finder", Target::Entry(e)) => self.show_in_finder(&current_drive, &e.key),
            ("Open in Terminal", Target::Entry(e)) => {
                let key = if e.is_dir { e.key.clone() } else { e.key.rsplit_once('/').map(|(d, _)| d.to_string()).unwrap_or_default() };
                match self.local_path(&current_drive, &key) {
                    Some(path) => open_path(&["-a", "Terminal"], &path),
                    None => self.engine.set_notice(format!("Mount {current_drive} to open it in Terminal.")),
                }
            }
            ("Copy path", Target::Entry(e)) => {
                let path = self.engine.mountpoint(&current_drive).join(e.key.trim_end_matches('/'));
                cx.write_to_clipboard(ClipboardItem::new_string(path.display().to_string()));
            }
            ("Rename", Target::Entry(e)) => self.modal = Some(Modal::RenameEntry { name: e.name.clone(), entry: e }),
            ("Delete", Target::Entry(e)) => self.modal = Some(Modal::DeleteEntry { entry: e }),
            ("Versions", Target::Entry(e)) => {
                let (client, drive, key) = (self.client(), current_drive.clone(), e.key.clone());
                self.modal = Some(Modal::Versions { entry: e, versions: None });
                let task = self.engine.spawn(async move { client.versions(&drive, &key).await });
                cx.spawn(async move |this, cx| {
                    let result = task.await;
                    let _ = this.update(cx, |this, cx| {
                        if let (Some(Modal::Versions { versions, .. }), Ok(Ok(list))) = (&mut this.modal, result) {
                            *versions = Some(list);
                        }
                        cx.notify();
                    });
                })
                .detach();
            }
            ("Keep offline", Target::Entry(e)) | ("Remove offline copy", Target::Entry(e)) => self.toggle_pin(&current_drive, &e, cx),
            _ => {}
        }
        window.focus(&self.focus);
        cx.notify();
    }

    /// Pin: download every block of the file and exempt it from eviction.
    fn toggle_pin(&mut self, drive: &str, e: &Entry, cx: &mut Context<Self>) {
        let cache = self.engine.cache();
        let version: Arc<str> = e.version.as_str().into();
        if cache.is_pinned(&version) {
            cache.set_pinned(&version, false);
            return;
        }
        cache.set_pinned(&version, true);
        let (client, drive, key, size) = (self.client(), drive.to_string(), e.key.clone(), e.size);
        self.run("Keep offline", cx, async move {
            use ludy_core::cache::BLOCK_SIZE;
            let mut idx = 0;
            while idx * BLOCK_SIZE < size {
                if !cache.contains(&version, idx) {
                    let start = idx * BLOCK_SIZE;
                    let data = client.get_range(&drive, &key, &version, start, BLOCK_SIZE.min(size - start)).await?;
                    cache.put(&version, idx, &data)?;
                }
                idx += 1;
            }
            Ok(())
        });
    }

    fn import(&mut self, cx: &mut Context<Self>) {
        let Screen::Browser { drive, prefix } = self.screen.clone() else { return };
        let picked = cx.prompt_for_paths(PathPromptOptions { files: true, directories: false, multiple: true, prompt: Some("Import".into()) });
        let client = self.client();
        let engine = self.engine.clone();
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = picked.await else { return };
            let task = engine.spawn(async move {
                for path in paths {
                    let Some(name) = path.file_name().and_then(|n| n.to_str()) else { continue };
                    client.put_file(&drive, &format!("{prefix}{name}"), &path).await?;
                }
                anyhow::Ok(())
            });
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                if let Ok(Err(e)) = result {
                    this.engine.set_notice(format!("Import failed: {e:#}"));
                }
                this.engine.refresh_drives();
                this.reload(cx);
            });
        })
        .detach();
    }

    fn submit_modal(&mut self, cx: &mut Context<Self>) {
        let Some(modal) = self.modal.take() else { return };
        let client = self.client();
        let current = match &self.screen {
            Screen::Browser { drive, prefix } => Some((drive.clone(), prefix.clone())),
            Screen::Drives => None,
        };
        match modal {
            Modal::NewDrive { name } if !name.trim().is_empty() => {
                let name = name.trim().to_string();
                self.run("Create drive", cx, async move { client.create_drive(&name).await });
            }
            Modal::RenameDrive { drive, name } if !name.trim().is_empty() && name.trim() != drive => {
                if self.drive(&drive).is_some_and(|d| d.mount.is_some()) {
                    self.engine.set_notice(format!("Eject {drive} before renaming it."));
                } else {
                    let name = name.trim().to_string();
                    self.run("Rename drive", cx, async move { client.rename_drive(&drive, &name).await });
                }
            }
            Modal::ForkDrive { drive, name } if !name.trim().is_empty() => {
                let name = name.trim().to_string();
                self.run("Fork drive", cx, async move { client.fork_drive(&drive, &name).await });
            }
            Modal::DeleteDrive { drive } => {
                if self.drive(&drive).is_some_and(|d| d.mount.is_some()) {
                    self.engine.set_notice(format!("Eject {drive} before deleting it."));
                } else {
                    if self.screen == (Screen::Browser { drive: drive.clone(), prefix: String::new() }) || current.as_ref().is_some_and(|(d, _)| *d == drive) {
                        self.screen = Screen::Drives;
                    }
                    self.run("Delete drive", cx, async move { client.delete_drive(&drive).await });
                }
            }
            Modal::Share { drive, email, edit } if email.contains('@') => {
                let role = if edit { "edit" } else { "view" };
                self.run("Share", cx, async move { client.share_drive(&drive, email.trim(), role).await });
            }
            Modal::NewFolder { name } if !name.trim().is_empty() => {
                if let Some((drive, prefix)) = current {
                    let key = format!("{prefix}{}/", name.trim());
                    self.run("New folder", cx, async move { client.put_bytes(&drive, &key, Vec::new().into()).await.map(|_| ()) });
                }
            }
            Modal::RenameEntry { entry, name } if !name.trim().is_empty() && name.trim() != entry.name => {
                if let Some((drive, prefix)) = current {
                    let to = format!("{prefix}{}", name.trim());
                    let from = entry.key.trim_end_matches('/').to_string();
                    self.run("Rename", cx, async move { client.rename(&drive, &from, &to).await.map(|_| ()) });
                }
            }
            Modal::DeleteEntry { entry } => {
                if let Some((drive, _)) = current {
                    self.run("Delete", cx, async move { client.delete(&drive, &entry.key).await });
                }
            }
            _ => {}
        }
        cx.notify();
    }

    fn restore(&mut self, entry: &Entry, version: String, cx: &mut Context<Self>) {
        let Screen::Browser { drive, .. } = self.screen.clone() else { return };
        let (client, key) = (self.client(), entry.key.clone());
        self.modal = None;
        self.run("Restore", cx, async move { client.restore(&drive, &key, &version).await.map(|_| ()) });
    }

    fn on_key(&mut self, ev: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let ks = &ev.keystroke;
        if ks.key == "escape" {
            if self.menu.take().is_some() || self.modal.take().is_some() {
                cx.notify();
            }
            return;
        }
        if self.modal.is_none() {
            return;
        }
        if ks.key == "enter" {
            self.submit_modal(cx);
            return;
        }
        let pasted = (ks.modifiers.platform && ks.key == "v").then(|| cx.read_from_clipboard().and_then(|i| i.text())).flatten();
        let Some(text) = self.modal.as_mut().and_then(Modal::text_mut) else { return };
        if ks.key == "backspace" {
            text.pop();
        } else if let Some(p) = pasted {
            text.push_str(p.lines().next().unwrap_or(""));
        } else if !ks.modifiers.platform && !ks.modifiers.control {
            match &ks.key_char {
                Some(c) if !c.chars().any(char::is_control) => text.push_str(c),
                _ => return,
            }
        } else {
            return;
        }
        cx.notify();
    }

    // ------------------------------------------------------------ rendering

    fn sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let on_drives = self.screen == Screen::Drives;
        let nav = |id: SharedString, label: String, active: bool, lit: Option<bool>| {
            div()
                .id(id)
                .h(px(30.))
                .px_2()
                .flex()
                .items_center()
                .gap_2()
                .rounded_md()
                .text_sm()
                .text_color(rgb(if active { TEXT } else { MUTED }))
                .when(active, |d| d.bg(rgb(PANEL_HI)))
                .hover(|s| s.bg(rgb(PANEL)))
                .cursor_pointer()
                .when_some(lit, |d, lit| d.child(div().size(px(6.)).rounded_full().bg(rgb(if lit { GOOD } else { FAINT }))))
                .child(div().truncate().child(label))
        };
        div()
            .w(px(210.))
            .h_full()
            .flex_none()
            .bg(rgb(SIDEBAR))
            .border_r_1()
            .border_color(rgb(BORDER))
            .flex()
            .flex_col()
            .p_3()
            .gap_1()
            .child(
                // room for the traffic lights
                div().h(px(34.)),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .pb_3()
                    .child(div().size(px(10.)).rounded_full().bg(rgb(ACCENT)))
                    .child(div().text_xl().font_weight(FontWeight::BOLD).text_color(rgb(TEXT)).child("ludy")),
            )
            .child(nav("nav-drives".into(), "All drives".into(), on_drives, None).on_click(cx.listener(|this, _, _, cx| {
                this.screen = Screen::Drives;
                cx.notify();
            })))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .px_2()
                    .pt_4()
                    .pb_1()
                    .child(div().text_xs().text_color(rgb(FAINT)).child("DRIVES"))
                    .child(icon_button("new-drive", "+").on_click(cx.listener(|this, _, _, cx| {
                        this.modal = Some(Modal::NewDrive { name: String::new() });
                        cx.notify();
                    }))),
            )
            .children(self.view.drives.iter().map(|d| {
                let name = d.info.name.clone();
                let active = matches!(&self.screen, Screen::Browser { drive, .. } if *drive == name);
                nav(SharedString::from(format!("nav-{name}")), name.clone(), active, Some(d.mount.is_some()))
                    .on_click(cx.listener(move |this, _, _, cx| this.open_drive(&name, cx)))
            }))
            .child(div().flex_1())
            .child(
                div()
                    .px_2()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .text_xs()
                            .text_color(rgb(MUTED))
                            .child(div().size(px(6.)).rounded_full().bg(rgb(if self.view.online { GOOD } else { BAD })))
                            .child(if self.view.online { "Cloud connected" } else { "Cloud unreachable" }),
                    )
                    .child(div().text_xs().text_color(rgb(FAINT)).truncate().child(self.view.server_url.clone())),
            )
    }

    fn stat(label: &'static str, value: String, sub: String) -> Div {
        div()
            .flex_1()
            .min_w(px(120.))
            .flex()
            .flex_col()
            .gap_1()
            .child(div().text_xs().text_color(rgb(FAINT)).child(label))
            .child(div().text_lg().font_weight(FontWeight::SEMIBOLD).text_color(rgb(TEXT)).child(value))
            .child(div().text_xs().text_color(rgb(MUTED)).child(sub))
    }

    /// Storage total plus the live numbers: streamed, rate, cache, uploads.
    fn status_panel(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let v = &self.view;
        let total: u64 = v.drives.iter().map(|d| d.info.bytes).sum();
        let mounted = v.drives.iter().filter(|d| d.mount.is_some()).count();
        let pending_files: usize = v.drives.iter().filter_map(|d| d.mount.as_ref()).map(|m| m.pending.files + m.pending.ops).sum();
        let pending_bytes: u64 = v.drives.iter().filter_map(|d| d.mount.as_ref()).map(|m| m.pending.bytes).sum();
        let uploading = v.drives.iter().filter_map(|d| d.mount.as_ref()).find_map(|m| m.pending.uploading.clone());
        let fill = if v.cache.cap > 0 { (v.cache.bytes as f32 / v.cache.cap as f32).min(1.0) } else { 0.0 };
        let link = v.server.as_ref().map(|s| {
            let bw = if s.mbps == 0 { "unlimited".to_string() } else { format!("{} Mbit/s", s.mbps) };
            format!("simulated link: {bw}, {} ms", s.latency_ms)
        });
        div()
            .rounded_xl()
            .bg(rgb(PANEL))
            .border_1()
            .border_color(rgb(BORDER))
            .p_5()
            .flex()
            .flex_col()
            .gap_4()
            .child(
                div()
                    .flex()
                    .items_end()
                    .justify_between()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(div().text_xs().text_color(rgb(FAINT)).child("IN THE CLOUD"))
                            .child(
                                div()
                                    .flex()
                                    .items_end()
                                    .gap_2()
                                    .child(div().text_3xl().font_weight(FontWeight::BOLD).text_color(rgb(TEXT)).child(fmt_bytes(total)))
                                    .child(div().pb_1().text_sm().text_color(rgb(MUTED)).child(format!(
                                        "across {} drive{}, {} mounted",
                                        v.drives.len(),
                                        if v.drives.len() == 1 { "" } else { "s" },
                                        mounted
                                    ))),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .items_end()
                            .gap_1()
                            .child(div().text_xs().text_color(rgb(FAINT)).child("ON THIS MAC"))
                            .child(div().text_lg().font_weight(FontWeight::SEMIBOLD).text_color(rgb(ACCENT)).child(fmt_bytes(v.cache.bytes))),
                    ),
            )
            .child(div().h(px(1.)).bg(rgb(BORDER)))
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap_4()
                    .child(Self::stat("STREAMING", fmt_rate(v.stats.down_rate), format!("{} streamed in {} ranges", fmt_bytes(v.stats.bytes_down), v.stats.range_requests)))
                    .child(Self::stat(
                        "UPLOADING",
                        if pending_files > 0 || v.stats.up_rate >= 1.0 { fmt_rate(v.stats.up_rate) } else { "up to date".into() },
                        match (&uploading, pending_files) {
                            (Some(name), _) => format!("{name} ({} waiting)", fmt_bytes(pending_bytes)),
                            (None, 0) => format!("{} uploaded", fmt_bytes(v.stats.bytes_up)),
                            (None, n) => format!("{n} change{} waiting", if n == 1 { "" } else { "s" }),
                        },
                    ))
                    .child(Self::stat("SERVED FROM CACHE", fmt_bytes(v.stats.cache_hit_bytes), format!("of {} read by apps", fmt_bytes(v.stats.bytes_read))))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(180.))
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(
                                div()
                                    .flex()
                                    .justify_between()
                                    .child(div().text_xs().text_color(rgb(FAINT)).child("CACHE"))
                                    .child(
                                        div()
                                            .id("clear-cache")
                                            .text_xs()
                                            .text_color(rgb(MUTED))
                                            .cursor_pointer()
                                            .hover(|s| s.text_color(rgb(TEXT)))
                                            .child("Clear")
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.engine.cache().clear();
                                                cx.notify();
                                            })),
                                    ),
                            )
                            .child(
                                div().text_lg().font_weight(FontWeight::SEMIBOLD).text_color(rgb(TEXT)).child(format!("{} of {}", fmt_bytes(v.cache.bytes), fmt_bytes(v.cache.cap))),
                            )
                            .child(
                                div()
                                    .h(px(6.))
                                    .w_full()
                                    .rounded_full()
                                    .bg(rgb(PANEL_HI))
                                    .child(div().h_full().w(gpui::relative(fill.max(0.004))).rounded_full().bg(rgb(ACCENT))),
                            )
                            .child(div().text_xs().text_color(rgb(MUTED)).child(format!(
                                "{} blocks, {} evicted{}",
                                v.cache.blocks,
                                fmt_bytes(v.cache.evicted_bytes),
                                if v.cache.pinned_bytes > 0 { format!(", {} kept offline", fmt_bytes(v.cache.pinned_bytes)) } else { String::new() }
                            ))),
                    ),
            )
            .when_some(link, |d, link| d.child(div().text_xs().text_color(rgb(FAINT)).child(link)))
    }

    /// A click handler that runs a drive action. Built by hand (rather than
    /// with `cx.listener`) so it can be returned from a function.
    fn drive_action(label: &'static str, name: &str, cx: &mut Context<Self>) -> impl Fn(&gpui::ClickEvent, &mut Window, &mut gpui::App) + 'static {
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
        let subtitle = match (&d.busy, &d.mount) {
            (Some(b), _) => b.clone(),
            (None, Some(m)) if stuck => format!("{} (eject did not finish)", m.mountpoint.display()),
            (None, Some(m)) => m.mountpoint.display().to_string().replacen(&std::env::var("HOME").unwrap_or_default(), "~", 1),
            (None, None) => "Not mounted".into(),
        };
        let detail = format!(
            "{}, {} item{}{}",
            fmt_bytes(d.info.bytes),
            d.info.objects,
            if d.info.objects == 1 { "" } else { "s" },
            match (&d.info.forked_from, d.info.shares.len()) {
                (Some(f), _) => format!(", fork of {f}"),
                (None, 0) => String::new(),
                (None, n) => format!(", shared with {n}"),
            }
        );
        let act = Self::drive_action;
        let menu_name = name.clone();
        let more_name = name.clone();
        div()
            .id(SharedString::from(format!("card-{name}")))
            .w(px(250.))
            .rounded_xl()
            .bg(rgb(PANEL))
            .border_1()
            .border_color(rgb(if mounted { 0x3d4250 } else { BORDER }))
            .when(mounted, |d| d.shadow_lg())
            .p_4()
            .flex()
            .flex_col()
            .items_center()
            .gap_3()
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
                    .flex()
                    .justify_between()
                    .child(
                        icon_button(format!("more-{name}"), "\u{22ef}").on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, ev: &MouseDownEvent, _, cx| {
                                this.menu = Some(Menu { at: ev.position, target: Target::Drive(more_name.clone()) });
                                cx.stop_propagation();
                                cx.notify();
                            }),
                        ),
                    )
                    .child(if mounted {
                        icon_button(format!("eject-{name}"), "\u{23cf}").on_click(act("Eject", &name, cx)).into_any_element()
                    } else {
                        div().size(px(28.)).into_any_element()
                    }),
            )
            .child(drive_glyph(mounted, 1.25))
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap_1()
                    .child(div().max_w_full().truncate().text_lg().font_weight(FontWeight::SEMIBOLD).text_color(rgb(TEXT)).child(name.clone()))
                    .child(div().max_w_full().truncate().text_xs().text_color(rgb(if stuck { BAD } else if mounted { GOOD } else { FAINT })).child(subtitle))
                    .child(div().max_w_full().truncate().text_xs().text_color(rgb(MUTED)).child(detail)),
            )
            .child(div().flex().gap_2().pt_1().children(if d.busy.is_some() {
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

    fn drives_screen(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("drives-screen")
            .size_full()
            .overflow_y_scroll()
            .p_8()
            .flex()
            .flex_col()
            .gap_6()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(div().text_2xl().font_weight(FontWeight::BOLD).text_color(rgb(TEXT)).child("Drives"))
                    .child(div().text_sm().text_color(rgb(MUTED)).child("Mount a drive and it shows up in the Finder. Files stay in the cloud and stream as apps read them.")),
            )
            .child(self.status_panel(cx))
            .child(if self.view.drives.is_empty() {
                div()
                    .py_8()
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap_3()
                    .child(drive_glyph(false, 1.0))
                    .child(div().text_sm().text_color(rgb(MUTED)).child(if self.view.online {
                        "No drives yet. Create one with the + in the sidebar."
                    } else {
                        "Waiting for the cloud server. Start it with: cargo run -p ludy-cloud"
                    }))
                    .into_any_element()
            } else {
                div().flex().flex_wrap().gap_4().children(self.view.drives.iter().map(|d| self.drive_card(d, cx))).into_any_element()
            })
    }

    fn entry_tile(&self, e: &Entry, cx: &mut Context<Self>) -> Stateful<Div> {
        let cache = self.engine.cache();
        let local = if e.is_dir { 0 } else { cache.version_bytes(&e.version) };
        let pinned = !e.is_dir && cache.is_pinned(&e.version);
        let tint = if pinned { ACCENT } else if local >= e.size && e.size > 0 { GOOD } else if local > 0 { BLUE } else { 0x3a404c };
        let kind = if e.is_dir {
            "Folder".to_string()
        } else if local == 0 {
            format!("{}, in the cloud", fmt_bytes(e.size))
        } else {
            format!("{}, {} here", fmt_bytes(e.size), fmt_bytes(local))
        };
        let (open, menu) = (e.clone(), e.clone());
        let base = div()
            .id(SharedString::from(format!("entry-{}", e.key)))
            .rounded_lg()
            .cursor_pointer()
            .hover(|s| s.bg(rgb(PANEL)))
            .on_click(cx.listener(move |this, ev: &gpui::ClickEvent, window, cx| {
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
            );
        if self.grid {
            base.w(px(168.))
                .p_3()
                .flex()
                .flex_col()
                .items_center()
                .gap_2()
                .child(div().h(px(60.)).flex().items_center().child(if e.is_dir { folder_glyph(1.0) } else { file_glyph(1.0, tint) }))
                .child(div().max_w_full().truncate().text_sm().text_color(rgb(TEXT)).child(e.name.clone()))
                .child(div().max_w_full().truncate().text_xs().text_color(rgb(MUTED)).child(kind))
        } else {
            base.w_full()
                .h(px(38.))
                .px_3()
                .flex()
                .items_center()
                .gap_3()
                .child(div().w(px(24.)).flex().justify_center().child(if e.is_dir { folder_glyph(0.4) } else { file_glyph(0.45, tint) }))
                .child(div().flex_1().truncate().text_sm().text_color(rgb(TEXT)).child(e.name.clone()))
                .child(div().w(px(230.)).truncate().text_xs().text_color(rgb(MUTED)).child(kind))
                .child(div().w(px(110.)).text_xs().text_color(rgb(FAINT)).child(if e.is_dir { String::new() } else { fmt_time(e.mtime_ms, now_ms()) }))
        }
    }

    fn browser_screen(&self, drive: &str, prefix: &str, cx: &mut Context<Self>) -> impl IntoElement {
        let dv = self.drive(drive);
        let mounted = dv.is_some_and(|d| d.mount.is_some());
        // Breadcrumb: drive / folder / folder
        let mut crumbs: Vec<(String, String)> = vec![(drive.to_string(), String::new())];
        let mut acc = String::new();
        for part in prefix.split('/').filter(|p| !p.is_empty()) {
            acc.push_str(part);
            acc.push('/');
            crumbs.push((part.to_string(), acc.clone()));
        }
        let last = crumbs.len() - 1;
        let drive_name = drive.to_string();
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(
                div()
                    .px_8()
                    .pt_8()
                    .pb_4()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_4()
                    .child(div().flex().items_center().gap_1().overflow_hidden().children(crumbs.into_iter().enumerate().map(|(i, (label, target))| {
                        div()
                            .flex()
                            .items_center()
                            .gap_1()
                            .when(i > 0, |d| d.child(div().text_color(rgb(FAINT)).child("/")))
                            .child(
                                div()
                                    .id(SharedString::from(format!("crumb-{i}")))
                                    .px_1()
                                    .rounded_md()
                                    .text_xl()
                                    .font_weight(FontWeight::BOLD)
                                    .text_color(rgb(if i == last { TEXT } else { MUTED }))
                                    .cursor_pointer()
                                    .hover(|s| s.text_color(rgb(TEXT)))
                                    .child(label)
                                    .on_click(cx.listener(move |this, _, _, cx| this.navigate(target.clone(), cx))),
                            )
                    })))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(button("import", "Import\u{2026}").on_click(cx.listener(|this, _, _, cx| this.import(cx))))
                            .child(button("new-folder", "New folder").on_click(cx.listener(|this, _, _, cx| {
                                this.modal = Some(Modal::NewFolder { name: String::new() });
                                cx.notify();
                            })))
                            .child(icon_button("refresh", "\u{21bb}").on_click(cx.listener(|this, _, _, cx| this.reload(cx))))
                            .child(
                                icon_button("toggle-view", if self.grid { "\u{2630}" } else { "\u{25a6}" }).on_click(cx.listener(|this, _, _, cx| {
                                    this.grid = !this.grid;
                                    cx.notify();
                                })),
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
                                primary_button("browser-mount", "Mount").on_click(cx.listener(move |this, _, _, _| this.engine.mount(&drive_name))).into_any_element()
                            }),
                    ),
            )
            .child(
                div()
                    .id("entries")
                    .flex_1()
                    .overflow_y_scroll()
                    .px_8()
                    .pb_8()
                    .child(if self.entries.is_empty() {
                        div()
                            .py_8()
                            .flex()
                            .justify_center()
                            .text_sm()
                            .text_color(rgb(MUTED))
                            .child(if self.loading { "Loading\u{2026}" } else { "This folder is empty. Import files, or copy them here in the Finder." })
                            .into_any_element()
                    } else if self.grid {
                        div().flex().flex_wrap().gap_2().children(self.entries.iter().map(|e| self.entry_tile(e, cx))).into_any_element()
                    } else {
                        div().flex().flex_col().children(self.entries.iter().map(|e| self.entry_tile(e, cx))).into_any_element()
                    }),
            )
    }

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

    fn render_menu(&self, menu: &Menu, cx: &mut Context<Self>) -> AnyElement {
        let items = self.menu_items(&menu.target);
        let target = menu.target.clone();
        deferred(
            anchored().position(menu.at).snap_to_window_with_margin(px(8.)).child(
                div()
                    .id("context-menu")
                    .occlude()
                    .w(px(210.))
                    .p_1()
                    .rounded_lg()
                    .bg(rgb(PANEL_HI))
                    .border_1()
                    .border_color(rgb(BORDER))
                    .shadow_lg()
                    .flex()
                    .flex_col()
                    .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                        this.menu = None;
                        cx.notify();
                    }))
                    .children(items.into_iter().map(|(label, danger)| {
                        let target = target.clone();
                        div()
                            .id(SharedString::from(format!("menu-{label}")))
                            .h(px(28.))
                            .px_3()
                            .flex()
                            .items_center()
                            .rounded_md()
                            .text_sm()
                            .text_color(rgb(if danger { BAD } else { TEXT }))
                            .cursor_pointer()
                            .hover(|s| s.bg(rgb(0x2c303a)))
                            .child(label)
                            .on_click(cx.listener(move |this, _, window, cx| this.menu_action(label, target.clone(), window, cx)))
                    })),
            ),
        )
        .with_priority(1)
        .into_any_element()
    }

    fn text_field(value: &str, placeholder: &'static str) -> Div {
        div()
            .h(px(36.))
            .px_3()
            .flex()
            .items_center()
            .rounded_md()
            .bg(rgb(BG))
            .border_1()
            .border_color(rgb(ACCENT))
            .text_sm()
            .child(if value.is_empty() {
                div().text_color(rgb(FAINT)).child(placeholder)
            } else {
                div().text_color(rgb(TEXT)).child(value.to_string())
            })
            .child(div().w(px(1.5)).h(px(16.)).ml(px(1.)).bg(rgb(ACCENT)))
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
                    .gap_3()
                    .child(div().text_sm().text_color(rgb(MUTED)).child("A fork is a new drive that starts with the same files. No data is copied; the two drives diverge as they change."))
                    .child(Self::text_field(name, "Name of the fork")),
                Some("Fork"),
                false,
            ),
            Modal::DeleteDrive { drive } => (
                format!("Delete {drive}?"),
                div().text_sm().text_color(rgb(MUTED)).child("The drive and every version of every file in it are removed from the cloud. This cannot be undone."),
                Some("Delete drive"),
                true,
            ),
            Modal::Share { drive, email, edit } => {
                let shares = self.drive(drive).map(|d| d.info.shares.clone()).unwrap_or_default();
                let role = |id: &'static str, label: &'static str, on: bool, value: bool, cx: &mut Context<Self>| {
                    div()
                        .id(id)
                        .px_3()
                        .h(px(30.))
                        .flex()
                        .items_center()
                        .rounded_md()
                        .text_sm()
                        .cursor_pointer()
                        .when(on, |d| d.bg(rgb(ACCENT)).text_color(rgb(ON_ACCENT)))
                        .when(!on, |d| d.text_color(rgb(MUTED)).hover(|s| s.bg(rgb(PANEL_HI))))
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
                        .gap_3()
                        .child(div().text_sm().text_color(rgb(MUTED)).child("Invite someone by email. They get the whole drive without downloading it. (Simulated: the cloud records the share; no mail is sent.)"))
                        .child(Self::text_field(email, "name@example.com"))
                        .child(div().flex().gap_1().child(role("role-view", "Can view", !*edit, false, cx)).child(role("role-edit", "Can edit", *edit, true, cx)))
                        .when(!shares.is_empty(), |d| {
                            d.child(div().text_xs().text_color(rgb(FAINT)).child("PEOPLE WITH ACCESS")).children(shares.into_iter().map(|s| {
                                let (drive, email) = (drive_for_remove.clone(), s.email.clone());
                                div()
                                    .flex()
                                    .items_center()
                                    .justify_between()
                                    .text_sm()
                                    .child(div().text_color(rgb(TEXT)).child(s.email.clone()))
                                    .child(
                                        div()
                                            .flex()
                                            .items_center()
                                            .gap_3()
                                            .child(div().text_xs().text_color(rgb(MUTED)).child(if s.role == "edit" { "Can edit" } else { "Can view" }))
                                            .child(
                                                div()
                                                    .id(SharedString::from(format!("unshare-{}", s.email)))
                                                    .text_xs()
                                                    .text_color(rgb(BAD))
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
                div().text_sm().text_color(rgb(MUTED)).child(if entry.is_dir { "The folder and everything in it are removed from the drive." } else { "The file and its versions are removed from the drive." }),
                Some("Delete"),
                true,
            ),
            Modal::Versions { entry, versions } => (
                format!("Versions of {}", entry.name),
                div().flex().flex_col().gap_1().children(match versions {
                    None => vec![div().text_sm().text_color(rgb(MUTED)).child("Loading\u{2026}").into_any_element()],
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
                                    .gap_3()
                                    .text_sm()
                                    .child(div().flex_1().text_color(rgb(TEXT)).child(fmt_time(v.mtime_ms, now_ms())))
                                    .child(div().w(px(90.)).text_xs().text_color(rgb(MUTED)).child(fmt_bytes(v.size)))
                                    .child(if is_current {
                                        div().w(px(80.)).text_xs().text_color(rgb(GOOD)).child("Current").into_any_element()
                                    } else {
                                        div()
                                            .id(SharedString::from(format!("restore-{}", v.id)))
                                            .w(px(80.))
                                            .text_xs()
                                            .text_color(rgb(ACCENT))
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
            .child(
                div()
                    .w(px(440.))
                    .rounded_xl()
                    .bg(rgb(PANEL))
                    .border_1()
                    .border_color(rgb(BORDER))
                    .shadow_lg()
                    .p_6()
                    .flex()
                    .flex_col()
                    .gap_4()
                    .child(div().text_lg().font_weight(FontWeight::SEMIBOLD).text_color(rgb(TEXT)).child(title))
                    .child(body)
                    .child(
                        div()
                            .flex()
                            .justify_end()
                            .gap_2()
                            .pt_2()
                            .child(button("modal-cancel", if confirm.is_some() { "Cancel" } else { "Close" }).on_click(cx.listener(|this, _, _, cx| {
                                this.modal = None;
                                cx.notify();
                            })))
                            .when_some(confirm, |d, label| {
                                let b = primary_button("modal-ok", label).on_click(cx.listener(|this, _, _, cx| this.submit_modal(cx)));
                                d.child(if danger { b.bg(rgb(BAD)).text_color(rgb(0xffffff)).hover(|s| s.bg(rgb(0xff8479))) } else { b })
                            }),
                    ),
            )
            .into_any_element()
    }
}

impl Render for LudyApp {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let main = match self.screen.clone() {
            Screen::Drives => self.drives_screen(cx).into_any_element(),
            Screen::Browser { drive, prefix } => self.browser_screen(&drive, &prefix, cx).into_any_element(),
        };
        let notice = self.view.notice.clone();
        div()
            .size_full()
            .relative()
            .flex()
            .bg(rgb(BG))
            .text_color(rgb(TEXT))
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::on_key))
            .child(self.sidebar(cx))
            .child(
                div().flex_1().h_full().flex().flex_col().overflow_hidden().child(div().flex_1().overflow_hidden().child(main)).when_some(notice, |d, text| {
                    d.child(
                        div()
                            .mx_8()
                            .mb_4()
                            .px_4()
                            .py_2()
                            .rounded_lg()
                            .bg(rgb(0x2a2118))
                            .border_1()
                            .border_color(rgb(0x5a4420))
                            .flex()
                            .items_center()
                            .justify_between()
                            .gap_4()
                            .child(div().flex_1().text_sm().text_color(rgb(ACCENT_HI)).child(text))
                            .child(icon_button("dismiss-notice", "\u{2715}").on_click(cx.listener(|this, _, _, cx| {
                                this.engine.clear_notice();
                                cx.notify();
                            }))),
                    )
                }),
            )
            .when_some(self.menu.as_ref(), |d, menu| d.child(self.render_menu(menu, cx)))
            .when_some(self.modal.as_ref(), |d, modal| d.child(self.render_modal(modal, cx)))
    }
}
