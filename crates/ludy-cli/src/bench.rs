//! `ludy bench`: fixed engine scenarios against the server at `$LUDY_SERVER`,
//! timed and printed as a table. No kernel mount is involved: reads and
//! writes go through the same `SealedDrive` calls the NFS adapter makes, so
//! the numbers are the engine's, the server's and the link's between them.
//!
//! It makes a drive of its own, whose key is never saved, and empties it at
//! the end. The drive itself stays on the server (nothing deletes a drive);
//! its content is collected after the server's grace period.
//!
//! Run it from a release build: `cargo run --release -p ludy-cli -- bench`.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use ludy_core::cache::{BlockCache, BLOCK_SIZE};
use ludy_core::engine::DEFAULT_SERVER;
use ludy_core::fs::{FsError, ROOT_ID};
use ludy_core::sealed_client::SealedClient;
use ludy_core::sealed_drive::{self, DriveKeys, SealedDrive};
use ludy_core::stats::Stats;
use ludy_proto::synth;

const MIB: u64 = 1 << 20;

struct Opts {
    server: String,
    quick: bool,
    label: String,
    out: Option<PathBuf>,
    dir: PathBuf,
}

struct Row {
    name: &'static str,
    result: String,
    note: String,
}

/// One "Mac": a cache and journals of its own.
struct Rig {
    client: SealedClient,
    cache: Arc<BlockCache>,
    stats: Arc<Stats>,
    dir: PathBuf,
    n: std::cell::Cell<u32>,
}

impl Rig {
    fn new(server: &str, dir: PathBuf) -> Result<Rig> {
        let stats = Arc::new(Stats::default());
        Ok(Rig { client: SealedClient::new(server, stats.clone()), cache: Arc::new(BlockCache::open(&dir.join("cache"), 2 << 30)?), stats, dir, n: Default::default() })
    }

    async fn open(&self, keys: &DriveKeys) -> Result<Arc<SealedDrive>> {
        self.n.set(self.n.get() + 1);
        let fs = SealedDrive::open("bench", keys.clone(), self.client.clone(), self.cache.clone(), self.stats.clone(), self.dir.join(format!("journal-{}", self.n.get())))?;
        fs.refresh().await?;
        Ok(fs)
    }

    fn requests(&self) -> u64 {
        self.stats.range_requests.load(std::sync::atomic::Ordering::Relaxed)
    }
}

fn fs(e: FsError) -> anyhow::Error {
    anyhow!("{e:?}")
}

fn secs(d: Duration) -> String {
    let s = d.as_secs_f64();
    if s < 0.01 {
        format!("{:.2} ms", s * 1e3)
    } else if s < 1.0 {
        format!("{:.0} ms", s * 1e3)
    } else {
        format!("{s:.2} s")
    }
}

fn rate(bytes: u64, d: Duration) -> String {
    format!("{:.0} MB/s", bytes as f64 / 1e6 / d.as_secs_f64().max(1e-9))
}

fn percentile(sorted: &[Duration], p: f64) -> Duration {
    sorted[((sorted.len() - 1) as f64 * p).round() as usize]
}

pub fn run(args: &[String]) -> Result<()> {
    let mut o = Opts {
        server: std::env::var("LUDY_SERVER").unwrap_or_else(|_| DEFAULT_SERVER.into()),
        quick: false,
        label: "run".into(),
        out: None,
        dir: std::env::temp_dir().join(format!("ludy-bench-{}", std::process::id())),
    };
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut value = || it.next().with_context(|| format!("{a} needs a value"));
        match a.as_str() {
            "--label" => o.label = value()?.clone(),
            "--out" => o.out = Some(PathBuf::from(value()?)),
            "--dir" => o.dir = PathBuf::from(value()?),
            "--quick" => o.quick = true,
            other => bail!("bench: unknown option {other}"),
        }
    }
    if cfg!(debug_assertions) {
        eprintln!("note: this is a debug build; run with --release for numbers worth keeping");
    }
    let _ = std::fs::remove_dir_all(&o.dir);
    std::fs::create_dir_all(&o.dir)?;
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(4).enable_all().build()?;
    let result = rt.block_on(scenarios(&o));
    let _ = std::fs::remove_dir_all(&o.dir);
    let (server_name, rows) = result?;

    let mut text = format!(
        "Ludy engine benchmark: {} ({} build), against {} at {}{}\n\n| Scenario | Result | Notes |\n|---|---|---|\n",
        o.label,
        if cfg!(debug_assertions) { "debug" } else { "release" },
        server_name,
        o.server,
        if o.quick { ", quick sizes" } else { "" },
    );
    for r in &rows {
        text.push_str(&format!("| {} | {} | {} |\n", r.name, r.result, r.note));
    }
    print!("{text}");
    if let Some(path) = &o.out {
        std::fs::write(path, &text).with_context(|| format!("write {}", path.display()))?;
    }
    Ok(())
}

/// Write `data` into a new file through the engine, as a copy into the mount would.
fn write_file(drive: &SealedDrive, dir: u64, name: &str, data: &[u8]) -> Result<u64> {
    let id = drive.create(dir, name, false).map_err(fs)?;
    for (i, piece) in data.chunks(BLOCK_SIZE as usize).enumerate() {
        drive.write(id, i as u64 * BLOCK_SIZE, piece).map_err(fs)?;
    }
    Ok(id)
}

async fn flushed(drive: &Arc<SealedDrive>, what: &str) -> Result<Duration> {
    let t = Instant::now();
    if !drive.flush(Duration::from_secs(1800)).await {
        bail!("{what} did not finish uploading: {:?}", drive.pending());
    }
    Ok(t.elapsed())
}

async fn read_through(drive: &Arc<SealedDrive>, id: u64, from: u64, len: u64) -> Result<Duration> {
    let t = Instant::now();
    let mut off = from;
    while off < from + len {
        let (data, eof) = drive.read(id, off, BLOCK_SIZE as u32).await.map_err(|e| anyhow!("read: {e:?}"))?;
        off += data.len() as u64;
        if eof || data.is_empty() {
            break;
        }
    }
    if off < from + len {
        bail!("read {} of {len} bytes", off - from);
    }
    Ok(t.elapsed())
}

async fn scenarios(o: &Opts) -> Result<(String, Vec<Row>)> {
    let here = Rig::new(&o.server, o.dir.join("here"))?;
    let server_name = here.client.hello().await.with_context(|| format!("nothing of Ludy's answers at {}; start a server first (docker compose up -d --wait) or set LUDY_SERVER", o.server))?;
    let keys = sealed_drive::create_drive(&here.client, "bench").await?;
    let work = here.open(&keys).await?;
    let mut rows = Vec::new();

    // ---- one large file: in, in again, patched, and read back
    let copy_len = if o.quick { 8 * MIB } else { 64 * MIB };
    let data = synth::bytes(4242, 0, copy_len as usize);
    let t = Instant::now();
    let copy = write_file(&work, ROOT_ID, "copy.bin", &data)?;
    let wrote = t.elapsed();
    let uploaded = flushed(&work, "the copy").await?;
    rows.push(Row { name: "Copy a file in: writes return", result: rate(copy_len, wrote), note: format!("{} MiB of incompressible data into the journal", copy_len / MIB) });
    rows.push(Row { name: "Copy a file in: upload", result: format!("{} ({})", secs(uploaded), rate(copy_len, uploaded)), note: "cut, seal, send the sealed shards, commit a sealed manifest".into() });

    write_file(&work, ROOT_ID, "copy of copy.bin", &data)?;
    let again = flushed(&work, "the duplicate").await?;
    rows.push(Row { name: "Copy the same content in again: upload", result: secs(again), note: "cut and sealed again; every sealed shard is already in the cloud".into() });

    work.refresh().await?;
    work.write(copy, copy_len / 2 + 777, &synth::bytes(77, 0, 4096)).map_err(fs)?;
    let patched = flushed(&work, "the patch").await?;
    rows.push(Row {
        name: "Change 4 KiB inside that file: upload",
        result: format!("{} ({:.1}x faster than uploading it)", secs(patched), uploaded.as_secs_f64() / patched.as_secs_f64()),
        note: "only the shard the write touched is cut, sealed and sent".into(),
    });

    work.refresh().await?;
    here.cache.clear();
    let before = here.requests();
    let t = Instant::now();
    work.read(copy, copy_len / 3, 64 * 1024).await.map_err(|e| anyhow!("read: {e:?}"))?;
    let part = t.elapsed();
    let part_requests = here.requests() - before;
    here.cache.clear();
    let whole = read_through(&work, copy, 0, copy_len).await?;
    rows.push(Row {
        name: "Read 64 KiB out of that file, cold",
        result: format!("{} ({:.0}x faster than reading it whole)", secs(part), whole.as_secs_f64() / part.as_secs_f64()),
        note: format!("{part_requests} range request(s), after the manifest; reading all {} MiB cold took {}", copy_len / MIB, secs(whole)),
    });

    // ---- reads of a larger file, from a second Mac that has none of it
    let read_len = if o.quick { 32 * MIB } else { 256 * MIB };
    let big = write_file(&work, ROOT_ID, "reads.bin", &synth::bytes(5151, 0, read_len as usize))?;
    flushed(&work, "the file to read").await?;
    let _ = big;
    let there = Rig::new(&o.server, o.dir.join("there"))?;
    let reader = there.open(&keys).await?;
    let file = reader.find_path("reads.bin").context("reads.bin")?;
    // The first 2 MiB before read-ahead has built up: what opening a clip costs.
    let head = read_through(&reader, file, 0, 8 * BLOCK_SIZE).await?;
    rows.push(Row { name: "First 2 MiB of a file, cold", result: secs(head), note: "the manifest, then 256 KiB reads from the start of the file".into() });
    there.cache.clear();
    for (name, note) in [("Sequential read, cold", "256 KiB reads, as the kernel issues them"), ("Sequential read, warm", "the same bytes again, from the disk cache")] {
        let took = read_through(&reader, file, 0, read_len).await?;
        rows.push(Row { name, result: rate(read_len, took), note: format!("{} MiB; {note}", read_len / MIB) });
    }
    there.cache.clear();
    let blocks = read_len / BLOCK_SIZE;
    let reads = if o.quick { 40 } else { 200 };
    for (name, note) in [("Random 64 KiB read, cold", "p50 / p99; a block each, nothing cached"), ("Random 64 KiB read, warm", "p50 / p99; from the disk cache")] {
        let mut times = Vec::with_capacity(reads);
        for i in 0..reads as u64 {
            // A different block every time, far from the last one.
            let off = (i * 37 % blocks) * BLOCK_SIZE + 4096;
            let t = Instant::now();
            reader.read(file, off, 64 * 1024).await.map_err(|e| anyhow!("read: {e:?}"))?;
            times.push(t.elapsed());
        }
        times.sort();
        rows.push(Row { name, result: format!("{} / {}", secs(percentile(&times, 0.5)), secs(percentile(&times, 0.99))), note: format!("{reads} reads; {note}") });
    }

    // ---- many small files
    let many: u64 = if o.quick { 500 } else { 5000 };
    let dir = work.mkdir(ROOT_ID, "flood").map_err(fs)?;
    let t = Instant::now();
    for i in 0..many {
        let id = work.create(dir, &format!("note_{i:05}.txt"), false).map_err(fs)?;
        work.write(id, 0, &synth::bytes(1000 + i, 0, 4096)).map_err(fs)?;
    }
    let local = t.elapsed();
    let synced = flushed(&work, "the small files").await?;
    rows.push(Row { name: "Create many small files", result: secs(local), note: format!("{many} files of 4 KiB, until the writes returned") });
    rows.push(Row { name: "Upload those files", result: format!("{} ({:.0} files/s)", secs(synced), many as f64 / synced.as_secs_f64()), note: format!("until all {many} are in the cloud") });

    // A third Mac opens the drive from nothing.
    let third = Rig::new(&o.server, o.dir.join("third"))?;
    let t = Instant::now();
    let other = third.open(&keys).await?;
    let opened = t.elapsed();
    let flood = other.find_path("flood").context("flood")?;
    let t = Instant::now();
    let entries = other.readdir(flood).map_err(fs)?.len() as u64;
    let listed = t.elapsed();
    if entries != many {
        bail!("listed {entries} of {many} files");
    }
    rows.push(Row { name: "Open a drive with many files", result: secs(opened), note: format!("{} nodes: read the feed, unwrap every key, open every name", many + 4) });
    rows.push(Row { name: "List that folder", result: secs(listed), note: "from the tree in memory".into() });
    let page = 100;
    let t = Instant::now();
    let (mut after, mut paged) = (0, 0u64);
    loop {
        let (entries, end) = other.readdir_page(flood, after, page).map_err(fs)?;
        paged += entries.len() as u64;
        after = entries.last().map(|(_, attr)| attr.id).unwrap_or(0);
        if end || entries.is_empty() {
            break;
        }
    }
    if paged != many {
        bail!("paged through {paged} of {many} files");
    }
    rows.push(Row { name: "List that folder in pages", result: secs(t.elapsed()), note: format!("{many} files, {page} at a time, as the NFS client asks for them") });

    // One change made on one Mac, picked up by another.
    write_file(&work, ROOT_ID, "new clip.mov", &synth::bytes(1, 0, 123_456))?;
    flushed(&work, "the new file").await?;
    let t = Instant::now();
    other.refresh().await?;
    let noticed = t.elapsed();
    if other.find_path("new clip.mov").is_none() {
        bail!("the remote change did not appear");
    }
    rows.push(Row { name: "Pick up one remote change", result: secs(noticed), note: format!("one request to the feed of a drive of {} nodes", many + 5) });

    let gone = many / 5;
    for i in 0..gone {
        work.remove(dir, &format!("note_{i:05}.txt")).map_err(fs)?;
    }
    let del = flushed(&work, "the deletes").await?;
    rows.push(Row { name: "Delete files", result: format!("{} ({:.0} files/s)", secs(del), gone as f64 / del.as_secs_f64()), note: format!("{gone} deletes, sent in order, 200 to a commit") });
    other.refresh().await?;
    let left = other.readdir(flood).map_err(fs)?.len() as u64;
    if left != many - gone {
        bail!("the folder holds {left} files, expected {}", many - gone);
    }

    // Leave the drive empty, so the server's collector takes the content back.
    for (name, _) in work.readdir(ROOT_ID).map_err(fs)? {
        work.remove_tree(ROOT_ID, &name).map_err(fs)?;
    }
    flushed(&work, "emptying the drive").await?;
    for drive in [work, reader, other] {
        drive.stop();
    }
    Ok((server_name, rows))
}
