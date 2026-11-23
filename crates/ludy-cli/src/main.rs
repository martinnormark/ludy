//! `ludy`: the engine without the window.

mod bench;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use ludy_core::engine::{Engine, EngineConfig, DEFAULT_SERVER};
use ludy_core::fs::ROOT_ID;
use ludy_core::human;
use ludy_core::transfer;

const USAGE: &str = "\
ludy: end-to-end encrypted cloud drives from the terminal

USAGE:
    ludy server                               say what answers at $LUDY_SERVER
    ludy drives                               list the drives this Mac holds a key for
    ludy mkdrive <drive>                      create a drive and keep its key here
    ludy key <drive>                          print a drive's key, to add the drive on another Mac
    ludy add <key> [name]                     add a drive from its key
    ludy forget <drive>                       remove a drive's key from this Mac
    ludy ls <drive> [folder]                  list a folder
    ludy put <drive> <file-or-folder> [path]  upload a file, or a folder and all in it
    ludy get <drive> <path> <dest>            download a file, or a folder and all in it
    ludy synth <drive> <path> <size> [seed]   add a file of generated content, e.g. 50GB
    ludy mount <drive> [--read-only] [--nobrowse] [--root DIR]
                                              mount until Ctrl-C, then eject
    ludy bench [--quick] [--label NAME] [--out FILE] [--dir DIR]
                                              time fixed scenarios in a drive of its own

The server is at $LUDY_SERVER (default http://127.0.0.1:8080, where
`docker compose up -d --wait` answers). Keys are kept in
$LUDY_HOME/drives.json (default ~/Library/Application Support/Ludy).
";

fn main() {
    if let Err(e) = run() {
        eprintln!("ludy: {e:#}");
        std::process::exit(1);
    }
}

/// `50GB`, `1.5m`, `4096`: binary multiples.
fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim();
    let split = s.find(|c: char| !c.is_ascii_digit() && c != '.').unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let n: f64 = num.parse().ok()?;
    let mult: u64 = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "k" | "kb" | "kib" => 1 << 10,
        "m" | "mb" | "mib" => 1 << 20,
        "g" | "gb" | "gib" => 1 << 30,
        "t" | "tb" | "tib" => 1 << 40,
        _ => return None,
    };
    Some((n * mult as f64) as u64)
}

/// A path inside a drive, split into its folder and its last part.
fn split_path(path: &str) -> Result<(&str, &str)> {
    let path = path.trim_matches('/');
    let (folder, name) = path.rsplit_once('/').unwrap_or(("", path));
    if name.is_empty() {
        bail!("a path inside the drive is needed");
    }
    Ok((folder, name))
}

fn run() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(cmd) = args.first() else {
        print!("{USAGE}");
        return Ok(());
    };
    if matches!(cmd.as_str(), "help" | "-h" | "--help") {
        print!("{USAGE}");
        return Ok(());
    }
    // The benchmark keeps its cache and journal in a folder of its own.
    if cmd == "bench" {
        return bench::run(&args[1..]);
    }
    let mut cfg = EngineConfig::from_env();
    let mut rest: Vec<&str> = Vec::new();
    let mut it = args[1..].iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--read-only" => cfg.read_only = true,
            "--nobrowse" => cfg.browse = false,
            "--root" => cfg.mount_root = PathBuf::from(it.next().context("--root needs a directory")?),
            other => rest.push(other),
        }
    }
    let server = cfg.server_url.clone();
    let engine = Engine::new(cfg)?;
    match (cmd.as_str(), rest.as_slice()) {
        ("server", []) => {
            let name = engine.block_on(engine.hello()).with_context(|| unreachable_hint(&server))?;
            println!("{server}: {name}");
        }
        ("drives", []) => {
            let drives = engine.drives();
            if drives.is_empty() {
                println!("this Mac holds no drive keys for {server}; make a drive with: ludy mkdrive <name>");
            }
            for d in drives {
                println!("{:<24} {}", d.name, d.id);
            }
        }
        ("mkdrive", [drive]) => {
            engine.block_on(engine.create_drive(drive)).with_context(|| unreachable_hint(&server))?;
            println!("created {drive} on {server}; its key is in {}", engine.config().keystore.display());
        }
        ("key", [drive]) => println!("{}", engine.export_drive(drive)?),
        ("add", [key, name @ ..]) => {
            let name = engine.block_on(engine.import_drive(key, name.first().copied()))?;
            println!("added {name}");
        }
        ("forget", [drive]) => {
            engine.block_on(engine.forget_drive(drive))?;
            println!("forgot {drive}: it is still on {server}, and nobody without its key can read it");
        }
        ("ls", [drive, folder @ ..]) => {
            let fs = engine.block_on(engine.open(drive))?;
            let folder = folder.first().copied().unwrap_or("");
            let dir = if folder.trim_matches('/').is_empty() { ROOT_ID } else { fs.find_path(folder.trim_matches('/')).ok_or_else(|| anyhow!("no folder {folder}"))? };
            for (name, a) in fs.contents(dir).map_err(|e| anyhow!("list: {e:?}"))? {
                println!("{:>12}  {name}{}", if a.is_dir { "-".to_string() } else { human(a.size) }, if a.is_dir { "/" } else { "" });
            }
        }
        ("put", [drive, src, dest @ ..]) => {
            let src = PathBuf::from(src);
            let dest = match dest {
                [d] => d.to_string(),
                _ => src.file_name().and_then(|n| n.to_str()).context("the source has no name")?.to_string(),
            };
            let (folder, name) = split_path(&dest)?;
            let fs = engine.block_on(engine.open(drive))?;
            let started = Instant::now();
            let dir = transfer::ensure_folder(&fs, folder)?;
            let (files, bytes) = transfer::copy_tree_in(&fs, dir, name, &src)?;
            // Everything is in the journal now; wait until the cloud has it.
            if !engine.block_on(fs.flush(Duration::from_secs(24 * 3600))) {
                bail!("the upload did not finish: {:?}", fs.pending().last_error);
            }
            let sent = engine.stats().bytes_up.load(std::sync::atomic::Ordering::Relaxed);
            println!("{drive}/{}: {files} file(s), {}; sent {} sealed in {:.1} s", dest.trim_matches('/'), human(bytes), human(sent), started.elapsed().as_secs_f64());
        }
        ("get", [drive, path, file]) => {
            let fs = engine.block_on(engine.open(drive))?;
            let id = if path.trim_matches('/').is_empty() { ROOT_ID } else { fs.find_path(path.trim_matches('/')).ok_or_else(|| anyhow!("nothing at {path}"))? };
            let (files, bytes) = engine.block_on(transfer::copy_tree_out(&fs, id, &PathBuf::from(file)))?;
            println!("{file}: {files} file(s), {}", human(bytes));
        }
        ("synth", [drive, path, size, seed @ ..]) => {
            let size = parse_size(size).with_context(|| format!("cannot parse size {size:?}"))?;
            let seed = seed.first().map(|s| s.parse()).transpose().context("seed must be a number")?.unwrap_or(0);
            let (folder, name) = split_path(path)?;
            let fs = engine.block_on(engine.open(drive))?;
            let dir = transfer::ensure_folder(&fs, folder)?;
            if !engine.block_on(fs.flush(Duration::from_secs(120))) {
                bail!("could not make the folder: {:?}", fs.pending().last_error);
            }
            engine.block_on(fs.add_synthetic(dir, name, size, seed))?;
            println!("{drive}/{}: {} generated from seed {seed}; nothing is stored for it", path.trim_matches('/'), human(size));
        }
        ("mount", [drive]) => mount(&engine, drive)?,
        _ => bail!("unknown command or wrong arguments\n\n{USAGE}"),
    }
    engine.shutdown();
    Ok(())
}

fn unreachable_hint(server: &str) -> String {
    if server == DEFAULT_SERVER {
        format!("nothing of Ludy's answers at {server}; start the server with: docker compose up -d --wait")
    } else {
        format!("nothing of Ludy's answers at {server} (LUDY_SERVER)")
    }
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
                if let Some(notice) = &v.notice {
                    println!("note: {notice}");
                    engine.clear_notice();
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
