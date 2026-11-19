//! The Ludy window's state and actions. Drawing is in `ui/render.rs`.

mod render;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use std::time::Instant;

use gpui::{point, px, ClipboardItem, Context, FocusHandle, KeyDownEvent, PathPromptOptions, Pixels, Point, ScrollStrategy, UniformListScrollHandle, Window};
use ludy_core::client::{CloudClient, Version};
use ludy_core::engine::{DriveView, Engine, View};

/// How the window starts. `demo` and `perf` exist so that screens which need
/// a click can be opened, and scrolling measured, without a hand on the mouse.
#[derive(Default)]
pub struct UiOptions {
    pub open: Option<String>,
    pub list: bool,
    /// "menu", "entry-menu", "share" or "new-drive".
    pub demo: Option<String>,
    /// Scroll the open folder by itself for a few seconds, print frame times, quit.
    pub perf: bool,
    /// Clicks to send to the window after launch, in window points:
    /// (x, y, kind) with kind 0 = click, 1 = right-click, 2 = double-click.
    pub clicks: Vec<(f32, f32, u8)>,
}

/// Frame pacing measured while the list scrolls itself.
struct Perf {
    started: Instant,
    last: Instant,
    intervals_ms: Vec<f32>,
    /// Time spent building elements (the window and its visible rows).
    build: std::time::Duration,
}

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
    scroll: UniformListScrollHandle,
    demo: Option<String>,
    perf_wanted: bool,
    perf: Option<Perf>,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

fn open_path(args: &[&str], path: &std::path::Path) {
    let _ = std::process::Command::new("/usr/bin/open").args(args).arg(path).spawn();
}

impl LudyApp {
    pub fn new(engine: Arc<Engine>, opts: UiOptions, window: &mut Window, cx: &mut Context<Self>) -> Self {
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
        // `--click`: post mouse events to our own window (see `inject.rs`).
        if !opts.clicks.is_empty() {
            let steps = opts.clicks.clone();
            cx.spawn(async move |_, cx| {
                let pause = |ms: u64| cx.background_executor().timer(Duration::from_millis(ms));
                pause(1500).await;
                for (x, y, kind) in steps {
                    crate::inject::mouse_move(x, y);
                    pause(80).await;
                    for nth in 1..=(if kind == 2 { 2 } else { 1 }) {
                        crate::inject::mouse_down(x, y, kind, nth);
                        pause(40).await;
                        crate::inject::mouse_up(x, y, kind, nth);
                        pause(60).await;
                    }
                    pause(900).await;
                }
            })
            .detach();
        }
        let view = engine.view();
        let mut app = LudyApp {
            engine,
            view,
            screen: Screen::Drives,
            entries: Vec::new(),
            loading: false,
            loaded_seq: 0,
            grid: !opts.list,
            menu: None,
            modal: None,
            focus,
            scroll: UniformListScrollHandle::new(),
            demo: opts.demo,
            perf_wanted: opts.perf,
            perf: None,
        };
        if let Some(target) = opts.open {
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
        self.run_demo(cx);
        // A commit on the open drive (from anywhere) reloads the listing.
        if let Screen::Browser { drive, .. } = &self.screen {
            let seq = self.drive(drive).map(|d| d.info.seq).unwrap_or(0);
            if seq != self.loaded_seq && !self.loading {
                self.reload(cx);
            }
        }
    }

    /// Open the surface named by `--demo` once there is something to open it on.
    fn run_demo(&mut self, cx: &mut Context<Self>) {
        let Some(first) = self.view.drives.first().map(|d| d.info.name.clone()) else { return };
        let at = point(px(470.), px(250.));
        match self.demo.as_deref() {
            Some("menu") => self.menu = Some(Menu { at, target: Target::Drive(first) }),
            Some("share") => self.modal = Some(Modal::Share { drive: first, email: String::new(), edit: false }),
            Some("new-drive") => self.modal = Some(Modal::NewDrive { name: String::new() }),
            Some("entry-menu") => match self.entries.iter().find(|e| !e.is_dir) {
                Some(e) => self.menu = Some(Menu { at, target: Target::Entry(e.clone()) }),
                None => return,
            },
            _ => return,
        }
        self.demo = None;
        cx.notify();
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
        self.scroll.scroll_to_item(0, ScrollStrategy::Top);
        self.reload(cx);
    }

    fn navigate(&mut self, prefix: String, cx: &mut Context<Self>) {
        if let Screen::Browser { prefix: p, .. } = &mut self.screen {
            // The old rows stay until the new listing lands, so a folder
            // change does not flash an empty pane.
            *p = prefix;
            self.scroll.scroll_to_item(0, ScrollStrategy::Top);
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
                    Ok(Err(e)) => {
                        this.entries.clear();
                        this.engine.set_notice(format!("Could not list folder: {e:#}"));
                    }
                    Err(e) => {
                        this.entries.clear();
                        this.engine.set_notice(format!("Could not list folder: {e}"));
                    }
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
}
