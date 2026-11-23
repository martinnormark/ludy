//! One end-to-end encrypted drive as a filesystem, over the node-tree
//! protocol (`ludy_proto::tree`).
//!
//! A tree built from what the cloud lists, reads that fetch only what an
//! application asks for, and writes that land in a local journal and upload
//! in the background. The cloud sees none of it in the clear:
//!
//! * The tree comes from the drive's feed of nodes. Each node's key is
//!   unwrapped with its folder's key, from the root key down, and its name
//!   and modification time are opened with that.
//! * A read finds the bytes through the file's sealed manifest and fetches
//!   only the frames of the sealed shards that hold them. The cache holds
//!   opened blocks, by shard.
//! * An upload cuts and seals what changed, sends the sealed shards the
//!   cloud lacks, and commits a sealed manifest.
//! * Creating, moving and deleting are ops on nodes with ids, queued and
//!   sent in commits that are safe to send twice.
//!
//! Not here yet: keeping a file offline (pinning).

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use bytes::{Bytes, BytesMut};
use futures_util::StreamExt;
use ludy_proto::crypt::{self, CutKey, DedupKey, FileMeta, NodeId, NodeKey, PrivateManifest, PrivateSegment, Random, ShardKey, FRAME, TAG};
use ludy_proto::shard::{self, sha256_hex};
use ludy_proto::tree::*;
use ludy_proto::synth;
use tokio::sync::Semaphore;

use crate::cache::{BlockCache, BLOCK_SIZE};
use crate::fs::{is_local_only, Attr, FsError, FsResult, Pending, ROOT_ID};
use crate::patch::{self, Piece as Patch};
use crate::rangeset::RangeSet;
use crate::sealed_client::{Reply, SealedClient};
use crate::stats::Stats;

/// Missing blocks next to each other in one shard are fetched with one range
/// request of at most this many blocks (2 MiB).
const MAX_RUN_BLOCKS: u64 = 8;
/// Upper bound for how far ahead of a sequential reader we prefetch.
const MAX_READAHEAD: u64 = 32 * BLOCK_SIZE;
const FETCH_PARALLELISM: usize = 8;
const UPLOAD_FILES: usize = 8;
const UPLOAD_PARALLELISM: usize = 4;
const CUT_READ_BYTES: u64 = 1 << 20;
const UPLOAD_QUIET: Duration = Duration::from_millis(1500);
/// Namespace ops sent in one commit.
const OPS_PER_COMMIT: usize = 200;
/// Sealed shards gathered before asking the cloud which of them it lacks.
const SEAL_BATCH_BYTES: usize = 8 << 20;
const LAYOUTS_REMEMBERED: usize = 256;
/// Frames of a sealed shard in one cache block.
const BLOCK_FRAMES: u64 = BLOCK_SIZE / FRAME as u64;
/// How often the poller asks the feed for changes.
const POLL: Duration = Duration::from_millis(1000);

/// The system's random generator, for ids, keys and nonces.
pub struct OsRandom;

impl Random for OsRandom {
    fn fill(&mut self, buf: &mut [u8]) {
        getrandom::getrandom(buf).expect("the system's random generator")
    }
}

/// What it takes to open a drive: where it is, and the key of its root.
/// Whoever has this can read and write everything in the drive.
#[derive(Clone, Debug)]
pub struct DriveKeys {
    /// The drive's id, 32 hex characters.
    pub drive: String,
    pub root: NodeId,
    pub root_key: NodeKey,
}

fn hex(bytes: &[u8]) -> String {
    crypt::hex(bytes)
}

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len() / 2).filter_map(|i| u8::from_str_radix(s.get(2 * i..2 * i + 2)?, 16).ok()).collect()
}

fn node_id(s: &str) -> Option<NodeId> {
    Some(NodeId(<[u8; 16]>::try_from(&unhex(s)[..]).ok()?))
}

fn now_ms() -> u64 {
    ludy_proto::now_ms()
}

/// Create an empty drive in the cloud and return what opens it.
pub async fn create_drive(client: &SealedClient, name: &str) -> anyhow::Result<DriveKeys> {
    let mut rng = OsRandom;
    let (drive, root) = (NodeId::random(&mut rng), NodeId::random(&mut rng));
    let (root_key, dedup) = (NodeKey::random(&mut rng), DedupKey::random(&mut rng));
    let body = CreateDrive {
        root: hex(&root.0),
        name: hex(&root_key.seal_name(&root, name, &mut rng).map_err(|e| anyhow::anyhow!("drive name: {e}"))?),
        // Wrapping the root key for people is not designed yet. Until it is,
        // the root key wrapped by itself stands here: it gives nothing away,
        // and lets a client check that the key it holds is this drive's.
        key: hex(&root_key.wrap_child(&root, &root_key, &mut rng)),
        dedup: hex(&root_key.wrap_dedup(&root, &dedup, &mut rng)),
    };
    let drive = hex(&drive.0);
    client.create_drive(&drive, &body).await?.accepted("create drive")?;
    Ok(DriveKeys { drive, root, root_key })
}

/// One entry of a file's history.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileVersion {
    /// The drive's sequence number the version was committed at.
    pub id: u64,
    pub size: u64,
    /// When the cloud committed it.
    pub committed_ms: u64,
    /// The file's modification time, as its writer sealed it.
    pub mtime_ms: u64,
}

/// A node's identity in the cloud. A node without one never leaves this Mac.
#[derive(Clone)]
struct Cloud {
    id: NodeId,
    key: NodeKey,
    /// The cloud has the node: its create has been answered.
    committed: bool,
}

/// A version of a file in the cloud.
#[derive(Clone, Debug, PartialEq)]
struct Remote {
    version: u64,
    size: u64,
    /// Name of the sealed manifest.
    manifest: Arc<str>,
}

struct Local {
    path: PathBuf,
    file: Arc<File>,
    extents: RangeSet,
    /// The cloud version underneath, and how much of it is still valid.
    base: Option<Remote>,
    base_len: u64,
    gen: u64,
    last_write: Instant,
}

struct FileNode {
    size: u64,
    remote: Option<Remote>,
    local: Option<Local>,
    uploading: bool,
}

enum Kind {
    Dir(BTreeMap<String, u64>),
    File(FileNode),
}

struct Node {
    parent: u64,
    /// Normalised (NFC), as the cloud compares names.
    name: String,
    mtime_ms: u64,
    kind: Kind,
    cloud: Option<Cloud>,
    /// The last full read of the feed that named this node.
    listed: u64,
}

/// Saved next to a journal file, so its writes survive a crash.
#[derive(serde::Serialize, serde::Deserialize)]
struct Meta {
    /// The file's node id in the cloud.
    node: String,
    size: u64,
    mtime_ms: u64,
    extents: Vec<(u64, u64)>,
    /// The version the writes were made over: (version, size, manifest).
    base: Option<(u64, u64, String)>,
    base_len: u64,
}

/// Journal state found on disk at start, waiting for the first read of the feed.
struct Recovered {
    ops: Vec<Op>,
    inflight: Option<(String, usize)>,
    files: Vec<(PathBuf, Meta)>,
}

struct Tree {
    nodes: HashMap<u64, Node>,
    by_cloud: HashMap<NodeId, u64>,
    next_id: u64,
    seq: u64,
    epoch: Option<u64>,
    /// Namespace ops waiting to be sent, in order.
    ops: VecDeque<Op>,
    /// The commit being sent: its id and how many ops from the front it
    /// carries. Kept until answered, so a retry is the same commit.
    inflight: Option<(String, usize)>,
    busy: bool,
    listings: u64,
    /// The cloud refused something this drive believed: read everything again.
    resync: bool,
}

/// Where a file's bytes are: its manifest, opened.
struct Layout {
    segs: Vec<Seg>,
}

struct Seg {
    /// Offset in the file.
    start: u64,
    n: u64,
    src: Src,
}

#[derive(Clone)]
enum Src {
    Shard { id: Arc<str>, key: ShardKey },
    Synth { seed: u64, off: u64 },
}

/// What a read needs to know about a file besides the bytes wanted.
#[derive(Clone)]
struct FileRef {
    id: NodeId,
    key: NodeKey,
}

#[derive(Default)]
struct ReadAhead {
    next: u64,
    streak: u32,
    ahead_until: u64,
}

/// One range request for consecutive blocks of one shard, in flight.
struct Run {
    first: u64,
    blocks: Mutex<Vec<Bytes>>,
    progress: tokio::sync::watch::Sender<Progress>,
}

#[derive(Clone, Default)]
struct Progress {
    arrived: u64,
    failed: Option<Arc<str>>,
}

impl Run {
    async fn block(&self, idx: u64) -> Result<Bytes, Arc<str>> {
        let n = idx - self.first;
        let mut progress = self.progress.subscribe();
        let seen = progress.wait_for(|p| p.arrived > n || p.failed.is_some()).await.map_err(|_| Arc::<str>::from("the fetch was abandoned"))?.clone();
        if seen.arrived > n {
            return Ok(self.blocks.lock().unwrap()[n as usize].clone());
        }
        Err(seen.failed.unwrap_or_else(|| "the fetch failed".into()))
    }
}

/// Ends a run however its task ends: its blocks stop counting as in flight,
/// and readers waiting for one that did not arrive get an error.
struct RunEnd {
    fs: Arc<SealedDrive>,
    shard: Arc<str>,
    run: Arc<Run>,
    count: u64,
    error: Option<String>,
}

impl Drop for RunEnd {
    fn drop(&mut self) {
        if let Ok(mut inflight) = self.fs.inflight.lock() {
            for idx in self.run.first..self.run.first + self.count {
                inflight.remove(&(self.shard.clone(), idx));
            }
        }
        let (count, error) = (self.count, self.error.take());
        self.run.progress.send_modify(|p| {
            if p.arrived < count {
                p.failed = Some(error.unwrap_or_else(|| "the fetch was abandoned".into()).into());
            }
        });
    }
}

enum Piece {
    Cached(u64),
    Pending(u64, Arc<Run>),
}

/// A blocking `Read` over buffers arriving on a channel.
struct ChannelReader {
    rx: tokio::sync::mpsc::Receiver<std::io::Result<Vec<u8>>>,
    buf: Vec<u8>,
    pos: usize,
}

impl std::io::Read for ChannelReader {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        while self.pos == self.buf.len() {
            match self.rx.blocking_recv() {
                Some(next) => {
                    self.buf = next?;
                    self.pos = 0;
                }
                None => return Ok(0),
            }
        }
        let n = out.len().min(self.buf.len() - self.pos);
        out[..n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

enum ReadSnap {
    Remote { file: FileRef, remote: Remote },
    Local { file: Option<FileRef>, journal: Arc<File>, extents: RangeSet, base: Option<Remote>, base_len: u64 },
}

/// How an upload ended, when it did not simply fail.
enum Uploaded {
    Done { version: u64, manifest: Arc<str>, layout: Arc<Layout> },
    /// The version the writes were made over can no longer be read.
    BaseGone,
    /// The cloud no longer has the file.
    NodeGone,
    /// The file was written while it was being read: try again later.
    Moved,
}

pub struct SealedDrive {
    pub name: String,
    keys: DriveKeys,
    client: SealedClient,
    cache: Arc<BlockCache>,
    stats: Arc<Stats>,
    journal_dir: PathBuf,
    tree: Mutex<Tree>,
    /// The drive's dedup key and the cut points that go with it, once fetched.
    dedup: OnceLock<(DedupKey, CutKey)>,
    inflight: Mutex<HashMap<(Arc<str>, u64), Arc<Run>>>,
    readahead: Mutex<HashMap<u64, ReadAhead>>,
    fetch_sem: Arc<Semaphore>,
    uploading: Mutex<Option<String>>,
    /// Why the last attempt to reach the cloud failed. Cleared by the next success.
    last_error: Mutex<Option<String>>,
    /// What the person at this Mac has to be told: a file kept under another
    /// name, changes that could not be saved. Stays until it is taken.
    notices: Mutex<Vec<String>>,
    pub last_activity_ms: AtomicU64,
    journal_seq: AtomicU64,
    recovered: Mutex<Option<Recovered>>,
    stopped: AtomicBool,
    /// Opened manifests, by manifest name.
    layouts: Mutex<HashMap<Arc<str>, Arc<Layout>>>,
    /// Manifests being fetched: several readers of one file share the fetch.
    layout_loads: Mutex<HashMap<Arc<str>, Arc<tokio::sync::Mutex<()>>>>,
    sync_lock: tokio::sync::Mutex<()>,
    /// Counts every change this drive has made in the cloud.
    sync_gen: AtomicU64,
}

fn write_atomic(path: &std::path::Path, data: &[u8]) {
    let tmp = path.with_extension("tmp");
    if std::fs::write(&tmp, data).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

fn remove_journal(path: &std::path::Path) {
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(path.with_extension("meta"));
}

fn drop_local(nodes: Vec<Node>) {
    for n in nodes {
        if let Kind::File(FileNode { local: Some(l), .. }) = n.kind {
            remove_journal(&l.path);
        }
    }
}

/// The name a local file or folder gets when the cloud already has another
/// one under its name: both are kept.
fn conflict_name(name: &str, what: &str, id: &NodeId) -> String {
    let tag = format!(" ({what} {})", &hex(&id.0)[..6]);
    let (stem, ext) = match name.rfind('.') {
        Some(dot) if dot > 0 && name.len() - dot <= 8 => name.split_at(dot),
        _ => (name, ""),
    };
    let mut stem = stem.to_string();
    while stem.len() + tag.len() + ext.len() > crypt::MAX_NAME {
        stem.pop();
    }
    format!("{stem}{tag}{ext}")
}

impl Tree {
    fn new(keys: &DriveKeys) -> Tree {
        let mut nodes = HashMap::new();
        let cloud = Cloud { id: keys.root, key: keys.root_key.clone(), committed: true };
        nodes.insert(ROOT_ID, Node { parent: ROOT_ID, name: String::new(), mtime_ms: now_ms(), kind: Kind::Dir(BTreeMap::new()), cloud: Some(cloud), listed: 0 });
        let by_cloud = HashMap::from([(keys.root, ROOT_ID)]);
        Tree { nodes, by_cloud, next_id: ROOT_ID + 1, seq: 0, epoch: None, ops: VecDeque::new(), inflight: None, busy: false, listings: 0, resync: false }
    }

    fn dir(&self, id: u64) -> FsResult<&BTreeMap<String, u64>> {
        match &self.nodes.get(&id).ok_or(FsError::NotFound)?.kind {
            Kind::Dir(c) => Ok(c),
            Kind::File(_) => Err(FsError::NotDir),
        }
    }

    fn dir_mut(&mut self, id: u64) -> FsResult<&mut BTreeMap<String, u64>> {
        match &mut self.nodes.get_mut(&id).ok_or(FsError::NotFound)?.kind {
            Kind::Dir(c) => Ok(c),
            Kind::File(_) => Err(FsError::NotDir),
        }
    }

    fn file(&self, id: u64) -> FsResult<&FileNode> {
        match &self.nodes.get(&id).ok_or(FsError::NotFound)?.kind {
            Kind::File(f) => Ok(f),
            Kind::Dir(_) => Err(FsError::IsDir),
        }
    }

    fn file_mut(&mut self, id: u64) -> FsResult<&mut FileNode> {
        match &mut self.nodes.get_mut(&id).ok_or(FsError::NotFound)?.kind {
            Kind::File(f) => Ok(f),
            Kind::Dir(_) => Err(FsError::IsDir),
        }
    }

    fn cloud(&self, id: u64) -> Option<&Cloud> {
        self.nodes.get(&id)?.cloud.as_ref()
    }

    fn path_of(&self, mut id: u64) -> String {
        let mut parts: Vec<&str> = Vec::new();
        while id != ROOT_ID {
            let Some(n) = self.nodes.get(&id) else { break };
            parts.push(&n.name);
            id = n.parent;
        }
        parts.reverse();
        parts.join("/")
    }

    fn find_path(&self, path: &str) -> Option<u64> {
        let mut id = ROOT_ID;
        for part in path.split('/').filter(|p| !p.is_empty()) {
            id = *self.dir(id).ok()?.get(&crypt::normalize_name(part).ok()?)?;
        }
        Some(id)
    }

    fn touch(&mut self, id: u64) {
        if let Some(n) = self.nodes.get_mut(&id) {
            n.mtime_ms = now_ms().max(n.mtime_ms + 1);
        }
    }

    fn insert(&mut self, parent: u64, name: &str, mtime_ms: u64, kind: Kind, cloud: Option<Cloud>) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        if let Some(c) = &cloud {
            self.by_cloud.insert(c.id, id);
        }
        self.nodes.insert(id, Node { parent, name: name.to_string(), mtime_ms, kind, cloud, listed: 0 });
        if let Ok(children) = self.dir_mut(parent) {
            children.insert(name.to_string(), id);
        }
        self.touch(parent);
        id
    }

    /// Detach a node and everything below it.
    fn detach(&mut self, id: u64) -> Vec<Node> {
        let mut out = Vec::new();
        let Some(node) = self.nodes.remove(&id) else { return out };
        if let Ok(children) = self.dir_mut(node.parent) {
            if children.get(&node.name) == Some(&id) {
                children.remove(&node.name);
            }
        }
        self.touch(node.parent);
        let mut stack: Vec<u64> = match &node.kind {
            Kind::Dir(c) => c.values().copied().collect(),
            Kind::File(_) => Vec::new(),
        };
        out.push(node);
        while let Some(child) = stack.pop() {
            if let Some(n) = self.nodes.remove(&child) {
                if let Kind::Dir(c) = &n.kind {
                    stack.extend(c.values().copied());
                }
                out.push(n);
            }
        }
        for n in &out {
            if let Some(c) = &n.cloud {
                self.by_cloud.remove(&c.id);
            }
        }
        out
    }

    /// Put a node under another folder or name. Whatever stood there goes.
    fn relink(&mut self, id: u64, parent: u64, name: &str) {
        let Some(node) = self.nodes.get(&id) else { return };
        let (old_parent, old_name) = (node.parent, node.name.clone());
        if old_parent == parent && old_name == name {
            return;
        }
        if let Some(other) = self.dir(parent).ok().and_then(|c| c.get(name).copied()).filter(|o| *o != id) {
            drop_local(self.detach(other));
        }
        if let Ok(children) = self.dir_mut(old_parent) {
            if children.get(&old_name) == Some(&id) {
                children.remove(&old_name);
            }
        }
        if let Ok(children) = self.dir_mut(parent) {
            children.insert(name.to_string(), id);
        }
        if let Some(node) = self.nodes.get_mut(&id) {
            node.parent = parent;
            node.name = name.to_string();
        }
        self.touch(old_parent);
        self.touch(parent);
    }

    fn has_unsynced(&self) -> bool {
        self.busy || !self.ops.is_empty() || self.nodes.values().any(|n| n.cloud.is_some() && matches!(&n.kind, Kind::File(f) if f.local.is_some() || f.uploading))
    }

    /// A name in `parent` that nothing there has.
    fn free_name(&self, parent: u64, wanted: String) -> String {
        let taken = |name: &str| self.dir(parent).map(|c| c.contains_key(name)).unwrap_or(false);
        if !taken(&wanted) {
            return wanted;
        }
        (2..).map(|n| format!("{wanted} {n}")).find(|name| !taken(name)).expect("a free name")
    }
}

fn load_journal(dir: &std::path::Path) -> (Option<Recovered>, u64) {
    let mut files = Vec::new();
    let mut next = 0;
    let entries: Vec<PathBuf> = std::fs::read_dir(dir).map(|d| d.flatten().map(|e| e.path()).collect()).unwrap_or_default();
    for path in &entries {
        match path.extension().and_then(|e| e.to_str()).unwrap_or("") {
            "journal" => {
                if let Some(n) = path.file_stem().and_then(|s| s.to_str()).and_then(|s| s.parse::<u64>().ok()) {
                    next = next.max(n + 1);
                }
                match std::fs::read(path.with_extension("meta")).ok().and_then(|raw| serde_json::from_slice::<Meta>(&raw).ok()) {
                    Some(meta) => files.push((path.clone(), meta)),
                    None => remove_journal(path),
                }
            }
            "meta" if !path.with_extension("journal").exists() => {
                let _ = std::fs::remove_file(path);
            }
            "tmp" => {
                let _ = std::fs::remove_file(path);
            }
            _ => {}
        }
    }
    files.sort_by(|a, b| a.0.cmp(&b.0));
    // The op log: `+<op>` joined the queue; `!<commit id> <n>` the first n
    // went out as that commit; `-<n>` that many left the front. A torn last
    // line (a crash mid-append) is ignored.
    let mut queue: VecDeque<Op> = VecDeque::new();
    let mut inflight = None;
    for line in std::fs::read_to_string(dir.join("ops.log")).unwrap_or_default().lines() {
        if let Some(op) = line.strip_prefix('+').and_then(|json| serde_json::from_str::<Op>(json).ok()) {
            queue.push_back(op);
        } else if let Some(n) = line.strip_prefix('-').and_then(|n| n.parse::<usize>().ok()) {
            queue.drain(..n.min(queue.len()));
            inflight = None;
        } else if let Some((id, n)) = line.strip_prefix('!').and_then(|rest| rest.split_once(' ')).and_then(|(id, n)| Some((id, n.parse::<usize>().ok()?))) {
            inflight = valid_id(id).then(|| (id.to_string(), n));
        }
    }
    let ops: Vec<Op> = queue.into();
    if files.is_empty() && ops.is_empty() {
        (None, next)
    } else {
        (Some(Recovered { inflight: inflight.filter(|(_, n)| *n <= ops.len()), ops, files }), next)
    }
}

impl SealedDrive {
    pub fn open(name: &str, keys: DriveKeys, client: SealedClient, cache: Arc<BlockCache>, stats: Arc<Stats>, journal_dir: PathBuf) -> std::io::Result<Arc<SealedDrive>> {
        std::fs::create_dir_all(&journal_dir)?;
        let (recovered, next_journal) = load_journal(&journal_dir);
        Ok(Arc::new(SealedDrive {
            name: name.to_string(),
            tree: Mutex::new(Tree::new(&keys)),
            keys,
            client,
            cache,
            stats,
            journal_dir,
            dedup: OnceLock::new(),
            inflight: Mutex::new(HashMap::new()),
            readahead: Mutex::new(HashMap::new()),
            fetch_sem: Arc::new(Semaphore::new(FETCH_PARALLELISM)),
            uploading: Mutex::new(None),
            last_error: Mutex::new(None),
            notices: Mutex::new(Vec::new()),
            last_activity_ms: AtomicU64::new(0),
            journal_seq: AtomicU64::new(next_journal),
            recovered: Mutex::new(recovered),
            stopped: AtomicBool::new(false),
            layouts: Mutex::new(HashMap::new()),
            layout_loads: Mutex::new(HashMap::new()),
            sync_lock: tokio::sync::Mutex::new(()),
            sync_gen: AtomicU64::new(0),
        }))
    }

    pub fn note_activity(&self) {
        self.last_activity_ms.store(now_ms(), Ordering::Relaxed);
    }

    pub fn seq(&self) -> u64 {
        self.tree.lock().unwrap().seq
    }

    fn fail(&self, what: String) {
        *self.last_error.lock().unwrap() = Some(what);
    }

    /// Something happened to a file or folder that its owner did not ask for.
    fn notice(&self, what: String) {
        let mut notices = self.notices.lock().unwrap();
        if notices.len() < 200 {
            notices.push(what);
        }
    }

    /// What has happened to files and folders here that nobody asked for,
    /// since this was last called: kept under another name, recovered, or
    /// local changes dropped because the cloud's state won.
    pub fn take_notices(&self) -> Vec<String> {
        std::mem::take(&mut self.notices.lock().unwrap())
    }

    // ------------------------------------------------------------ journal

    fn persist_meta(&self, t: &Tree, id: u64) {
        let Some(node) = t.nodes.get(&id) else { return };
        let Kind::File(f) = &node.kind else { return };
        let Some(l) = &f.local else { return };
        let path = l.path.with_extension("meta");
        let Some(cloud) = &node.cloud else {
            let _ = std::fs::remove_file(path);
            return;
        };
        let meta = Meta {
            node: hex(&cloud.id.0),
            size: f.size,
            mtime_ms: node.mtime_ms,
            extents: l.extents.ranges(),
            base: l.base.as_ref().map(|r| (r.version, r.size, r.manifest.to_string())),
            base_len: l.base_len,
        };
        write_atomic(&path, &serde_json::to_vec(&meta).expect("meta serializes"));
    }

    fn log_append(&self, text: &str) {
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(self.journal_dir.join("ops.log")) {
            let _ = f.write_all(text.as_bytes());
        }
    }

    /// Queue a namespace op and save it.
    fn push_op(&self, t: &mut Tree, op: Op) {
        self.log_append(&format!("+{}\n", serde_json::to_string(&op).expect("op serializes")));
        t.ops.push_back(op);
    }

    /// Write the op log anew from the queue as it stands.
    fn rewrite_ops(&self, t: &Tree) {
        let path = self.journal_dir.join("ops.log");
        if t.ops.is_empty() {
            let _ = std::fs::remove_file(path);
            return;
        }
        let mut text: String = t.ops.iter().map(|op| format!("+{}\n", serde_json::to_string(op).expect("op serializes"))).collect();
        if let Some((id, n)) = &t.inflight {
            text.push_str(&format!("!{id} {n}\n"));
        }
        write_atomic(&path, text.as_bytes());
    }

    /// The op that creates local node `id` in the cloud, from the tree as it is.
    fn create_op(&self, t: &Tree, id: u64) -> Option<Op> {
        let node = t.nodes.get(&id)?;
        let (cloud, folder) = (node.cloud.as_ref()?, t.cloud(node.parent)?);
        let mut rng = OsRandom;
        Some(Op::Create {
            id: hex(&cloud.id.0),
            parent: hex(&folder.id.0),
            kind: if matches!(node.kind, Kind::Dir(_)) { ludy_proto::tree::Kind::Folder } else { ludy_proto::tree::Kind::File },
            name_hash: hex(&folder.key.name_hash(&node.name).ok()?),
            name: hex(&cloud.key.seal_name(&cloud.id, &node.name, &mut rng).ok()?),
            key: hex(&folder.key.wrap_child(&cloud.id, &cloud.key, &mut rng)),
        })
    }

    fn move_op(&self, t: &Tree, id: u64) -> Option<Op> {
        let node = t.nodes.get(&id)?;
        let (cloud, folder) = (node.cloud.as_ref()?, t.cloud(node.parent)?);
        let mut rng = OsRandom;
        Some(Op::Move {
            node: hex(&cloud.id.0),
            parent: hex(&folder.id.0),
            name_hash: hex(&folder.key.name_hash(&node.name).ok()?),
            name: hex(&cloud.key.seal_name(&cloud.id, &node.name, &mut rng).ok()?),
            key: hex(&folder.key.wrap_child(&cloud.id, &cloud.key, &mut rng)),
        })
    }

    /// Give a node an identity in the cloud and queue its creation; for a
    /// folder, the same for everything in it.
    fn adopt(&self, t: &mut Tree, id: u64) {
        let mut stack = vec![id];
        while let Some(at) = stack.pop() {
            let Some(node) = t.nodes.get(&at) else { continue };
            if is_local_only(&node.name) || t.cloud(node.parent).is_none() {
                continue;
            }
            let mut rng = OsRandom;
            let cloud = Cloud { id: NodeId::random(&mut rng), key: NodeKey::random(&mut rng), committed: false };
            t.by_cloud.insert(cloud.id, at);
            let node = t.nodes.get_mut(&at).expect("checked");
            node.cloud = Some(cloud);
            match &mut node.kind {
                Kind::Dir(children) => stack.extend(children.values().copied()),
                Kind::File(f) => {
                    if let Some(l) = &mut f.local {
                        l.gen += 1;
                        l.last_write = Instant::now();
                    }
                }
            }
            if let Some(op) = self.create_op(t, at) {
                self.push_op(t, op);
            }
            self.persist_meta(t, at);
        }
    }

    /// Take a node's cloud identity away (it stays on this Mac) and queue
    /// its deletion in the cloud.
    fn disown(&self, t: &mut Tree, id: u64) {
        let Some(cloud) = t.nodes.get(&id).and_then(|n| n.cloud.clone()) else { return };
        self.push_op(t, Op::Delete { node: hex(&cloud.id.0) });
        let mut stack = vec![id];
        while let Some(at) = stack.pop() {
            let Some(node) = t.nodes.get_mut(&at) else { continue };
            if let Some(c) = node.cloud.take() {
                t.by_cloud.remove(&c.id);
            }
            if let Some(Kind::Dir(children)) = t.nodes.get(&at).map(|n| &n.kind) {
                stack.extend(children.values().copied());
            }
            self.persist_meta(t, at);
        }
    }

    // ------------------------------------------------------------ the feed

    pub fn sync_token(&self) -> u64 {
        self.sync_gen.load(Ordering::Relaxed)
    }

    pub fn has_unsynced(&self) -> bool {
        self.tree.lock().unwrap().has_unsynced()
    }

    /// The drive's dedup key, fetched and unwrapped once.
    async fn dedup(&self) -> anyhow::Result<&(DedupKey, CutKey)> {
        if let Some(d) = self.dedup.get() {
            return Ok(d);
        }
        let head = self.client.head(&self.keys.drive).await?.accepted("open drive")?;
        let (root, key) = (&self.keys.root, &self.keys.root_key);
        // The root key wrapped by itself: does the key we hold open this drive?
        if head.root != hex(&root.0) || key.unwrap_child(root, &unhex(&head.key)).ok().as_ref() != Some(key) {
            anyhow::bail!("the key for drive {} does not open it", self.name);
        }
        let dedup = key.unwrap_dedup(root, &unhex(&head.dedup)).map_err(|e| anyhow::anyhow!("the drive's dedup key: {e}"))?;
        let cut = CutKey::for_drive(&dedup);
        Ok(self.dedup.get_or_init(|| (dedup, cut)))
    }

    /// Ask the cloud what changed and fold it into the tree. The first time,
    /// and whenever the cloud cannot say what changed, that is everything.
    /// Returns false, having changed nothing, while local changes are still
    /// waiting to be sent.
    pub async fn refresh(&self) -> anyhow::Result<bool> {
        self.dedup().await?;
        let token = self.sync_token();
        let (mut since, mut epoch) = {
            let t = self.tree.lock().unwrap();
            if t.resync || self.recovered.lock().unwrap().is_some() {
                (0, None)
            } else {
                (t.seq, t.epoch)
            }
        };
        let full = since == 0;
        let mut nodes: Vec<NodeInfo> = Vec::new();
        let (seq, epoch) = loop {
            let page = self.client.feed(&self.keys.drive, since, epoch, FEED_PAGE).await?.accepted("feed")?;
            if page.reset {
                if since == 0 && epoch.is_none() {
                    anyhow::bail!("the cloud cannot list drive {}", self.name);
                }
                // Another drive now has this id, or we are ahead of it: start over.
                self.tree.lock().unwrap().resync = true;
                (since, epoch) = (0, None);
                nodes.clear();
                continue;
            }
            since = page.nodes.last().map(|n| n.seq).unwrap_or(since);
            epoch = Some(page.epoch);
            nodes.extend(page.nodes);
            if !page.more {
                break (page.seq, page.epoch);
            }
        };
        let full = full || self.tree.lock().unwrap().resync;
        Ok(self.apply_feed(&nodes, seq, epoch, full, token))
    }

    fn apply_feed(&self, nodes: &[NodeInfo], seq: u64, epoch: u64, full: bool, token: u64) -> bool {
        let mut t = self.tree.lock().unwrap();
        if t.has_unsynced() || token != self.sync_gen.load(Ordering::Relaxed) {
            return false;
        }
        if full {
            t.listings += 1;
        }
        let listing = t.listings;
        // A child can come before the folder whose key opens it: hold it
        // back until the folder has been seen.
        let mut waiting: Vec<&NodeInfo> = nodes.iter().filter(|n| !self.apply_node(&mut t, n, listing)).collect();
        while !waiting.is_empty() {
            let before = waiting.len();
            waiting.retain(|n| !self.apply_node(&mut t, n, listing));
            if waiting.len() == before {
                break;
            }
        }
        if full {
            // What a full read did not name, the cloud no longer has.
            let gone: Vec<u64> = t.nodes.iter().filter(|(id, n)| **id != ROOT_ID && n.listed != listing && n.cloud.as_ref().is_some_and(|c| c.committed)).map(|(id, _)| *id).collect();
            for id in gone {
                drop_local(t.detach(id));
            }
        }
        t.seq = seq;
        t.epoch = Some(epoch);
        t.resync = false;
        if let Some(rec) = self.recovered.lock().unwrap().take() {
            self.recover(&mut t, rec);
        }
        true
    }

    /// Make the tree show one node of the feed. Returns false if its folder
    /// is not in the tree (yet).
    fn apply_node(&self, t: &mut Tree, n: &NodeInfo, listing: u64) -> bool {
        let Some(id) = node_id(&n.id) else { return true };
        if id == self.keys.root {
            return true;
        }
        let existing = t.by_cloud.get(&id).copied();
        if n.deleted {
            if let Some(local) = existing {
                drop_local(t.detach(local));
                self.readahead.lock().unwrap().remove(&local);
            }
            return true;
        }
        let Some(parent) = n.parent.as_deref().and_then(node_id).and_then(|p| t.by_cloud.get(&p).copied()) else {
            return false;
        };
        let Some(folder_key) = t.cloud(parent).map(|c| c.key.clone()) else { return true };
        let opened = folder_key.unwrap_child(&id, &unhex(&n.key)).and_then(|key| Ok((key.open_name(&id, &unhex(&n.name))?, key)));
        let (name, key) = match opened {
            Ok(opened) => opened,
            Err(e) => {
                self.notice(format!("something in {} does not open with its folder's key and is not shown: {e}", self.name));
                return true;
            }
        };
        let remote = n.version.as_ref().map(|v| {
            let mtime = key.open_meta(&id, &unhex(&v.meta)).map(|m| m.mtime_ms).ok();
            (Remote { version: v.id, size: v.size, manifest: v.manifest.as_str().into() }, mtime)
        });
        let local = match existing {
            Some(local) => {
                t.relink(local, parent, &name);
                local
            }
            None => {
                // Something of this Mac's stands under that name: the cloud's node takes it.
                if let Some(other) = t.dir(parent).ok().and_then(|c| c.get(&name).copied()) {
                    drop_local(t.detach(other));
                }
                let kind = match n.kind {
                    ludy_proto::tree::Kind::Folder => Kind::Dir(BTreeMap::new()),
                    ludy_proto::tree::Kind::File => Kind::File(FileNode { size: 0, remote: None, local: None, uploading: false }),
                };
                t.insert(parent, &name, now_ms(), kind, Some(Cloud { id, key: key.clone(), committed: true }))
            }
        };
        let Some(node) = t.nodes.get_mut(&local) else { return true };
        node.listed = listing;
        if let Some(c) = &mut node.cloud {
            c.committed = true;
        }
        if let (Kind::File(f), Some((remote, mtime))) = (&mut node.kind, remote) {
            if f.remote.as_ref() != Some(&remote) {
                f.size = remote.size;
                f.remote = Some(remote);
                node.mtime_ms = mtime.unwrap_or(node.mtime_ms);
                self.readahead.lock().unwrap().remove(&local);
            }
        }
        true
    }

    /// Put back what an earlier run had not sent: the effect of its queued
    /// ops on the fresh tree, and its journal files. The cloud may already
    /// have the commit that was on its way, so every step tolerates finding
    /// its work done.
    fn recover(&self, t: &mut Tree, rec: Recovered) {
        for op in rec.ops {
            match &op {
                Op::Create { id, parent, kind, name, key, .. } => {
                    let ids = node_id(id).zip(node_id(parent).and_then(|p| t.by_cloud.get(&p).copied()));
                    if let Some((id, parent)) = ids.filter(|(id, _)| !t.by_cloud.contains_key(id)) {
                        let opened = t.cloud(parent).and_then(|folder| {
                            let node_key = folder.key.unwrap_child(&id, &unhex(key)).ok()?;
                            Some((node_key.open_name(&id, &unhex(name)).ok()?, node_key))
                        });
                        if let Some((name, node_key)) = opened {
                            let kind = match kind {
                                ludy_proto::tree::Kind::Folder => Kind::Dir(BTreeMap::new()),
                                ludy_proto::tree::Kind::File => Kind::File(FileNode { size: 0, remote: None, local: None, uploading: false }),
                            };
                            if let Some(other) = t.dir(parent).ok().and_then(|c| c.get(&name).copied()) {
                                drop_local(t.detach(other));
                            }
                            t.insert(parent, &name, now_ms(), kind, Some(Cloud { id, key: node_key, committed: false }));
                        }
                    }
                }
                Op::Move { node, parent, name, key, .. } => {
                    let ids = node_id(node).and_then(|n| Some((n, *t.by_cloud.get(&n)?, *t.by_cloud.get(&node_id(parent)?)?)));
                    if let Some((id, local, parent)) = ids {
                        let name = t.cloud(parent).and_then(|folder| folder.key.unwrap_child(&id, &unhex(key)).ok()?.open_name(&id, &unhex(name)).ok());
                        if let Some(name) = name {
                            t.relink(local, parent, &name);
                        }
                    }
                }
                Op::Delete { node } => {
                    if let Some(local) = node_id(node).and_then(|n| t.by_cloud.get(&n).copied()).filter(|l| *l != ROOT_ID) {
                        drop_local(t.detach(local));
                    }
                }
                Op::Put { .. } => continue,
            }
            t.ops.push_back(op);
        }
        t.inflight = rec.inflight;
        for (path, meta) in rec.files {
            let Ok(file) = std::fs::OpenOptions::new().read(true).write(true).open(&path) else {
                remove_journal(&path);
                continue;
            };
            // The cloud deleted the file in the meantime, or it never got there: the cloud wins.
            let Some(local) = node_id(&meta.node).and_then(|n| t.by_cloud.get(&n).copied()) else {
                remove_journal(&path);
                continue;
            };
            let Some(node) = t.nodes.get_mut(&local) else { continue };
            let Kind::File(f) = &mut node.kind else {
                remove_journal(&path);
                continue;
            };
            let base = meta.base.map(|(version, size, manifest)| Remote { version, size, manifest: manifest.as_str().into() });
            f.size = meta.size;
            f.local = Some(Local { path, file: Arc::new(file), extents: RangeSet::from_ranges(&meta.extents), base, base_len: meta.base_len, gen: 0, last_write: Instant::now() });
            node.mtime_ms = meta.mtime_ms;
        }
        self.rewrite_ops(t);
    }

    // ------------------------------------------------------------ metadata

    fn attr_of(&self, t: &Tree, id: u64) -> FsResult<Attr> {
        let node = t.nodes.get(&id).ok_or(FsError::NotFound)?;
        Ok(match &node.kind {
            Kind::Dir(_) => Attr { id, is_dir: true, size: 0, mtime_ms: node.mtime_ms, local_bytes: 0 },
            Kind::File(f) => {
                let local_bytes = match (&f.local, &f.remote) {
                    (Some(l), _) => l.extents.total(),
                    (None, Some(r)) => self.cached_bytes(r),
                    (None, None) => 0,
                };
                Attr { id, is_dir: false, size: f.size, mtime_ms: node.mtime_ms, local_bytes }
            }
        })
    }

    /// Bytes of a version in the cache, as far as its manifest is known here.
    fn cached_bytes(&self, remote: &Remote) -> u64 {
        let Some(layout) = self.layouts.lock().unwrap().get(&remote.manifest).cloned() else { return 0 };
        layout.segs.iter().map(|s| if let Src::Shard { id, .. } = &s.src { self.cache.version_bytes(id) } else { 0 }).sum()
    }

    pub fn getattr(&self, id: u64) -> FsResult<Attr> {
        self.attr_of(&self.tree.lock().unwrap(), id)
    }

    pub fn lookup(&self, dir: u64, name: &str) -> FsResult<u64> {
        let t = self.tree.lock().unwrap();
        match name {
            "." => Ok(dir),
            ".." => Ok(t.nodes.get(&dir).ok_or(FsError::NotFound)?.parent),
            _ => {
                let name = crypt::normalize_name(name).map_err(|_| FsError::NotFound)?;
                t.dir(dir)?.get(&name).copied().ok_or(FsError::NotFound)
            }
        }
    }

    pub fn readdir(&self, dir: u64) -> FsResult<Vec<(String, Attr)>> {
        let t = self.tree.lock().unwrap();
        let mut out = Vec::new();
        for (name, id) in t.dir(dir)? {
            if let Ok(attr) = self.attr_of(&t, *id) {
                out.push((name.clone(), attr));
            }
        }
        Ok(out)
    }

    /// One page of a folder: up to `max` entries after the entry with id
    /// `after` (0: from the start), and whether that was the last page.
    pub fn readdir_page(&self, dir: u64, after: u64, max: usize) -> FsResult<(Vec<(String, Attr)>, bool)> {
        use std::ops::Bound;
        let t = self.tree.lock().unwrap();
        let children = t.dir(dir)?;
        let from = match t.nodes.get(&after) {
            Some(n) if after != 0 && n.parent == dir && children.get(&n.name) == Some(&after) => Bound::Excluded(n.name.as_str()),
            _ => Bound::Unbounded,
        };
        let mut rest = children.range::<str, _>((from, Bound::Unbounded));
        let mut out = Vec::with_capacity(max.min(children.len()));
        for (name, id) in rest.by_ref().take(max) {
            if let Ok(attr) = self.attr_of(&t, *id) {
                out.push((name.clone(), attr));
            }
        }
        let end = rest.next().is_none();
        Ok((out, end))
    }

    /// The path of a node from the drive's root.
    pub fn path_of(&self, id: u64) -> String {
        self.tree.lock().unwrap().path_of(id)
    }

    pub fn find_path(&self, path: &str) -> Option<u64> {
        self.tree.lock().unwrap().find_path(path)
    }

    /// How many files the drive holds and how many bytes they add up to, as
    /// the tree here has them.
    pub fn totals(&self) -> (u64, u64) {
        let t = self.tree.lock().unwrap();
        t.nodes.values().fold((0, 0), |(files, bytes), n| match &n.kind {
            Kind::File(f) => (files + 1, bytes + f.size),
            Kind::Dir(_) => (files, bytes),
        })
    }

    /// A file's history in the cloud, newest first.
    pub async fn versions(&self, id: u64) -> anyhow::Result<Vec<FileVersion>> {
        let file = self.committed_file(id)?;
        let entries = self.client.versions(&self.keys.drive, &hex(&file.id.0)).await?.accepted("versions")?;
        Ok(entries
            .into_iter()
            .map(|e| FileVersion { id: e.id, size: e.size, committed_ms: e.at_ms, mtime_ms: file.key.open_meta(&file.id, &unhex(&e.meta)).map(|m| m.mtime_ms).unwrap_or(e.at_ms) })
            .collect())
    }

    /// Make an earlier version of a file its newest one. No content moves:
    /// the new version names the old one's manifest and shards.
    pub async fn restore(&self, id: u64, version: u64) -> anyhow::Result<()> {
        let file = self.committed_file(id)?;
        let node = hex(&file.id.0);
        let entries = self.client.versions(&self.keys.drive, &node).await?.accepted("versions")?;
        let old = entries.into_iter().find(|e| e.id == version).ok_or_else(|| anyhow::anyhow!("the cloud no longer keeps that version"))?;
        let sealed = self.client.get_manifest(&self.keys.drive, &old.manifest).await?.ok_or_else(|| anyhow::anyhow!("the cloud no longer has that version's manifest"))?;
        let manifest = file.key.open_manifest(&file.id, &sealed).map_err(|e| anyhow::anyhow!("that version's manifest does not open: {e}"))?;
        let mut rng = OsRandom;
        let meta = hex(&file.key.seal_meta(&file.id, &FileMeta { mtime_ms: now_ms() }, &mut rng));
        let put = Op::Put { node, base: None, size: old.size, manifest: old.manifest, shards: manifest.shard_ids(), meta };
        self.client.commit(&self.keys.drive, &Commit { id: hex(&NodeId::random(&mut rng).0), ops: vec![put] }).await?.accepted("restore")?;
        self.sync_gen.fetch_add(1, Ordering::SeqCst);
        let _ = self.refresh().await;
        Ok(())
    }

    /// Add a file whose content is generated, not stored: `size` bytes of
    /// the synthetic stream for `seed`. It takes no storage and no upload,
    /// and reads like any other file. For demos and tests of very large files.
    pub async fn add_synthetic(&self, dir: u64, name: &str, size: u64, seed: u64) -> anyhow::Result<()> {
        let name = Self::name(name).map_err(|e| anyhow::anyhow!("{name:?} cannot be a file's name ({e:?})"))?;
        let folder = {
            let t = self.tree.lock().unwrap();
            if t.dir(dir).map_err(|e| anyhow::anyhow!("not a folder ({e:?})"))?.contains_key(&name) {
                anyhow::bail!("{name} is already there");
            }
            t.cloud(dir).filter(|c| c.committed).cloned().ok_or_else(|| anyhow::anyhow!("the folder has not reached the cloud yet"))?
        };
        let mut rng = OsRandom;
        let (id, key) = (NodeId::random(&mut rng), NodeKey::random(&mut rng));
        let manifest = PrivateManifest { size, segments: if size == 0 { Vec::new() } else { vec![PrivateSegment::Synth { seed, off: 0, n: size }] } };
        manifest.validate().map_err(|e| anyhow::anyhow!("the manifest is wrong: {e}"))?;
        let sealed = key.seal_manifest(&id, &manifest, &mut rng);
        let manifest_name = sha256_hex(&sealed);
        self.client.put_manifest(&self.keys.drive, &manifest_name, Bytes::from(sealed)).await?;
        let node = hex(&id.0);
        let ops = vec![
            Op::Create {
                id: node.clone(),
                parent: hex(&folder.id.0),
                kind: ludy_proto::tree::Kind::File,
                name_hash: hex(&folder.key.name_hash(&name).map_err(|e| anyhow::anyhow!("name: {e}"))?),
                name: hex(&key.seal_name(&id, &name, &mut rng).map_err(|e| anyhow::anyhow!("name: {e}"))?),
                key: hex(&folder.key.wrap_child(&id, &key, &mut rng)),
            },
            Op::Put { node, base: Some(0), size, manifest: manifest_name, shards: Vec::new(), meta: hex(&key.seal_meta(&id, &FileMeta { mtime_ms: now_ms() }, &mut rng)) },
        ];
        self.client.commit(&self.keys.drive, &Commit { id: hex(&NodeId::random(&mut rng).0), ops }).await?.accepted("add a synthetic file")?;
        self.sync_gen.fetch_add(1, Ordering::SeqCst);
        let _ = self.refresh().await;
        Ok(())
    }

    /// The cloud's side of a file that has reached it and has nothing
    /// waiting to be uploaded.
    fn committed_file(&self, id: u64) -> anyhow::Result<FileRef> {
        let t = self.tree.lock().unwrap();
        let f = t.file(id).map_err(|_| anyhow::anyhow!("not a file"))?;
        if f.local.is_some() || f.uploading {
            anyhow::bail!("the file is still uploading");
        }
        let c = t.cloud(id).filter(|c| c.committed).ok_or_else(|| anyhow::anyhow!("the file has not reached the cloud yet"))?;
        Ok(FileRef { id: c.id, key: c.key.clone() })
    }

    /// The cloud version currently backing a file, if it has one.
    pub fn version_of(&self, id: u64) -> Option<u64> {
        self.tree.lock().unwrap().file(id).ok()?.remote.as_ref().map(|r| r.version)
    }

    // ------------------------------------------------------------ reading

    /// Read up to `len` bytes at `offset`. Returns the bytes and whether the
    /// read reached the end of the file.
    pub async fn read(self: &Arc<Self>, id: u64, offset: u64, len: u32) -> FsResult<(Vec<u8>, bool)> {
        let (snap, size) = {
            let t = self.tree.lock().unwrap();
            let f = t.file(id)?;
            let file = t.cloud(id).map(|c| FileRef { id: c.id, key: c.key.clone() });
            let snap = match (&f.local, &f.remote, file) {
                (Some(l), _, file) => ReadSnap::Local { file, journal: l.file.clone(), extents: l.extents.clone(), base: l.base.clone(), base_len: l.base_len },
                (None, Some(r), Some(file)) => ReadSnap::Remote { file, remote: r.clone() },
                _ => return Ok((Vec::new(), true)),
            };
            (snap, f.size)
        };
        if offset >= size || len == 0 {
            return Ok((Vec::new(), offset >= size));
        }
        let len = (len as u64).min(size - offset);
        if let ReadSnap::Remote { file, remote } = &snap {
            self.plan_readahead(id, file, remote, offset, len);
        }
        let data = self.read_snap(&snap, offset, len).await?;
        Stats::add(&self.stats.bytes_read, data.len() as u64);
        Ok((data, offset + len >= size))
    }

    async fn read_snap(self: &Arc<Self>, snap: &ReadSnap, offset: u64, len: u64) -> FsResult<Vec<u8>> {
        match snap {
            ReadSnap::Remote { file, remote } => self.read_remote(file, remote, offset, len).await,
            ReadSnap::Local { file, journal, extents, base, base_len } => {
                let mut out = vec![0u8; len as usize];
                for (start, end, is_local) in extents.segments(offset, offset + len) {
                    let dst = &mut out[(start - offset) as usize..(end - offset) as usize];
                    if is_local {
                        journal.read_exact_at(dst, start).map_err(|e| FsError::Io(e.to_string()))?;
                    } else if let (Some(remote), Some(file)) = (base, file) {
                        // Not written locally: from the version underneath,
                        // or a hole (zeros) past its end.
                        let upto = end.min(*base_len).min(remote.size);
                        if start < upto {
                            let data = self.read_remote(file, remote, start, upto - start).await?;
                            dst[..data.len()].copy_from_slice(&data);
                        }
                    }
                }
                Ok(out)
            }
        }
    }

    /// Where a version's bytes are: fetch its sealed manifest, check it is
    /// the one that was named, and open it with the file's key.
    async fn layout(&self, file: &FileRef, remote: &Remote) -> anyhow::Result<Option<Arc<Layout>>> {
        if let Some(known) = self.layouts.lock().unwrap().get(&remote.manifest) {
            return Ok(Some(known.clone()));
        }
        // One fetch per manifest at a time: whoever comes second waits and
        // then finds it opened.
        let gate = self.layout_loads.lock().unwrap().entry(remote.manifest.clone()).or_default().clone();
        let _one = gate.lock().await;
        if let Some(known) = self.layouts.lock().unwrap().get(&remote.manifest) {
            return Ok(Some(known.clone()));
        }
        let fetched = self.client.get_manifest(&self.keys.drive, &remote.manifest).await;
        self.layout_loads.lock().unwrap().remove(&remote.manifest);
        let Some(sealed) = fetched? else {
            return Ok(None);
        };
        if sha256_hex(&sealed) != *remote.manifest {
            anyhow::bail!("the cloud sent another manifest than the one asked for");
        }
        let manifest = file.key.open_manifest(&file.id, &sealed).map_err(|e| anyhow::anyhow!("a manifest of {} does not open: {e}", self.name))?;
        Ok(Some(self.remember_layout(&remote.manifest, &manifest)))
    }

    fn remember_layout(&self, name: &Arc<str>, manifest: &PrivateManifest) -> Arc<Layout> {
        let mut pos = 0;
        let mut segs = Vec::with_capacity(manifest.segments.len());
        for s in &manifest.segments {
            let src = match s {
                PrivateSegment::Shard { id, key, .. } => Src::Shard { id: hex(id).into(), key: key.clone() },
                PrivateSegment::Synth { seed, off, .. } => Src::Synth { seed: *seed, off: *off },
            };
            segs.push(Seg { start: pos, n: s.len(), src });
            pos += s.len();
        }
        let layout = Arc::new(Layout { segs });
        let mut known = self.layouts.lock().unwrap();
        if known.len() >= LAYOUTS_REMEMBERED {
            known.clear();
        }
        known.insert(name.clone(), layout.clone());
        layout
    }

    async fn read_remote(self: &Arc<Self>, file: &FileRef, remote: &Remote, offset: u64, len: u64) -> FsResult<Vec<u8>> {
        let len = len.min(remote.size.saturating_sub(offset));
        if len == 0 {
            return Ok(Vec::new());
        }
        let layout = match self.layout(file, remote).await {
            Ok(Some(layout)) => layout,
            Ok(None) => return Err(FsError::Io("the cloud no longer has this version".into())),
            Err(e) => return Err(FsError::Io(format!("{e:#}"))),
        };
        let end = offset + len;
        let mut out = Vec::with_capacity(len as usize);
        let first = layout.segs.partition_point(|s| s.start + s.n <= offset);
        for seg in &layout.segs[first..] {
            if seg.start >= end {
                break;
            }
            let (from, to) = (offset.max(seg.start) - seg.start, end.min(seg.start + seg.n) - seg.start);
            match &seg.src {
                Src::Synth { seed, off } => out.extend_from_slice(&synth::bytes(*seed, off + from, (to - from) as usize)),
                Src::Shard { id, key } => out.extend_from_slice(&self.read_shard(id, key, seg.n, from, to - from).await?),
            }
        }
        if out.len() as u64 != len {
            return Err(FsError::Io("the manifest is shorter than the file".into()));
        }
        Ok(out)
    }

    /// Bytes `offset..offset + len` of a shard of `n` plaintext bytes.
    async fn read_shard(self: &Arc<Self>, id: &Arc<str>, key: &ShardKey, n: u64, offset: u64, len: u64) -> FsResult<Vec<u8>> {
        let end = offset + len;
        let (b0, b1) = (offset / BLOCK_SIZE, (end - 1) / BLOCK_SIZE);
        let mut out = vec![0u8; len as usize];
        for piece in self.plan(id, key, n, b0, b1) {
            let (idx, block) = match piece {
                Piece::Cached(idx) => match self.cache.get(id, idx).filter(|data| {
                    // A block cut short on disk is damage, not data.
                    let whole = data.len() as u64 == BLOCK_SIZE.min(n - idx * BLOCK_SIZE);
                    if !whole {
                        self.cache.discard(id, idx);
                    }
                    whole
                }) {
                    Some(data) => {
                        let overlap = (end.min((idx + 1) * BLOCK_SIZE) - offset.max(idx * BLOCK_SIZE)).min(data.len() as u64);
                        Stats::add(&self.stats.cache_hit_bytes, overlap);
                        (idx, data)
                    }
                    // Evicted between planning and reading: fetch it after all.
                    None => {
                        let again = self.plan(id, key, n, idx, idx).pop();
                        match again {
                            Some(Piece::Pending(idx, run)) => (idx, run.block(idx).await.map_err(|e| FsError::Io(e.to_string()))?),
                            Some(Piece::Cached(idx)) => (idx, self.cache.get(id, idx).ok_or_else(|| FsError::Io("the cache lost a block twice".into()))?),
                            None => return Err(FsError::Io("no way to fetch a block".into())),
                        }
                    }
                },
                Piece::Pending(idx, run) => (idx, run.block(idx).await.map_err(|e| FsError::Io(e.to_string()))?),
            };
            let block_start = idx * BLOCK_SIZE;
            let (from, to) = (offset.max(block_start), end.min(block_start + block.len() as u64));
            if from < to {
                out[(from - offset) as usize..(to - offset) as usize].copy_from_slice(&block[(from - block_start) as usize..(to - block_start) as usize]);
            }
        }
        Ok(out)
    }

    /// Work out how to get blocks `b0..=b1` of a shard: from the cache, from
    /// a fetch already running, or by starting range requests.
    fn plan(self: &Arc<Self>, id: &Arc<str>, key: &ShardKey, n: u64, b0: u64, b1: u64) -> Vec<Piece> {
        let mut pieces = Vec::with_capacity((b1 - b0 + 1) as usize);
        let mut inflight = self.inflight.lock().unwrap();
        let mut run: Vec<u64> = Vec::new();
        let mut started: Vec<(Arc<Run>, u64)> = Vec::new();
        let mut flush = |run: &mut Vec<u64>, pieces: &mut Vec<Piece>, inflight: &mut HashMap<(Arc<str>, u64), Arc<Run>>| {
            if run.is_empty() {
                return;
            }
            let fetch = Arc::new(Run { first: run[0], blocks: Mutex::new(Vec::with_capacity(run.len())), progress: tokio::sync::watch::channel(Progress::default()).0 });
            started.push((fetch.clone(), run.len() as u64));
            for idx in run.drain(..) {
                inflight.insert((id.clone(), idx), fetch.clone());
                pieces.push(Piece::Pending(idx, fetch.clone()));
            }
        };
        for idx in b0..=b1 {
            if self.cache.contains(id, idx) {
                flush(&mut run, &mut pieces, &mut inflight);
                pieces.push(Piece::Cached(idx));
            } else if let Some(fetch) = inflight.get(&(id.clone(), idx)) {
                let fetch = fetch.clone();
                flush(&mut run, &mut pieces, &mut inflight);
                pieces.push(Piece::Pending(idx, fetch));
            } else {
                run.push(idx);
                if run.len() as u64 == MAX_RUN_BLOCKS {
                    flush(&mut run, &mut pieces, &mut inflight);
                }
            }
        }
        flush(&mut run, &mut pieces, &mut inflight);
        drop(inflight);
        for (fetch, count) in started {
            let mut end = RunEnd { fs: self.clone(), shard: id.clone(), run: fetch, count, error: None };
            let (id, key) = (id.clone(), key.clone());
            tokio::spawn(async move {
                let (this, run) = (end.fs.clone(), end.run.clone());
                end.error = this.fetch_run(&run, &id, &key, n, count).await.err();
                drop(end);
            });
        }
        pieces.sort_by_key(|p| match p {
            Piece::Cached(i) | Piece::Pending(i, _) => *i,
        });
        pieces
    }

    /// Receive the sealed frames of a run of blocks, open them block by
    /// block, and pass each on as soon as it is in the cache.
    async fn fetch_run(&self, run: &Run, id: &Arc<str>, key: &ShardKey, n: u64, count: u64) -> Result<(), String> {
        let sealed_frame = (FRAME + TAG) as u64;
        // A block is `BLOCK_FRAMES` frames; the shard's last block may be short.
        let sealed_block = |idx: u64| crypt::sealed_len(BLOCK_SIZE.min(n - idx * BLOCK_SIZE));
        let start = run.first * BLOCK_FRAMES * sealed_frame;
        let len: u64 = (run.first..run.first + count).map(sealed_block).sum();
        let _permit = self.fetch_sem.acquire().await.map_err(|e| e.to_string())?;
        let mut body = self.client.open_shard_range(&self.keys.drive, id, start, len).await.map_err(|e| format!("{e:#}"))?;
        let mut arrived = 0u64;
        let mut block = BytesMut::new();
        while let Some(mut chunk) = body.chunk().await.map_err(|e| format!("{e:#}"))? {
            while !chunk.is_empty() {
                let idx = run.first + arrived;
                let want = sealed_block(idx) as usize;
                let take = (want - block.len()).min(chunk.len());
                block.extend_from_slice(&chunk.split_to(take));
                if block.len() < want {
                    continue;
                }
                let sealed = block.split().freeze();
                let (cache, shard, key) = (self.cache.clone(), id.clone(), key.clone());
                let opened = tokio::task::spawn_blocking(move || {
                    let plain = crypt::open_frames(&key, n, idx * BLOCK_FRAMES, &sealed).map_err(|e| format!("a shard does not open: {e}"))?;
                    let _ = cache.put(&shard, idx, &plain);
                    Ok::<Bytes, String>(Bytes::from(plain))
                })
                .await
                .map_err(|e| e.to_string())??;
                run.blocks.lock().unwrap().push(opened);
                arrived += 1;
                run.progress.send_modify(|p| p.arrived = arrived);
            }
        }
        Ok(())
    }

    /// Sequential readers get what lies ahead fetched before they ask,
    /// further the longer the streak lasts.
    fn plan_readahead(self: &Arc<Self>, id: u64, file: &FileRef, remote: &Remote, offset: u64, len: u64) {
        let end = offset + len;
        let range = {
            let mut ra = self.readahead.lock().unwrap();
            let s = ra.entry(id).or_default();
            let slack = 4 * BLOCK_SIZE;
            if offset + slack >= s.next && offset <= s.next + slack {
                // Only a read that gets further counts: several readers of
                // one place are not one reader moving on.
                if end > s.next {
                    s.streak += 1;
                }
            } else {
                s.streak = 0;
                s.ahead_until = 0;
            }
            s.next = if s.streak == 0 { end } else { s.next.max(end).min(remote.size) };
            if s.streak < 2 {
                return;
            }
            let window = (BLOCK_SIZE << s.streak.min(5)).min(MAX_READAHEAD);
            let from = end.max(s.ahead_until);
            let limit = (end + window).min(remote.size);
            // Top up a whole request at a time, so requests overlap instead
            // of going out in batches that all finish together.
            let step = (MAX_RUN_BLOCKS * BLOCK_SIZE).min(window / 2);
            let fits = limit.saturating_sub(from) / step * step;
            let to = if limit == remote.size { limit } else { from + fits };
            if from >= to {
                return;
            }
            s.ahead_until = to;
            (from, to)
        };
        let (this, file, remote) = (self.clone(), file.clone(), remote.clone());
        tokio::spawn(async move {
            // Planning starts the fetches; nobody waits for them here.
            let Ok(Some(layout)) = this.layout(&file, &remote).await else { return };
            let first = layout.segs.partition_point(|s| s.start + s.n <= range.0);
            for seg in &layout.segs[first..] {
                if seg.start >= range.1 {
                    break;
                }
                if let Src::Shard { id, key } = &seg.src {
                    let (from, to) = (range.0.max(seg.start) - seg.start, range.1.min(seg.start + seg.n) - seg.start);
                    let _ = this.plan(id, key, seg.n, from / BLOCK_SIZE, (to - 1) / BLOCK_SIZE);
                }
            }
        });
    }

    // ------------------------------------------------------------ writing

    fn new_journal_file(&self) -> FsResult<(PathBuf, Arc<File>)> {
        let n = self.journal_seq.fetch_add(1, Ordering::Relaxed);
        let path = self.journal_dir.join(format!("{n:08}.journal"));
        let file = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(true).open(&path).map_err(|e| FsError::Io(e.to_string()))?;
        Ok((path, Arc::new(file)))
    }

    fn ensure_local<'a>(&self, t: &'a mut Tree, id: u64) -> FsResult<&'a mut Local> {
        let f = t.file_mut(id)?;
        if f.local.is_none() {
            let (path, file) = self.new_journal_file()?;
            let base_len = f.remote.as_ref().map(|r| r.size.min(f.size)).unwrap_or(0);
            f.local = Some(Local { path, file, extents: RangeSet::default(), base: f.remote.clone(), base_len, gen: 0, last_write: Instant::now() });
        }
        Ok(f.local.as_mut().expect("just set"))
    }

    fn name(name: &str) -> FsResult<String> {
        crypt::normalize_name(name).map_err(|_| if name.len() > crypt::MAX_NAME { FsError::NameTooLong } else { FsError::Invalid })
    }

    pub fn create(&self, dir: u64, name: &str, exclusive: bool) -> FsResult<u64> {
        let name = Self::name(name)?;
        let mut t = self.tree.lock().unwrap();
        if let Some(existing) = t.dir(dir)?.get(&name).copied() {
            if exclusive {
                return Err(FsError::Exists);
            }
            t.file(existing)?;
            return Ok(existing);
        }
        let (path, file) = self.new_journal_file()?;
        let local = Local { path, file, extents: RangeSet::default(), base: None, base_len: 0, gen: 0, last_write: Instant::now() };
        let node = FileNode { size: 0, remote: None, local: Some(local), uploading: false };
        let id = t.insert(dir, &name, now_ms(), Kind::File(node), None);
        self.adopt(&mut t, id);
        Ok(id)
    }

    pub fn write(&self, id: u64, offset: u64, data: &[u8]) -> FsResult<Attr> {
        let mut t = self.tree.lock().unwrap();
        let local = self.ensure_local(&mut t, id)?;
        local.file.write_all_at(data, offset).map_err(|e| FsError::Io(e.to_string()))?;
        local.extents.insert(offset, offset + data.len() as u64);
        local.gen += 1;
        local.last_write = Instant::now();
        let f = t.file_mut(id)?;
        f.size = f.size.max(offset + data.len() as u64);
        if let Some(n) = t.nodes.get_mut(&id) {
            n.mtime_ms = now_ms();
        }
        Stats::add(&self.stats.bytes_written, data.len() as u64);
        self.persist_meta(&t, id);
        self.attr_of(&t, id)
    }

    pub fn set_size(&self, id: u64, size: u64) -> FsResult<()> {
        let mut t = self.tree.lock().unwrap();
        if t.file(id)?.size == size {
            return Ok(());
        }
        let local = self.ensure_local(&mut t, id)?;
        local.file.set_len(size).map_err(|e| FsError::Io(e.to_string()))?;
        local.extents.truncate(size);
        local.base_len = local.base_len.min(size);
        local.gen += 1;
        local.last_write = Instant::now();
        t.file_mut(id)?.size = size;
        if let Some(n) = t.nodes.get_mut(&id) {
            n.mtime_ms = now_ms();
        }
        self.persist_meta(&t, id);
        Ok(())
    }

    /// Set a file's modification time. For a file the cloud has, that is a
    /// new version with the same content.
    pub fn set_mtime(&self, id: u64, mtime_ms: u64) -> FsResult<()> {
        let mut t = self.tree.lock().unwrap();
        let node = t.nodes.get_mut(&id).ok_or(FsError::NotFound)?;
        if node.mtime_ms == mtime_ms {
            return Ok(());
        }
        node.mtime_ms = mtime_ms;
        if matches!(&node.kind, Kind::File(f) if f.remote.is_some() || f.local.is_some()) && node.cloud.is_some() {
            let local = self.ensure_local(&mut t, id)?;
            local.gen += 1;
            local.last_write = Instant::now();
        }
        self.persist_meta(&t, id);
        Ok(())
    }

    pub fn mkdir(&self, dir: u64, name: &str) -> FsResult<u64> {
        let name = Self::name(name)?;
        let mut t = self.tree.lock().unwrap();
        if t.dir(dir)?.contains_key(&name) {
            return Err(FsError::Exists);
        }
        let id = t.insert(dir, &name, now_ms(), Kind::Dir(BTreeMap::new()), None);
        self.adopt(&mut t, id);
        Ok(id)
    }

    /// Remove a file or an empty folder, as `unlink` and `rmdir` do.
    pub fn remove(&self, dir: u64, name: &str) -> FsResult<()> {
        self.remove_node(dir, name, false)
    }

    /// Remove a file, or a folder and everything in it.
    pub fn remove_tree(&self, dir: u64, name: &str) -> FsResult<()> {
        self.remove_node(dir, name, true)
    }

    fn remove_node(&self, dir: u64, name: &str, with_contents: bool) -> FsResult<()> {
        let name = Self::name(name).map_err(|_| FsError::NotFound)?;
        let mut t = self.tree.lock().unwrap();
        let id = *t.dir(dir)?.get(&name).ok_or(FsError::NotFound)?;
        if !with_contents && matches!(&t.nodes.get(&id).ok_or(FsError::NotFound)?.kind, Kind::Dir(children) if !children.is_empty()) {
            return Err(FsError::NotEmpty);
        }
        if let Some(cloud) = t.cloud(id).cloned() {
            self.push_op(&mut t, Op::Delete { node: hex(&cloud.id.0) });
        }
        drop_local(t.detach(id));
        self.readahead.lock().unwrap().remove(&id);
        Ok(())
    }

    pub fn rename(&self, from_dir: u64, from_name: &str, to_dir: u64, to_name: &str) -> FsResult<()> {
        let to_name = Self::name(to_name)?;
        let from_name = Self::name(from_name).map_err(|_| FsError::NotFound)?;
        let mut t = self.tree.lock().unwrap();
        let id = *t.dir(from_dir)?.get(&from_name).ok_or(FsError::NotFound)?;
        t.dir(to_dir)?;
        if from_dir == to_dir && from_name == to_name {
            return Ok(());
        }
        // A folder cannot go into itself.
        let mut at = to_dir;
        while at != ROOT_ID {
            if at == id {
                return Err(FsError::Invalid);
            }
            at = t.nodes.get(&at).ok_or(FsError::NotFound)?.parent;
        }
        // Replace whatever is at the destination.
        if let Some(target) = t.dir(to_dir)?.get(&to_name).copied() {
            if matches!(&t.nodes[&target].kind, Kind::Dir(children) if !children.is_empty()) {
                return Err(FsError::NotEmpty);
            }
            if let Some(cloud) = t.cloud(target).cloned() {
                self.push_op(&mut t, Op::Delete { node: hex(&cloud.id.0) });
            }
            drop_local(t.detach(target));
        }
        t.relink(id, to_dir, &to_name);
        // Does the node belong in the cloud where it is now?
        let belongs = !is_local_only(&to_name) && t.cloud(to_dir).is_some();
        match (t.cloud(id).is_some(), belongs) {
            (true, true) => {
                if let Some(op) = self.move_op(&t, id) {
                    self.push_op(&mut t, op);
                }
            }
            (true, false) => self.disown(&mut t, id),
            // A temporary file renamed into place: it has to be uploaded now.
            (false, true) => self.adopt(&mut t, id),
            (false, false) => {}
        }
        Ok(())
    }

    // ------------------------------------------------------------ uploading

    pub fn pending(&self) -> Pending {
        let t = self.tree.lock().unwrap();
        let mut p = Pending { ops: t.ops.len(), ..Default::default() };
        for n in t.nodes.values() {
            if let Kind::File(f) = &n.kind {
                if n.cloud.is_some() && (f.local.is_some() || f.uploading) {
                    p.files += 1;
                    p.bytes += f.size;
                }
            }
        }
        p.uploading = self.uploading.lock().unwrap().clone();
        p.last_error = self.last_error.lock().unwrap().clone();
        p
    }

    /// One pass of the uploader: send queued namespace ops in order, then
    /// upload every file that has been quiet long enough (or all of them
    /// with `force`). Returns true if it did any work.
    pub async fn sync_once(self: &Arc<Self>, force: bool) -> bool {
        let _one_pass = self.sync_lock.lock().await;
        let mut worked = false;
        loop {
            // Namespace ops first, in order, many to a commit.
            loop {
                let commit = {
                    let mut t = self.tree.lock().unwrap();
                    if t.ops.is_empty() {
                        t.inflight = None;
                        break;
                    }
                    let (id, n) = match t.inflight.clone() {
                        Some((id, n)) => (id, n.min(t.ops.len())),
                        None => {
                            // Saved before it is sent: if the answer is lost,
                            // or this process dies, the same commit goes again.
                            let (id, n) = (hex(&NodeId::random(&mut OsRandom).0), t.ops.len().min(OPS_PER_COMMIT));
                            self.log_append(&format!("!{id} {n}\n"));
                            t.inflight = Some((id.clone(), n));
                            (id, n)
                        }
                    };
                    t.busy = true;
                    Commit { id, ops: t.ops.iter().take(n).cloned().collect() }
                };
                let answer = self.client.commit(&self.keys.drive, &commit).await;
                // Was the id taken by a node, or the name? Only the cloud knows.
                let id_exists = match &answer {
                    Ok(Reply::Refused(r)) if r.error == E_EXISTS => match r.op.and_then(|i| commit.ops.get(i)) {
                        Some(Op::Create { id, .. }) => matches!(self.client.node(&self.keys.drive, id).await, Ok(Reply::Ok(n)) if !n.deleted),
                        _ => false,
                    },
                    _ => false,
                };
                let mut t = self.tree.lock().unwrap();
                t.busy = false;
                match answer {
                    Ok(Reply::Ok(_)) => {
                        let sent = commit.ops.len().min(t.ops.len());
                        let done: Vec<Op> = t.ops.drain(..sent).collect();
                        for op in done {
                            if let Op::Create { id, .. } = &op {
                                let local = node_id(id).and_then(|id| t.by_cloud.get(&id).copied());
                                if let Some(c) = local.and_then(|l| t.nodes.get_mut(&l)).and_then(|n| n.cloud.as_mut()) {
                                    c.committed = true;
                                }
                            }
                        }
                        t.inflight = None;
                        if t.ops.is_empty() {
                            self.rewrite_ops(&t);
                        } else {
                            self.log_append(&format!("-{}\n", commit.ops.len()));
                        }
                        self.sync_gen.fetch_add(1, Ordering::Relaxed);
                        *self.last_error.lock().unwrap() = None;
                        worked = true;
                    }
                    Ok(Reply::Refused(r)) => {
                        // None of the commit was applied. Deal with the op
                        // that was refused; the rest goes again.
                        t.inflight = None;
                        self.refused(&mut t, &commit, &r, id_exists);
                        self.rewrite_ops(&t);
                        worked = true;
                    }
                    Err(e) => {
                        self.fail(format!("{e:#}"));
                        return worked;
                    }
                }
            }
            // Then files, several at a time.
            let ready: Vec<u64> = {
                let t = self.tree.lock().unwrap();
                if !t.ops.is_empty() {
                    continue;
                }
                t.nodes
                    .iter()
                    .filter_map(|(id, n)| match &n.kind {
                        Kind::File(f) if n.cloud.is_some() && !f.uploading => f.local.as_ref().filter(|l| force || l.last_write.elapsed() >= UPLOAD_QUIET).map(|_| *id),
                        _ => None,
                    })
                    .collect()
            };
            if ready.is_empty() {
                break;
            }
            let mut failed = false;
            for batch in ready.chunks(UPLOAD_FILES) {
                if !self.tree.lock().unwrap().ops.is_empty() {
                    break;
                }
                let results = futures_util::future::join_all(batch.iter().map(|id| self.upload(*id, force))).await;
                worked |= results.iter().any(|r| matches!(r, Ok(true)));
                if results.iter().any(|r| r.is_err()) {
                    failed = true;
                    break;
                }
            }
            if failed {
                break;
            }
        }
        worked
    }

    /// The cloud refused op `r.op` of a commit, and with it the whole
    /// commit. Take that op out of the queue and settle what this Mac had
    /// believed, so that nothing written here is lost and everything behind
    /// it in the queue can go.
    fn refused(&self, t: &mut Tree, commit: &Commit, r: &Refusal, id_exists: bool) {
        let index = r.op.unwrap_or(0).min(commit.ops.len().saturating_sub(1));
        if index >= t.ops.len() {
            return;
        }
        let Some(op) = t.ops.remove(index) else { return };
        let local_of = |t: &Tree, id: &str| node_id(id).and_then(|id| t.by_cloud.get(&id).copied());
        match (&op, r.error.as_str()) {
            // The cloud has this very node: an earlier send of it got through.
            (Op::Create { id, .. }, E_EXISTS) if id_exists => {
                if let Some(c) = local_of(t, id).and_then(|l| t.nodes.get_mut(&l)).and_then(|n| n.cloud.as_mut()) {
                    c.committed = true;
                }
            }
            // Someone else's node has the name. Keep both: ours gets another name.
            (Op::Create { id, .. }, E_EXISTS) => {
                let Some(local) = local_of(t, id) else { return };
                let (parent, name, cloud_id) = (t.nodes[&local].parent, t.nodes[&local].name.clone(), t.nodes[&local].cloud.as_ref().map(|c| c.id));
                let Some(cloud_id) = cloud_id else { return };
                let renamed = t.free_name(parent, conflict_name(&name, "conflict", &cloud_id));
                t.relink(local, parent, &renamed);
                if let Some(again) = self.create_op(t, local) {
                    t.ops.insert(index, again);
                }
                t.resync = true;
                self.notice(format!("{name} already exists in the cloud; yours is kept as {renamed}"));
            }
            // The folder it was created in is gone from the cloud.
            (Op::Create { id, .. }, E_NO_NODE | E_WRONG_KIND) => {
                let Some(local) = local_of(t, id) else { return };
                let name = t.nodes[&local].name.clone();
                let has_content = matches!(&t.nodes[&local].kind, Kind::File(f) if f.local.is_some());
                let cloud_id = t.nodes[&local].cloud.as_ref().map(|c| c.id);
                match (has_content, cloud_id) {
                    // A file with content: keep it, at the top of the drive.
                    (true, Some(cloud_id)) => {
                        let renamed = t.free_name(ROOT_ID, conflict_name(&name, "recovered", &cloud_id));
                        t.relink(local, ROOT_ID, &renamed);
                        if let Some(again) = self.create_op(t, local) {
                            t.ops.insert(index, again);
                        }
                        self.notice(format!("the folder of {name} was deleted in the cloud; the file is kept as {renamed}"));
                    }
                    _ => {
                        // A folder, or an empty file. What was in a folder
                        // comes up for the same treatment, op by op.
                        if let Some(node) = t.nodes.get_mut(&local) {
                            if let Some(c) = node.cloud.take() {
                                t.by_cloud.remove(&c.id);
                            }
                        }
                        if !matches!(&t.nodes[&local].kind, Kind::Dir(c) if !c.is_empty()) {
                            drop_local(t.detach(local));
                        }
                        self.notice(format!("{name} could not be created: its folder was deleted in the cloud"));
                    }
                }
                t.resync = true;
            }
            // Already gone.
            (Op::Delete { .. }, E_NO_NODE) => {}
            // A move or delete the cloud will not have: its state wins, and
            // the next read of the feed shows it.
            (Op::Move { .. } | Op::Delete { .. }, _) => {
                t.resync = true;
                self.notice(format!("the cloud refused a change ({}): {}", r.error, r.detail));
            }
            _ => {
                t.resync = true;
                self.notice(format!("the cloud refused {op:?}: {} ({})", r.error, r.detail));
            }
        }
    }

    /// Cut bytes `start..end` of a snapshot into shards, seal them, send the
    /// ones the cloud lacks, and return their segments. One pass: at most a
    /// few sealed shards are in memory.
    async fn seal_run(self: &Arc<Self>, snap: &Arc<ReadSnap>, start: u64, end: u64) -> anyhow::Result<Vec<PrivateSegment>> {
        let (dedup, cut) = self.dedup().await?;
        let (tx, rx) = tokio::sync::mpsc::channel::<std::io::Result<Vec<u8>>>(4);
        let (this, feed_snap) = (self.clone(), snap.clone());
        let feeder = tokio::spawn(async move {
            let mut offset = start;
            while offset < end {
                let n = (end - offset).min(CUT_READ_BYTES);
                let read = this.read_snap(&feed_snap, offset, n).await.map_err(|e| std::io::Error::other(format!("{e:?}")));
                let failed = read.is_err();
                if tx.send(read).await.is_err() || failed {
                    break;
                }
                offset += n;
            }
        });
        // Cutting and sealing are CPU work: off the threads the mount answers on.
        let (sealed_tx, mut sealed_rx) = tokio::sync::mpsc::channel::<std::io::Result<crypt::SealedShard>>(2);
        let (dedup, cut) = (dedup.clone(), cut.clone());
        let sealer = tokio::task::spawn_blocking(move || {
            for piece in crypt::cut(ChannelReader { rx, buf: Vec::new(), pos: 0 }, &cut) {
                let sealed = piece.map(|p| crypt::seal_shard(&p.data, &dedup));
                let failed = sealed.is_err();
                if sealed_tx.blocking_send(sealed).is_err() || failed {
                    break;
                }
            }
        });
        let mut segments = Vec::new();
        let mut batch: Vec<crypt::SealedShard> = Vec::new();
        let mut total = 0u64;
        loop {
            let next = sealed_rx.recv().await;
            let done = next.is_none();
            if let Some(sealed) = next {
                let sealed = sealed?;
                total += sealed.n as u64;
                segments.push(PrivateSegment::from(&sealed));
                batch.push(sealed);
            }
            if done || batch.iter().map(|s| s.bytes.len()).sum::<usize>() >= SEAL_BATCH_BYTES {
                self.send_shards(std::mem::take(&mut batch)).await?;
            }
            if done {
                break;
            }
        }
        feeder.abort();
        sealer.await?;
        if total != end - start {
            anyhow::bail!("cut {total} bytes of a {} byte run", end - start);
        }
        Ok(segments)
    }

    /// Ask which of these sealed shards the cloud lacks, and send those.
    async fn send_shards(&self, batch: Vec<crypt::SealedShard>) -> anyhow::Result<()> {
        if batch.is_empty() {
            return Ok(());
        }
        let mut names: Vec<String> = batch.iter().map(|s| s.id_hex()).collect();
        names.sort();
        names.dedup();
        let lacking = self.client.missing(&self.keys.drive, &Blobs { shards: names, manifests: vec![] }).await?.shards;
        let mut todo = Vec::new();
        for sealed in batch {
            let name = sealed.id_hex();
            if lacking.contains(&name) && !todo.iter().any(|(n, _)| *n == name) {
                todo.push((name, Bytes::from(sealed.bytes)));
            } else {
                Stats::add(&self.stats.bytes_skipped, sealed.n as u64);
            }
        }
        let mut sends = futures_util::stream::iter(todo).map(|(name, bytes)| async move { self.client.put_shard(&self.keys.drive, &name, bytes).await }).buffer_unordered(UPLOAD_PARALLELISM);
        while let Some(sent) = sends.next().await {
            sent?;
        }
        Ok(())
    }

    /// Seal and commit one journaled file as a new version.
    #[allow(clippy::too_many_arguments)]
    async fn upload_sealed(self: &Arc<Self>, file: &FileRef, size: u64, mtime_ms: u64, snap: &Arc<ReadSnap>, gen: u64, local_id: u64) -> anyhow::Result<Uploaded> {
        let ReadSnap::Local { extents, base, base_len, .. } = &**snap else {
            anyhow::bail!("nothing local to upload");
        };
        let whole = |size: u64| if size == 0 { Vec::new() } else { vec![Patch::Run { start: 0, end: size }] };
        // Plan a patch over the version underneath, unless everything was rewritten.
        let mut kept_keys: HashMap<String, (ShardKey, [u8; 32])> = HashMap::new();
        let pieces = match base {
            Some(remote) if size > 0 && !extents.covers(0, size) => {
                let Some(layout) = self.layout(file, remote).await? else {
                    return Ok(Uploaded::BaseGone);
                };
                let segments: Vec<shard::Segment> = layout
                    .segs
                    .iter()
                    .map(|s| match &s.src {
                        Src::Shard { id, key } => {
                            let raw = <[u8; 32]>::try_from(&unhex(id)[..]).unwrap_or([0; 32]);
                            kept_keys.insert(id.to_string(), (key.clone(), raw));
                            shard::Segment::Shard { h: id.to_string(), n: s.n }
                        }
                        Src::Synth { seed, off } => shard::Segment::Synth { seed: *seed, off: *off, n: s.n },
                    })
                    .collect();
                let dirty: Vec<(u64, u64)> = extents.segments(0, size).into_iter().filter(|s| s.2).map(|s| (s.0, s.1)).collect();
                patch::plan(&segments, (*base_len).min(remote.size), size, &dirty)
            }
            _ => whole(size),
        };
        let mut kept: Vec<String> = Vec::new();
        let mut manifest = PrivateManifest { size, segments: Vec::with_capacity(pieces.len()) };
        for piece in pieces {
            match piece {
                Patch::Keep(shard::Segment::Shard { h, n }) => {
                    let (key, id) = kept_keys.get(&h).cloned().ok_or_else(|| anyhow::anyhow!("a kept shard is not in the base manifest"))?;
                    kept.push(h);
                    manifest.segments.push(PrivateSegment::Shard { id, n: n as u32, key });
                }
                Patch::Keep(shard::Segment::Synth { seed, off, n }) => manifest.segments.push(PrivateSegment::Synth { seed, off, n }),
                Patch::Run { start, end } => manifest.segments.extend(self.seal_run(snap, start, end).await?),
            }
        }
        manifest.validate().map_err(|e| anyhow::anyhow!("the new manifest is wrong: {e}"))?;

        let mut rng = OsRandom;
        let sealed = file.key.seal_manifest(&file.id, &manifest, &mut rng);
        let manifest_name: Arc<str> = sha256_hex(&sealed).into();
        self.client.put_manifest(&self.keys.drive, &manifest_name, Bytes::from(sealed)).await?;
        let meta = hex(&file.key.seal_meta(&file.id, &FileMeta { mtime_ms }, &mut rng));

        // The bytes were read while the file could still be written. If it
        // was, what was sealed may be a mix: leave it for the next pass.
        if self.tree.lock().unwrap().file(local_id).ok().and_then(|f| f.local.as_ref()).map(|l| l.gen) != Some(gen) {
            return Ok(Uploaded::Moved);
        }

        let shards: Vec<String> = manifest.shard_ids();
        let put = |base: Option<u64>| Op::Put { node: hex(&file.id.0), base, size, manifest: manifest_name.to_string(), shards: shards.clone(), meta: meta.clone() };
        let mut based_on = Some(base.as_ref().map(|r| r.version).unwrap_or(0));
        for _attempt in 0..3 {
            let commit = Commit { id: hex(&NodeId::random(&mut rng).0), ops: vec![put(based_on)] };
            match self.client.commit(&self.keys.drive, &commit).await? {
                Reply::Ok(done) => {
                    let layout = self.remember_layout(&manifest_name, &manifest);
                    return Ok(Uploaded::Done { version: done.seq, manifest: manifest_name, layout });
                }
                // Someone else wrote the file since. The last writer wins;
                // their version stays in the file's history.
                Reply::Refused(r) if r.error == E_CONFLICT => based_on = None,
                Reply::Refused(r) if r.error == E_NO_NODE => return Ok(Uploaded::NodeGone),
                // A shard of the old version that this one keeps was removed
                // in the cloud, and its bytes are not here to send again.
                Reply::Refused(r) if r.error == E_MISSING && r.missing.shards.iter().any(|s| kept.contains(s)) => return Ok(Uploaded::BaseGone),
                Reply::Refused(r) if r.error == E_MISSING => anyhow::bail!("the cloud lost shards that were just sent; they will be sent again"),
                Reply::Refused(r) => anyhow::bail!("the cloud refused the new version: {} ({})", r.error, r.detail),
            }
        }
        anyhow::bail!("the file keeps changing in the cloud")
    }

    /// Publish one journaled file as a new version.
    async fn upload(self: &Arc<Self>, id: u64, force: bool) -> FsResult<bool> {
        let (path, file, size, gen, mtime_ms, snap) = {
            let mut t = self.tree.lock().unwrap();
            // Every queued create and move has to have reached the cloud first.
            if !t.ops.is_empty() {
                return Ok(false);
            }
            let path = t.path_of(id);
            let Some(node) = t.nodes.get(&id) else { return Ok(false) };
            let Some(cloud) = node.cloud.clone().filter(|c| c.committed) else { return Ok(false) };
            let mtime_ms = node.mtime_ms;
            let f = t.file_mut(id)?;
            let Some(l) = &f.local else { return Ok(false) };
            if f.uploading || !(force || l.last_write.elapsed() >= UPLOAD_QUIET) {
                return Ok(false);
            }
            let file = FileRef { id: cloud.id, key: cloud.key };
            let snap = ReadSnap::Local { file: Some(file.clone()), journal: l.file.clone(), extents: l.extents.clone(), base: l.base.clone(), base_len: l.base_len };
            let out = (path, file, f.size, l.gen, mtime_ms, snap);
            f.uploading = true;
            out
        };
        *self.uploading.lock().unwrap() = Some(path.clone());
        let snap = Arc::new(snap);
        let result = self.upload_sealed(&file, size, mtime_ms, &snap, gen, id).await;
        *self.uploading.lock().unwrap() = None;

        let fully_local = matches!(&*snap, ReadSnap::Local { extents, .. } if extents.covers(0, size));
        let (layout, seed) = {
            let mut t = self.tree.lock().unwrap();
            if let Ok(f) = t.file_mut(id) {
                f.uploading = false;
            }
            let (version, manifest, layout) = match result {
                Ok(Uploaded::Done { version, manifest, layout }) => (version, manifest, layout),
                Ok(Uploaded::Moved) => return Ok(false),
                // What these writes were made over cannot be read any more,
                // so the file cannot be completed: the cloud's state wins.
                Ok(Uploaded::BaseGone) => {
                    self.notice(format!("{path} was replaced in the cloud and its old version is gone; local changes to it were dropped"));
                    drop_local(t.detach(id));
                    t.resync = true;
                    self.sync_gen.fetch_add(1, Ordering::Relaxed);
                    return Ok(true);
                }
                Ok(Uploaded::NodeGone) => {
                    if fully_local && t.cloud(t.nodes[&id].parent).is_some_and(|c| c.committed) {
                        // Everything of the file is here: it exists again, as a new node.
                        if let Some(old) = t.nodes.get_mut(&id).and_then(|n| n.cloud.take()) {
                            t.by_cloud.remove(&old.id);
                        }
                        if let Ok(f) = t.file_mut(id) {
                            f.remote = None;
                            if let Some(l) = &mut f.local {
                                l.base = None;
                                l.base_len = 0;
                            }
                        }
                        self.adopt(&mut t, id);
                        self.notice(format!("{path} was deleted in the cloud while it was being rewritten here; it has been saved again"));
                    } else {
                        self.notice(format!("{path} was deleted in the cloud; local changes to it were dropped"));
                        drop_local(t.detach(id));
                    }
                    t.resync = true;
                    self.sync_gen.fetch_add(1, Ordering::Relaxed);
                    return Ok(true);
                }
                Err(e) => {
                    self.fail(format!("upload {path}: {e:#}"));
                    if let Ok(f) = t.file_mut(id) {
                        if let Some(l) = &mut f.local {
                            l.last_write = Instant::now(); // try again after a pause
                        }
                    }
                    return Err(FsError::Io(format!("{e:#}")));
                }
            };
            self.sync_gen.fetch_add(1, Ordering::Relaxed);
            *self.last_error.lock().unwrap() = None;
            let mut seed: Option<(Arc<File>, PathBuf)> = None;
            if let Ok(f) = t.file_mut(id) {
                f.remote = Some(Remote { version, size, manifest });
                if f.local.as_ref().map(|l| l.gen) == Some(gen) {
                    if let Some(l) = f.local.take() {
                        let _ = std::fs::remove_file(l.path.with_extension("meta"));
                        if fully_local && size <= self.cache.cap() / 4 {
                            seed = Some((l.file, l.path));
                        } else {
                            let _ = std::fs::remove_file(&l.path);
                        }
                    }
                }
            }
            (layout, seed)
        };
        // Keep the freshly written bytes warm: move them into the cache
        // under the shards they became.
        if let Some((journal, path)) = seed {
            let cache = self.cache.clone();
            let _ = tokio::task::spawn_blocking(move || {
                let mut buf = vec![0u8; BLOCK_SIZE as usize];
                'segs: for seg in &layout.segs {
                    let Src::Shard { id, .. } = &seg.src else { continue };
                    let mut idx = 0;
                    while idx * BLOCK_SIZE < seg.n {
                        let n = (seg.n - idx * BLOCK_SIZE).min(BLOCK_SIZE) as usize;
                        if journal.read_exact_at(&mut buf[..n], seg.start + idx * BLOCK_SIZE).is_err() || cache.put(id, idx, &buf[..n]).is_err() {
                            break 'segs;
                        }
                        idx += 1;
                    }
                }
                let _ = std::fs::remove_file(&path);
            })
            .await;
        }
        Ok(true)
    }

    /// Upload everything now and wait until nothing is pending.
    pub async fn flush(self: &Arc<Self>, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            self.sync_once(true).await;
            if self.pending().is_idle() {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    // ------------------------------------------------------------ tasks

    pub async fn run_uploader(self: Arc<Self>) {
        while !self.stopped.load(Ordering::Relaxed) {
            if !self.sync_once(false).await {
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        }
    }

    /// Background loop: ask the feed what changed, so changes made
    /// elsewhere show up in the mount within a second or two.
    pub async fn run_poller(self: Arc<Self>) {
        while !self.stopped.load(Ordering::Relaxed) {
            if !self.has_unsynced() {
                let _ = self.refresh().await;
            }
            tokio::time::sleep(POLL).await;
        }
    }

    /// Stop the background loops. Writes that have not reached the cloud
    /// stay in the journal folder and are picked up by the next start.
    pub fn stop(&self) {
        self.stopped.store(true, Ordering::Relaxed);
        let t = self.tree.lock().unwrap();
        let unsent = self.recovered.lock().unwrap().is_some() || !t.ops.is_empty() || t.nodes.values().any(|n| n.cloud.is_some() && matches!(&n.kind, Kind::File(f) if f.local.is_some()));
        if !unsent {
            let _ = std::fs::remove_dir_all(&self.journal_dir);
        }
    }
}
