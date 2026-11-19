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
        let extra: Vec<&&str> = have.iter().filter(|k| !want.contains(k)).collect();
        let missing: Vec<&&str> = want.iter().filter(|k| !have.contains(k)).collect();
        assert!(extra.is_empty() && missing.is_empty(), "the cloud's files differ from what was written: extra {extra:?}, missing {missing:?}");
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
        // Nothing more and nothing less than the cloud lists: no file a
        // delete should have removed, no stray folder.
        let want = tree_from(&self.client.list("media").await.unwrap());
        let have = tree_of(&self.fs);
        let extra: Vec<_> = have.difference(&want).collect();
        let missing: Vec<_> = want.difference(&have).collect();
        assert!(extra.is_empty() && missing.is_empty(), "the drive shows extra {extra:?} and lacks {missing:?}");
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

// ------------------------------------------------- second pass: nastier interleavings

/// `mv` on a cloud file, then open it at once: the rename has not reached
/// the cloud yet, so the content still lives under the old name there.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cloud_file_renamed_locally_can_be_read_and_patched_before_the_rename_syncs() {
    let mut c = chaos(0).await;
    let body = synth::bytes(51, 0, 3 * MIB);
    c.client.put_bytes("media", "rushes/take one.mov", body.clone().into()).await.unwrap();
    c.fs.refresh().await.unwrap();
    let dir = id(&c.fs, "rushes");
    c.fs.rename(dir, "take one.mov", dir, "take 1 (selected).mov").unwrap();
    c.fs.rename(ROOT_ID, "rushes", ROOT_ID, "selects").unwrap();
    let file = id(&c.fs, "selects/take 1 (selected).mov");

    // Nothing has synced. Reading must go to the cloud's old key.
    let (got, _) = within(10, "reading a renamed file", c.fs.read(file, MIB as u64, 5000)).await.unwrap();
    assert!(got == body[MIB..MIB + 5000]);
    // Patch it under its new name, then let everything sync.
    c.fs.write(file, 2 * MIB as u64, b"cut here").unwrap();
    let mut want = body.clone();
    want[2 * MIB..2 * MIB + 8].copy_from_slice(b"cut here");
    let (got, _) = c.fs.read(file, 2 * MIB as u64 - 4, 16).await.unwrap();
    assert!(got == want[2 * MIB - 4..2 * MIB + 12]);
    c.expect.insert("selects/take 1 (selected).mov".into(), want);
    c.converge().await;
    // A patch, not a re-upload: under 3 MiB went up.
    assert!(c.stats.sample().bytes_up < 3 * MIB as u64 + 3 * MIB as u64, "{}", c.stats.sample().bytes_up);
    c.assert_no_leaks().await;
}

/// The application keeps working on files whose upload is in flight:
/// writes more, renames one, deletes another.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn files_changed_renamed_and_deleted_while_they_upload_end_up_right() {
    let mut c = chaos(0).await;
    let cloud = c.server.cloud();
    cloud.faults.hold_commits.store(true, Ordering::Relaxed);
    let grows = c.put(ROOT_ID, "grows.bin", synth::bytes(61, 0, 2 * MIB));
    c.put(ROOT_ID, "moves.bin", synth::bytes(62, 0, 2 * MIB));
    c.put(ROOT_ID, "dies.bin", synth::bytes(63, 0, 2 * MIB));
    let fs = c.fs.clone();
    let flush = tokio::spawn(async move { fs.flush(Duration::from_secs(60)).await });
    until(20, "three commits reaching the cloud", || cloud.faults.commits_held.load(Ordering::Relaxed) == 3).await;

    // All three are mid-upload now.
    c.fs.write(grows, 2 * MIB as u64, b" and a tail").unwrap();
    c.expect.get_mut("grows.bin").unwrap().extend_from_slice(b" and a tail");
    c.fs.rename(ROOT_ID, "moves.bin", ROOT_ID, "moved.bin").unwrap();
    let moved = c.expect.remove("moves.bin").unwrap();
    c.expect.insert("moved.bin".into(), moved);
    c.fs.remove(ROOT_ID, "dies.bin").unwrap();
    c.expect.remove("dies.bin");

    cloud.faults.hold_commits.store(false, Ordering::Relaxed);
    assert!(within(70, "the flush", flush).await.unwrap(), "{:?}", c.fs.pending());
    c.converge().await;
    c.assert_no_leaks().await;
}

/// Names that need care in a URL, in a path, or in a JSON file.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn awkward_names_round_trip() {
    let mut c = chaos(0).await;
    let names = [
        "100% done?.txt",
        "a#b&c=d+e.txt",
        "naïve café — résumé.txt",
        "日本語のファイル.txt",
        "emoji 🎬🎞.mov",
        " leading and trailing space ",
        "back\\slash and \"quotes\".txt",
        "semi;colon,comma.txt",
        "..hidden..dots..",
        "tab\there.txt",
    ];
    let dir = c.fs.mkdir(ROOT_ID, "strange names & folders #1").unwrap();
    for (i, name) in names.iter().enumerate() {
        c.put(dir, name, synth::bytes(700 + i as u64, 0, 1000 + i));
    }
    let longest = "n".repeat(255);
    c.put(dir, &longest, b"255 byte name".to_vec());
    c.fs.rename(dir, "100% done?.txt", dir, "200% done??.txt").unwrap();
    let moved = c.expect.remove("strange names & folders #1/100% done?.txt").unwrap();
    c.expect.insert("strange names & folders #1/200% done??.txt".into(), moved);
    c.fs.remove(dir, "tab\there.txt").unwrap();
    c.expect.remove("strange names & folders #1/tab\there.txt");
    c.converge().await;

    // And they survive a crash on their way up.
    c.server.kill();
    c.put(dir, "written offline ✈.txt", b"offline".to_vec());
    c.fs.rename(dir, "emoji 🎬🎞.mov", dir, "emoji renamed 🎬.mov").unwrap();
    let moved = c.expect.remove("strange names & folders #1/emoji 🎬🎞.mov").unwrap();
    c.expect.insert("strange names & folders #1/emoji renamed 🎬.mov".into(), moved);
    c.server.restart().await.unwrap();
    c.fs = DriveFs::new("media", c.client.clone(), c.cache.clone(), c.stats.clone(), c.dir.path().join("journal")).unwrap();
    c.fs.refresh().await.unwrap();
    c.converge().await;
}

/// Two writers: someone else replaces a file in the cloud while a local
/// patch to the old version is waiting. The local save lands last and wins;
/// the other version stays in the history.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_local_patch_and_a_remote_edit_both_end_up_in_the_history() {
    let mut c = chaos(0).await;
    let v1 = synth::bytes(71, 0, 3 * MIB);
    c.client.put_bytes("media", "shared.bin", v1.clone().into()).await.unwrap();
    c.fs.refresh().await.unwrap();
    let file = id(&c.fs, "shared.bin");
    c.fs.write(file, 100, b"mine").unwrap();
    let theirs = synth::bytes(72, 0, 2 * MIB);
    c.client.put_bytes("media", "shared.bin", theirs.clone().into()).await.unwrap();

    let mut mine = v1.clone();
    mine[100..104].copy_from_slice(b"mine");
    c.expect.insert("shared.bin".into(), mine);
    c.converge().await;
    let versions = c.client.versions("media", "shared.bin").await.unwrap();
    assert_eq!(versions.len(), 3, "original, theirs, mine");
    let second = c.client.get_range("media", "shared.bin", &versions[1].id, 0, versions[1].size).await.unwrap();
    assert!(second[..] == theirs[..], "the other writer's version is still there");
}

/// Quit in the middle of an upload, start again: the upload is done over.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_quit_in_the_middle_of_an_upload_is_finished_by_the_next_start() {
    let mut c = chaos(0).await;
    let cloud = c.server.cloud();
    cloud.faults.hold_commits.store(true, Ordering::Relaxed);
    c.put(ROOT_ID, "half sent.bin", synth::bytes(81, 0, 6 * MIB));
    c.put(ROOT_ID, "small.txt", b"also on its way".to_vec());
    let fs = c.fs.clone();
    let flush = tokio::spawn(async move { fs.flush(Duration::from_secs(60)).await });
    until(20, "a commit reaching the cloud", || cloud.faults.commits_held.load(Ordering::Relaxed) >= 1).await;
    // The process goes away with requests in flight.
    flush.abort();
    c.fs.stop();
    cloud.faults.hold_commits.store(false, Ordering::Relaxed);

    c.fs = DriveFs::new("media", c.client.clone(), c.cache.clone(), c.stats.clone(), c.dir.path().join("journal")).unwrap();
    c.fs.refresh().await.unwrap();
    c.converge().await;
    c.assert_no_leaks().await;
}

/// Found by the soak: a new file is renamed over a file the cloud has, and
/// deleted again before it was ever uploaded. The cloud's file must go too.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn replacing_a_cloud_file_with_a_new_one_and_deleting_that_leaves_nothing() {
    let mut c = chaos(0).await;
    c.client.put_bytes("media", "report.txt", b"the old report".to_vec().into()).await.unwrap();
    c.client.put_bytes("media", "notes/keep.txt", b"keep".to_vec().into()).await.unwrap();
    c.fs.refresh().await.unwrap();
    c.expect.insert("notes/keep.txt".into(), b"keep".to_vec());

    let draft = c.fs.create(ROOT_ID, "draft.txt", false).unwrap();
    c.fs.write(draft, 0, b"the new report").unwrap();
    c.fs.rename(ROOT_ID, "draft.txt", ROOT_ID, "report.txt").unwrap();
    c.fs.remove(ROOT_ID, "report.txt").unwrap();
    c.converge().await;

    // The same, but the replacement is kept: one file, the new content.
    c.client.put_bytes("media", "plan.txt", b"old plan".to_vec().into()).await.unwrap();
    c.fs.refresh().await.unwrap();
    let draft = c.fs.create(ROOT_ID, "plan draft.txt", false).unwrap();
    c.fs.write(draft, 0, b"new plan").unwrap();
    c.fs.rename(ROOT_ID, "plan draft.txt", ROOT_ID, "plan.txt").unwrap();
    c.expect.insert("plan.txt".into(), b"new plan".to_vec());
    c.converge().await;
}

// ------------------------------------------------- the change feed

/// Everything the drive shows: (path, is a folder, size).
fn tree_of(fs: &DriveFs) -> std::collections::BTreeSet<(String, bool, u64)> {
    let mut out = std::collections::BTreeSet::new();
    let mut stack = vec![(ROOT_ID, String::new())];
    while let Some((dir, path)) = stack.pop() {
        for (name, attr) in fs.readdir(dir).unwrap() {
            let child = if path.is_empty() { name } else { format!("{path}/{name}") };
            out.insert((child.clone(), attr.is_dir, attr.size));
            if attr.is_dir {
                stack.push((attr.id, child));
            }
        }
    }
    out
}

/// What a full listing says the drive should show.
fn tree_from(listing: &ludy_core::client::Listing) -> std::collections::BTreeSet<(String, bool, u64)> {
    let mut out = std::collections::BTreeSet::new();
    for o in &listing.objects {
        let (path, is_marker) = match o.key.strip_suffix('/') {
            Some(p) => (p, true),
            None => (o.key.as_str(), false),
        };
        let mut folder = String::new();
        let parts: Vec<&str> = path.split('/').collect();
        for part in &parts[..parts.len() - usize::from(!is_marker)] {
            if !folder.is_empty() {
                folder.push('/');
            }
            folder.push_str(part);
            out.insert((folder.clone(), true, 0));
        }
        if !is_marker {
            out.insert((o.key.clone(), false, o.size));
        }
    }
    out
}

/// Someone else works in the drive. After every change the drive asks only
/// for what changed, and must end up showing exactly what a full listing
/// would: files, folders that exist only through their content, folders
/// kept by a marker, renames, restores, a file and a folder trading names.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn applying_only_what_changed_matches_a_full_listing() {
    let seed: u64 = std::env::var("CHAOS_SEED").ok().and_then(|s| s.parse().ok()).unwrap_or(7);
    let mut c = chaos(0).await;
    let mut n = 0u64;
    let mut rnd = move |below: u64| {
        n += 1;
        synth::word(seed ^ 0xfeed, n) % below.max(1)
    };
    let folders = ["", "a", "a/b", "c", "c/d/e"];
    for step in 0..400u64 {
        let listing = c.client.list("media").await.unwrap();
        let files: Vec<&str> = listing.objects.iter().map(|o| o.key.as_str()).filter(|k| !k.ends_with('/')).collect();
        let folder = folders[rnd(folders.len() as u64) as usize];
        let at = |name: &str| if folder.is_empty() { name.to_string() } else { format!("{folder}/{name}") };
        let what = match rnd(20) {
            0..=6 => {
                let key = at(&format!("f{}", rnd(8)));
                c.client.put_bytes("media", &key, synth::bytes(step, 0, 8 * (1 + rnd(300) as usize)).into()).await.unwrap();
                format!("put {key}")
            }
            7..=9 if !files.is_empty() => {
                let key = files[rnd(files.len() as u64) as usize].to_string();
                c.client.delete("media", &key).await.unwrap();
                format!("delete {key}")
            }
            10 if !folder.is_empty() => {
                c.client.delete("media", &format!("{folder}/")).await.unwrap();
                format!("delete folder {folder}")
            }
            11 => {
                let key = at(&format!("empty{}/", rnd(3)));
                c.client.put_bytes("media", &key, Vec::new().into()).await.unwrap();
                format!("marker {key}")
            }
            12..=13 if !files.is_empty() => {
                let from = files[rnd(files.len() as u64) as usize].to_string();
                let to = at(&format!("f{}", rnd(8)));
                c.client.rename("media", &from, &to).await.unwrap();
                format!("rename {from} -> {to}")
            }
            14 => {
                let (from, to) = (folders[1 + rnd(4) as usize], format!("moved{}", rnd(2)));
                c.client.rename("media", from, &to).await.unwrap();
                format!("rename folder {from} -> {to}")
            }
            15 if !files.is_empty() => {
                let key = files[rnd(files.len() as u64) as usize].to_string();
                let versions = c.client.versions("media", &key).await.unwrap();
                c.client.restore("media", &key, &versions[0].id).await.unwrap();
                format!("restore {key}")
            }
            // A file and a folder trade the name "swap".
            16 => {
                c.client.delete("media", "swap/").await.unwrap();
                c.client.put_bytes("media", "swap", b"a file now".to_vec().into()).await.unwrap();
                "swap becomes a file".to_string()
            }
            17 => {
                c.client.delete("media", "swap").await.unwrap();
                c.client.put_bytes("media", "swap/inner.txt", b"a folder now".to_vec().into()).await.unwrap();
                "swap becomes a folder".to_string()
            }
            _ => "nothing".to_string(),
        };
        // Look after one to five changes, not after every one.
        if rnd(3) != 0 && step + 1 < 400 {
            continue;
        }
        assert!(c.fs.refresh().await.unwrap(), "step {step}: {what}: not applied");
        let want = tree_from(&c.client.list("media").await.unwrap());
        let have = tree_of(&c.fs);
        let extra: Vec<_> = have.difference(&want).collect();
        let missing: Vec<_> = want.difference(&have).collect();
        assert!(extra.is_empty() && missing.is_empty(), "seed {seed}, step {step} ({what}): the drive shows extra {extra:?}, lacks {missing:?}");
    }
    // It really was incremental: one listing by the drive at the start, then feeds.
    let log = c.log();
    assert!(log.iter().filter(|e| e.method == "GET" && e.key.starts_with("?feed")).count() >= 100);

    // After a redeploy the cloud cannot say what changed; the drive falls
    // back to a full listing and carries on from there.
    c.client.put_bytes("media", "before the restart.txt", b"x".to_vec().into()).await.unwrap();
    c.server.restart().await.unwrap();
    c.client.put_bytes("media", "after the restart.txt", b"y".to_vec().into()).await.unwrap();
    assert!(c.fs.refresh().await.unwrap());
    assert_eq!(tree_of(&c.fs), tree_from(&c.client.list("media").await.unwrap()));
    let feeds: Vec<String> = c.log().iter().filter(|e| e.key.starts_with("?feed")).map(|e| e.key.clone()).collect();
    assert_eq!(feeds.len(), 1);
    assert!(feeds[0].contains("reset"), "{feeds:?}");
    c.client.put_bytes("media", "and then.txt", b"z".to_vec().into()).await.unwrap();
    assert!(c.fs.refresh().await.unwrap());
    assert!(c.fs.find_path("and then.txt").is_some());
    assert!(c.log().iter().any(|e| e.key.starts_with("?feed") && !e.key.contains("reset")));
}

/// A small patch is one request: the manifest it is planned against is
/// remembered, and its shards travel with the commit.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_small_patch_is_a_single_request() {
    let mut c = chaos(0).await;
    let mut data = synth::bytes(95, 0, 20 * MIB);
    let file = c.put(ROOT_ID, "film.bin", data.clone());
    assert!(c.fs.flush(Duration::from_secs(30)).await);
    let before = c.log().len();

    for (round, at) in [(0u64, 7 * MIB + 5), (1, 13 * MIB + 9)] {
        let patch = synth::bytes(96 + round, 0, 4096);
        c.fs.write(file, at as u64, &patch).unwrap();
        data[at..at + 4096].copy_from_slice(&patch);
        assert!(c.fs.flush(Duration::from_secs(30)).await);
    }
    c.expect.insert("film.bin".into(), data);
    let log = c.log();
    let mine: Vec<&LogEntry> = log[before..].iter().filter(|e| e.method != "GET" || !e.key.is_empty()).collect();
    let asked = mine.iter().filter(|e| e.method == "POST" && e.drive == "_shards").count();
    let manifests = mine.iter().filter(|e| e.key.contains("?manifest")).count();
    let commits = mine.iter().filter(|e| e.method == "PUT" && e.key == "film.bin").count();
    let shards = mine.iter().filter(|e| e.method == "PUT" && e.drive == "_shards").count();
    assert_eq!((asked, manifests, commits), (0, 0, 2), "two patches, two requests: {mine:?}");
    assert!((2..=6).contains(&shards), "{shards} shards travelled inside those requests");
    c.converge().await;

    // A file someone else uploaded: the first patch has to ask for its manifest, the second does not.
    let mut theirs = synth::bytes(97, 0, 12 * MIB);
    c.client.put_bytes("media", "theirs.bin", theirs.clone().into()).await.unwrap();
    c.fs.refresh().await.unwrap();
    let file = id(&c.fs, "theirs.bin");
    let before = c.log().len();
    for (round, at) in [(0u64, 3 * MIB), (1, 9 * MIB)] {
        c.fs.write(file, at as u64, &synth::bytes(98 + round, 0, 1000)).unwrap();
        theirs[at..at + 1000].copy_from_slice(&synth::bytes(98 + round, 0, 1000));
        assert!(c.fs.flush(Duration::from_secs(30)).await);
        c.fs.refresh().await.unwrap();
    }
    let log = c.log();
    assert_eq!(log[before..].iter().filter(|e| e.key.contains("?manifest")).count(), 1);
    c.expect.insert("theirs.bin".into(), theirs);
    c.converge().await;
    c.assert_no_leaks().await;
}

/// The drive uploaded the file itself and remembers its manifest. Someone
/// deletes the file in the cloud; a local patch follows. Two outcomes are
/// acceptable and both are checked: if the drive still holds the rest of
/// the file, the edit is saved and the file exists again (the last writer
/// wins and nothing is lost); if it does not, the cloud's delete wins and
/// the drive says so. What must not happen is an upload that fails for ever.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_patch_to_a_file_the_drive_uploaded_and_someone_deleted_does_not_wedge() {
    for cache_cleared in [false, true] {
        let mut c = chaos(0).await;
        let original = synth::bytes(111, 0, 6 * MIB);
        let file = c.put(ROOT_ID, "mine.bin", original.clone());
        assert!(c.fs.flush(Duration::from_secs(30)).await);
        c.fs.refresh().await.unwrap();
        c.client.delete("media", "mine.bin").await.unwrap();
        assert_eq!(c.client.server_stats().await.unwrap().shards, 0, "the cloud dropped the content with the file");
        if cache_cleared {
            c.cache.clear();
        }
        c.fs.write(file, 3 * MIB as u64 + 10, b"edited after the delete").unwrap();
        c.put(ROOT_ID, "bystander.txt", b"must not be held up".to_vec());

        let settled = within(60, "the flush", c.fs.flush(Duration::from_secs(25))).await;
        assert!(settled, "cache cleared: {cache_cleared}: the drive wedged: {:?}", c.fs.pending());
        if cache_cleared {
            c.expect.remove("mine.bin");
            c.converge().await;
            assert!(c.fs.find_path("mine.bin").is_none());
            assert!(c.fs.pending().last_error.is_some_and(|e| e.contains("dropped")));
        } else {
            let mut edited = original.clone();
            edited[3 * MIB + 10..3 * MIB + 33].copy_from_slice(b"edited after the delete");
            c.expect.insert("mine.bin".into(), edited);
            c.converge().await;
        }
        c.assert_no_leaks().await;
    }
}

/// Deleting the last file in a folder, or moving it out, leaves the folder
/// in place, in the drive and in the cloud, whether or not the folder ever
/// had a marker object of its own.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_folder_emptied_locally_stays() {
    let mut c = chaos(0).await;
    c.client.put_bytes("media", "raw/take 1.mov", b"one".to_vec().into()).await.unwrap();
    c.client.put_bytes("media", "raw/take 2.mov", b"two".to_vec().into()).await.unwrap();
    c.client.put_bytes("media", "inbox/letter.txt", b"dear".to_vec().into()).await.unwrap();
    c.fs.refresh().await.unwrap();
    let raw = id(&c.fs, "raw");
    c.fs.remove(raw, "take 1.mov").unwrap();
    c.fs.remove(raw, "take 2.mov").unwrap();
    c.fs.rename(id(&c.fs, "inbox"), "letter.txt", ROOT_ID, "letter.txt").unwrap();
    c.expect.insert("letter.txt".into(), b"dear".to_vec());
    c.converge().await;
    assert!(c.fs.find_path("raw").is_some() && c.fs.find_path("inbox").is_some());
    let keys: Vec<String> = c.client.list("media").await.unwrap().objects.into_iter().map(|o| o.key).collect();
    assert_eq!(keys, ["inbox/", "letter.txt", "raw/"]);
    // Removing the folder itself removes it everywhere.
    c.fs.remove(ROOT_ID, "raw").unwrap();
    c.converge().await;
    assert!(c.fs.find_path("raw").is_none());
}

// ------------------------------------------------- soak

/// A long random run: an application doing ordinary and unkind things to a
/// drive while the cloud is killed, restarted, stalled and made to refuse
/// requests, and the app itself is killed and started again. The seed is
/// printed; `CHAOS_SEED=n CHAOS_STEPS=n cargo test -p ludy-core --test chaos soak -- --ignored --nocapture`
/// repeats a run.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "minutes long; run on demand"]
async fn soak() {
    let seed: u64 = std::env::var("CHAOS_SEED").ok().and_then(|s| s.parse().ok()).unwrap_or_else(|| ludy_cloud::now_ms() % 100_000);
    let steps: u64 = std::env::var("CHAOS_STEPS").ok().and_then(|s| s.parse().ok()).unwrap_or(400);
    eprintln!("soak: CHAOS_SEED={seed} CHAOS_STEPS={steps}");
    let mut c = chaos(1).await;
    let mut uploader = tokio::spawn(c.fs.clone().run_uploader());
    let mut poller = tokio::spawn(c.fs.clone().run_poller());
    let mut n = 0u64;
    let mut rnd = move |below: u64| {
        n += 1;
        synth::word(seed, n) % below.max(1)
    };
    let mut dirs: Vec<String> = vec![String::new()];
    // Someone else's files, in a folder the local side never touches.
    let mut remote: Vec<String> = Vec::new();
    let mut down = false;
    let mut trace: Vec<String> = Vec::new();

    for step in 0..steps {
        let files: Vec<String> = c.expect.keys().filter(|k| !k.starts_with("remote/")).cloned().collect();
        let pick_file = |r: u64| files.get(r as usize % files.len().max(1)).cloned();
        let what = rnd(108);
        let line;
        match what {
            // create or overwrite a file
            0..=29 => {
                let dir = dirs[rnd(dirs.len() as u64) as usize].clone();
                let name = format!("f{}", rnd(40));
                let size = match rnd(10) {
                    0 => 0,
                    1 => 600_000 + rnd(900_000) as usize,
                    _ => 1 + rnd(5000) as usize,
                };
                let key = if dir.is_empty() { name.clone() } else { format!("{dir}/{name}") };
                line = format!("create {key} ({size} bytes)");
                let parent = c.fs.find_path(&dir).unwrap();
                let data = synth::bytes(seed ^ step, 0, size.next_multiple_of(8))[..size].to_vec();
                match c.fs.create(parent, &name, false) {
                    Ok(file) => {
                        c.fs.set_size(file, 0).unwrap();
                        for (i, piece) in data.chunks(BLOCK_SIZE as usize).enumerate() {
                            c.fs.write(file, i as u64 * BLOCK_SIZE, piece).unwrap();
                        }
                        c.expect.insert(key, data);
                    }
                    Err(FsError::IsDir) => {}
                    Err(e) => panic!("step {step}: {line}: {e:?}"),
                }
            }
            // patch a file in place
            30..=44 => {
                let Some(key) = pick_file(rnd(1000)) else { continue };
                let len = c.expect[&key].len();
                if len == 0 {
                    continue;
                }
                let at = rnd(len as u64) as usize;
                let patch = synth::bytes(step, 0, 64)[..(1 + rnd(60) as usize)].to_vec();
                line = format!("patch {key} at {at} (+{})", patch.len());
                // Reading the rest of a cloud file needs the cloud; writing never does.
                c.fs.write(id(&c.fs, &key), at as u64, &patch).unwrap();
                let data = c.expect.get_mut(&key).unwrap();
                if data.len() < at + patch.len() {
                    data.resize(at + patch.len(), 0);
                }
                data[at..at + patch.len()].copy_from_slice(&patch);
            }
            // truncate or grow
            45..=49 => {
                let Some(key) = pick_file(rnd(1000)) else { continue };
                let new_len = rnd(c.expect[&key].len() as u64 + 2000) as usize;
                line = format!("resize {key} to {new_len}");
                c.fs.set_size(id(&c.fs, &key), new_len as u64).unwrap();
                c.expect.get_mut(&key).unwrap().resize(new_len, 0);
            }
            // rename a file
            50..=59 => {
                let Some(key) = pick_file(rnd(1000)) else { continue };
                let (from_dir, from_name) = key.rsplit_once('/').unwrap_or(("", &key));
                let to_dir = dirs[rnd(dirs.len() as u64) as usize].clone();
                let to_name = format!("f{}", rnd(40));
                let to_key = if to_dir.is_empty() { to_name.clone() } else { format!("{to_dir}/{to_name}") };
                line = format!("rename {key} -> {to_key}");
                match c.fs.rename(c.fs.find_path(from_dir).unwrap(), from_name, c.fs.find_path(&to_dir).unwrap(), &to_name) {
                    Ok(()) => {
                        if to_key != key {
                            let data = c.expect.remove(&key).unwrap();
                            c.expect.insert(to_key, data);
                        }
                    }
                    Err(FsError::NotEmpty) | Err(FsError::IsDir) => {}
                    Err(e) => panic!("step {step}: {line}: {e:?}"),
                }
            }
            // delete a file
            60..=67 => {
                let Some(key) = pick_file(rnd(1000)) else { continue };
                line = format!("delete {key}");
                let (dir, name) = key.rsplit_once('/').unwrap_or(("", &key));
                c.fs.remove(c.fs.find_path(dir).unwrap(), name).unwrap();
                c.expect.remove(&key);
            }
            // make a folder
            68..=73 => {
                let parent = dirs[rnd(dirs.len() as u64) as usize].clone();
                if parent.matches('/').count() >= 3 {
                    continue;
                }
                let name = format!("d{}", rnd(6));
                let path = if parent.is_empty() { name.clone() } else { format!("{parent}/{name}") };
                line = format!("mkdir {path}");
                match c.fs.mkdir(c.fs.find_path(&parent).unwrap(), &name) {
                    Ok(_) => dirs.push(path),
                    Err(FsError::Exists) => {}
                    Err(e) => panic!("step {step}: {line}: {e:?}"),
                }
            }
            // rename a folder, with everything in it
            74..=77 => {
                if dirs.len() < 2 {
                    continue;
                }
                let from = dirs[1 + rnd(dirs.len() as u64 - 1) as usize].clone();
                let (parent, name) = from.rsplit_once('/').unwrap_or(("", &from));
                let to_name = format!("d{}", rnd(6));
                let to = if parent.is_empty() { to_name.clone() } else { format!("{parent}/{to_name}") };
                line = format!("rename folder {from} -> {to}");
                let p = c.fs.find_path(parent).unwrap();
                match c.fs.rename(p, name, p, &to_name) {
                    Ok(()) if to != from => {
                        let prefix = format!("{from}/");
                        // An empty folder at the destination was replaced.
                        dirs.retain(|d| d != &to && !d.starts_with(&format!("{to}/")));
                        for d in dirs.iter_mut() {
                            if *d == from {
                                *d = to.clone();
                            } else if let Some(rest) = d.strip_prefix(&prefix) {
                                *d = format!("{to}/{rest}");
                            }
                        }
                        c.expect = c.expect.iter().map(|(k, v)| (k.strip_prefix(&prefix).map(|rest| format!("{to}/{rest}")).unwrap_or_else(|| k.clone()), v.clone())).collect();
                    }
                    Ok(()) => {}
                    Err(FsError::NotEmpty) | Err(FsError::NotDir) => {}
                    Err(e) => panic!("step {step}: {line}: {e:?}"),
                }
            }
            // read something back and compare (only what cannot need a dead cloud)
            78..=85 => {
                let Some(key) = pick_file(rnd(1000)) else { continue };
                let want = &c.expect[&key];
                if want.is_empty() || down {
                    continue;
                }
                let at = rnd(want.len() as u64) as usize;
                let len = (1 + rnd(8000) as usize).min(want.len() - at);
                line = format!("read {key} at {at}+{len}");
                match within(30, &line, c.fs.read(id(&c.fs, &key), at as u64, len as u32)).await {
                    Ok((got, _)) => assert!(got[..] == want[at..at + len], "step {step}: {line}: wrong bytes"),
                    // The cloud may be refusing or stalled; an error is allowed, wrong bytes never.
                    Err(FsError::Io(_)) => {}
                    Err(e) => panic!("step {step}: {line}: {e:?}"),
                }
            }
            // the cloud dies, or comes back
            86..=89 => {
                if down {
                    line = "cloud restarts".into();
                    c.server.restart().await.unwrap();
                    down = false;
                } else {
                    line = "cloud is killed".into();
                    c.server.kill();
                    down = true;
                }
            }
            // the cloud refuses or stalls for a moment
            90..=93 => {
                if down {
                    continue;
                }
                let stall = rnd(2) == 0;
                line = format!("cloud {} for a moment", if stall { "stalls" } else { "refuses" });
                let cloud = c.server.cloud();
                let flag = if stall { &cloud.faults.stall } else { &cloud.faults.unavailable };
                flag.store(true, Ordering::Relaxed);
                tokio::time::sleep(Duration::from_millis(50 + rnd(250))).await;
                flag.store(false, Ordering::Relaxed);
            }
            // the app is killed and started again (it needs the cloud to start)
            94..=96 => {
                if down {
                    continue;
                }
                line = "app is killed and restarted".into();
                uploader.abort();
                poller.abort();
                let _ = (&mut uploader).await;
                let _ = (&mut poller).await;
                c.fs = DriveFs::new("media", c.client.clone(), c.cache.clone(), c.stats.clone(), c.dir.path().join("journal")).unwrap();
                within(60, "the listing after a restart", async {
                    while c.fs.refresh().await.is_err() {
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                })
                .await;
                uploader = tokio::spawn(c.fs.clone().run_uploader());
                poller = tokio::spawn(c.fs.clone().run_poller());
            }
            // someone else adds, replaces, renames or deletes a file in the cloud
            100..=107 => {
                if down {
                    continue;
                }
                let key = format!("remote/r{}", rnd(12));
                let op = rnd(4);
                let done = if op <= 1 {
                    let data = synth::bytes(seed ^ step, 0, 8 * (1 + rnd(400) as usize));
                    line = format!("remote put {key}");
                    c.client.put_bytes("media", &key, data.clone().into()).await.map(|_| {
                        c.expect.insert(key.clone(), data);
                        if !remote.contains(&key) {
                            remote.push(key.clone());
                        }
                    })
                } else if remote.is_empty() {
                    continue;
                } else if op == 2 {
                    let from = remote.swap_remove(rnd(remote.len() as u64) as usize);
                    line = format!("remote rename {from} -> {key}");
                    c.client.rename("media", &from, &key).await.map(|_| {
                        let data = c.expect.remove(&from).unwrap();
                        c.expect.insert(key.clone(), data);
                        remote.retain(|k| k != &key);
                        remote.push(key.clone());
                    })
                } else {
                    let gone = remote.swap_remove(rnd(remote.len() as u64) as usize);
                    line = format!("remote delete {gone}");
                    c.client.delete("media", &gone).await.map(|_| {
                        c.expect.remove(&gone);
                    })
                };
                // The cloud may be refusing or stalled at this moment; then
                // the other side simply did not get its change in. But a
                // request that failed may still have been carried out.
                if done.is_err() {
                    let listing = within(60, "a listing after a failed remote change", async {
                        loop {
                            if let Ok(l) = c.client.list("media").await {
                                break l;
                            }
                            tokio::time::sleep(Duration::from_millis(50)).await;
                        }
                    })
                    .await;
                    c.expect.retain(|k, _| !k.starts_with("remote/"));
                    remote.clear();
                    for o in listing.objects.iter().filter(|o| o.key.starts_with("remote/")) {
                        let data = c.client.get_range("media", &o.key, &o.version, 0, o.size).await.unwrap().to_vec();
                        c.expect.insert(o.key.clone(), data);
                        remote.push(o.key.clone());
                    }
                }
            }
            // let time pass, so the uploader's quiet period can elapse
            _ => {
                line = "pause".into();
                tokio::time::sleep(Duration::from_millis(rnd(400))).await;
            }
        }
        trace.push(format!("{step}: {line}"));
        // The drive's own view must match the model after every step.
        // (The other side's files arrive when the poller gets to them.)
        for (key, want) in c.expect.iter().filter(|(k, _)| !k.starts_with("remote/")) {
            let Some(node) = c.fs.find_path(key) else {
                panic!("after step {step} ({line}) {key} is missing from the drive\n{}", trace[trace.len().saturating_sub(15)..].join("\n"));
            };
            let size = c.fs.getattr(node).unwrap().size;
            assert_eq!(size, want.len() as u64, "after step {step} ({line}) {key} has the wrong size\n{}", trace[trace.len().saturating_sub(15)..].join("\n"));
        }
    }
    if down {
        c.server.restart().await.unwrap();
    }
    poller.abort();
    eprintln!("soak: {} files, {} folders at the end; converging", c.expect.len(), dirs.len());
    c.converge().await;
    uploader.abort();
    c.assert_no_leaks().await;
}
