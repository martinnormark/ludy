//! End-to-end tests of the engine against an in-process cloud server.
//! No kernel mount is involved: reads and writes go through `DriveFs` and
//! the NFS adapter directly, and the server's request log is the witness.

use std::sync::Arc;
use std::time::{Duration, Instant};

use ludy_cloud::{synth, Cloud, Config, LogEntry};
use ludy_core::cache::{BlockCache, BLOCK_SIZE};
use ludy_core::client::CloudClient;
use ludy_core::drive::{DriveFs, FsError, ROOT_ID};
use ludy_core::stats::Stats;

const GIB: u64 = 1 << 30;

struct Rig {
    _dir: tempfile::TempDir,
    cloud: Arc<Cloud>,
    client: CloudClient,
    cache: Arc<BlockCache>,
    stats: Arc<Stats>,
    fs: Arc<DriveFs>,
}

impl Rig {
    /// GET requests for object bodies, i.e. ranges actually served.
    fn ranges(&self) -> Vec<LogEntry> {
        self.cloud.log_since(0).into_iter().filter(|e| e.method == "GET" && e.range.is_some()).collect()
    }
}

async fn rig_with(latency_ms: u64, cache_cap: u64) -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let cfg = Config { data_dir: dir.path().join("cloud"), addr: "127.0.0.1:0".parse().unwrap(), latency_ms, mbps: 0, print_log: false };
    let (addr, cloud, _task) = ludy_cloud::serve(cfg).await.unwrap();
    let stats = Arc::new(Stats::default());
    let client = CloudClient::new(&format!("http://{addr}"), stats.clone());
    client.create_drive("media").await.unwrap();
    let cache = Arc::new(BlockCache::open(&dir.path().join("cache"), cache_cap).unwrap());
    let fs = DriveFs::new("media", client.clone(), cache.clone(), stats.clone(), dir.path().join("journal")).unwrap();
    Rig { _dir: dir, cloud, client, cache, stats, fs }
}

async fn rig() -> Rig {
    rig_with(0, 64 << 20).await
}

fn pattern(len: usize, salt: u8) -> Vec<u8> {
    (0..len).map(|i| ((i * 31 + i / 251) as u8) ^ salt).collect()
}

fn id(fs: &DriveFs, path: &str) -> u64 {
    fs.find_path(path).unwrap_or_else(|| panic!("no node at {path}"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn listing_shows_real_sizes_and_downloads_nothing() {
    let r = rig().await;
    r.client.put_synthetic("media", "raw/take-01.braw", 50 * GIB, 7).await.unwrap();
    r.client.put_bytes("media", "notes/readme.txt", pattern(1234, 0).into()).await.unwrap();
    assert!(r.fs.refresh().await.unwrap());

    let root: Vec<String> = r.fs.readdir(ROOT_ID).unwrap().into_iter().map(|(n, _)| n).collect();
    assert_eq!(root, ["notes", "raw"]);
    let big = r.fs.getattr(id(&r.fs, "raw/take-01.braw")).unwrap();
    assert_eq!(big.size, 50 * GIB);
    assert_eq!(big.local_bytes, 0, "nothing of the 50 GB file is on this machine");
    assert_eq!(r.fs.getattr(id(&r.fs, "notes/readme.txt")).unwrap().size, 1234);

    assert!(r.ranges().is_empty(), "listing must not fetch object bodies");
    assert_eq!(r.cache.snapshot().bytes, 0);
    assert_eq!(r.stats.sample().bytes_down, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reading_the_middle_of_a_50gb_file_fetches_only_that_range() {
    let r = rig().await;
    r.client.put_synthetic("media", "huge.bin", 50 * GIB, 99).await.unwrap();
    r.fs.refresh().await.unwrap();
    let file = id(&r.fs, "huge.bin");

    let offset = 25 * GIB + 12_345;
    let (data, eof) = r.fs.read(file, offset, 64 * 1024).await.unwrap();
    assert!(!eof);
    assert_eq!(data, synth::bytes(99, offset, 64 * 1024), "bytes must match the generator");

    let ranges = r.ranges();
    assert_eq!(ranges.len(), 1, "one range request: {ranges:?}");
    let (start, end) = ranges[0].range.unwrap();
    assert!(start <= offset && end >= offset + 64 * 1024 - 1);
    assert!(end - start + 1 <= 2 * BLOCK_SIZE, "fetched {} bytes for a 64 KiB read", end - start + 1);
    assert!(r.cache.snapshot().bytes <= 2 * BLOCK_SIZE);

    // The same read again is served from the disk cache.
    let (again, _) = r.fs.read(file, offset, 64 * 1024).await.unwrap();
    assert_eq!(again, data);
    assert_eq!(r.ranges().len(), 1, "second read must not reach the server");
    assert!(r.stats.sample().cache_hit_bytes >= 64 * 1024);

    // The very end of the file.
    let (tail, eof) = r.fs.read(file, 50 * GIB - 10, 4096).await.unwrap();
    assert!(eof);
    assert_eq!(tail, synth::bytes(99, 50 * GIB - 10, 10));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_new_cache_instance_reuses_blocks_from_disk() {
    let r = rig().await;
    r.client.put_synthetic("media", "huge.bin", GIB, 5).await.unwrap();
    r.fs.refresh().await.unwrap();
    let (first, _) = r.fs.read(id(&r.fs, "huge.bin"), 700_000_000, 100_000).await.unwrap();
    let before = r.ranges().len();

    // Simulate an app restart: a fresh cache object over the same folder.
    let cache2 = Arc::new(BlockCache::open(&r._dir.path().join("cache"), 64 << 20).unwrap());
    let fs2 = DriveFs::new("media", r.client.clone(), cache2, r.stats.clone(), r._dir.path().join("journal2")).unwrap();
    fs2.refresh().await.unwrap();
    let (second, _) = fs2.read(id(&fs2, "huge.bin"), 700_000_000, 100_000).await.unwrap();
    assert_eq!(first, second);
    assert_eq!(r.ranges().len(), before, "warm cache: no request after restart");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sequential_reads_trigger_read_ahead_in_coalesced_ranges() {
    let r = rig().await;
    r.client.put_synthetic("media", "clip.mov", GIB, 3).await.unwrap();
    r.fs.refresh().await.unwrap();
    let file = id(&r.fs, "clip.mov");
    let mut off = 0u64;
    for _ in 0..12 {
        let (data, _) = r.fs.read(file, off, BLOCK_SIZE as u32).await.unwrap();
        assert_eq!(data, synth::bytes(3, off, BLOCK_SIZE as usize));
        off += BLOCK_SIZE;
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    let ranges = r.ranges();
    let furthest = ranges.iter().map(|e| e.range.unwrap().1).max().unwrap();
    assert!(furthest >= off + 4 * BLOCK_SIZE, "read-ahead should be well past the reader: read to {off}, fetched to {furthest}");
    assert!(ranges.iter().all(|e| e.bytes <= 8 * BLOCK_SIZE), "each request is at most 2 MiB");
    assert!(ranges.len() < 12, "sequential blocks are coalesced: {} requests for 12 reads", ranges.len());
    // No block is fetched twice.
    let total: u64 = ranges.iter().map(|e| e.bytes).sum();
    assert_eq!(total, r.cache.snapshot().bytes);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_large_read_is_fetched_in_parallel() {
    let r = rig_with(300, 64 << 20).await;
    r.client.put_synthetic("media", "clip.mov", GIB, 3).await.unwrap();
    r.fs.refresh().await.unwrap();
    let file = id(&r.fs, "clip.mov");
    let t0 = Instant::now();
    // 8 MiB = four 2 MiB range requests, each delayed 300 ms by the server.
    let (data, _) = r.fs.read(file, 100 * BLOCK_SIZE, (32 * BLOCK_SIZE) as u32).await.unwrap();
    let elapsed = t0.elapsed();
    assert_eq!(data.len() as u64, 32 * BLOCK_SIZE);
    assert_eq!(data, synth::bytes(3, 100 * BLOCK_SIZE, (32 * BLOCK_SIZE) as usize));
    assert_eq!(r.ranges().len(), 4);
    assert!(elapsed < Duration::from_millis(1000), "four 300 ms requests took {elapsed:?}; they should overlap");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_readers_of_one_block_share_a_single_fetch() {
    let r = rig_with(150, 64 << 20).await;
    r.client.put_synthetic("media", "clip.mov", GIB, 3).await.unwrap();
    r.fs.refresh().await.unwrap();
    let file = id(&r.fs, "clip.mov");
    let reads = (0..6).map(|i| {
        let fs = r.fs.clone();
        tokio::spawn(async move { fs.read(file, 5 * BLOCK_SIZE + i * 1000, 4096).await.unwrap().0 })
    });
    for (i, h) in reads.enumerate() {
        assert_eq!(h.await.unwrap(), synth::bytes(3, 5 * BLOCK_SIZE + i as u64 * 1000, 4096));
    }
    assert_eq!(r.ranges().len(), 1, "six readers of the same block, one request");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_cache_stays_under_its_cap() {
    let cap = 4 * BLOCK_SIZE;
    let r = rig_with(0, cap).await;
    r.client.put_synthetic("media", "clip.mov", GIB, 11).await.unwrap();
    r.fs.refresh().await.unwrap();
    let file = id(&r.fs, "clip.mov");
    for i in 0..20u64 {
        let off = i * 10 * BLOCK_SIZE + 17;
        let (data, _) = r.fs.read(file, off, 50_000).await.unwrap();
        assert_eq!(data, synth::bytes(11, off, 50_000));
        assert!(r.cache.snapshot().bytes <= cap, "cache {} over cap {cap}", r.cache.snapshot().bytes);
    }
    assert!(r.cache.snapshot().evicted_bytes > 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_write_returns_at_once_and_uploads_in_the_background() {
    let r = rig_with(200, 64 << 20).await;
    r.fs.refresh().await.unwrap();
    let body = pattern(700_000, 0x5a);

    let t0 = Instant::now();
    let file = r.fs.create(ROOT_ID, "cut.prproj", false).unwrap();
    for (i, chunk) in body.chunks(100_000).enumerate() {
        r.fs.write(file, i as u64 * 100_000, chunk).unwrap();
    }
    assert!(t0.elapsed() < Duration::from_millis(150), "writes must not wait for the 200 ms cloud: {:?}", t0.elapsed());

    // Readable straight away, from the journal.
    let (back, eof) = r.fs.read(file, 0, 1_000_000).await.unwrap();
    assert!(eof);
    assert_eq!(back, body);
    let pending = r.fs.pending();
    assert_eq!((pending.files, pending.bytes), (1, 700_000));
    assert!(r.client.list("media").await.unwrap().objects.is_empty(), "nothing uploaded yet");

    assert!(r.fs.flush(Duration::from_secs(20)).await);
    assert!(r.fs.pending().is_idle());
    let listing = r.client.list("media").await.unwrap();
    assert_eq!(listing.objects.len(), 1);
    assert_eq!((listing.objects[0].key.as_str(), listing.objects[0].size), ("cut.prproj", 700_000));
    let remote = r.client.get_range("media", "cut.prproj", &listing.objects[0].version, 0, 700_000).await.unwrap();
    assert_eq!(&remote[..], &body[..]);

    // After the upload the bytes are in the block cache: reading them again
    // does not download what we just wrote.
    let before = r.ranges().len();
    let (again, _) = r.fs.read(file, 123_456, 200_000).await.unwrap();
    assert_eq!(again, body[123_456..323_456]);
    assert_eq!(r.ranges().len(), before);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn patching_part_of_a_cloud_file_merges_with_the_rest() {
    let r = rig().await;
    let original = pattern(1_000_000, 1);
    r.client.put_bytes("media", "doc.bin", original.clone().into()).await.unwrap();
    r.fs.refresh().await.unwrap();
    let file = id(&r.fs, "doc.bin");

    r.fs.write(file, 300_000, b"PATCHED").unwrap();
    let mut expected = original.clone();
    expected[300_000..300_007].copy_from_slice(b"PATCHED");
    let (merged, _) = r.fs.read(file, 299_990, 40).await.unwrap();
    assert_eq!(merged, expected[299_990..300_030]);

    // Appending past the end grows the file.
    r.fs.write(file, 1_000_000, b"tail").unwrap();
    expected.extend_from_slice(b"tail");
    assert_eq!(r.fs.getattr(file).unwrap().size, 1_000_004);

    assert!(r.fs.flush(Duration::from_secs(20)).await);
    let obj = r.client.list("media").await.unwrap().objects.remove(0);
    assert_eq!(obj.size, 1_000_004);
    assert_eq!(obj.versions, 2, "the edit is a new version of the same object");
    let remote = r.client.get_range("media", "doc.bin", &obj.version, 0, obj.size).await.unwrap();
    assert_eq!(&remote[..], &expected[..]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn truncate_then_rewrite_replaces_the_content() {
    let r = rig().await;
    r.client.put_bytes("media", "notes.txt", pattern(50_000, 2).into()).await.unwrap();
    r.fs.refresh().await.unwrap();
    let file = id(&r.fs, "notes.txt");
    r.fs.set_size(file, 0).unwrap();
    r.fs.write(file, 0, b"short and new").unwrap();
    let (data, eof) = r.fs.read(file, 0, 4096).await.unwrap();
    assert!(eof);
    assert_eq!(data, b"short and new");
    // Growing with truncate leaves zeros, not old cloud bytes.
    r.fs.set_size(file, 20).unwrap();
    let (data, _) = r.fs.read(file, 0, 4096).await.unwrap();
    assert_eq!(data, b"short and new\0\0\0\0\0\0\0");
    assert!(r.fs.flush(Duration::from_secs(20)).await);
    let obj = r.client.list("media").await.unwrap().objects.remove(0);
    assert_eq!(&r.client.get_range("media", "notes.txt", &obj.version, 0, obj.size).await.unwrap()[..], b"short and new\0\0\0\0\0\0\0");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn folders_renames_and_deletes_are_replayed_in_order() {
    let r = rig().await;
    r.client.put_bytes("media", "old/keep.txt", pattern(10, 3).into()).await.unwrap();
    r.fs.refresh().await.unwrap();

    let dir = r.fs.mkdir(ROOT_ID, "footage").unwrap();
    let a = r.fs.create(dir, "a.tmp", false).unwrap();
    r.fs.write(a, 0, b"clip a").unwrap();
    // Safe-save: write a temp file, then rename it into place.
    r.fs.rename(dir, "a.tmp", dir, "a.mov").unwrap();
    // Move a cloud file into the new folder, and rename a folder.
    r.fs.rename(id(&r.fs, "old"), "keep.txt", dir, "kept.txt").unwrap();
    r.fs.remove(ROOT_ID, "old").unwrap();
    r.fs.rename(ROOT_ID, "footage", ROOT_ID, "day-1").unwrap();
    assert_eq!(r.fs.mkdir(ROOT_ID, "day-1"), Err(FsError::Exists));

    assert!(r.fs.flush(Duration::from_secs(20)).await);
    let mut keys: Vec<String> = r.client.list("media").await.unwrap().objects.into_iter().map(|o| o.key).collect();
    keys.sort();
    assert_eq!(keys, ["day-1/", "day-1/a.mov", "day-1/kept.txt"]);

    // The tree survives a refresh from the cloud unchanged, ids included.
    let before = id(&r.fs, "day-1/a.mov");
    assert!(r.fs.refresh().await.unwrap());
    assert_eq!(id(&r.fs, "day-1/a.mov"), before);

    r.fs.remove(id(&r.fs, "day-1"), "a.mov").unwrap();
    assert_eq!(r.fs.remove(ROOT_ID, "day-1"), Err(FsError::NotEmpty));
    assert!(r.fs.flush(Duration::from_secs(20)).await);
    let keys: Vec<String> = r.client.list("media").await.unwrap().objects.into_iter().map(|o| o.key).collect();
    assert_eq!(keys, ["day-1/", "day-1/kept.txt"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn finder_droppings_never_reach_the_cloud() {
    let r = rig().await;
    r.fs.refresh().await.unwrap();
    for name in [".DS_Store", "._photo.jpg", ".Spotlight-V100"] {
        let f = r.fs.create(ROOT_ID, name, false).unwrap();
        r.fs.write(f, 0, b"local metadata").unwrap();
    }
    let real = r.fs.create(ROOT_ID, "photo.jpg", false).unwrap();
    r.fs.write(real, 0, b"jpeg").unwrap();
    assert!(r.fs.flush(Duration::from_secs(20)).await);
    let keys: Vec<String> = r.client.list("media").await.unwrap().objects.into_iter().map(|o| o.key).collect();
    assert_eq!(keys, ["photo.jpg"]);
    // Still present locally, and a cloud refresh does not remove them.
    r.fs.refresh().await.unwrap();
    assert!(r.fs.find_path(".DS_Store").is_some());
    let (data, _) = r.fs.read(id(&r.fs, "._photo.jpg"), 0, 100).await.unwrap();
    assert_eq!(data, b"local metadata");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn changes_made_on_the_server_appear_within_seconds() {
    let r = rig().await;
    r.client.put_bytes("media", "shared.txt", b"version one".to_vec().into()).await.unwrap();
    r.fs.refresh().await.unwrap();
    let file = id(&r.fs, "shared.txt");
    assert_eq!(r.fs.read(file, 0, 100).await.unwrap().0, b"version one");
    let poller = tokio::spawn(r.fs.clone().run_poller());

    // Another machine (here: a plain HTTP client) adds, changes and deletes.
    let t0 = Instant::now();
    r.client.put_bytes("media", "from-elsewhere/new.txt", b"hello".to_vec().into()).await.unwrap();
    r.client.put_bytes("media", "shared.txt", b"version two, longer".to_vec().into()).await.unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while r.fs.find_path("from-elsewhere/new.txt").is_none() || r.fs.getattr(file).unwrap().size != 19 {
        assert!(Instant::now() < deadline, "remote change not picked up within 5 s");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(t0.elapsed() < Duration::from_secs(2), "picked up after {:?}", t0.elapsed());
    assert_eq!(r.fs.read(file, 0, 100).await.unwrap().0, b"version two, longer", "same node, new content");

    r.client.delete("media", "from-elsewhere/new.txt").await.unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while r.fs.find_path("from-elsewhere").is_some() {
        assert!(Instant::now() < deadline, "remote delete not picked up within 5 s");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    poller.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_listing_never_undoes_local_changes_that_are_still_uploading() {
    let r = rig().await;
    r.client.put_bytes("media", "a.txt", b"cloud".to_vec().into()).await.unwrap();
    r.fs.refresh().await.unwrap();
    let f = r.fs.create(ROOT_ID, "local.txt", false).unwrap();
    r.fs.write(f, 0, b"not uploaded yet").unwrap();
    assert!(!r.fs.refresh().await.unwrap(), "refresh is deferred while a write is pending");
    assert!(r.fs.find_path("local.txt").is_some());
    assert!(r.fs.flush(Duration::from_secs(20)).await);
    assert!(r.fs.refresh().await.unwrap());
    assert!(r.fs.find_path("local.txt").is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pinning_downloads_the_whole_file_and_protects_it_from_eviction() {
    let cap = 8 * BLOCK_SIZE;
    let r = rig_with(0, cap).await;
    let size = 5 * BLOCK_SIZE + 1000;
    r.client.put_synthetic("media", "keep.mov", size, 21).await.unwrap();
    r.client.put_synthetic("media", "other.mov", GIB, 22).await.unwrap();
    r.fs.refresh().await.unwrap();
    let keep = id(&r.fs, "keep.mov");
    let version = r.fs.version_of(keep).unwrap();
    r.cache.set_pinned(&version, true);
    assert_eq!(r.fs.fetch_all(keep).await.unwrap(), size);
    assert_eq!(r.fs.getattr(keep).unwrap().local_bytes, size);

    // Churn through far more than the cap of another file.
    let other = id(&r.fs, "other.mov");
    for i in 0..30u64 {
        r.fs.read(other, i * 3 * BLOCK_SIZE, 1000).await.unwrap();
    }
    assert_eq!(r.cache.version_bytes(&version), size, "pinned blocks survive");
    let before = r.ranges().len();
    let (data, _) = r.fs.read(keep, 0, size as u32).await.unwrap();
    assert_eq!(data, synth::bytes(21, 0, size as usize));
    assert_eq!(r.ranges().len(), before, "a pinned file reads without the network");
}

mod nfs_adapter {
    use super::*;
    use ludy_core::nfs::NfsDrive;
    use nfsserve::nfs::{nfsstat3, sattr3, set_size3};
    use nfsserve::vfs::NFSFileSystem;

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn serves_listing_reads_and_writes() {
        let r = rig().await;
        r.client.put_synthetic("media", "raw/huge.bin", 50 * GIB, 4).await.unwrap();
        for i in 0..40 {
            r.client.put_bytes("media", &format!("many/file-{i:02}.txt"), pattern(10 + i, 0).into()).await.unwrap();
        }
        r.fs.refresh().await.unwrap();
        let nfs = NfsDrive::new(r.fs.clone(), false);

        let raw = nfs.lookup(nfs.root_dir(), &b"raw".to_vec().into()).await.unwrap();
        let huge = nfs.lookup(raw, &b"huge.bin".to_vec().into()).await.unwrap();
        let attr = nfs.getattr(huge).await.unwrap();
        assert_eq!(attr.size, 50 * GIB);
        assert_eq!(attr.used, 0, "Finder's 'on disk' figure: nothing local yet");
        let (data, eof) = nfs.read(huge, 40 * GIB, 8192).await.unwrap();
        assert!(!eof);
        assert_eq!(data, synth::bytes(4, 40 * GIB, 8192));
        assert!(nfs.getattr(huge).await.unwrap().used >= 8192);

        // Paginated directory listing: every entry exactly once.
        let many = nfs.lookup(nfs.root_dir(), &b"many".to_vec().into()).await.unwrap();
        let mut names = Vec::new();
        let mut cookie = 0;
        loop {
            let page = nfs.readdir(many, cookie, 7).await.unwrap();
            assert!(page.entries.len() <= 7);
            for e in &page.entries {
                names.push(String::from_utf8(e.name.0.clone()).unwrap());
                cookie = e.fileid;
            }
            if page.end {
                break;
            }
        }
        assert_eq!(names.len(), 40);
        assert_eq!(names, (0..40).map(|i| format!("file-{i:02}.txt")).collect::<Vec<_>>());

        // File handles round-trip, and a handle from another mount is stale.
        let fh = nfs.id_to_fh(huge);
        assert_eq!(nfs.fh_to_id(&fh).unwrap(), huge);
        tokio::time::sleep(Duration::from_millis(5)).await;
        let other = NfsDrive::new(r.fs.clone(), false);
        assert!(matches!(other.fh_to_id(&fh), Err(nfsstat3::NFS3ERR_STALE)));

        // Create + write + truncate through the NFS surface.
        let (id, _) = nfs.create(nfs.root_dir(), &b"new.txt".to_vec().into(), sattr3::default()).await.unwrap();
        let attr = nfs.write(id, 0, b"hello nfs").await.unwrap();
        assert_eq!(attr.size, 9);
        let attr = nfs.setattr(id, sattr3 { size: set_size3::size(5), ..Default::default() }).await.unwrap();
        assert_eq!(attr.size, 5);
        assert_eq!(nfs.read(id, 0, 100).await.unwrap(), (b"hello".to_vec(), true));
        assert!(matches!(nfs.create_exclusive(nfs.root_dir(), &b"new.txt".to_vec().into()).await, Err(nfsstat3::NFS3ERR_EXIST)));

        // A read-only export refuses writes.
        let ro = NfsDrive::new(r.fs.clone(), true);
        assert!(matches!(ro.write(id, 0, b"x").await, Err(nfsstat3::NFS3ERR_ROFS)));
    }
}
