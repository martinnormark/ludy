//! The sealed drive (`ludy_core::sealed_drive`) against the Worker under
//! `wrangler dev`: what the engine's tests check on the old cloud, for the
//! engine that speaks the node-tree protocol and encrypts everything.
//!
//! Every test that writes ends by reading the drive back with
//! `worker_rig::read_drive`, which holds the root key and none of the
//! engine's code: the cloud has to hold what the application wrote, sealed
//! the way the format says.
//!
//! These need Node and `app/worker/node_modules`, so they are ignored by default:
//!
//! ```text
//! cargo test -p ludy-core --test sealed_engine -- --ignored --test-threads=1
//! ```

mod worker_rig;

use std::collections::BTreeMap;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use ludy_core::cache::{BlockCache, BLOCK_SIZE};
use ludy_core::drive::{FsError, ROOT_ID};
use ludy_core::sealed_client::SealedClient;
use ludy_core::sealed_drive::{create_drive, DriveKeys, SealedDrive};
use ludy_core::stats::Stats;
use ludy_proto::crypt::{FRAME, TAG};
use ludy_proto::synth;
use worker_rig::*;

const MIB: usize = 1 << 20;
const NEVER: i64 = 1_000_000_000_000;

struct Bench {
    worker: Worker,
    /// In front of the Worker; loses nothing until a test tells it to.
    proxy: Proxy,
    dir: tempfile::TempDir,
    keys: DriveKeys,
    /// Reads the drive from outside, with the root key.
    reader: Client,
}

/// One instance of the engine on the drive: a Mac, with a cache and a journal of its own.
struct Mac {
    fs: Arc<SealedDrive>,
    stats: Arc<Stats>,
    cache: Arc<BlockCache>,
}

async fn bench() -> Option<Bench> {
    if !available() {
        return None;
    }
    let worker = Worker::start(NEVER).await;
    let client = SealedClient::new(&worker.url(), Arc::new(Stats::default()));
    let keys = create_drive(&client, "Footage").await.unwrap();
    let reader = Client::new(&worker.url());
    let proxy = Proxy::start(worker.port, 1).await;
    Some(Bench { worker, proxy, dir: tempfile::tempdir().unwrap(), keys, reader })
}

impl Bench {
    /// Open the drive as the Mac called `who`. The same name again is the
    /// same Mac after a restart: same cache folder, same journal.
    async fn mac(&self, who: &str) -> Mac {
        self.mac_with(who, 256 << 20).await
    }

    async fn mac_with(&self, who: &str, cache_cap: u64) -> Mac {
        let mac = self.open(who, cache_cap, false);
        assert!(mac.fs.refresh().await.unwrap(), "the first look at the drive");
        mac
    }

    /// The same, through the proxy and without reusing connections, so that
    /// every request of this Mac is one the proxy may lose. The drive is not
    /// looked at yet: the Worker may be away.
    fn faulty_mac(&self, who: &str) -> Mac {
        self.open(who, 256 << 20, true)
    }

    /// How often a sealed manifest has been fetched from the Worker.
    fn manifest_fetches(&self) -> usize {
        self.worker.log().lines().filter(|l| l.contains("GET /drives/") && l.contains("/manifests/")).count()
    }

    fn open(&self, who: &str, cache_cap: u64, faulty: bool) -> Mac {
        let stats = Arc::new(Stats::default());
        let client = if faulty { SealedClient::with_pool(&self.proxy.url(), stats.clone(), false) } else { SealedClient::new(&self.worker.url(), stats.clone()) };
        let cache = Arc::new(BlockCache::open(&self.dir.path().join(who).join("cache"), cache_cap).unwrap());
        let fs = SealedDrive::open("Footage", self.keys.clone(), client, cache.clone(), stats.clone(), self.dir.path().join(who).join("journal")).unwrap();
        Mac { fs, stats, cache }
    }

    /// What the cloud holds, read from outside.
    async fn cloud(&self) -> BTreeMap<String, Entry> {
        read_drive(&self.reader, &self.keys.drive, self.keys.root.0, &self.keys.root_key).await
    }

    /// Paths and contents in the cloud; a folder's content is `None`.
    async fn cloud_files(&self) -> BTreeMap<String, Option<Vec<u8>>> {
        self.cloud().await.into_iter().map(|(path, e)| (path, e.content)).collect()
    }

    async fn settled(&self) {
        self.reader.check(&self.keys.drive, "at the end").await;
        let troubles = log_troubles(&self.worker.log());
        assert!(troubles.is_empty(), "the Worker's log has errors:\n{}", troubles.join("\n"));
    }
}

impl Mac {
    fn id(&self, path: &str) -> u64 {
        self.fs.find_path(path).unwrap_or_else(|| panic!("no node at {path}"))
    }

    /// Create `dir/name` and write `data` to it, in the pieces the kernel would.
    fn put(&self, dir: u64, name: &str, data: &[u8]) -> u64 {
        let file = self.fs.create(dir, name, false).unwrap();
        self.fs.set_size(file, 0).unwrap();
        for (i, piece) in data.chunks(BLOCK_SIZE as usize).enumerate() {
            self.fs.write(file, i as u64 * BLOCK_SIZE, piece).unwrap();
        }
        file
    }

    async fn flush(&self) {
        assert!(self.fs.flush(Duration::from_secs(60)).await, "the drive never settled: {:?}", self.fs.pending());
    }

    async fn read_all(&self, id: u64) -> Vec<u8> {
        let size = self.fs.getattr(id).unwrap().size;
        let mut out = Vec::with_capacity(size as usize);
        while (out.len() as u64) < size {
            let (data, _) = self.fs.read(id, out.len() as u64, MIB as u32).await.unwrap();
            assert!(!data.is_empty());
            out.extend_from_slice(&data);
        }
        out
    }

    fn down(&self) -> u64 {
        self.stats.bytes_down.load(Ordering::Relaxed)
    }

    fn up(&self) -> u64 {
        self.stats.bytes_up.load(Ordering::Relaxed)
    }

    fn requests(&self) -> u64 {
        self.stats.range_requests.load(Ordering::Relaxed)
    }

    /// Every path in this Mac's view of the drive, with sizes; folders have `None`.
    fn tree(&self) -> BTreeMap<String, Option<u64>> {
        let mut out = BTreeMap::new();
        let mut stack = vec![ROOT_ID];
        while let Some(dir) = stack.pop() {
            for (_, attr) in self.fs.readdir(dir).unwrap() {
                out.insert(self.fs.path_of(attr.id), (!attr.is_dir).then_some(attr.size));
                if attr.is_dir {
                    stack.push(attr.id);
                }
            }
        }
        out
    }
}

fn sizes(cloud: &BTreeMap<String, Entry>) -> BTreeMap<String, Option<u64>> {
    cloud.iter().map(|(p, e)| (p.clone(), e.content.as_ref().map(|c| c.len() as u64))).collect()
}

/// The sealed size of one cache block: four frames and their tags.
const SEALED_BLOCK: u64 = BLOCK_SIZE + (BLOCK_SIZE / FRAME as u64) * TAG as u64;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs Node and app/worker/node_modules"]
async fn what_is_written_is_in_the_cloud_sealed_and_another_mac_reads_it() {
    let Some(b) = bench().await else { return };
    let a = b.mac("a").await;
    let big = synth::bytes(1, 0, 5 * MIB + 12_345);
    let small = b"shot one, take three\n".to_vec();
    let raw = a.fs.mkdir(ROOT_ID, "raw").unwrap();
    let clip = a.put(raw, "clip.mov", &big);
    a.put(ROOT_ID, "shot-list.txt", &small);
    a.put(raw, "empty.bin", b"");
    a.fs.mkdir(raw, "empty folder").unwrap();
    a.fs.set_mtime(clip, 1_600_000_000_123).unwrap();

    // The writes are readable at once, before anything has left this Mac.
    assert_eq!(a.up(), 0);
    assert!(a.read_all(clip).await == big);
    a.flush().await;

    let cloud = b.cloud().await;
    let want: BTreeMap<String, Option<Vec<u8>>> = BTreeMap::from([
        ("raw".into(), None),
        ("raw/clip.mov".into(), Some(big.clone())),
        ("raw/empty.bin".into(), Some(vec![])),
        ("raw/empty folder".into(), None),
        ("shot-list.txt".into(), Some(small.clone())),
    ]);
    assert!(cloud.iter().map(|(p, e)| (p.clone(), e.content.clone())).collect::<BTreeMap<_, _>>() == want, "the cloud holds {:?}", sizes(&cloud));
    assert_eq!(cloud["raw/clip.mov"].mtime_ms, 1_600_000_000_123, "the modification time travels sealed with the version");

    // Nothing the cloud stores contains a name or a stretch of the content.
    let log = b.worker.log();
    assert!(!log.contains("clip.mov") && !log.contains("shot-list"));

    // Another Mac lists it without downloading content, then reads it.
    let c = b.mac("c").await;
    assert_eq!(c.tree(), sizes(&cloud));
    assert_eq!(c.requests(), 0, "listing fetched no shard");
    assert_eq!(c.fs.getattr(c.id("raw/clip.mov")).unwrap().mtime_ms, 1_600_000_000_123);
    assert!(c.read_all(c.id("raw/clip.mov")).await == big);
    assert!(c.read_all(c.id("shot-list.txt")).await == small);
    assert_eq!(c.read_all(c.id("raw/empty.bin")).await, b"");
    b.settled().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs Node and app/worker/node_modules"]
async fn a_read_in_the_middle_fetches_only_the_frames_around_it() {
    let Some(b) = bench().await else { return };
    let a = b.mac("a").await;
    let data = synth::bytes(2, 0, 24 * MIB);
    a.put(ROOT_ID, "long.mov", &data);
    a.flush().await;

    let c = b.mac("c").await;
    let file = c.id("long.mov");
    assert_eq!(c.fs.getattr(file).unwrap().size, 24 * MIB as u64);
    let listed = c.down();
    let at = 12 * MIB as u64 + 777;
    let (got, _) = c.fs.read(file, at, 65536).await.unwrap();
    assert!(got == data[at as usize..at as usize + 65536]);
    let manifest_and_block = c.down() - listed;
    eprintln!("mid-file read: {} bytes downloaded, {} range request(s)", manifest_and_block, c.requests());
    // The manifest (a few hundred bytes for a dozen shards) and at most two blocks.
    assert!(c.requests() <= 2, "{} range requests for a 64 KiB read", c.requests());
    assert!(manifest_and_block <= 2 * SEALED_BLOCK + 4096, "{manifest_and_block} bytes for a 64 KiB read");

    // Again: nothing. And a Mac restarted over the same cache folder: only the manifest.
    let (before, requests) = (c.down(), c.requests());
    let (again, _) = c.fs.read(file, at, 65536).await.unwrap();
    assert!(again == got);
    assert_eq!((c.down(), c.requests()), (before, requests), "a second read needs the cloud for nothing");
    drop(c);
    let c = b.mac("c").await;
    let (again, _) = c.fs.read(c.id("long.mov"), at, 65536).await.unwrap();
    assert!(again == got);
    assert_eq!(c.requests(), 0, "after a restart the blocks are still in the cache");
    b.settled().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs Node and app/worker/node_modules"]
async fn sequential_and_concurrent_reads_share_requests() {
    let Some(b) = bench().await else { return };
    let a = b.mac("a").await;
    let data = synth::bytes(3, 0, 20 * MIB);
    a.put(ROOT_ID, "long.mov", &data);
    a.flush().await;

    // Six readers of one block: one request.
    let c = b.mac("c").await;
    let file = c.id("long.mov");
    let fetched = b.manifest_fetches();
    let reads = (0..6).map(|_| c.fs.read(file, 5 * MIB as u64, 4096));
    for r in futures_util::future::join_all(reads).await {
        assert!(r.unwrap().0 == data[5 * MIB..5 * MIB + 4096]);
    }
    assert_eq!(c.requests(), 1, "six readers of one block share one fetch");
    assert_eq!(b.manifest_fetches() - fetched, 1, "and one fetch of the file's manifest");

    // A sequential reader: read-ahead goes out in a few large requests.
    let d = b.mac("d").await;
    let file = d.id("long.mov");
    let mut out = Vec::new();
    while out.len() < 16 * MIB {
        let (piece, _) = d.fs.read(file, out.len() as u64, BLOCK_SIZE as u32).await.unwrap();
        out.extend_from_slice(&piece);
    }
    assert!(out == data[..16 * MIB]);
    let blocks = 16 * MIB as u64 / BLOCK_SIZE;
    eprintln!("sequential read: {blocks} blocks in {} range requests", d.requests());
    // A request cannot span two shards, and where shards end differs from
    // drive to drive: about a third of the blocks, well under half.
    assert!(d.requests() <= blocks / 2, "{} requests for {blocks} blocks", d.requests());

    // The cache stays under its cap however much is read.
    let e = b.mac_with("e", 4 << 20).await;
    assert!(e.read_all(e.id("long.mov")).await == data);
    assert!(e.cache.snapshot().bytes <= 4 << 20, "the cache holds {} bytes", e.cache.snapshot().bytes);
    b.settled().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs Node and app/worker/node_modules"]
async fn a_small_change_in_a_large_file_sends_only_the_shards_it_touches() {
    let Some(b) = bench().await else { return };
    let a = b.mac("a").await;
    let mut data = synth::bytes(4, 0, 24 * MIB);
    let file = a.put(ROOT_ID, "project.bin", &data);
    a.flush().await;
    let first = a.up();
    assert!(first >= 24 * MIB as u64, "every byte goes up the first time");

    // The same bytes under another name: nothing but a manifest.
    a.put(ROOT_ID, "copy.bin", &data);
    a.flush().await;
    let copied = a.up() - first;
    eprintln!("identical content: {copied} bytes uploaded");
    assert!(copied < 4096, "{copied} bytes uploaded for content the drive already holds");

    // 4 KiB written in the middle, by a Mac that has none of the file cached.
    let c = b.mac("c").await;
    let file_c = c.id("project.bin");
    let patch = vec![0xEEu8; 4096];
    c.fs.write(file_c, 12 * MIB as u64, &patch).unwrap();
    data[12 * MIB..12 * MIB + 4096].copy_from_slice(&patch);
    // Readable at once, merged with the cloud's version underneath.
    let (merged, _) = c.fs.read(file_c, 12 * MIB as u64 - 100, 4296).await.unwrap();
    assert!(merged == data[12 * MIB - 100..12 * MIB + 4196]);
    c.flush().await;
    eprintln!("4 KiB patch: {} bytes uploaded, {} downloaded to cut the touched shard again", c.up(), c.down());
    assert!(c.up() < 12 * MIB as u64, "{} bytes uploaded for a 4 KiB change", c.up());
    assert!(c.down() < 12 * MIB as u64, "{} bytes downloaded for a 4 KiB change", c.down());

    // Shorter, then longer: zeros, not old bytes.
    a.fs.refresh().await.unwrap();
    a.fs.set_size(file, 3 * MIB as u64 + 5).unwrap();
    a.fs.set_size(file, 4 * MIB as u64).unwrap();
    a.fs.write(file, 4 * MIB as u64 - 3, b"end").unwrap();
    a.flush().await;
    let mut want = data[..3 * MIB + 5].to_vec();
    want.resize(4 * MIB - 3, 0);
    want.extend_from_slice(b"end");
    let cloud = b.cloud_files().await;
    assert!(cloud["project.bin"].as_deref() == Some(&want[..]), "truncate and grow");
    assert!(cloud["copy.bin"].as_deref() == Some(&synth::bytes(4, 0, 24 * MIB)[..]), "the copy is untouched by the patch");

    // Cut down to almost nothing and grown back to full length: whole shards
    // of the old version lie in the part that is zeros now, and none of them
    // may come back.
    let copy = a.id("copy.bin");
    a.fs.set_size(copy, 1000).unwrap();
    a.fs.set_size(copy, 24 * MIB as u64).unwrap();
    assert!(a.fs.read(copy, 10 * MIB as u64, 4096).await.unwrap().0 == vec![0u8; 4096], "zeros at once, before any upload");
    a.flush().await;
    let mut want = synth::bytes(4, 0, 1000);
    want.resize(24 * MIB, 0);
    assert!(b.cloud_files().await["copy.bin"].as_deref() == Some(&want[..]), "old bytes came back after a truncate");
    let c = b.mac("fresh").await;
    assert!(c.fs.read(c.id("copy.bin"), 10 * MIB as u64, 4096).await.unwrap().0 == vec![0u8; 4096]);
    b.settled().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs Node and app/worker/node_modules"]
async fn folders_renames_and_deletes_reach_the_cloud_in_order() {
    let Some(b) = bench().await else { return };
    let a = b.mac("a").await;
    let raw = a.fs.mkdir(ROOT_ID, "raw").unwrap();
    let day1 = a.fs.mkdir(raw, "day 1").unwrap();
    a.put(day1, "a.mov", b"aaa");
    a.put(day1, "b.mov", b"bbb");
    a.put(ROOT_ID, "notes.txt", b"v1");
    a.flush().await;

    // A rename, a move of a folder with its files, a save by temp file and rename, a delete.
    a.fs.rename(day1, "a.mov", day1, "take one.mov").unwrap();
    let edit = a.fs.mkdir(ROOT_ID, "edit").unwrap();
    a.fs.rename(raw, "day 1", edit, "selects").unwrap();
    a.put(ROOT_ID, "notes.txt.tmp", b"v2, saved over");
    a.fs.rename(ROOT_ID, "notes.txt.tmp", ROOT_ID, "notes.txt").unwrap();
    a.fs.remove(edit, "selects").unwrap_err(); // not empty
    let selects = a.id("edit/selects");
    a.fs.remove(selects, "b.mov").unwrap();
    assert_eq!(a.fs.rename(ROOT_ID, "edit", selects, "inside itself"), Err(FsError::Invalid));
    // Before any of it is sent, this Mac already shows it.
    let want: BTreeMap<String, Option<u64>> = BTreeMap::from([
        ("edit".into(), None),
        ("edit/selects".into(), None),
        ("edit/selects/take one.mov".into(), Some(3)),
        ("notes.txt".into(), Some(14)),
        ("raw".into(), None),
    ]);
    assert_eq!(a.tree(), want);
    a.flush().await;
    let cloud = b.cloud().await;
    assert_eq!(sizes(&cloud), want);
    assert_eq!(cloud["notes.txt"].content.as_deref(), Some(&b"v2, saved over"[..]));
    assert_eq!(cloud["edit/selects/take one.mov"].content.as_deref(), Some(&b"aaa"[..]));

    // Another Mac that was open all along follows by asking what changed.
    let c = b.mac("c").await;
    assert_eq!(c.tree(), want);
    a.fs.rename(a.id("edit"), "selects", ROOT_ID, "final").unwrap();
    a.put(a.id("final"), "c.mov", b"ccc");
    a.fs.remove(ROOT_ID, "notes.txt").unwrap();
    a.fs.write(a.id("final/take one.mov"), 3, b"+more").unwrap();
    a.flush().await;
    assert!(c.fs.refresh().await.unwrap());
    let want: BTreeMap<String, Option<u64>> =
        BTreeMap::from([("edit".into(), None), ("final".into(), None), ("final/c.mov".into(), Some(3)), ("final/take one.mov".into(), Some(8)), ("raw".into(), None)]);
    assert_eq!(c.tree(), want);
    assert_eq!(sizes(&b.cloud().await), want);
    assert_eq!(c.read_all(c.id("final/take one.mov")).await, b"aaa+more");
    b.settled().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs Node and app/worker/node_modules"]
async fn finder_droppings_stay_here_and_names_compare_normalised() {
    let Some(b) = bench().await else { return };
    let a = b.mac("a").await;
    a.put(ROOT_ID, ".DS_Store", b"finder's");
    a.put(ROOT_ID, "._clip.mov", b"resource fork");
    let hidden = a.fs.mkdir(ROOT_ID, ".Trashes").unwrap();
    a.put(hidden, "thrown away", b"x");
    // A temporary name that never leaves this Mac, renamed into place: now it does.
    a.put(ROOT_ID, "._tmp", b"real content");
    a.fs.rename(ROOT_ID, "._tmp", ROOT_ID, "real.txt").unwrap();

    // One name in two spellings is one file, whichever the application uses.
    let (nfc, nfd) = ("caf\u{e9}.txt", "cafe\u{301}.txt");
    let file = a.put(ROOT_ID, nfd, b"ol\xc3\xa9");
    assert_eq!(a.fs.lookup(ROOT_ID, nfc), Ok(file));
    assert_eq!(a.fs.lookup(ROOT_ID, nfd), Ok(file));
    assert_eq!(a.fs.create(ROOT_ID, nfc, false), Ok(file));
    assert_eq!(a.fs.create(ROOT_ID, nfc, true), Err(FsError::Exists));
    assert_eq!(a.fs.mkdir(ROOT_ID, nfc), Err(FsError::Exists));
    assert_eq!(a.fs.create(ROOT_ID, "a/b", false), Err(FsError::Invalid));
    assert_eq!(a.fs.create(ROOT_ID, &"x".repeat(256), false), Err(FsError::NameTooLong));
    a.flush().await;

    let cloud = b.cloud_files().await;
    assert_eq!(cloud.keys().map(String::as_str).collect::<Vec<_>>(), vec![nfc, "real.txt"]);
    assert_eq!(cloud["real.txt"].as_deref(), Some(&b"real content"[..]));
    // They are still here, on this Mac.
    assert_eq!(a.read_all(a.id(".DS_Store")).await, b"finder's");
    assert_eq!(a.read_all(a.id(".Trashes/thrown away")).await, b"x");
    // And a real file renamed to such a name leaves the cloud.
    a.fs.rename(ROOT_ID, "real.txt", ROOT_ID, "._real.txt").unwrap();
    a.flush().await;
    assert_eq!(b.cloud_files().await.keys().map(String::as_str).collect::<Vec<_>>(), vec![nfc]);
    b.settled().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs Node and app/worker/node_modules"]
async fn unsent_writes_survive_the_app_being_killed() {
    let Some(b) = bench().await else { return };
    let a = b.mac("a").await;
    a.put(ROOT_ID, "kept.txt", b"in the cloud before the crash");
    let big = synth::bytes(9, 0, 6 * MIB);
    a.put(ROOT_ID, "base.bin", &big);
    a.flush().await;

    // New files, a new folder, a rename, a delete and a patch, none of it sent.
    let folder = a.fs.mkdir(ROOT_ID, "new folder").unwrap();
    a.put(folder, "one.txt", b"one");
    a.put(folder, "two.bin", &synth::bytes(10, 0, 700_000));
    a.fs.rename(ROOT_ID, "kept.txt", folder, "moved.txt").unwrap();
    a.fs.write(a.id("base.bin"), 3 * MIB as u64, b"patched while offline").unwrap();
    a.put(ROOT_ID, "doomed.txt", b"x");
    a.fs.remove(ROOT_ID, "doomed.txt").unwrap();
    let before = a.tree();
    assert!(a.fs.has_unsynced());
    // The app dies: no flush, no goodbye. The journal folder is what is left.
    drop(a);

    let a = b.mac("a").await;
    assert_eq!(a.tree(), before, "after the restart this Mac shows what it showed before");
    a.flush().await;
    let mut patched = big.clone();
    patched[3 * MIB..3 * MIB + 21].copy_from_slice(b"patched while offline");
    let want: BTreeMap<String, Option<Vec<u8>>> = BTreeMap::from([
        ("base.bin".into(), Some(patched)),
        ("new folder".into(), None),
        ("new folder/moved.txt".into(), Some(b"in the cloud before the crash".to_vec())),
        ("new folder/one.txt".into(), Some(b"one".to_vec())),
        ("new folder/two.bin".into(), Some(synth::bytes(10, 0, 700_000))),
    ]);
    assert!(b.cloud_files().await == want, "the cloud holds {:?}", sizes(&b.cloud().await));
    // Nothing is left to recover.
    drop(a);
    let a = b.mac("a").await;
    assert!(!a.fs.has_unsynced());
    b.settled().await;
}

// --------------------------------------------------- two Macs, one drive

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs Node and app/worker/node_modules"]
async fn two_macs_that_create_the_same_name_both_keep_their_file() {
    let Some(b) = bench().await else { return };
    let (a, c) = (b.mac("a").await, b.mac("c").await);
    a.put(ROOT_ID, "plan.txt", b"the plan, as A wrote it");
    c.put(ROOT_ID, "plan.txt", b"the plan, as C wrote it");
    let folder = c.fs.mkdir(ROOT_ID, "shared").unwrap();
    c.put(folder, "inside.txt", b"in C's folder");
    a.fs.mkdir(ROOT_ID, "shared").unwrap();
    a.flush().await;
    c.flush().await;

    let cloud = b.cloud_files().await;
    let names: Vec<&str> = cloud.keys().map(String::as_str).collect();
    let told = c.fs.take_notices();
    eprintln!("same name twice: the cloud holds {names:?}; C is told: {told:?}");
    assert_eq!(told.len(), 2, "one notice for the file and one for the folder");
    assert!(a.fs.take_notices().is_empty(), "A lost nothing and is told nothing");
    assert_eq!(cloud["plan.txt"].as_deref(), Some(&b"the plan, as A wrote it"[..]));
    let kept: Vec<&String> = cloud.keys().filter(|k| k.starts_with("plan (conflict ") && k.ends_with(").txt")).collect();
    assert_eq!(kept.len(), 1, "C's file is kept under another name: {names:?}");
    assert_eq!(cloud[kept[0]].as_deref(), Some(&b"the plan, as C wrote it"[..]));
    // C's folder is kept too, with what C put in it.
    let folder: Vec<&String> = cloud.keys().filter(|k| k.starts_with("shared (conflict ") && !k.contains('/')).collect();
    assert_eq!(folder.len(), 1, "{names:?}");
    assert_eq!(cloud[&format!("{}/inside.txt", folder[0])].as_deref(), Some(&b"in C's folder"[..]));
    assert_eq!(cloud.len(), 5);
    // Both Macs end up showing what the cloud has.
    assert!(a.fs.refresh().await.unwrap() && c.fs.refresh().await.unwrap());
    assert_eq!(a.tree(), sizes(&b.cloud().await));
    assert_eq!(c.tree(), sizes(&b.cloud().await));
    b.settled().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs Node and app/worker/node_modules"]
async fn the_last_writer_wins_and_the_other_version_stays_in_history() {
    let Some(b) = bench().await else { return };
    let a = b.mac("a").await;
    let file = a.put(ROOT_ID, "budget.xls", &synth::bytes(20, 0, 2 * MIB));
    a.flush().await;
    let c = b.mac("c").await;

    a.fs.write(file, 0, b"A's numbers").unwrap();
    c.fs.write(c.id("budget.xls"), MIB as u64, b"C's numbers").unwrap();
    a.flush().await;
    c.flush().await;

    // C wrote over the version it knew. Its save is the file now: the old
    // content with C's change, without A's.
    let mut want = synth::bytes(20, 0, 2 * MIB);
    want[MIB..MIB + 11].copy_from_slice(b"C's numbers");
    let cloud = b.cloud().await;
    assert!(cloud["budget.xls"].content.as_deref() == Some(&want[..]));
    let node = b.reader.whole_feed(&b.keys.drive, 100).await.1.into_iter().find(|n| n.version.is_some()).unwrap();
    assert_eq!(b.reader.versions(&b.keys.drive, &node.id).await.unwrap().len(), 3, "the first version, A's and C's");
    assert!(a.fs.refresh().await.unwrap());
    assert!(a.read_all(file).await == want, "A sees C's save after looking again");
    b.settled().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs Node and app/worker/node_modules"]
async fn changes_to_what_the_cloud_deleted_are_kept_when_they_can_be() {
    let Some(b) = bench().await else { return };
    let a = b.mac("a").await;
    let folder = a.fs.mkdir(ROOT_ID, "old project").unwrap();
    a.put(ROOT_ID, "rewritten.txt", b"before");
    a.put(ROOT_ID, "patched.bin", &synth::bytes(30, 0, 2 * MIB));
    a.flush().await;
    let c = b.mac("c").await;

    // C, not knowing: rewrites one file whole, changes a few bytes of another,
    // and saves a new file into a folder.
    let rewritten = c.id("rewritten.txt");
    c.fs.set_size(rewritten, 0).unwrap();
    c.fs.write(rewritten, 0, b"after, all of it written by C").unwrap();
    c.fs.write(c.id("patched.bin"), 100, b"just these bytes").unwrap();
    c.put(c.id("old project"), "late.txt", b"saved into a folder that is going away");
    let sub = c.fs.mkdir(c.id("old project"), "new subfolder").unwrap();
    c.put(sub, "deep.txt", b"in a new folder inside the folder that is going away");
    // A deletes all of it.
    a.fs.remove(ROOT_ID, "rewritten.txt").unwrap();
    a.fs.remove(ROOT_ID, "patched.bin").unwrap();
    a.fs.remove(ROOT_ID, "old project").unwrap();
    let _ = folder;
    a.flush().await;
    c.flush().await;

    let cloud = b.cloud_files().await;
    let names: Vec<&str> = cloud.keys().map(String::as_str).collect();
    let told = c.fs.take_notices();
    eprintln!("deleted under changes: the cloud holds {names:?}; C is told: {told:?}");
    // Everything of the rewritten file was on C: it exists again.
    assert_eq!(cloud.get("rewritten.txt").and_then(|c| c.as_deref()), Some(&b"after, all of it written by C"[..]));
    // The new file's folder is gone: the file is kept at the top.
    let recovered: Vec<&String> = cloud.keys().filter(|k| k.starts_with("late (recovered ")).collect();
    assert_eq!(recovered.len(), 1, "{names:?}");
    assert_eq!(cloud[recovered[0]].as_deref(), Some(&b"saved into a folder that is going away"[..]));
    // So is the file from the new subfolder; the empty subfolder itself is not.
    let deep: Vec<&String> = cloud.keys().filter(|k| k.starts_with("deep (recovered ")).collect();
    assert_eq!(deep.len(), 1, "{names:?}");
    assert_eq!(cloud[deep[0]].as_deref(), Some(&b"in a new folder inside the folder that is going away"[..]));
    // Sixteen bytes of a file whose other two megabytes are gone cannot be a file: the delete stands, and C says so.
    assert!(!cloud.contains_key("patched.bin"));
    assert_eq!(cloud.len(), 3, "{names:?}");
    assert!(told.iter().any(|n| n.contains("patched.bin") && n.contains("dropped")), "C has to be told its change was lost: {told:?}");
    assert!(told.iter().any(|n| n.contains("late.txt") && n.contains("recovered")), "{told:?}");
    assert!(told.iter().any(|n| n.contains("rewritten.txt")), "{told:?}");
    assert!(c.fs.take_notices().is_empty(), "a notice is given once");
    assert!(c.fs.refresh().await.unwrap());
    assert_eq!(c.tree(), sizes(&b.cloud().await));
    b.settled().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs Node and app/worker/node_modules"]
async fn a_rename_that_lost_to_another_mac_gives_way() {
    let Some(b) = bench().await else { return };
    let a = b.mac("a").await;
    a.put(ROOT_ID, "draft.txt", b"draft");
    a.fs.mkdir(ROOT_ID, "one").unwrap();
    a.fs.mkdir(ROOT_ID, "two").unwrap();
    a.flush().await;
    let c = b.mac("c").await;
    // A takes the name C is about to rename to; and both move the same file.
    a.put(ROOT_ID, "final.txt", b"A's final");
    a.flush().await;
    c.fs.rename(ROOT_ID, "draft.txt", ROOT_ID, "final.txt").unwrap();
    c.flush().await;
    assert!(c.fs.refresh().await.unwrap());
    let cloud = b.cloud().await;
    assert_eq!(cloud["final.txt"].content.as_deref(), Some(&b"A's final"[..]), "C's rename replaced nothing in the cloud");
    assert_eq!(cloud["draft.txt"].content.as_deref(), Some(&b"draft"[..]));
    assert_eq!(c.tree(), sizes(&cloud), "C shows what the cloud has again");
    assert_eq!(c.fs.take_notices().len(), 1, "C is told its rename did not happen");

    assert!(a.fs.refresh().await.unwrap());
    a.fs.rename(ROOT_ID, "draft.txt", a.id("one"), "draft.txt").unwrap();
    c.fs.rename(ROOT_ID, "draft.txt", c.id("two"), "draft.txt").unwrap();
    a.flush().await;
    c.flush().await;
    assert!(a.fs.refresh().await.unwrap() && c.fs.refresh().await.unwrap());
    let cloud = b.cloud().await;
    assert!(cloud.contains_key("two/draft.txt") && !cloud.contains_key("one/draft.txt"), "the later move stands");
    assert_eq!(a.tree(), sizes(&cloud));
    assert_eq!(c.tree(), sizes(&cloud));
    b.settled().await;
}

// ------------------------------------------------------------ destruction

/// The reply to the commit that creates a file is lost, and then the app
/// dies. On the next start the same commit goes out again and is recognised:
/// one file, under its own name.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs Node and app/worker/node_modules"]
async fn a_lost_reply_and_then_a_crash_do_not_duplicate_a_file() {
    let Some(b) = bench().await else { return };
    for (round, forget) in [(0, false), (1, true)] {
        let a = b.faulty_mac("a");
        assert!(a.fs.refresh().await.unwrap());
        let folder = a.fs.mkdir(ROOT_ID, &format!("day {round}")).unwrap();
        a.put(folder, "clip.mov", &synth::bytes(40 + round, 0, 300_000));
        // The commit that creates the folder and the file gets through; its answer does not.
        b.proxy.faults.lose_next_replies.store(1, Ordering::SeqCst);
        a.fs.sync_once(true).await;
        assert_eq!(b.proxy.faults.replies_lost.load(Ordering::SeqCst), round + 1, "the reply was lost");
        assert_eq!(a.fs.pending().ops, 2, "the engine does not know its commit arrived");
        let seq = b.reader.head(&b.keys.drive).await.unwrap().seq;
        drop(a);
        if forget {
            // As after a power cut that also lost the note of which commit was on its way.
            let log = b.dir.path().join("a/journal/ops.log");
            let kept: String = std::fs::read_to_string(&log).unwrap().lines().filter(|l| !l.starts_with('!')).map(|l| format!("{l}\n")).collect();
            std::fs::write(&log, kept).unwrap();
        }
        let a = b.faulty_mac("a");
        assert!(a.fs.refresh().await.unwrap());
        a.flush().await;
        assert!(a.fs.take_notices().is_empty(), "nothing went wrong that the person has to hear about");
        // Two creates were already there; only the file's content was new.
        assert_eq!(b.reader.head(&b.keys.drive).await.unwrap().seq, seq + 1, "nothing was created twice");
    }
    let cloud = b.cloud_files().await;
    assert_eq!(cloud.keys().map(String::as_str).collect::<Vec<_>>(), vec!["day 0", "day 0/clip.mov", "day 1", "day 1/clip.mov"]);
    assert!(cloud["day 1/clip.mov"].as_deref() == Some(&synth::bytes(41, 0, 300_000)[..]));
    b.settled().await;
}

/// A flood of small files and a large copy, with the Worker killed while
/// they upload, more written while it is away, and the app killed too.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs Node and app/worker/node_modules"]
async fn the_worker_and_the_app_die_in_the_middle_of_a_flood() {
    let Some(mut b) = bench().await else { return };
    b.proxy.rates(40, 30);
    let mut want: BTreeMap<String, Option<Vec<u8>>> = BTreeMap::new();
    let a = b.faulty_mac("a");
    assert!(a.fs.refresh().await.unwrap());
    let flood = a.fs.mkdir(ROOT_ID, "flood").unwrap();
    want.insert("flood".into(), None);
    for i in 0..200u64 {
        let data = synth::bytes(100 + i, 0, 8 * (1 + i as usize % 300));
        a.put(flood, &format!("f{i:03}"), &data);
        want.insert(format!("flood/f{i:03}"), Some(data));
    }
    let big = synth::bytes(7, 0, 12 * MIB);
    a.put(ROOT_ID, "copy.bin", &big);
    want.insert("copy.bin".into(), Some(big));

    // Keep the uploader going, as the app does, and kill the Worker once the
    // large copy is on its way up: the small files add up to 160 KB, so a
    // megabyte sent means shards of the copy have gone and more are in flight.
    let uploading = {
        let fs = a.fs.clone();
        tokio::spawn(async move {
            loop {
                fs.sync_once(true).await;
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
    };
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    while a.up() < MIB as u64 {
        assert!(std::time::Instant::now() < deadline, "the uploads never got going: {:?}", a.fs.pending());
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    let sent_before = a.up();
    b.worker.kill9();
    assert!(sent_before < 12 * MIB as u64 && a.fs.has_unsynced(), "the kill came after everything had been sent ({sent_before} bytes); the test did not test");
    // Still writing while the cloud is away.
    for i in 0..40u64 {
        let data = synth::bytes(900 + i, 0, 4096);
        a.put(flood, &format!("late{i:02}"), &data);
        want.insert(format!("flood/late{i:02}"), Some(data));
    }
    a.fs.rename(ROOT_ID, "flood", ROOT_ID, "renamed while away").unwrap();
    want = want.into_iter().map(|(k, v)| (k.replacen("flood", "renamed while away", 1), v)).collect();
    // The uploader keeps trying against a dead Worker for a moment, then the app dies too.
    tokio::time::sleep(Duration::from_millis(500)).await;
    uploading.abort();
    let _ = uploading.await;
    assert!(a.fs.has_unsynced());
    // And the app dies as well, with the cloud still away.
    let before = a.tree();
    drop(a);
    b.worker.restart().await;

    let a = b.faulty_mac("a");
    for attempt in 0.. {
        match a.fs.refresh().await {
            Ok(true) => break,
            _ if attempt < 50 => tokio::time::sleep(Duration::from_millis(100)).await,
            other => panic!("the drive does not open after the restart: {other:?}"),
        }
    }
    assert_eq!(a.tree(), before);
    assert!(a.fs.flush(Duration::from_secs(120)).await, "never settled: {:?}", a.fs.pending());
    b.proxy.rates(0, 0);
    let cloud = b.cloud_files().await;
    let extra: Vec<&String> = cloud.keys().filter(|k| !want.contains_key(*k)).collect();
    let missing: Vec<&String> = want.keys().filter(|k| !cloud.contains_key(*k)).collect();
    assert!(extra.is_empty() && missing.is_empty(), "the cloud has extra {extra:?} and lacks {missing:?}");
    assert!(cloud == want, "a file has the wrong content");
    let f = &b.proxy.faults;
    eprintln!(
        "flood: {sent_before} bytes had gone up when the Worker was killed; the proxy lost {} replies and {} requests of {}",
        f.replies_lost.load(Ordering::SeqCst), f.requests_lost.load(Ordering::SeqCst), f.connections.load(Ordering::SeqCst)
    );
    assert!(f.replies_lost.load(Ordering::SeqCst) > 0 && f.requests_lost.load(Ordering::SeqCst) > 0, "the faults did not fire");
    b.settled().await;
}

// -------------------------------------------------------------------- soak

/// A Mac with its background loops running, as the app runs it.
struct Live {
    mac: Mac,
    tasks: Vec<tokio::task::JoinHandle<()>>,
    /// Notices from before each crash; nobody should have been told anything.
    told: Vec<String>,
    /// Bytes downloaded and range requests made by earlier lives of the app.
    earlier: (u64, u64),
}

impl Live {
    async fn start(b: &Bench, who: &str, told: Vec<String>, earlier: (u64, u64)) -> Live {
        // A cache too small to hold much, so reads keep going to the cloud.
        let mac = b.open(who, 1 << 20, true);
        let deadline = std::time::Instant::now() + Duration::from_secs(90);
        while !matches!(mac.fs.refresh().await, Ok(true)) {
            assert!(std::time::Instant::now() < deadline, "the drive does not open");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let tasks = vec![tokio::spawn(mac.fs.clone().run_uploader()), tokio::spawn(mac.fs.clone().run_poller())];
        Live { mac, tasks, told, earlier }
    }

    fn downloaded(&self) -> (u64, u64) {
        (self.earlier.0 + self.mac.down(), self.earlier.1 + self.mac.requests())
    }

    /// The app dies where it stands: its loops stop mid-request and nothing is flushed.
    async fn crash(mut self, b: &Bench, who: &str) -> (Vec<String>, (u64, u64)) {
        for t in &self.tasks {
            t.abort();
        }
        for t in self.tasks.drain(..) {
            let _ = t.await;
        }
        // Whatever the dead loops had in flight lets go of the drive.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while Arc::strong_count(&self.mac.fs) > 1 && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        self.told.extend(self.mac.fs.take_notices());
        let downloaded = self.downloaded();
        let told = std::mem::take(&mut self.told);
        drop(self);
        // And the cache is gone with it, so that what is read next comes from the cloud.
        let _ = std::fs::remove_dir_all(b.dir.path().join(who).join("cache"));
        (told, downloaded)
    }
}

fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_string()
    } else {
        format!("{dir}/{name}")
    }
}

/// Random work through the engine on one Mac while the Worker is killed and
/// frozen, requests and replies are lost, the app is killed and started
/// again, and another Mac writes into a folder of its own. At the end the
/// cloud, read from outside, holds exactly what was written.
///
/// The seed fixes the order of operations and faults. It does not fix the
/// drive's keys, which come from the system, so shard boundaries differ from
/// run to run.
///
/// `SEALED_SEED=7 SEALED_STEPS=400 cargo test -p ludy-core --test sealed_engine soak -- --ignored --nocapture`
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "minutes long; needs Node and app/worker/node_modules"]
async fn soak() {
    let seed: u64 = std::env::var("SEALED_SEED").ok().and_then(|s| s.parse().ok()).unwrap_or_else(|| ludy_proto::now_ms() % 100_000);
    let steps: u64 = std::env::var("SEALED_STEPS").ok().and_then(|s| s.parse().ok()).unwrap_or(300);
    eprintln!("sealed soak: SEALED_SEED={seed} SEALED_STEPS={steps}");
    let Some(mut b) = bench().await else { return };
    let started = std::time::Instant::now();
    let mut n = 0u64;
    let mut rnd = move |below: u64| {
        n += 1;
        synth::word(seed, n) % below.max(1)
    };
    let other = b.mac("other").await;
    let remote_dir = other.fs.mkdir(ROOT_ID, "remote").unwrap();
    other.flush().await;
    let mut expect: BTreeMap<String, Option<Vec<u8>>> = BTreeMap::from([("remote".to_string(), None)]);
    let mut a = Live::start(&b, "a", vec![], (0, 0)).await;
    b.proxy.rates(50, 30);
    let names = ["a", "b", "c", "d", "notes.txt", "clip.mov", "caf\u{e9}", "x y"];
    let mut trace: Vec<String> = Vec::new();
    let mut thaw: Option<tokio::task::JoinHandle<()>> = None;
    let (mut kills, mut crashes, mut freezes, mut reads_ok, mut reads_failed, mut remote_n) = (0u32, 0u32, 0u32, 0u32, 0u32, 0u64);

    for step in 0..steps {
        let mine = |k: &String| k != "remote" && !k.starts_with("remote/");
        let dirs: Vec<String> = std::iter::once(String::new()).chain(expect.iter().filter(|(k, v)| v.is_none() && mine(k)).map(|(k, _)| k.clone())).collect();
        let files: Vec<String> = expect.iter().filter(|(k, v)| v.is_some() && mine(k)).map(|(k, _)| k.clone()).collect();
        let all: Vec<String> = expect.keys().filter(|k| mine(k)).cloned().collect();
        let fs = a.mac.fs.clone();
        let dir = dirs[rnd(dirs.len() as u64) as usize].clone();
        let name = names[rnd(names.len() as u64) as usize];
        let path = join(&dir, name);
        let dir_id = fs.find_path(&dir).unwrap_or_else(|| panic!("step {step}: the engine has no folder {dir:?}\n{}", trace.join("\n")));
        let line;
        match rnd(100) {
            0..=24 => {
                let size = match rnd(10) {
                    0 => 0,
                    1 => 300_000 + rnd(900_000) as usize,
                    _ => 1 + rnd(6000) as usize,
                };
                line = format!("write {path} ({size} bytes)");
                let data = synth::bytes(seed ^ step, 0, size.next_multiple_of(8))[..size].to_vec();
                match expect.get(&path) {
                    Some(None) => assert_eq!(fs.create(dir_id, name, false), Err(FsError::IsDir), "step {step}: {line}"),
                    _ => {
                        a.mac.put(dir_id, name, &data);
                        expect.insert(path, Some(data));
                    }
                }
            }
            25..=39 => {
                let Some(path) = files.get(rnd(files.len() as u64) as usize).cloned() else { continue };
                let content = expect.get_mut(&path).unwrap().as_mut().unwrap();
                let at = rnd(content.len() as u64 + 1) as usize;
                let patch = synth::bytes(step, 0, 64)[..1 + rnd(60) as usize].to_vec();
                line = format!("patch {path} at {at} (+{})", patch.len());
                fs.write(fs.find_path(&path).unwrap(), at as u64, &patch).unwrap();
                if content.len() < at + patch.len() {
                    content.resize(at + patch.len(), 0);
                }
                content[at..at + patch.len()].copy_from_slice(&patch);
            }
            40..=46 => {
                let Some(path) = files.get(rnd(files.len() as u64) as usize).cloned() else { continue };
                let content = expect.get_mut(&path).unwrap().as_mut().unwrap();
                let size = rnd(content.len() as u64 + 2000) as usize;
                line = format!("resize {path} to {size}");
                fs.set_size(fs.find_path(&path).unwrap(), size as u64).unwrap();
                content.resize(size, 0);
            }
            47..=58 => {
                let Some(from) = all.get(rnd(all.len() as u64) as usize).cloned() else { continue };
                let (from_dir, from_name) = from.rsplit_once('/').unwrap_or(("", &from));
                line = format!("rename {from} to {path}");
                let result = fs.rename(fs.find_path(from_dir).unwrap(), from_name, dir_id, name);
                let into_itself = dir == from || dir.starts_with(&format!("{from}/"));
                let target_full = expect.get(&path).is_some_and(|t| t.is_none()) && expect.keys().any(|k| k.starts_with(&format!("{path}/")));
                if from == path {
                    assert_eq!(result, Ok(()), "step {step}: {line}");
                } else if into_itself {
                    assert_eq!(result, Err(FsError::Invalid), "step {step}: {line}");
                } else if target_full {
                    assert_eq!(result, Err(FsError::NotEmpty), "step {step}: {line}");
                } else {
                    assert_eq!(result, Ok(()), "step {step}: {line}");
                    expect.remove(&path);
                    let moved: Vec<String> = expect.keys().filter(|k| **k == from || k.starts_with(&format!("{from}/"))).cloned().collect();
                    for old in moved {
                        let value = expect.remove(&old).unwrap();
                        expect.insert(format!("{path}{}", &old[from.len()..]), value);
                    }
                }
            }
            59..=66 => {
                line = format!("mkdir {path}");
                match (fs.mkdir(dir_id, name), expect.contains_key(&path)) {
                    (Ok(_), false) => {
                        expect.insert(path, None);
                    }
                    (Err(FsError::Exists), true) => {}
                    (other, existed) => panic!("step {step}: {line}: {other:?} (existed: {existed})"),
                }
            }
            67..=74 => {
                let Some(path) = all.get(rnd(all.len() as u64) as usize).cloned() else { continue };
                let (parent, name) = path.rsplit_once('/').unwrap_or(("", &path));
                line = format!("remove {path}");
                let result = fs.remove(fs.find_path(parent).unwrap(), name);
                if expect.keys().any(|k| k.starts_with(&format!("{path}/"))) {
                    assert_eq!(result, Err(FsError::NotEmpty), "step {step}: {line}");
                } else {
                    assert_eq!(result, Ok(()), "step {step}: {line}");
                    expect.remove(&path);
                }
            }
            // Read a file back. With the cloud away or losing messages a read may fail; it may never be wrong.
            75..=82 => {
                // Any file this Mac shows, the other Mac's included.
                let known: Vec<String> = expect.iter().filter(|(k, v)| v.is_some() && fs.find_path(k).is_some()).map(|(k, _)| k.clone()).collect();
                let Some(path) = known.get(rnd(known.len() as u64) as usize).cloned() else { continue };
                line = format!("read {path}");
                let want = expect[&path].as_ref().unwrap();
                let id = fs.find_path(&path).unwrap();
                let mut got = Vec::new();
                let mut failed = false;
                while got.len() < want.len() {
                    match fs.read(id, got.len() as u64, MIB as u32).await {
                        Ok((piece, _)) if !piece.is_empty() => got.extend_from_slice(&piece),
                        Ok(_) => break,
                        Err(FsError::Io(_)) => {
                            failed = true;
                            break;
                        }
                        Err(e) => panic!("step {step}: {line}: {e:?}"),
                    }
                }
                if failed {
                    reads_failed += 1;
                } else {
                    assert!(got == *want, "step {step}: {line}: the bytes read are not the bytes written\n{}", trace.join("\n"));
                    reads_ok += 1;
                }
            }
            // The other Mac saves into its own folder, or deletes there.
            83..=86 => {
                if let Some(t) = thaw.take() {
                    t.await.unwrap();
                }
                let theirs: Vec<String> = expect.keys().filter(|k| k.starts_with("remote/")).cloned().collect();
                if !theirs.is_empty() && rnd(4) == 0 {
                    let gone = theirs[rnd(theirs.len() as u64) as usize].clone();
                    line = format!("the other Mac removes {gone}");
                    other.fs.remove(remote_dir, &gone["remote/".len()..]).unwrap();
                    expect.remove(&gone);
                } else {
                    remote_n += 1;
                    let data = synth::bytes(seed ^ (step << 20), 0, 8 * (1 + rnd(4000) as usize));
                    line = format!("the other Mac writes remote/r{remote_n} ({} bytes)", data.len());
                    other.put(remote_dir, &format!("r{remote_n}"), &data);
                    expect.insert(format!("remote/r{remote_n}"), Some(data));
                }
                other.flush().await;
            }
            87..=89 => {
                let ms = 300 + rnd(2500);
                line = format!("freeze the Worker for {ms} ms");
                if let Some(t) = thaw.take() {
                    t.await.unwrap();
                }
                b.worker.freeze();
                let pid = b.worker.pid();
                thaw = Some(tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(ms)).await;
                    signal_group(pid, libc::SIGCONT);
                }));
                freezes += 1;
            }
            90..=92 => {
                line = "kill -9 the Worker, keep writing, start it again".into();
                if let Some(t) = thaw.take() {
                    t.await.unwrap();
                }
                b.worker.kill9();
                for i in 0..3 {
                    let data = synth::bytes(step * 10 + i, 0, 2048);
                    let name = format!("while-away-{step}-{i}");
                    a.mac.put(ROOT_ID, &name, &data);
                    expect.insert(name, Some(data));
                }
                b.worker.restart().await;
                kills += 1;
            }
            93..=95 => {
                line = "the app is killed and started again".into();
                if let Some(t) = thaw.take() {
                    t.await.unwrap();
                }
                let before = a.mac.tree();
                let (told, downloaded) = a.crash(&b, "a").await;
                a = Live::start(&b, "a", told, downloaded).await;
                let mine_only = |tree: BTreeMap<String, Option<u64>>| tree.into_iter().filter(|(k, _)| k != "remote" && !k.starts_with("remote/")).collect::<BTreeMap<_, _>>();
                assert_eq!(mine_only(a.mac.tree()), mine_only(before), "step {step}: after the restart this Mac shows something else\n{}", trace.join("\n"));
                crashes += 1;
            }
            // Let everything go up and look at the cloud, so that what is read next is read from there.
            96..=97 => {
                line = "save everything and look at the cloud".into();
                if fs.flush(Duration::from_secs(20)).await {
                    let _ = fs.refresh().await;
                }
            }
            _ => {
                let ms = 100 + rnd(900);
                line = format!("wait {ms} ms");
                tokio::time::sleep(Duration::from_millis(ms)).await;
            }
        }
        trace.push(format!("{step}: {line}"));
    }

    // The faults stop. Everything settles, and the cloud holds what was written.
    if let Some(t) = thaw.take() {
        t.await.unwrap();
    }
    b.proxy.rates(0, 0);
    assert!(a.mac.fs.flush(Duration::from_secs(180)).await, "the drive never settled: {:?}\n{}", a.mac.fs.pending(), trace.join("\n"));
    other.flush().await;
    let cloud = b.cloud().await;
    let files: BTreeMap<String, Option<Vec<u8>>> = cloud.iter().map(|(p, e)| (p.clone(), e.content.clone())).collect();
    let extra: Vec<&String> = files.keys().filter(|k| !expect.contains_key(*k)).collect();
    let missing: Vec<&String> = expect.keys().filter(|k| !files.contains_key(*k)).collect();
    assert!(extra.is_empty() && missing.is_empty(), "the cloud has extra {extra:?} and lacks {missing:?}\n{}", trace.join("\n"));
    for (path, want) in &expect {
        assert!(files[path] == *want, "{path} does not hold what was written ({:?} bytes against {:?})\n{}", files[path].as_ref().map(Vec::len), want.as_ref().map(Vec::len), trace.join("\n"));
    }
    // Both Macs, once they have looked, show exactly the cloud.
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    while !(matches!(a.mac.fs.refresh().await, Ok(true)) && a.mac.tree() == sizes(&cloud)) {
        assert!(std::time::Instant::now() < deadline, "this Mac does not come to show what the cloud has: {:?}", a.mac.fs.pending());
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(other.fs.refresh().await.unwrap());
    assert_eq!(other.tree(), sizes(&cloud));
    let (downloaded, f) = (a.downloaded(), &b.proxy.faults);
    let mut told = a.told.clone();
    told.extend(a.mac.fs.take_notices());
    assert!(told.is_empty(), "nobody else touched this Mac's files, so it should have nothing to report: {told:?}");
    b.settled().await;

    // Delete everything from this Mac: nothing may stay in use.
    let mut paths: Vec<String> = expect.keys().cloned().collect();
    paths.sort_by_key(|p| std::cmp::Reverse(p.matches('/').count()));
    for path in &paths {
        let (parent, name) = path.rsplit_once('/').unwrap_or(("", path));
        a.mac.fs.remove(a.mac.fs.find_path(parent).unwrap(), name).unwrap();
    }
    assert!(a.mac.fs.flush(Duration::from_secs(120)).await);
    assert!(b.cloud().await.is_empty());
    let stats = b.reader.collect(&b.keys.drive).await;
    assert_eq!((stats["versions"].as_u64(), stats["blobs_in_use"].as_u64(), stats["nodes"].as_u64()), (Some(0), Some(0), Some(1)), "something is still in use after everything was deleted");
    b.settled().await;

    eprintln!("\n| Sealed engine soak, seed {seed}, {steps} steps | |");
    eprintln!("|---|---|");
    eprintln!("| Wall time | {:.0} s |", started.elapsed().as_secs_f64());
    eprintln!("| Files and folders at the end | {} |", expect.len());
    eprintln!("| kill -9 of the Worker / kills of the app / freezes | {kills} / {crashes} / {freezes} |");
    eprintln!("| Proxy: connections / replies lost / requests lost | {} / {} / {} |", f.connections.load(Ordering::SeqCst), f.replies_lost.load(Ordering::SeqCst), f.requests_lost.load(Ordering::SeqCst));
    eprintln!("| Reads checked against what was written / reads that failed while the cloud was away | {reads_ok} / {reads_failed} |");
    eprintln!("| Read back from the cloud by this Mac: bytes / range requests | {} / {} |", downloaded.0, downloaded.1);
    assert!(downloaded.1 > 0, "no read went to the cloud; the reads did not test the read path");
    assert!(f.replies_lost.load(Ordering::SeqCst) > 0 && f.requests_lost.load(Ordering::SeqCst) > 0, "the faults did not fire");
}
