//! `ludy bench`: fixed engine scenarios against a cloud started in-process,
//! timed and printed as a table. No kernel mount is involved: reads and
//! writes go through the same `DriveFs` calls the NFS adapter makes, so the
//! numbers are the engine's and the simulated link's, without the kernel.
//!
//! Run it from a release build: `cargo run --release -p ludy-cli -- bench`.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use ludy_cloud::harness::Harness;
use ludy_cloud::store::Content;
use ludy_cloud::{synth, Config};
use ludy_core::cache::{BlockCache, BLOCK_SIZE};
use ludy_core::client::CloudClient;
use ludy_core::drive::{DriveFs, ROOT_ID};
use ludy_core::stats::Stats;

const MIB: u64 = 1 << 20;
const GIB: u64 = 1 << 30;

struct Opts {
    latency_ms: u64,
    mbps: u64,
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

struct Rig {
    client: CloudClient,
    cache: Arc<BlockCache>,
    stats: Arc<Stats>,
    dir: PathBuf,
    n: std::cell::Cell<u32>,
}

impl Rig {
    fn drive(&self, name: &str) -> Result<Arc<DriveFs>> {
        self.n.set(self.n.get() + 1);
        Ok(DriveFs::new(name, self.client.clone(), self.cache.clone(), self.stats.clone(), self.dir.join(format!("journal-{}", self.n.get())))?)
    }
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

/// SplitMix64, for offsets that are the same on every run.
fn mix(i: u64) -> u64 {
    synth::word(0xbe7c, i)
}

pub fn run(args: &[String]) -> Result<()> {
    let mut o = Opts {
        latency_ms: 30,
        mbps: 400,
        quick: false,
        label: "run".into(),
        out: None,
        dir: std::env::temp_dir().join(format!("ludy-bench-{}", std::process::id())),
    };
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut value = || it.next().with_context(|| format!("{a} needs a value"));
        match a.as_str() {
            "--latency-ms" => o.latency_ms = value()?.parse()?,
            "--mbps" => o.mbps = value()?.parse()?,
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
    let rows = rt.block_on(scenarios(&o));
    let _ = std::fs::remove_dir_all(&o.dir);
    let rows = rows?;

    let mut text = format!(
        "Ludy engine benchmark: {} ({} build), simulated link {} Mbit/s, {} ms{}\n\n| Scenario | Result | Notes |\n|---|---|---|\n",
        o.label,
        if cfg!(debug_assertions) { "debug" } else { "release" },
        o.mbps,
        o.latency_ms,
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

async fn scenarios(o: &Opts) -> Result<Vec<Row>> {
    let cfg = Config { data_dir: o.dir.join("cloud"), addr: "127.0.0.1:0".parse()?, latency_ms: o.latency_ms, mbps: o.mbps, print_log: false };
    // A large drive, written straight into the cloud's data folder before it
    // starts: folders of 100 synthetic clips each.
    let huge: u64 = if o.quick { 5_000 } else { 50_000 };
    {
        let mut objects = serde_json::Map::new();
        for i in 0..huge {
            let version = serde_json::json!({ "id": format!("{:012x}{:04x}", 1_700_000_000_000u64 + i / 65536, i % 65536), "size": 1000 + i * 7919, "mtime_ms": 1_700_000_000_000u64 + i,
                "seq": i + 2, "content": { "kind": "synthetic", "seed": i } });
            objects.insert(format!("shoot_{:03}/clip_{:02}.mov", i / 100, i % 100), serde_json::json!([version]));
        }
        let catalog = serde_json::json!({ "created_ms": 1_700_000_000_000u64, "seq": huge + 2, "forked_from": null, "shares": [], "objects": objects });
        std::fs::create_dir_all(o.dir.join("cloud/drives"))?;
        std::fs::write(o.dir.join("cloud/drives/huge.json"), serde_json::to_vec(&catalog)?)?;
    }
    let server = Harness::start(cfg).await?;
    let cloud = server.cloud();
    let stats = Arc::new(Stats::default());
    let client = CloudClient::new(&server.url(), stats.clone());
    let rig = Rig { client: client.clone(), cache: Arc::new(BlockCache::open(&o.dir.join("cache"), GIB)?), stats, dir: o.dir.clone(), n: Default::default() };
    let mut rows = Vec::new();
    let link_mb_s = if o.mbps == 0 { "unlimited".to_string() } else { format!("{:.0} MB/s", o.mbps as f64 / 8.0) };

    // ---- the link itself, as reference points
    client.create_drive("media").await?;
    client.put_synthetic("media", "big.bin", 50 * GIB, 7).await?;
    let big = client.list("media").await?.objects.remove(0);
    let t = Instant::now();
    client.get_range("media", "big.bin", &big.version, 3 * GIB, 64 * 1024).await?;
    let raw_small = t.elapsed();
    let raw_len = if o.quick { 8 * MIB } else { 32 * MIB };
    let t = Instant::now();
    client.get_range("media", "big.bin", &big.version, 4 * GIB, raw_len).await?;
    let raw_big = t.elapsed();
    rows.push(Row { name: "Link: one 64 KiB range request", result: secs(raw_small), note: "straight to the server; the floor for a cold read".into() });
    rows.push(Row { name: "Link: one large range request", result: rate(raw_len, raw_big), note: format!("the link allows {link_mb_s}") });

    // ---- reads
    let fs = rig.drive("media")?;
    fs.refresh().await?;
    let file = fs.find_path("big.bin").context("big.bin")?;
    let seq_len = if o.quick { 32 * MIB } else { 256 * MIB };
    for (name, note) in [("Sequential read, cold", "256 KiB reads, as the kernel issues them"), ("Sequential read, warm", "the same bytes again, from the disk cache")] {
        let t = Instant::now();
        let mut off = GIB;
        while off < GIB + seq_len {
            let (data, _) = fs.read(file, off, BLOCK_SIZE as u32).await.map_err(|e| anyhow::anyhow!("read: {e:?}"))?;
            off += data.len() as u64;
        }
        rows.push(Row { name, result: rate(seq_len, t.elapsed()), note: format!("{} MiB; {note}", seq_len / MIB) });
    }
    let reads = if o.quick { 40 } else { 200 };
    for (name, note) in [("Random 64 KiB read, cold", "p50 / p99; one range request each"), ("Random 64 KiB read, warm", "p50 / p99; from the disk cache")] {
        let mut times = Vec::with_capacity(reads);
        for i in 0..reads as u64 {
            let off = 2 * GIB + mix(i) % (40 * GIB);
            let t = Instant::now();
            fs.read(file, off, 64 * 1024).await.map_err(|e| anyhow::anyhow!("read: {e:?}"))?;
            times.push(t.elapsed());
        }
        times.sort();
        rows.push(Row { name, result: format!("{} / {}", secs(percentile(&times, 0.5)), secs(percentile(&times, 0.99))), note: format!("{reads} reads; {note}") });
    }

    // The start of a file read front to back, before read-ahead has built
    // up: what opening a clip costs. Nothing above has touched these blocks.
    let head = 8 * BLOCK_SIZE;
    let t = Instant::now();
    let mut off = 0;
    while off < head {
        let (data, _) = fs.read(file, off, BLOCK_SIZE as u32).await.map_err(|e| anyhow::anyhow!("read: {e:?}"))?;
        off += data.len() as u64;
    }
    rows.push(Row { name: "First 2 MiB of a file, cold", result: secs(t.elapsed()), note: "256 KiB reads from the start of the file".into() });

    // ---- a folder with many files
    let many = if o.quick { 500 } else { 5000 };
    client.create_drive("many").await?;
    for i in 0..many {
        cloud.store.commit("many", &format!("clip_{i:05}.mov"), 1000 + i * 7919, Content::Synthetic { seed: i }, None, None).await.map_err(|e| anyhow::anyhow!("{e}"))?;
    }
    let fs_many = rig.drive("many")?;
    let t = Instant::now();
    fs_many.refresh().await?;
    let listed = t.elapsed();
    let t = Instant::now();
    let entries = fs_many.readdir(ROOT_ID).map_err(|e| anyhow::anyhow!("{e:?}"))?.len();
    let readdir = t.elapsed();
    if entries as u64 != many {
        bail!("listed {entries} of {many} files");
    }
    rows.push(Row { name: "Open a drive with many files", result: secs(listed), note: format!("{many} files: fetch the listing and build the tree") });
    rows.push(Row { name: "List that folder again", result: secs(readdir), note: "from the tree in memory".into() });
    // The kernel's NFS client asks for a folder a page at a time.
    let page = 100;
    let t = Instant::now();
    let (mut after, mut paged) = (0, 0u64);
    loop {
        let (entries, end) = fs_many.readdir_page(ROOT_ID, after, page).map_err(|e| anyhow::anyhow!("{e:?}"))?;
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

    // ---- writes
    client.create_drive("work").await?;
    let work = rig.drive("work")?;
    work.refresh().await?;
    let flood = if o.quick { 100u64 } else { 1000 };
    let dir = work.mkdir(ROOT_ID, "flood").map_err(|e| anyhow::anyhow!("{e:?}"))?;
    let t = Instant::now();
    for i in 0..flood {
        let id = work.create(dir, &format!("note_{i:04}.txt"), false).map_err(|e| anyhow::anyhow!("{e:?}"))?;
        work.write(id, 0, &synth::bytes(1000 + i, 0, 4096)).map_err(|e| anyhow::anyhow!("{e:?}"))?;
    }
    let local = t.elapsed();
    let t = Instant::now();
    if !work.flush(Duration::from_secs(1800)).await {
        bail!("flood did not finish uploading: {:?}", work.pending());
    }
    let synced = t.elapsed();
    let on_server = client.list("work").await?.objects.iter().filter(|o| o.key.starts_with("flood/") && !o.key.ends_with('/')).count() as u64;
    if on_server != flood {
        bail!("{on_server} of {flood} flood files reached the cloud");
    }
    rows.push(Row { name: "Create many small files", result: secs(local), note: format!("{flood} files of 4 KiB, until the writes returned") });
    rows.push(Row {
        name: "Upload those files",
        result: format!("{} ({:.0} files/s)", secs(synced), flood as f64 / synced.as_secs_f64()),
        note: format!("until all {flood} are in the cloud"),
    });

    let copy_len = if o.quick { 8 * MIB } else { 64 * MIB };
    let data = synth::bytes(4242, 0, copy_len as usize);
    let id = work.create(ROOT_ID, "copy.bin", false).map_err(|e| anyhow::anyhow!("{e:?}"))?;
    let t = Instant::now();
    for (i, piece) in data.chunks(BLOCK_SIZE as usize).enumerate() {
        work.write(id, i as u64 * BLOCK_SIZE, piece).map_err(|e| anyhow::anyhow!("{e:?}"))?;
    }
    let wrote = t.elapsed();
    let t = Instant::now();
    if !work.flush(Duration::from_secs(600)).await {
        bail!("copy did not upload: {:?}", work.pending());
    }
    let uploaded = t.elapsed();
    rows.push(Row { name: "Copy a file in: writes return", result: rate(copy_len, wrote), note: format!("{} MiB of incompressible data into the journal", copy_len / MIB) });
    rows.push(Row { name: "Copy a file in: upload", result: format!("{} ({})", secs(uploaded), rate(copy_len, uploaded)), note: format!("cut, hash, send shards, commit; the link allows {link_mb_s}") });

    let id2 = work.create(ROOT_ID, "copy of copy.bin", false).map_err(|e| anyhow::anyhow!("{e:?}"))?;
    for (i, piece) in data.chunks(BLOCK_SIZE as usize).enumerate() {
        work.write(id2, i as u64 * BLOCK_SIZE, piece).map_err(|e| anyhow::anyhow!("{e:?}"))?;
    }
    let t = Instant::now();
    if !work.flush(Duration::from_secs(600)).await {
        bail!("duplicate did not upload: {:?}", work.pending());
    }
    rows.push(Row { name: "Copy the same content in again: upload", result: secs(t.elapsed()), note: "every shard is already in the cloud".into() });

    // Space's two published comparisons.
    work.refresh().await?;
    work.write(id, copy_len / 2 + 777, &synth::bytes(77, 0, 4096)).map_err(|e| anyhow::anyhow!("{e:?}"))?;
    let t = Instant::now();
    if !work.flush(Duration::from_secs(600)).await {
        bail!("patch did not upload: {:?}", work.pending());
    }
    let patched = t.elapsed();
    rows.push(Row {
        name: "Change 4 KiB inside that file: upload",
        result: format!("{} ({:.1}x faster than re-uploading)", secs(patched), uploaded.as_secs_f64() / patched.as_secs_f64()),
        note: "Space states about 11x for 4 KiB in 64 MiB".into(),
    });

    work.refresh().await?;
    rig.cache.clear();
    let obj = client.list("work").await?.objects.into_iter().find(|o| o.key == "copy.bin").context("copy.bin")?;
    let t = Instant::now();
    work.read(id, copy_len / 3, 64 * 1024).await.map_err(|e| anyhow::anyhow!("read: {e:?}"))?;
    let part = t.elapsed();
    let t = Instant::now();
    client.get_range("work", "copy.bin", &obj.version, 0, obj.size).await?;
    let whole = t.elapsed();
    rows.push(Row {
        name: "Read 64 KiB out of that file, cold",
        result: format!("{} ({:.0}x faster than fetching it whole)", secs(part), whole.as_secs_f64() / part.as_secs_f64()),
        note: "Space states about 34x for 64 KiB of 64 MiB".into(),
    });

    // ---- the same engine at 100 ms, where round trips dominate
    client.set_server_config(Some(100), Some(o.mbps)).await?;
    let slow = work.create(ROOT_ID, "slow link.bin", false).map_err(|e| anyhow::anyhow!("{e:?}"))?;
    let data = synth::bytes(4343, 0, copy_len as usize);
    for (i, piece) in data.chunks(BLOCK_SIZE as usize).enumerate() {
        work.write(slow, i as u64 * BLOCK_SIZE, piece).map_err(|e| anyhow::anyhow!("{e:?}"))?;
    }
    let t = Instant::now();
    if !work.flush(Duration::from_secs(600)).await {
        bail!("slow copy did not upload: {:?}", work.pending());
    }
    let slow_upload = t.elapsed();
    work.refresh().await?;
    work.write(slow, copy_len / 2 + 777, &synth::bytes(78, 0, 4096)).map_err(|e| anyhow::anyhow!("{e:?}"))?;
    let t = Instant::now();
    if !work.flush(Duration::from_secs(600)).await {
        bail!("slow patch did not upload: {:?}", work.pending());
    }
    let slow_patch = t.elapsed();
    rows.push(Row {
        name: "Change 4 KiB in a file, at 100 ms latency",
        result: format!("{} ({:.1}x faster than the {} re-upload)", secs(slow_patch), slow_upload.as_secs_f64() / slow_patch.as_secs_f64(), secs(slow_upload)),
        note: format!("{} MiB file; round trips dominate here", copy_len / MIB),
    });
    client.set_server_config(Some(o.latency_ms), Some(o.mbps)).await?;

    // ---- a large drive
    let big = rig.drive("huge")?;
    let t = Instant::now();
    big.refresh().await?;
    rows.push(Row { name: "Open a large drive", result: secs(t.elapsed()), note: format!("{huge} files in {} folders", huge / 100) });
    client.put_synthetic("huge", "arrivals/new clip.mov", 123_456, 1).await?;
    let t = Instant::now();
    if !big.refresh().await? {
        bail!("the large drive did not take the remote change");
    }
    let noticed = t.elapsed();
    if big.find_path("arrivals/new clip.mov").is_none() {
        bail!("the remote change did not appear");
    }
    rows.push(Row { name: "Pick up one remote change in it", result: secs(noticed), note: "after the long-poll woke: fetch what changed and apply it".into() });

    let (into, gone) = if o.quick { (100u64, 200u64) } else { (500, 1000) };
    let incoming = big.mkdir(ROOT_ID, "incoming").map_err(|e| anyhow::anyhow!("{e:?}"))?;
    for i in 0..into {
        let id = big.create(incoming, &format!("note_{i:04}.txt"), false).map_err(|e| anyhow::anyhow!("{e:?}"))?;
        big.write(id, 0, &synth::bytes(9000 + i, 0, 4096)).map_err(|e| anyhow::anyhow!("{e:?}"))?;
    }
    let t = Instant::now();
    if !big.flush(Duration::from_secs(1800)).await {
        bail!("uploads into the large drive did not finish: {:?}", big.pending());
    }
    let up = t.elapsed();
    rows.push(Row { name: "Upload small files into it", result: format!("{} ({:.0} files/s)", secs(up), into as f64 / up.as_secs_f64()), note: format!("{into} files of 4 KiB") });
    for i in 0..gone {
        let dir = big.find_path(&format!("shoot_{:03}", i / 100)).context("shoot folder")?;
        big.remove(dir, &format!("clip_{:02}.mov", i % 100)).map_err(|e| anyhow::anyhow!("{e:?}"))?;
    }
    let t = Instant::now();
    if !big.flush(Duration::from_secs(1800)).await {
        bail!("deletes in the large drive did not finish: {:?}", big.pending());
    }
    let del = t.elapsed();
    rows.push(Row { name: "Delete files from it", result: format!("{} ({:.0} files/s)", secs(del), gone as f64 / del.as_secs_f64()), note: format!("{gone} deletes, replayed in order") });
    let left = client.list("huge").await?.objects.iter().filter(|o| !o.key.ends_with('/')).count() as u64;
    if left != huge + 1 + into - gone {
        bail!("the large drive holds {left} files, expected {}", huge + 1 + into - gone);
    }

    for fs in [fs, fs_many, work, big] {
        fs.stop();
    }
    drop(server);
    Ok(rows)
}
