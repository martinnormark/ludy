//! Chaos tests: the engine against a cloud that dies, stalls, restarts and
//! loses things in the middle of the work.
//!
//! Every test ends with the same oracle: once the cloud is back and the
//! drive has settled, the cloud holds exactly the files the application
//! wrote, byte for byte, and nothing else. Faults are triggered on observed
//! events (so many shards received, a commit waiting), not on sleeps, and
//! every step runs under a timeout: a hang is a failure.

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ludy_cloud::harness::Harness;
use ludy_cloud::{synth, Config, LogEntry};
use ludy_core::cache::{BlockCache, BLOCK_SIZE};
use ludy_core::client::CloudClient;
use ludy_core::drive::{DriveFs, FsError, ROOT_ID};
use ludy_core::stats::Stats;

const MIB: usize = 1 << 20;

struct Chaos {
    dir: tempfile::TempDir,
    server: Harness,
    client: CloudClient,
    cache: Arc<BlockCache>,
    stats: Arc<Stats>,
    fs: Arc<DriveFs>,
    /// What the application believes is on the drive: key -> content.
    expect: BTreeMap<String, Vec<u8>>,
}

async fn chaos(latency_ms: u64) -> Chaos {
    let dir = tempfile::tempdir().unwrap();
    let cfg = Config { data_dir: dir.path().join("cloud"), addr: "127.0.0.1:0".parse().unwrap(), latency_ms, mbps: 0, print_log: false };
    let server = Harness::start(cfg).await.unwrap();
    let stats = Arc::new(Stats::default());
    let client = CloudClient::new(&server.url(), stats.clone());
    client.create_drive("media").await.unwrap();
    let cache = Arc::new(BlockCache::open(&dir.path().join("cache"), 256 << 20).unwrap());
    let fs = DriveFs::new("media", client.clone(), cache.clone(), stats.clone(), dir.path().join("journal")).unwrap();
    fs.refresh().await.unwrap();
    Chaos { dir, server, client, cache, stats, fs, expect: BTreeMap::new() }
}

/// Fail instead of hanging.
async fn within<T>(secs: u64, what: &str, f: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(secs), f).await.unwrap_or_else(|_| panic!("{what} did not finish within {secs} s"))
}

async fn until(secs: u64, what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while !cond() {
        assert!(Instant::now() < deadline, "{what} did not happen within {secs} s");
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
}

fn id(fs: &DriveFs, path: &str) -> u64 {
    fs.find_path(path).unwrap_or_else(|| panic!("no node at {path}"))
}

impl Chaos {
    fn log(&self) -> Vec<LogEntry> {
        self.server.cloud().log_since(0)
    }

    fn shard_puts(&self) -> usize {
        self.log().iter().filter(|e| e.method == "PUT" && e.drive == "_shards" && e.status < 300).count()
    }

    /// Files that have reached the cloud so far (small ones go up in one request, without separate shards).
    fn uploads(&self) -> usize {
        self.log().iter().filter(|e| e.method == "PUT" && e.drive == "media" && e.status < 300 && !e.key.is_empty() && !e.key.ends_with('/')).count()
    }

    /// Create `dir/name` through the drive with `data`, and remember it.
    fn put(&mut self, dir: u64, name: &str, data: Vec<u8>) -> u64 {
        let file = self.fs.create(dir, name, false).unwrap();
        for (i, piece) in data.chunks(BLOCK_SIZE as usize).enumerate() {
            self.fs.write(file, i as u64 * BLOCK_SIZE, piece).unwrap();
        }
        self.expect.insert(self.fs.key_of(file), data);
        file
    }

    /// The oracle. The cloud must be up. Uploads everything, then compares
    /// the cloud with what the application wrote.
    async fn converge(&self) {
        let settled = within(120, "the final flush", self.fs.flush(Duration::from_secs(90))).await;
        assert!(settled, "the drive never settled: {:?}", self.fs.pending());
        let listing = self.client.list("media").await.unwrap();
        let files: Vec<_> = listing.objects.iter().filter(|o| !o.key.ends_with('/')).collect();
        let have: Vec<&str> = files.iter().map(|o| o.key.as_str()).collect();
        let want: Vec<&str> = self.expect.keys().map(String::as_str).collect();
        assert_eq!(have, want, "the cloud's files differ from what was written");
        for o in files {
            let want = &self.expect[&o.key];
            assert_eq!(o.size, want.len() as u64, "{} has the wrong size", o.key);
            if o.size > 0 {
                let got = self.client.get_range("media", &o.key, &o.version, 0, o.size).await.unwrap();
                assert!(got[..] == want[..], "{} has the wrong content", o.key);
            }
        }
        assert!(self.fs.pending().is_idle());
        // And the drive agrees with the cloud once it has looked again.
        assert!(self.fs.refresh().await.unwrap());
        for (key, want) in &self.expect {
            let attr = self.fs.getattr(id(&self.fs, key)).unwrap();
            assert_eq!(attr.size, want.len() as u64, "{key} has the wrong size in the drive");
        }
    }

    /// Deleting every drive must leave no shard behind: nothing leaked.
    async fn assert_no_leaks(&self) {
        for d in self.client.drives().await.unwrap() {
            self.client.delete_drive(&d.name).await.unwrap();
        }
        self.server.cloud().store.sweep_orphans();
        let stats = self.client.server_stats().await.unwrap();
        assert_eq!((stats.shards, stats.manifests), (0, 0), "shards or manifests leaked");
    }
}

// ------------------------------------------------- the scenario that was asked for

/// Flood a folder with files, start a large copy, and "deploy" the server
/// in the middle: kill it while shards are arriving, keep writing while it
/// is down, bring it back on the same data.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_flood_and_a_copy_survive_the_server_being_redeployed() {
    let mut c = chaos(2).await;
    let uploader = tokio::spawn(c.fs.clone().run_uploader());

    let dir = c.fs.mkdir(ROOT_ID, "flood").unwrap();
    for i in 0..300u64 {
        c.put(dir, &format!("note_{i:04}.txt"), synth::bytes(100 + i, 0, 2048));
    }
    c.put(ROOT_ID, "copy.bin", synth::bytes(7, 0, 12 * MIB));

    // The uploader starts after its quiet period. Kill mid-flight.
    until(30, "40 files arriving", || c.uploads() >= 40).await;
    c.server.kill();

    // The application carries on while the cloud is away.
    for i in 300..360u64 {
        c.put(dir, &format!("note_{i:04}.txt"), synth::bytes(100 + i, 0, 2048));
    }
    let late = c.put(ROOT_ID, "written while down.bin", synth::bytes(8, 0, 3 * MIB));
    // Reads of what was written locally do not need the cloud.
    let (data, _) = within(5, "a local read during the outage", c.fs.read(late, MIB as u64, 4096)).await.unwrap();
    assert!(data == synth::bytes(8, MIB as u64, 4096));
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!c.fs.pending().is_idle());

    c.server.restart().await.unwrap();
    c.converge().await;
    uploader.abort();
    c.assert_no_leaks().await;
}

/// The same flood, with the server answering 503 for a while instead of dying.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_flood_survives_the_cloud_refusing_requests() {
    let mut c = chaos(1).await;
    let uploader = tokio::spawn(c.fs.clone().run_uploader());
    let dir = c.fs.mkdir(ROOT_ID, "burst").unwrap();
    for i in 0..120u64 {
        c.put(dir, &format!("f{i:03}"), synth::bytes(500 + i, 0, 1000 + i as usize));
    }
    until(30, "20 files arriving", || c.uploads() >= 20).await;
    c.server.cloud().faults.unavailable.store(true, Ordering::Relaxed);
    c.fs.rename(ROOT_ID, "burst", ROOT_ID, "burst renamed").unwrap();
    c.expect = c.expect.iter().map(|(k, v)| (k.replacen("burst/", "burst renamed/", 1), v.clone())).collect();
    c.fs.remove(id(&c.fs, "burst renamed"), "f007").unwrap();
    c.expect.remove("burst renamed/f007");
    tokio::time::sleep(Duration::from_millis(400)).await;
    c.server.cloud().faults.unavailable.store(false, Ordering::Relaxed);
    c.converge().await;
    uploader.abort();
    c.assert_no_leaks().await;
}

// ------------------------------------------------- between the shards and the commit

/// The cloud cleans up shards that no manifest lists yet, while our commit
/// is on its way. The commit is refused with the list of what is missing;
/// the client sends those again and commits.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shards_swept_before_the_commit_are_sent_again() {
    let mut c = chaos(0).await;
    let cloud = c.server.cloud();
    cloud.faults.hold_commits.store(true, Ordering::Relaxed);
    c.put(ROOT_ID, "clip.bin", synth::bytes(21, 0, 5 * MIB));
    let fs = c.fs.clone();
    let flush = tokio::spawn(async move { fs.flush(Duration::from_secs(30)).await });
    until(20, "the commit reaching the cloud", || cloud.faults.commits_held.load(Ordering::Relaxed) == 1).await;
    let sent = c.shard_puts();
    assert!(cloud.store.sweep_orphans() >= 1);
    cloud.faults.hold_commits.store(false, Ordering::Relaxed);
    assert!(within(40, "the flush", flush).await.unwrap());

    let commits: Vec<u16> = c.log().iter().filter(|e| e.method == "PUT" && e.key == "clip.bin").map(|e| e.status).collect();
    assert_eq!(commits, [409, 201], "refused once, then accepted");
    assert_eq!(c.shard_puts(), 2 * sent, "every shard was sent a second time");
    c.converge().await;
    c.assert_no_leaks().await;
}

/// The server is redeployed after the shards arrived and before the commit.
/// Restarting sweeps the uncommitted shards, so the upload starts over.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_restart_between_the_shards_and_the_commit_loses_nothing() {
    let mut c = chaos(0).await;
    c.server.cloud().faults.hold_commits.store(true, Ordering::Relaxed);
    c.put(ROOT_ID, "clip.bin", synth::bytes(22, 0, 5 * MIB));
    let fs = c.fs.clone();
    let flush = tokio::spawn(async move { fs.flush(Duration::from_secs(60)).await });
    let cloud = c.server.cloud();
    until(20, "the commit reaching the cloud", || cloud.faults.commits_held.load(Ordering::Relaxed) == 1).await;
    drop(cloud);
    c.server.restart().await.unwrap();
    assert_eq!(c.client.server_stats().await.unwrap().shards, 0, "the restart swept the uncommitted shards");
    assert!(within(70, "the flush", flush).await.unwrap(), "{:?}", c.fs.pending());
    c.converge().await;
    c.assert_no_leaks().await;
}

// ------------------------------------------------- a cloud that accepts and never answers

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stalled_cloud_makes_reads_fail_in_time_and_nothing_hangs() {
    let mut c = chaos(0).await;
    c.client.put_synthetic("media", "huge.bin", 50 << 30, 9).await.unwrap();
    c.fs.refresh().await.unwrap();
    let huge = id(&c.fs, "huge.bin");
    c.fs.read(huge, 0, 4096).await.unwrap();

    c.server.cloud().faults.stall.store(true, Ordering::Relaxed);
    // A read that needs the cloud fails; it must not hang and must not invent bytes.
    let t = Instant::now();
    let cold = within(25, "a cold read against a stalled cloud", c.fs.read(huge, 30 << 30, 65536)).await;
    assert!(matches!(cold, Err(FsError::Io(_))), "{cold:?}");
    assert!(t.elapsed() < Duration::from_secs(20), "took {:?} to give up", t.elapsed());
    // What is cached still reads.
    let (warm, _) = within(2, "a cached read", c.fs.read(huge, 0, 4096)).await.unwrap();
    assert!(warm == synth::bytes(9, 0, 4096));
    // A write returns at once, and a flush gives up at its deadline.
    let file = c.put(ROOT_ID, "note.txt", b"written into the void".to_vec());
    assert!(!within(40, "a flush against a stalled cloud", c.fs.flush(Duration::from_secs(3))).await);
    let (mine, _) = within(2, "reading my own write", c.fs.read(file, 0, 100)).await.unwrap();
    assert_eq!(mine, b"written into the void");

    c.server.cloud().faults.stall.store(false, Ordering::Relaxed);
    // The same cold read now works: the failed attempt left nothing stuck.
    let (data, _) = within(25, "the cold read after the stall", c.fs.read(huge, 30 << 30, 65536)).await.unwrap();
    assert!(data == synth::bytes(9, 30 << 30, 65536));
    c.expect.insert("huge.bin".into(), Vec::new());
    let settled = within(120, "the flush after the stall", c.fs.flush(Duration::from_secs(90))).await;
    assert!(settled, "{:?}", c.fs.pending());
    assert_eq!(&c.client.get_range("media", "note.txt", &c.client.list("media").await.unwrap().objects[1].version, 0, 21).await.unwrap()[..], b"written into the void");
}

// ------------------------------------------------- two things draining the queue at once

/// Eject flushes while the background uploader is still running. Nothing may
/// be skipped or done twice.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_uploader_and_a_flush_running_together_lose_nothing() {
    let mut c = chaos(1).await;
    let uploader = tokio::spawn(c.fs.clone().run_uploader());
    for round in 0..6u64 {
        let dir = c.fs.mkdir(ROOT_ID, &format!("d{round}")).unwrap();
        for i in 0..25u64 {
            c.put(dir, &format!("f{i:02}"), synth::bytes(round * 100 + i, 0, 700));
        }
        // Rename a file, delete another, move the folder: namespace changes queue up behind each other.
        c.fs.rename(dir, "f03", dir, "f03 renamed").unwrap();
        let data = c.expect.remove(&format!("d{round}/f03")).unwrap();
        c.expect.insert(format!("d{round}/f03 renamed"), data);
        c.fs.remove(dir, "f04").unwrap();
        c.expect.remove(&format!("d{round}/f04"));
        if round % 2 == 0 {
            c.fs.rename(ROOT_ID, &format!("d{round}"), ROOT_ID, &format!("moved {round}")).unwrap();
            c.expect = c.expect.iter().map(|(k, v)| (k.replacen(&format!("d{round}/"), &format!("moved {round}/"), 1), v.clone())).collect();
        }
        let fs = c.fs.clone();
        let flush = tokio::spawn(async move { fs.flush(Duration::from_secs(60)).await });
        assert!(within(70, "a flush beside the uploader", flush).await.unwrap(), "{:?}", c.fs.pending());
    }
    c.converge().await;
    uploader.abort();
    c.assert_no_leaks().await;
}

// ------------------------------------------------- things the cloud will never accept

/// A key longer than the cloud allows can never be stored. It has to be
/// refused when it is created, not queued to block everything behind it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_path_too_long_for_the_cloud_is_refused_up_front_and_blocks_nothing() {
    let mut c = chaos(0).await;
    let long = "x".repeat(250);
    let mut dir = ROOT_ID;
    let mut refused = None;
    for depth in 0..6 {
        match c.fs.mkdir(dir, &long) {
            Ok(next) => dir = next,
            Err(e) => {
                refused = Some((depth, e));
                break;
            }
        }
    }
    let (depth, error) = refused.expect("a 1,500 byte path was accepted");
    assert_eq!(depth, 4, "four levels of 250 bytes fit in 1,024; the fifth does not");
    assert_eq!(error, FsError::NameTooLong);
    // A file whose key would be too long is refused the same way.
    assert_eq!(c.fs.create(dir, &"y".repeat(100), false), Err(FsError::NameTooLong));
    // So is a rename that would push something already there past the limit.
    let top = c.fs.mkdir(ROOT_ID, "short").unwrap();
    c.put(top, &"z".repeat(200), b"deep".to_vec());
    assert_eq!(c.fs.rename(ROOT_ID, "short", dir, "moved"), Err(FsError::NameTooLong));

    c.put(ROOT_ID, "ordinary.txt", b"still goes up".to_vec());
    c.converge().await;
}

// ------------------------------------------------- the file underneath disappears

/// Someone deletes a file in the cloud while a local patch to it is waiting.
/// The patch cannot be completed (the rest of the file is gone); the delete
/// wins, the drive says so, and other uploads are not held up.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_patch_whose_base_was_deleted_in_the_cloud_does_not_wedge_the_drive() {
    let mut c = chaos(0).await;
    c.client.put_bytes("media", "shared.bin", synth::bytes(31, 0, 3 * MIB).into()).await.unwrap();
    c.fs.refresh().await.unwrap();
    let shared = id(&c.fs, "shared.bin");
    c.fs.write(shared, 1000, b"my edit").unwrap();
    c.client.delete("media", "shared.bin").await.unwrap();
    c.put(ROOT_ID, "mine.txt", b"unrelated".to_vec());

    let settled = within(60, "the flush", c.fs.flush(Duration::from_secs(20))).await;
    assert!(settled, "one lost base wedged the drive: {:?}", c.fs.pending());
    c.converge().await;
    assert!(c.fs.find_path("shared.bin").is_none(), "the cloud's delete won");
}

/// The same, but the local change rewrote the whole file: nothing is needed
/// from the deleted version, so the file is simply created again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_full_rewrite_survives_its_base_being_deleted_in_the_cloud() {
    let mut c = chaos(0).await;
    c.client.put_bytes("media", "doc.txt", b"old old old".to_vec().into()).await.unwrap();
    c.fs.refresh().await.unwrap();
    let doc = id(&c.fs, "doc.txt");
    c.fs.set_size(doc, 0).unwrap();
    c.fs.write(doc, 0, b"entirely new").unwrap();
    c.expect.insert("doc.txt".into(), b"entirely new".to_vec());
    c.client.delete("media", "doc.txt").await.unwrap();
    c.converge().await;
}

// ------------------------------------------------- a listing that is already out of date

/// A listing fetched before an upload finished must not be applied after it:
/// it would make the file just uploaded vanish or revert.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_listing_older_than_the_last_upload_is_not_applied() {
    let mut c = chaos(0).await;
    c.client.put_bytes("media", "doc.txt", b"version one".to_vec().into()).await.unwrap();
    c.fs.refresh().await.unwrap();

    let token = c.fs.sync_token();
    let stale = c.client.list("media").await.unwrap();
    let doc = id(&c.fs, "doc.txt");
    c.fs.set_size(doc, 0).unwrap();
    c.fs.write(doc, 0, b"version two, longer").unwrap();
    c.put(ROOT_ID, "new.txt", b"brand new".to_vec());
    assert!(c.fs.flush(Duration::from_secs(20)).await);

    assert!(!c.fs.apply_listing_if_current(&stale, token), "a stale listing was applied");
    assert!(c.fs.find_path("new.txt").is_some(), "the new file vanished");
    let (data, _) = c.fs.read(doc, 0, 100).await.unwrap();
    assert_eq!(data, b"version two, longer");
    // A listing taken now is fine.
    assert!(c.fs.refresh().await.unwrap());
    c.expect.insert("doc.txt".into(), b"version two, longer".to_vec());
    c.converge().await;
}

// ------------------------------------------------- a damaged cache

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_truncated_cache_block_is_fetched_again_not_served() {
    let c = chaos(0).await;
    c.client.put_synthetic("media", "big.bin", 8 << 20, 5).await.unwrap();
    c.fs.refresh().await.unwrap();
    let big = id(&c.fs, "big.bin");
    let want = synth::bytes(5, 0, 2 * BLOCK_SIZE as usize);
    let (first, _) = c.fs.read(big, 0, 2 * BLOCK_SIZE as u32).await.unwrap();
    assert!(first == want);

    // Cut every cached block file short, as a crash or a full disk might.
    let mut damaged = 0;
    for version in std::fs::read_dir(c.dir.path().join("cache")).unwrap().flatten().filter(|e| e.path().is_dir()) {
        for block in std::fs::read_dir(version.path()).unwrap().flatten() {
            let f = std::fs::OpenOptions::new().write(true).open(block.path()).unwrap();
            f.set_len(1000).unwrap();
            damaged += 1;
        }
    }
    assert!(damaged >= 2);
    let before = c.stats.sample().range_requests;
    let (again, _) = c.fs.read(big, 0, 2 * BLOCK_SIZE as u32).await.unwrap();
    assert!(again == want, "a truncated block was served");
    assert!(c.stats.sample().range_requests > before, "the blocks were fetched again");
    assert!(c.cache.snapshot().bytes >= 2 * BLOCK_SIZE);
}

// ------------------------------------------------- the app dies with writes unsent

/// The application wrote files and the cloud was down; then the app was
/// killed (or the Mac lost power). Starting again must find those writes
/// and upload them: nothing a `write` returned success for may be lost.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unsent_writes_survive_the_app_dying_while_the_cloud_is_down() {
    let mut c = chaos(0).await;
    c.client.put_bytes("media", "existing.bin", synth::bytes(41, 0, 4 * MIB).into()).await.unwrap();
    c.client.put_bytes("media", "old name.txt", b"renamed while offline".to_vec().into()).await.unwrap();
    c.client.put_bytes("media", "doomed.txt", b"deleted while offline".to_vec().into()).await.unwrap();
    c.fs.refresh().await.unwrap();
    c.expect.insert("existing.bin".into(), synth::bytes(41, 0, 4 * MIB));

    c.server.kill();
    // New files, a patch to a cloud file, a folder, a rename and a delete, all offline.
    let dir = c.fs.mkdir(ROOT_ID, "offline").unwrap();
    for i in 0..20u64 {
        c.put(dir, &format!("n{i:02}.txt"), synth::bytes(900 + i, 0, 3000));
    }
    c.put(dir, "big.bin", synth::bytes(42, 0, 3 * MIB));
    c.put(ROOT_ID, "empty.txt", Vec::new());
    let existing = id(&c.fs, "existing.bin");
    c.fs.write(existing, 2 * MIB as u64 + 5, b"patched offline").unwrap();
    c.expect.get_mut("existing.bin").unwrap()[2 * MIB + 5..2 * MIB + 20].copy_from_slice(b"patched offline");
    c.fs.rename(ROOT_ID, "old name.txt", dir, "new name.txt").unwrap();
    c.expect.insert("offline/new name.txt".into(), b"renamed while offline".to_vec());
    c.fs.remove(ROOT_ID, "doomed.txt").unwrap();
    assert!(!within(10, "a flush with the cloud down", c.fs.flush(Duration::from_secs(1))).await);

    // The process dies: no eject, no stop. A new one starts on the same folders.
    let old = std::mem::replace(
        &mut c.fs,
        DriveFs::new("unused", c.client.clone(), c.cache.clone(), c.stats.clone(), c.dir.path().join("journal-unused")).unwrap(),
    );
    drop(old);
    c.server.restart().await.unwrap();
    c.fs = DriveFs::new("media", c.client.clone(), c.cache.clone(), c.stats.clone(), c.dir.path().join("journal")).unwrap();
    c.fs.refresh().await.unwrap();

    // Before anything uploads, the drive already shows what was written.
    assert_eq!(c.fs.getattr(id(&c.fs, "offline/big.bin")).unwrap().size, 3 * MIB as u64);
    assert!(c.fs.find_path("doomed.txt").is_none());
    assert!(c.fs.find_path("old name.txt").is_none());
    let (patched, _) = c.fs.read(id(&c.fs, "existing.bin"), 2 * MIB as u64, 30).await.unwrap();
    assert!(patched[..] == c.expect["existing.bin"][2 * MIB..2 * MIB + 30]);
    assert!(!c.fs.pending().is_idle());

    c.converge().await;
    c.assert_no_leaks().await;
}

/// Quitting with unsent writes must keep them for the next start too.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unsent_writes_survive_a_quit_with_the_cloud_down() {
    let mut c = chaos(0).await;
    c.server.kill();
    c.put(ROOT_ID, "draft.txt", b"typed on a plane".to_vec());
    assert!(!within(10, "a flush with the cloud down", c.fs.flush(Duration::from_secs(1))).await);
    c.fs.stop();

    c.server.restart().await.unwrap();
    c.fs = DriveFs::new("media", c.client.clone(), c.cache.clone(), c.stats.clone(), c.dir.path().join("journal")).unwrap();
    c.fs.refresh().await.unwrap();
    assert!(c.fs.find_path("draft.txt").is_some());
    c.converge().await;

    // A clean stop with nothing pending leaves no journal behind.
    c.fs.stop();
    assert_eq!(std::fs::read_dir(c.dir.path().join("journal")).map(|d| d.count()).unwrap_or(0), 0);
}
