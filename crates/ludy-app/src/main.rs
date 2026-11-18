//! Ludy: cloud drives that mount in the Finder and stream on demand.
//!
//! usage: ludy-app [--mount DRIVE]... [--open DRIVE[/FOLDER]]
//!
//! `--mount` mounts a drive at launch; `--open` starts in the file browser.
//!
//! Configuration comes from the environment (see `EngineConfig::from_env`):
//! `LUDY_SERVER` (default http://127.0.0.1:9000), `LUDY_CACHE_MB`,
//! `LUDY_MOUNT_ROOT` (default ~/Ludy), `LUDY_READ_ONLY`, `LUDY_NOBROWSE`.

mod theme;
mod ui;

use std::sync::Arc;

use gpui::{actions, point, prelude::*, px, size, App, Application, Bounds, KeyBinding, Menu, MenuItem, TitlebarOptions, WindowBounds, WindowOptions};
use ludy_core::engine::{Engine, EngineConfig};

actions!(ludy, [Quit]);

fn main() {
    let mut automount: Vec<String> = Vec::new();
    let mut open: Option<String> = None;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--mount" => automount.extend(args.next()),
            "--open" => open = args.next(),
            "-h" | "--help" => {
                println!("usage: ludy-app [--mount DRIVE]... [--open DRIVE[/FOLDER]]\n\nStart the simulated cloud first: cargo run -p ludy-cloud");
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

    Application::new().run(move |cx: &mut App| {
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

        let bounds = Bounds::centered(None, size(px(1180.), px(780.)), cx);
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            window_min_size: Some(size(px(820.), px(560.))),
            titlebar: Some(TitlebarOptions { title: Some("Ludy".into()), appears_transparent: true, traffic_light_position: Some(point(px(14.), px(14.))) }),
            ..Default::default()
        };
        let engine = engine.clone();
        cx.open_window(options, move |window, cx| cx.new(|cx| ui::LudyApp::new(engine, open, window, cx))).expect("open the Ludy window");
        cx.activate(true);
    });
}
