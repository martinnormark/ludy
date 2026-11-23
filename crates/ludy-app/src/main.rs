//! Ludy: cloud drives that mount in the Finder and stream on demand.
//!
//! usage: ludy-app [--mount DRIVE]... [--open DRIVE[/FOLDER]] [--list]
//!
//! `--mount` mounts a drive at launch; `--open` starts in the file browser,
//! `--list` in its list view. For checking the app without a hand on the
//! mouse: `--demo menu|entry-menu|key|new-drive` opens that surface, and
//! `--perf` scrolls the open folder by itself and prints frame times.
//! `--click X,Y` (also `--rclick`, `--dclick`; repeatable, window points)
//! posts mouse events to the app's own window, and `--size WxH` sets the
//! window size.
//!
//! Configuration comes from the environment (see `EngineConfig::from_env`):
//! `LUDY_SERVER` (default http://127.0.0.1:8080, where `docker compose up`
//! answers), `LUDY_HOME`, `LUDY_CACHE_MB`, `LUDY_CACHE_DIR`,
//! `LUDY_MOUNT_ROOT` (default ~/Ludy), `LUDY_READ_ONLY`, `LUDY_NOBROWSE`.

mod assets;
mod inject;
mod theme;
mod ui;

use std::sync::Arc;

use gpui::{actions, point, prelude::*, px, size, App, Application, Bounds, KeyBinding, Menu, MenuItem, TitlebarOptions, WindowBounds, WindowOptions};
use ludy_core::engine::{Engine, EngineConfig};

actions!(ludy, [Quit]);

fn main() {
    let mut automount: Vec<String> = Vec::new();
    let mut opts = ui::UiOptions::default();
    let mut window_size: (f32, f32) = (1180., 780.);
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--mount" => automount.extend(args.next()),
            "--open" => opts.open = args.next(),
            "--list" => opts.list = true,
            "--demo" => opts.demo = args.next(),
            "--perf" => opts.perf = true,
            "--click" | "--rclick" | "--dclick" => {
                let kind = match a.as_str() {
                    "--rclick" => 1,
                    "--dclick" => 2,
                    _ => 0,
                };
                let at = args.next().unwrap_or_default();
                match at.split_once(',').and_then(|(x, y)| Some((x.parse().ok()?, y.parse().ok()?))) {
                    Some((x, y)) => opts.clicks.push((x, y, kind)),
                    None => {
                        eprintln!("ludy-app: {a} needs X,Y in window points");
                        std::process::exit(2);
                    }
                }
            }
            "--size" => {
                let s = args.next().unwrap_or_default();
                window_size = s.split_once('x').and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?))).unwrap_or(window_size);
            }
            "-h" | "--help" => {
                println!("usage: ludy-app [--mount DRIVE]... [--open DRIVE[/FOLDER]] [--list]\n\nStart the server first: docker compose up -d --wait\nIt is looked for at $LUDY_SERVER (default http://127.0.0.1:8080).");
                return;
            }
            other => {
                eprintln!("ludy-app: unknown argument {other:?}");
                std::process::exit(2);
            }
        }
    }
    let engine = match Engine::new(EngineConfig::from_env()) {
        Ok(e) => Arc::new(e),
        Err(e) => {
            eprintln!("ludy-app: cannot start the engine: {e:#}");
            std::process::exit(1);
        }
    };
    for drive in &automount {
        engine.mount(drive);
    }
    // Ctrl-C in the terminal (or `kill`) must not leave mounted volumes
    // behind with no server: eject them, then exit.
    let on_signal = engine.clone();
    std::thread::spawn(move || {
        on_signal.wait_for_signal();
        eprintln!("ludy-app: ejecting drives before exit");
        on_signal.shutdown();
        std::process::exit(130);
    });

    Application::new().with_assets(assets::Assets).run(move |cx: &mut App| {
        cx.bind_keys([KeyBinding::new("cmd-q", Quit, None)]);
        cx.on_action(|_: &Quit, cx| cx.quit());
        cx.set_menus(vec![Menu { name: "Ludy".into(), items: vec![MenuItem::action("Quit Ludy", Quit)] }]);
        // Closing the window quits; quitting ejects every mounted drive.
        cx.on_window_closed(|cx| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
        let quit_engine = engine.clone();
        cx.on_app_quit(move |_| {
            let engine = quit_engine.clone();
            async move { engine.shutdown() }
        })
        .detach();

        let bounds = Bounds::centered(None, size(px(window_size.0), px(window_size.1)), cx);
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            window_min_size: Some(size(px(820.), px(560.))),
            titlebar: Some(TitlebarOptions { title: Some("Ludy".into()), appears_transparent: true, traffic_light_position: Some(point(px(16.), px(18.))) }),
            ..Default::default()
        };
        let engine = engine.clone();
        cx.open_window(options, move |window, cx| cx.new(|cx| ui::LudyApp::new(engine, opts, window, cx))).expect("open the Ludy window");
        cx.activate(true);
    });
}
