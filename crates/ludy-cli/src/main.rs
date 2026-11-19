//! `ludy`: the engine without the window.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use ludy_cloud::{human, parse_size};
use ludy_core::engine::{Engine, EngineConfig};

const USAGE: &str = "\
ludy: cloud drives from the terminal

USAGE:
    ludy drives                              list drives in the cloud
    ludy mkdrive <drive>                     create a drive
    ludy put <drive> <file> [key]            upload a file as shards; sends only new ones
    ludy synth <drive> <key> <size> [seed]   add a synthetic object, e.g. 50GB
    ludy log [--follow]                      print the cloud's request log
    ludy mount <drive> [--read-only] [--nobrowse] [--root DIR]
                                             mount until Ctrl-C, then eject

The cloud is at $LUDY_SERVER (default http://127.0.0.1:9000).
";

fn main() {
    if let Err(e) = run() {
        eprintln!("ludy: {e:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(cmd) = args.first() else {
        print!("{USAGE}");
        return Ok(());
    };
    let mut cfg = EngineConfig::from_env();
    let mut rest: Vec<&str> = Vec::new();
    let mut follow = false;
    let mut it = args[1..].iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--read-only" => cfg.read_only = true,
            "--nobrowse" => cfg.browse = false,
            "--root" => cfg.mount_root = PathBuf::from(it.next().context("--root needs a directory")?),
            "--follow" | "-f" => follow = true,
            other => rest.push(other),
        }
    }
    let engine = Engine::new(cfg)?;
    let client = engine.client();
    match (cmd.as_str(), rest.as_slice()) {
        ("drives", []) => {
            for d in engine.block_on(client.drives())? {
                println!("{:<24} {:>10}  {} object(s){}", d.name, human(d.bytes), d.objects, d.forked_from.map(|f| format!("  fork of {f}")).unwrap_or_default());
            }
        }
        ("mkdrive", [drive]) => engine.block_on(client.create_drive(drive))?,
        ("put", [drive, file, key @ ..]) => {
            let path = PathBuf::from(file);
            let key = match key {
                [k] => k.to_string(),
                _ => path.file_name().and_then(|n| n.to_str()).context("file has no name")?.to_string(),
            };
            // Cut into shards here; only the ones the cloud lacks are sent.
            let (c, sent) = engine.block_on(client.put_file_sharded(drive, &key, &path))?;
            println!(
                "{drive}/{key} version {}: sent {} in {} shard(s), {} already in the cloud",
                c.version,
                human(sent.sent_bytes),
                sent.sent_shards,
                human(sent.skipped_bytes)
            );
        }
        ("synth", [drive, key, size, seed @ ..]) => {
            let size = parse_size(size).with_context(|| format!("cannot parse size {size:?}"))?;
            let seed = seed.first().map(|s| s.parse()).transpose().context("seed must be a number")?.unwrap_or(0);
            engine.block_on(client.put_synthetic(drive, key, size, seed))?;
            println!("{drive}/{key}: {} generated from seed {seed}", human(size));
        }
        ("log", []) => {
            let mut since = 0;
            loop {
                for e in engine.block_on(client.server_log(since))? {
                    since = e.n;
                    let range = e.range.map(|(a, b)| format!("bytes={a}-{b} ({})", human(b + 1 - a))).unwrap_or_else(|| "-".into());
                    println!("{:>6} {:<6} {} /{}/{}  {range}  {}ms", e.n, e.method, e.status, e.drive, e.key, e.ms);
                }
                if !follow {
                    break;
                }
                std::thread::sleep(Duration::from_millis(500));
            }
        }
        ("mount", [drive]) => mount(&engine, drive)?,
        ("help" | "-h" | "--help", _) => print!("{USAGE}"),
        _ => bail!("unknown command or wrong arguments\n\n{USAGE}"),
    }
    Ok(())
}

fn mount(engine: &Engine, drive: &str) -> Result<()> {
    let mounted = engine.block_on(engine.mount_now(drive))?;
    println!("mounted {drive} at {} (NFS on 127.0.0.1:{}); Ctrl-C to eject", mounted.mountpoint.display(), mounted.port);
    let done = std::sync::atomic::AtomicBool::new(false);
    std::thread::scope(|scope| {
        scope.spawn(|| {
            let mut last = (0, 0, 0);
            while !done.load(std::sync::atomic::Ordering::Relaxed) {
                let v = engine.view();
                let now = (v.stats.bytes_down, v.stats.bytes_up, v.cache.bytes);
                if now != last {
                    last = now;
                    println!(
                        "streamed {} in {} ranges ({}/s) | cache {} of {} | cache hits {} | uploaded {}",
                        human(v.stats.bytes_down),
                        v.stats.range_requests,
                        human(v.stats.down_rate as u64),
                        human(v.cache.bytes),
                        human(v.cache.cap),
                        human(v.stats.cache_hit_bytes),
                        human(v.stats.bytes_up),
                    );
                }
                std::thread::sleep(Duration::from_secs(1));
            }
        });
        // Ctrl-C or `kill`: fall through to the eject below.
        engine.wait_for_signal();
        done.store(true, std::sync::atomic::Ordering::Relaxed);
    });
    println!("ejecting {drive}…");
    engine.block_on(engine.eject_now(drive))?;
    println!("ejected");
    Ok(())
}
