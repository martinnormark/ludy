//! One drive as a filesystem: a tree of nodes built from the cloud listing,
//! reads that fetch only the blocks an application asks for, and writes that
//! land in a local journal and upload in the background.
//!
//! This layer knows nothing about NFS; [`crate::nfs`] adapts it.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use futures_util::future::{BoxFuture, FutureExt, Shared};
use futures_util::StreamExt;
use ludy_cloud::shard;
use tokio::sync::Semaphore;

use crate::cache::{BlockCache, BLOCK_SIZE};
use crate::client::{CloudClient, Committed, Listing, Manifest, Segment};
use crate::patch::{self, Piece as Patch};
use crate::rangeset::RangeSet;
use crate::stats::Stats;

pub const ROOT_ID: u64 = 1;
/// Missing blocks next to each other are fetched with one range request of at
/// most this many blocks (2 MiB).
const MAX_RUN_BLOCKS: u64 = 8;
/// Upper bound for how far ahead of a sequential reader we prefetch (8 MiB).
const MAX_READAHEAD_BLOCKS: u64 = 32;
const FETCH_PARALLELISM: usize = 8;
/// How many shards one upload sends at a time.
const UPLOAD_PARALLELISM: usize = 4;
/// Read size when streaming a run through the chunker.
const CUT_READ_BYTES: u64 = 1 << 20;
/// A written file is uploaded once it has been left alone for this long.
const UPLOAD_QUIET: Duration = Duration::from_millis(1500);

#[derive(Clone, Debug, PartialEq)]
pub enum FsError {
    NotFound,
    NotDir,
    IsDir,
    Exists,
    NotEmpty,
    Invalid,
    Io(String),
}

pub type FsResult<T> = Result<T, FsError>;

#[derive(Clone, Debug, PartialEq)]
pub struct Attr {
    pub id: u64,
    pub is_dir: bool,
    pub size: u64,
    pub mtime_ms: u64,
    /// Bytes of this file held on this machine (cache or journal).
    pub local_bytes: u64,
}

#[derive(Clone, Debug)]
struct Remote {
    version: Arc<str>,
    size: u64,
}

/// Journal state of a file that has been written through the mount.
struct Local {
    path: PathBuf,
    file: Arc<File>,
    /// Byte ranges whose content lives in the journal file.
    extents: RangeSet,
    /// The cloud version underneath, and how much of it is still valid
    /// (a truncate shortens it).
    base: Option<(Remote, String)>,
    base_len: u64,
    /// Bumped on every change, so an upload can tell if it raced a write.
    gen: u64,
    last_write: Instant,
}

struct FileNode {
    size: u64,
    remote: Option<Remote>,
    local: Option<Local>,
    /// Finder droppings (`.DS_Store`, `._*`) stay on this machine.
    local_only: bool,
    uploading: bool,
}

enum Kind {
    Dir(BTreeMap<String, u64>),
    File(FileNode),
}

struct Node {
    parent: u64,
    name: String,
    mtime_ms: u64,
    kind: Kind,
}

/// A namespace change waiting to be replayed on the cloud, in order.
#[derive(Clone, Debug, PartialEq)]
enum NsOp {
    Delete { key: String },
    Rename { from: String, to: String },
    Mkdir { key: String },
}

struct Tree {
    nodes: HashMap<u64, Node>,
    next_id: u64,
    /// Drive sequence number of the last listing applied.
    seq: u64,
    ops: VecDeque<NsOp>,
    /// A namespace op or upload is executing right now.
    busy: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Pending {
    /// Files written locally and not yet uploaded.
    pub files: usize,
    pub bytes: u64,
    /// Renames, deletes and new folders not yet replayed.
    pub ops: usize,
    pub uploading: Option<String>,
    pub last_error: Option<String>,
}

impl Pending {
    pub fn is_idle(&self) -> bool {
        self.files == 0 && self.ops == 0 && self.uploading.is_none()
    }
}

#[derive(Default)]
struct ReadAhead {
    next: u64,
    streak: u32,
    ahead_until: u64,
}

struct Run {
    first: u64,
    data: Bytes,
}

type RunFuture = Shared<BoxFuture<'static, Result<Arc<Run>, Arc<str>>>>;

enum Piece {
    Cached(u64),
    Pending(u64, RunFuture),
}

/// Everything needed to read a file's bytes without holding the tree lock.
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
    Remote { remote: Remote, key: String },
    Local { file: Arc<File>, extents: RangeSet, base: Option<(Remote, String)>, base_len: u64 },
}

pub struct DriveFs {
    pub name: String,
    client: CloudClient,
    cache: Arc<BlockCache>,
    stats: Arc<Stats>,
    journal_dir: PathBuf,
    tree: Mutex<Tree>,
    inflight: Mutex<HashMap<(Arc<str>, u64), RunFuture>>,
    readahead: Mutex<HashMap<u64, ReadAhead>>,
    fetch_sem: Arc<Semaphore>,
    uploading: Mutex<Option<String>>,
    last_error: Mutex<Option<String>>,
    /// Milliseconds since the Unix epoch of the last filesystem call.
    pub last_activity_ms: AtomicU64,
    journal_seq: AtomicU64,
    stopped: AtomicBool,
}

fn now_ms() -> u64 {
    ludy_cloud::now_ms()
}

/// Names Finder and Spotlight create on any volume. They never leave this Mac.
pub fn is_local_only(name: &str) -> bool {
    name.starts_with("._")
        || matches!(
            name,
            ".DS_Store" | ".Spotlight-V100" | ".Trashes" | ".fseventsd" | ".TemporaryItems" | ".apdisk" | ".VolumeIcon.icns"
        )
        || name.starts_with(".metadata_never_index")
}

fn valid_name(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains('/') && name.len() <= 255
}

impl Tree {
    fn new() -> Tree {
        let mut nodes = HashMap::new();
        nodes.insert(ROOT_ID, Node { parent: ROOT_ID, name: String::new(), mtime_ms: now_ms(), kind: Kind::Dir(BTreeMap::new()) });
        Tree { nodes, next_id: ROOT_ID + 1, seq: 0, ops: VecDeque::new(), busy: false }
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

    /// The cloud key of a node: its path from the root, `/`-separated.
    fn key_of(&self, mut id: u64) -> String {
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
            id = *self.dir(id).ok()?.get(part)?;
        }
        Some(id)
    }

    fn touch(&mut self, id: u64) {
        if let Some(n) = self.nodes.get_mut(&id) {
            n.mtime_ms = now_ms().max(n.mtime_ms + 1);
        }
    }

    fn insert(&mut self, parent: u64, name: &str, mtime_ms: u64, kind: Kind) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        self.nodes.insert(id, Node { parent, name: name.to_string(), mtime_ms, kind });
        if let Ok(children) = self.dir_mut(parent) {
            children.insert(name.to_string(), id);
        }
        self.touch(parent);
        id
    }

    /// Detach a node (and, for a folder, everything below it) from the tree.
    fn detach(&mut self, id: u64) -> Vec<Node> {
        let mut out = Vec::new();
        let Some(node) = self.nodes.remove(&id) else { return out };
        if let Ok(children) = self.dir_mut(node.parent) {
            children.remove(&node.name);
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
        out
    }

    fn ensure_dir(&mut self, path: &str, mtime_ms: u64) -> Option<u64> {
        let mut id = ROOT_ID;
        for part in path.split('/').filter(|p| !p.is_empty()) {
            let existing = self.dir(id).ok()?.get(part).copied();
            id = match existing {
                Some(child) => {
                    if !matches!(self.nodes.get(&child)?.kind, Kind::Dir(_)) {
                        return None;
                    }
                    child
                }
                None => self.insert(id, part, mtime_ms, Kind::Dir(BTreeMap::new())),
            };
        }
        Some(id)
    }

    fn has_unsynced(&self) -> bool {
        self.busy
            || !self.ops.is_empty()
            || self.nodes.values().any(|n| matches!(&n.kind, Kind::File(f) if !f.local_only && (f.local.is_some() || f.uploading)))
    }
}

fn drop_local(nodes: Vec<Node>) {
    for n in nodes {
        if let Kind::File(FileNode { local: Some(l), .. }) = n.kind {
            let _ = std::fs::remove_file(&l.path);
        }
    }
}

impl DriveFs {
    pub fn new(name: &str, client: CloudClient, cache: Arc<BlockCache>, stats: Arc<Stats>, journal_dir: PathBuf) -> std::io::Result<Arc<DriveFs>> {
        // Journal files from an earlier run cannot be matched to nodes any more.
        let _ = std::fs::remove_dir_all(&journal_dir);
        std::fs::create_dir_all(&journal_dir)?;
        Ok(Arc::new(DriveFs {
            name: name.to_string(),
            client,
            cache,
            stats,
            journal_dir,
            tree: Mutex::new(Tree::new()),
            inflight: Mutex::new(HashMap::new()),
            readahead: Mutex::new(HashMap::new()),
            fetch_sem: Arc::new(Semaphore::new(FETCH_PARALLELISM)),
            uploading: Mutex::new(None),
            last_error: Mutex::new(None),
            last_activity_ms: AtomicU64::new(0),
            journal_seq: AtomicU64::new(0),
            stopped: AtomicBool::new(false),
        }))
    }

    pub fn note_activity(&self) {
        self.last_activity_ms.store(now_ms(), Ordering::Relaxed);
    }

    pub fn seq(&self) -> u64 {
        self.tree.lock().unwrap().seq
    }

    // ------------------------------------------------------------ listing

    /// Fetch the drive's listing and fold it into the tree.
    pub async fn refresh(&self) -> anyhow::Result<bool> {
        let listing = self.client.list(&self.name).await?;
        Ok(self.apply_listing(&listing))
    }

    /// Fold a cloud listing into the tree. Node ids of paths that still exist
    /// are kept. Returns false (and changes nothing) while local changes are
    /// still waiting to upload, so the listing cannot undo them.
    pub fn apply_listing(&self, listing: &Listing) -> bool {
        let mut t = self.tree.lock().unwrap();
        if t.has_unsynced() {
            return false;
        }
        let mut want_files: HashSet<&str> = HashSet::new();
        let mut want_dirs: HashSet<String> = HashSet::new();
        for o in &listing.objects {
            let (dir_path, is_marker) = match o.key.strip_suffix('/') {
                Some(d) => (d, true),
                None => (o.key.rsplit_once('/').map(|(d, _)| d).unwrap_or(""), false),
            };
            let mut acc = String::new();
            for part in dir_path.split('/').filter(|p| !p.is_empty()) {
                if !acc.is_empty() {
                    acc.push('/');
                }
                acc.push_str(part);
                want_dirs.insert(acc.clone());
            }
            if is_marker {
                t.ensure_dir(dir_path, o.mtime_ms);
                continue;
            }
            want_files.insert(o.key.as_str());
            let Some(parent) = t.ensure_dir(dir_path, o.mtime_ms) else { continue };
            let name = o.key.rsplit('/').next().unwrap_or(&o.key);
            let remote = Remote { version: o.version.as_str().into(), size: o.size };
            match t.dir(parent).ok().and_then(|c| c.get(name).copied()) {
                Some(id) => {
                    let Some(node) = t.nodes.get_mut(&id) else { continue };
                    if let Kind::File(f) = &mut node.kind {
                        if f.remote.as_ref().map(|r| &*r.version) != Some(&*remote.version) {
                            f.remote = Some(remote);
                            f.size = o.size;
                            node.mtime_ms = o.mtime_ms;
                            self.readahead.lock().unwrap().remove(&id);
                        }
                    }
                }
                None => {
                    let file = FileNode { size: o.size, remote: Some(remote), local: None, local_only: false, uploading: false };
                    t.insert(parent, name, o.mtime_ms, Kind::File(file));
                }
            }
        }
        // Remove what the cloud no longer has. Children before parents.
        let mut ids: Vec<(usize, u64)> = t.nodes.keys().filter(|id| **id != ROOT_ID).map(|id| (t.key_of(*id).matches('/').count(), *id)).collect();
        ids.sort_by(|a, b| b.0.cmp(&a.0));
        for (_, id) in ids {
            let Some(node) = t.nodes.get(&id) else { continue };
            let key = t.key_of(id);
            let gone = match &node.kind {
                Kind::File(f) => !f.local_only && !want_files.contains(key.as_str()),
                Kind::Dir(children) => {
                    !want_dirs.contains(&key)
                        && children.values().all(|c| matches!(t.nodes.get(c).map(|n| &n.kind), Some(Kind::File(f)) if f.local_only))
                }
            };
            if gone {
                drop_local(t.detach(id));
            }
        }
        t.seq = listing.seq;
        true
    }

    // ------------------------------------------------------------ metadata

    fn attr_of(&self, t: &Tree, id: u64) -> FsResult<Attr> {
        let node = t.nodes.get(&id).ok_or(FsError::NotFound)?;
        Ok(match &node.kind {
            Kind::Dir(_) => Attr { id, is_dir: true, size: 0, mtime_ms: node.mtime_ms, local_bytes: 0 },
            Kind::File(f) => {
                let local_bytes = match (&f.local, &f.remote) {
                    (Some(l), _) => l.extents.total(),
                    (None, Some(r)) => self.cache.version_bytes(&r.version),
                    (None, None) => 0,
                };
                Attr { id, is_dir: false, size: f.size, mtime_ms: node.mtime_ms, local_bytes }
            }
        })
    }

    pub fn getattr(&self, id: u64) -> FsResult<Attr> {
        self.attr_of(&self.tree.lock().unwrap(), id)
    }

    pub fn lookup(&self, dir: u64, name: &str) -> FsResult<u64> {
        let t = self.tree.lock().unwrap();
        match name {
            "." => Ok(dir),
            ".." => Ok(t.nodes.get(&dir).ok_or(FsError::NotFound)?.parent),
            _ => t.dir(dir)?.get(name).copied().ok_or(FsError::NotFound),
        }
    }

    /// Children of a folder in name order.
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

    /// The cloud key (path) of a node.
    pub fn key_of(&self, id: u64) -> String {
        self.tree.lock().unwrap().key_of(id)
    }

    pub fn find_path(&self, path: &str) -> Option<u64> {
        self.tree.lock().unwrap().find_path(path)
    }

    /// The cloud version currently backing a file, if it has one.
    pub fn version_of(&self, id: u64) -> Option<Arc<str>> {
        self.tree.lock().unwrap().file(id).ok()?.remote.as_ref().map(|r| r.version.clone())
    }

    // ------------------------------------------------------------ reading

    /// Read up to `len` bytes at `offset`. Returns the bytes and whether the
    /// read reached the end of the file.
    pub async fn read(self: &Arc<Self>, id: u64, offset: u64, len: u32) -> FsResult<(Vec<u8>, bool)> {
        let (snap, size) = {
            let t = self.tree.lock().unwrap();
            let f = t.file(id)?;
            let snap = match (&f.local, &f.remote) {
                (Some(l), _) => ReadSnap::Local { file: l.file.clone(), extents: l.extents.clone(), base: l.base.clone(), base_len: l.base_len },
                (None, Some(r)) => ReadSnap::Remote { remote: r.clone(), key: t.key_of(id) },
                (None, None) => return Ok((Vec::new(), true)),
            };
            (snap, f.size)
        };
        if offset >= size || len == 0 {
            return Ok((Vec::new(), offset >= size));
        }
        let len = (len as u64).min(size - offset);
        if let ReadSnap::Remote { remote, key } = &snap {
            self.plan_readahead(id, remote, key, offset, len);
        }
        let data = self.read_snap(&snap, offset, len).await?;
        Stats::add(&self.stats.bytes_read, data.len() as u64);
        Ok((data, offset + len >= size))
    }

    async fn read_snap(self: &Arc<Self>, snap: &ReadSnap, offset: u64, len: u64) -> FsResult<Vec<u8>> {
        match snap {
            ReadSnap::Remote { remote, key } => self.read_remote(remote, key, offset, len).await,
            ReadSnap::Local { file, extents, base, base_len } => {
                let mut out = vec![0u8; len as usize];
                for (start, end, is_local) in extents.segments(offset, offset + len) {
                    let dst = &mut out[(start - offset) as usize..(end - offset) as usize];
                    if is_local {
                        file.read_exact_at(dst, start).map_err(|e| FsError::Io(e.to_string()))?;
                    } else if let Some((remote, key)) = base {
                        // Not written locally: comes from the version underneath,
                        // or is a hole (zeros) past its end.
                        let upto = end.min(*base_len).min(remote.size);
                        if start < upto {
                            let data = self.read_remote(remote, key, start, upto - start).await?;
                            dst[..data.len()].copy_from_slice(&data);
                        }
                    }
                }
                Ok(out)
            }
        }
    }

    /// Sequential readers get blocks fetched ahead of them, further the longer
    /// the streak lasts. Random reads fetch only what they ask for.
    fn plan_readahead(self: &Arc<Self>, id: u64, remote: &Remote, key: &str, offset: u64, len: u64) {
        let last_block = (offset + len - 1) / BLOCK_SIZE;
        let total_blocks = remote.size.div_ceil(BLOCK_SIZE);
        let range = {
            let mut ra = self.readahead.lock().unwrap();
            let s = ra.entry(id).or_default();
            // The kernel issues its own read-ahead in parallel, so reads can
            // arrive slightly out of order; allow some slack.
            let slack = 4 * BLOCK_SIZE;
            if offset + slack >= s.next && offset <= s.next + slack {
                s.streak += 1;
            } else {
                s.streak = 0;
                s.ahead_until = 0;
            }
            s.next = s.next.max(offset + len).min(remote.size);
            if s.streak == 0 {
                s.next = offset + len;
            }
            if s.streak < 2 {
                return;
            }
            let window = (1u64 << s.streak.min(5)).min(MAX_READAHEAD_BLOCKS);
            let from = (last_block + 1).max(s.ahead_until);
            // Top up only once the reader has used half of what is ahead, so
            // read-ahead goes out as a few large requests, not many small ones.
            if from > last_block + 1 + window / 2 {
                return;
            }
            let to = (last_block + 1 + window).min(total_blocks);
            if from >= to {
                return;
            }
            s.ahead_until = to;
            (from, to - 1)
        };
        // Planning starts the fetches; nobody needs to wait for them here.
        let _ = self.plan(remote, key, range.0, range.1);
    }

    /// Work out how to get blocks `b0..=b1`: from the cache, from a fetch
    /// that is already running, or by starting new range requests.
    fn plan(self: &Arc<Self>, remote: &Remote, key: &str, b0: u64, b1: u64) -> Vec<Piece> {
        let mut pieces = Vec::with_capacity((b1 - b0 + 1) as usize);
        let mut inflight = self.inflight.lock().unwrap();
        let mut run: Vec<u64> = Vec::new();
        let flush = |run: &mut Vec<u64>, pieces: &mut Vec<Piece>, inflight: &mut HashMap<(Arc<str>, u64), RunFuture>| {
            if run.is_empty() {
                return;
            }
            let fut = self.start_fetch(remote, key, run[0], run.len() as u64);
            for idx in run.drain(..) {
                inflight.insert((remote.version.clone(), idx), fut.clone());
                pieces.push(Piece::Pending(idx, fut.clone()));
            }
        };
        for idx in b0..=b1 {
            if self.cache.contains(&remote.version, idx) {
                flush(&mut run, &mut pieces, &mut inflight);
                pieces.push(Piece::Cached(idx));
            } else if let Some(fut) = inflight.get(&(remote.version.clone(), idx)) {
                let fut = fut.clone();
                flush(&mut run, &mut pieces, &mut inflight);
                pieces.push(Piece::Pending(idx, fut));
            } else {
                run.push(idx);
                if run.len() as u64 == MAX_RUN_BLOCKS {
                    flush(&mut run, &mut pieces, &mut inflight);
                }
            }
        }
        flush(&mut run, &mut pieces, &mut inflight);
        pieces.sort_by_key(|p| match p {
            Piece::Cached(i) | Piece::Pending(i, _) => *i,
        });
        pieces
    }

    /// One range request for `count` consecutive blocks starting at `first`.
    fn start_fetch(self: &Arc<Self>, remote: &Remote, key: &str, first: u64, count: u64) -> RunFuture {
        let this = self.clone();
        let remote = remote.clone();
        let key = key.to_string();
        let fut: RunFuture = async move {
            let start = first * BLOCK_SIZE;
            let len = ((first + count) * BLOCK_SIZE).min(remote.size) - start;
            let result = async {
                let _permit = this.fetch_sem.acquire().await.map_err(|e| e.to_string())?;
                let data = this.client.get_range(&this.name, &key, &remote.version, start, len).await.map_err(|e| format!("{e:#}"))?;
                let (cache, version, blocks) = (this.cache.clone(), remote.version.clone(), data.clone());
                tokio::task::spawn_blocking(move || {
                    for (i, block) in blocks.chunks(BLOCK_SIZE as usize).enumerate() {
                        let _ = cache.put(&version, first + i as u64, block);
                    }
                })
                .await
                .map_err(|e| e.to_string())?;
                Ok::<Bytes, String>(data)
            }
            .await;
            let mut inflight = this.inflight.lock().unwrap();
            for idx in first..first + count {
                inflight.remove(&(remote.version.clone(), idx));
            }
            match result {
                Ok(data) => Ok(Arc::new(Run { first, data })),
                Err(e) => Err(Arc::<str>::from(e)),
            }
        }
        .boxed()
        .shared();
        // Drive it even if no reader is waiting (read-ahead).
        tokio::spawn(fut.clone());
        fut
    }

    async fn read_remote(self: &Arc<Self>, remote: &Remote, key: &str, offset: u64, len: u64) -> FsResult<Vec<u8>> {
        let len = len.min(remote.size.saturating_sub(offset));
        if len == 0 {
            return Ok(Vec::new());
        }
        let end = offset + len;
        let (b0, b1) = (offset / BLOCK_SIZE, (end - 1) / BLOCK_SIZE);
        let mut out = vec![0u8; len as usize];
        let pieces = self.plan(remote, key, b0, b1);
        for piece in pieces {
            let (idx, block) = match piece {
                Piece::Cached(idx) => match self.cache.get(&remote.version, idx) {
                    Some(data) => {
                        let overlap = (end.min((idx + 1) * BLOCK_SIZE) - offset.max(idx * BLOCK_SIZE)).min(data.len() as u64);
                        Stats::add(&self.stats.cache_hit_bytes, overlap);
                        (idx, data)
                    }
                    // Evicted between planning and reading: fetch it after all.
                    None => (idx, self.fetch_block(remote, key, idx).await?),
                },
                Piece::Pending(idx, fut) => {
                    let run = fut.await.map_err(|e| FsError::Io(e.to_string()))?;
                    let from = ((idx - run.first) * BLOCK_SIZE) as usize;
                    let to = (from + BLOCK_SIZE as usize).min(run.data.len());
                    (idx, run.data.slice(from..to))
                }
            };
            let block_start = idx * BLOCK_SIZE;
            let from = offset.max(block_start);
            let to = end.min(block_start + block.len() as u64);
            if from < to {
                out[(from - offset) as usize..(to - offset) as usize].copy_from_slice(&block[(from - block_start) as usize..(to - block_start) as usize]);
            }
        }
        Ok(out)
    }

    async fn fetch_block(self: &Arc<Self>, remote: &Remote, key: &str, idx: u64) -> FsResult<Bytes> {
        let start = idx * BLOCK_SIZE;
        let len = BLOCK_SIZE.min(remote.size - start);
        self.client.get_range(&self.name, key, &remote.version, start, len).await.map_err(|e| FsError::Io(format!("{e:#}")))
    }

    /// Download every block of a file into the cache (used by "pin").
    pub async fn fetch_all(self: &Arc<Self>, id: u64) -> FsResult<u64> {
        let (remote, key) = {
            let t = self.tree.lock().unwrap();
            let f = t.file(id)?;
            (f.remote.clone().ok_or(FsError::Invalid)?, t.key_of(id))
        };
        let total = remote.size.div_ceil(BLOCK_SIZE);
        let mut b = 0;
        while b < total {
            let to = (b + 4 * MAX_RUN_BLOCKS).min(total) - 1;
            for piece in self.plan(&remote, &key, b, to) {
                if let Piece::Pending(_, fut) = piece {
                    fut.await.map_err(|e| FsError::Io(e.to_string()))?;
                }
            }
            b = to + 1;
        }
        Ok(remote.size)
    }

    // ------------------------------------------------------------ writing

    fn new_journal_file(&self) -> FsResult<(PathBuf, Arc<File>)> {
        let n = self.journal_seq.fetch_add(1, Ordering::Relaxed);
        let path = self.journal_dir.join(format!("{n:08}.journal"));
        let file = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(true).open(&path).map_err(|e| FsError::Io(e.to_string()))?;
        Ok((path, Arc::new(file)))
    }

    /// Make sure a file has journal state, layering it over its cloud version.
    fn ensure_local<'a>(&self, t: &'a mut Tree, id: u64) -> FsResult<&'a mut Local> {
        let key = t.key_of(id);
        let f = t.file_mut(id)?;
        if f.local.is_none() {
            let (path, file) = self.new_journal_file()?;
            let base_len = f.remote.as_ref().map(|r| r.size.min(f.size)).unwrap_or(0);
            let base = f.remote.clone().map(|r| (r, key));
            f.local = Some(Local { path, file, extents: RangeSet::default(), base, base_len, gen: 0, last_write: Instant::now() });
        }
        Ok(f.local.as_mut().expect("just set"))
    }

    pub fn create(&self, dir: u64, name: &str, exclusive: bool) -> FsResult<u64> {
        if !valid_name(name) {
            return Err(FsError::Invalid);
        }
        let mut t = self.tree.lock().unwrap();
        if let Some(existing) = t.dir(dir)?.get(name).copied() {
            if exclusive {
                return Err(FsError::Exists);
            }
            t.file(existing)?;
            return Ok(existing);
        }
        let (path, file) = self.new_journal_file()?;
        let local = Local { path, file, extents: RangeSet::default(), base: None, base_len: 0, gen: 0, last_write: Instant::now() };
        let node = FileNode { size: 0, remote: None, local: Some(local), local_only: is_local_only(name), uploading: false };
        Ok(t.insert(dir, name, now_ms(), Kind::File(node)))
    }

    /// Write through the mount: the bytes go to the journal file and the call
    /// returns. The uploader publishes the file once writing has settled.
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
        Ok(())
    }

    pub fn set_mtime(&self, id: u64, mtime_ms: u64) -> FsResult<()> {
        let mut t = self.tree.lock().unwrap();
        t.nodes.get_mut(&id).ok_or(FsError::NotFound)?.mtime_ms = mtime_ms;
        Ok(())
    }

    pub fn mkdir(&self, dir: u64, name: &str) -> FsResult<u64> {
        if !valid_name(name) {
            return Err(FsError::Invalid);
        }
        let mut t = self.tree.lock().unwrap();
        if t.dir(dir)?.contains_key(name) {
            return Err(FsError::Exists);
        }
        let id = t.insert(dir, name, now_ms(), Kind::Dir(BTreeMap::new()));
        if !is_local_only(name) {
            let key = format!("{}/", t.key_of(id));
            t.ops.push_back(NsOp::Mkdir { key });
        }
        Ok(id)
    }

    pub fn remove(&self, dir: u64, name: &str) -> FsResult<()> {
        let mut t = self.tree.lock().unwrap();
        let id = *t.dir(dir)?.get(name).ok_or(FsError::NotFound)?;
        let key = t.key_of(id);
        let op = match &t.nodes.get(&id).ok_or(FsError::NotFound)?.kind {
            Kind::Dir(children) => {
                if !children.is_empty() {
                    return Err(FsError::NotEmpty);
                }
                (!is_local_only(name)).then(|| NsOp::Delete { key: format!("{key}/") })
            }
            // Only objects the cloud has (or is receiving) need deleting there.
            Kind::File(f) => (!f.local_only && (f.remote.is_some() || f.uploading)).then_some(NsOp::Delete { key }),
        };
        drop_local(t.detach(id));
        t.ops.extend(op);
        self.readahead.lock().unwrap().remove(&id);
        Ok(())
    }

    pub fn rename(&self, from_dir: u64, from_name: &str, to_dir: u64, to_name: &str) -> FsResult<()> {
        if !valid_name(to_name) {
            return Err(FsError::Invalid);
        }
        let mut t = self.tree.lock().unwrap();
        let id = *t.dir(from_dir)?.get(from_name).ok_or(FsError::NotFound)?;
        t.dir(to_dir)?;
        if from_dir == to_dir && from_name == to_name {
            return Ok(());
        }
        let old_key = t.key_of(id);
        let is_dir = matches!(t.nodes[&id].kind, Kind::Dir(_));
        // Replace whatever is at the destination.
        if let Some(target) = t.dir(to_dir)?.get(to_name).copied() {
            if let Kind::Dir(children) = &t.nodes[&target].kind {
                if !children.is_empty() {
                    return Err(FsError::NotEmpty);
                }
            }
            drop_local(t.detach(target));
        }
        t.dir_mut(from_dir)?.remove(from_name);
        t.touch(from_dir);
        t.dir_mut(to_dir)?.insert(to_name.to_string(), id);
        t.touch(to_dir);
        let was_local_only = is_local_only(from_name);
        let now_local_only = is_local_only(to_name);
        {
            let node = t.nodes.get_mut(&id).ok_or(FsError::NotFound)?;
            node.parent = to_dir;
            node.name = to_name.to_string();
            if let Kind::File(f) = &mut node.kind {
                f.local_only = now_local_only;
                if was_local_only && !now_local_only {
                    // A temp file renamed into place: it has to be uploaded now.
                    if let Some(l) = &mut f.local {
                        l.gen += 1;
                        l.last_write = Instant::now();
                    }
                }
            }
        }
        let new_key = t.key_of(id);
        let on_cloud = is_dir || matches!(&t.nodes[&id].kind, Kind::File(f) if f.remote.is_some() || f.uploading);
        if on_cloud && !was_local_only {
            if now_local_only {
                t.ops.push_back(NsOp::Delete { key: if is_dir { format!("{old_key}/") } else { old_key } });
            } else {
                t.ops.push_back(NsOp::Rename { from: old_key, to: new_key });
            }
        }
        Ok(())
    }

    // ------------------------------------------------------------ uploading

    pub fn pending(&self) -> Pending {
        let t = self.tree.lock().unwrap();
        let mut p = Pending { ops: t.ops.len(), ..Default::default() };
        for n in t.nodes.values() {
            if let Kind::File(f) = &n.kind {
                if !f.local_only && (f.local.is_some() || f.uploading) {
                    p.files += 1;
                    p.bytes += f.size;
                }
            }
        }
        p.uploading = self.uploading.lock().unwrap().clone();
        p.last_error = self.last_error.lock().unwrap().clone();
        p
    }

    /// One pass of the uploader: replay queued namespace changes in order,
    /// then upload every file that has been quiet long enough (or all of
    /// them with `force`). Returns true if it did any work.
    pub async fn sync_once(self: &Arc<Self>, force: bool) -> bool {
        let mut worked = false;
        loop {
            let op = {
                let mut t = self.tree.lock().unwrap();
                let op = t.ops.front().cloned();
                t.busy = op.is_some();
                op
            };
            let Some(op) = op else { break };
            let result = match &op {
                NsOp::Delete { key } => self.client.delete(&self.name, key).await,
                NsOp::Rename { from, to } => self.client.rename(&self.name, from, to).await.map(|_| ()),
                NsOp::Mkdir { key } => self.client.put_bytes(&self.name, key, Bytes::new()).await.map(|_| ()),
            };
            let mut t = self.tree.lock().unwrap();
            t.busy = false;
            match result {
                Ok(()) => {
                    // The queue only grows at the back, so the front is still `op`.
                    t.ops.pop_front();
                    *self.last_error.lock().unwrap() = None;
                    worked = true;
                }
                Err(e) => {
                    *self.last_error.lock().unwrap() = Some(format!("{e:#}"));
                    return worked;
                }
            }
        }
        loop {
            let next = {
                let t = self.tree.lock().unwrap();
                if !t.ops.is_empty() {
                    None
                } else {
                    t.nodes.iter().find_map(|(id, n)| match &n.kind {
                        Kind::File(f) if !f.local_only && !f.uploading => {
                            f.local.as_ref().filter(|l| force || l.last_write.elapsed() >= UPLOAD_QUIET).map(|_| *id)
                        }
                        _ => None,
                    })
                }
            };
            let Some(id) = next else { break };
            if self.upload(id).await.is_err() {
                break;
            }
            worked = true;
        }
        worked
    }

    /// Send a journaled file to the cloud as shards, moving only what the
    /// cloud does not already hold.
    ///
    /// The new content is planned as a patch over the version underneath:
    /// segments no write touched are kept by reference, and only the runs
    /// around the writes are read (journal over base) and cut into shards.
    /// Of those, only the ones the cloud lacks are sent. Then the manifest is
    /// committed. A small write into a 50 GB file moves a shard or two.
    async fn upload_sharded(self: &Arc<Self>, key: &str, size: u64, snap: &Arc<ReadSnap>, mtime_ms: u64) -> anyhow::Result<Committed> {
        let ReadSnap::Local { extents, base, base_len, .. } = &**snap else {
            anyhow::bail!("{key} has no local changes to upload");
        };
        let whole = |size: u64| if size == 0 { Vec::new() } else { vec![Patch::Run { start: 0, end: size }] };
        let pieces = match base {
            // Everything was rewritten locally: the old version does not matter.
            Some((remote, base_key)) if size > 0 && !extents.covers(0, size) => {
                match self.client.manifest(&self.name, base_key, Some(&remote.version)).await? {
                    Some(m) => {
                        let dirty: Vec<(u64, u64)> = extents.segments(0, size).into_iter().filter(|s| s.2).map(|s| (s.0, s.1)).collect();
                        patch::plan(&m.segments, (*base_len).min(remote.size), size, &dirty)
                    }
                    // Whole-object content from before shards: read it all.
                    None => whole(size),
                }
            }
            _ => whole(size),
        };

        // Pass 1: cut every run into shards, keeping only (offset, length, hash).
        let mut manifest = Manifest { size, segments: Vec::with_capacity(pieces.len()) };
        let mut fresh: Vec<(u64, u64, String)> = Vec::new();
        for piece in pieces {
            match piece {
                Patch::Keep(seg) => manifest.segments.push(seg),
                Patch::Run { start, end } => {
                    for (offset, n, hash) in self.cut_run(snap, start, end).await? {
                        manifest.segments.push(Segment::Shard { h: hash.clone(), n });
                        fresh.push((offset, n, hash));
                    }
                }
            }
        }
        manifest.coalesce();

        // Ask once which of them the cloud lacks.
        let hashes: Vec<String> = fresh.iter().map(|s| s.2.clone()).collect::<HashSet<_>>().into_iter().collect();
        let mut missing: HashSet<String> = self.client.missing_shards(&hashes).await?.into_iter().collect();
        let skipped: u64 = fresh.iter().filter(|s| !missing.contains(&s.2)).map(|s| s.1).sum();
        Stats::add(&self.stats.bytes_skipped, skipped);

        // Where each shard of the new file starts, to read any of them back.
        let mut at: HashMap<String, (u64, u64)> = HashMap::new();
        let mut pos = 0;
        for seg in &manifest.segments {
            if let Segment::Shard { h, n } = seg {
                at.entry(h.clone()).or_insert((pos, *n));
            }
            pos += seg.len();
        }

        for _attempt in 0..2 {
            // Pass 2: read the missing shards again and send them, a few at a time.
            let todo: Vec<(String, u64, u64)> = missing.drain().filter_map(|h| at.get(&h).map(|&(offset, n)| (h, offset, n))).collect();
            let mut sends = futures_util::stream::iter(todo)
                .map(|(hash, offset, n)| {
                    let (this, snap) = (self.clone(), snap.clone());
                    async move {
                        let data = this.read_snap(&snap, offset, n).await.map_err(|e| anyhow::anyhow!("read {offset}+{n}: {e:?}"))?;
                        if shard::sha256_hex(&data) != hash {
                            anyhow::bail!("the file changed while it was uploading");
                        }
                        this.client.put_shard(&hash, Bytes::from(data)).await
                    }
                })
                .buffer_unordered(UPLOAD_PARALLELISM);
            while let Some(sent) = sends.next().await {
                sent?;
            }
            drop(sends);
            match self.client.commit_manifest(&self.name, key, &manifest, Some(mtime_ms)).await? {
                Ok(committed) => return Ok(committed),
                // A shard the cloud held was cleaned up in between: send it too.
                Err(gone) => missing = gone.into_iter().collect(),
            }
        }
        anyhow::bail!("{key}: the cloud keeps reporting missing shards")
    }

    /// Cut bytes `start..end` of a snapshot into shards and return their
    /// (offset in the file, length, hash). The bytes are streamed through the
    /// chunker; at most one shard is in memory.
    async fn cut_run(self: &Arc<Self>, snap: &Arc<ReadSnap>, start: u64, end: u64) -> anyhow::Result<Vec<(u64, u64, String)>> {
        // Short enough to be a single shard whatever the content.
        if end - start <= shard::MIN_SHARD as u64 {
            let data = self.read_snap(snap, start, end - start).await.map_err(|e| anyhow::anyhow!("read {start}..{end}: {e:?}"))?;
            return Ok(vec![(start, end - start, shard::sha256_hex(&data))]);
        }
        // The chunker reads synchronously; reads from the journal and the
        // cloud are async. A bounded channel joins the two.
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
        let cut = tokio::task::spawn_blocking(move || {
            shard::chunks(ChannelReader { rx, buf: Vec::new(), pos: 0 })
                .map(|c| c.map(|c| (start + c.offset, c.data.len() as u64, c.hash)))
                .collect::<std::io::Result<Vec<_>>>()
        })
        .await;
        feeder.abort();
        let shards = cut??;
        let total: u64 = shards.iter().map(|s| s.1).sum();
        if total != end - start {
            anyhow::bail!("cut {total} bytes of a {} byte run", end - start);
        }
        Ok(shards)
    }

    /// Publish one journaled file as a new version.
    async fn upload(self: &Arc<Self>, id: u64) -> FsResult<()> {
        let (key, size, gen, mtime_ms, snap) = {
            let mut t = self.tree.lock().unwrap();
            let key = t.key_of(id);
            let mtime_ms = t.nodes.get(&id).ok_or(FsError::NotFound)?.mtime_ms;
            let f = t.file_mut(id)?;
            let Some(l) = &f.local else { return Ok(()) };
            let snap = ReadSnap::Local { file: l.file.clone(), extents: l.extents.clone(), base: l.base.clone(), base_len: l.base_len };
            let out = (key, f.size, l.gen, mtime_ms, snap);
            f.uploading = true;
            t.busy = true;
            out
        };
        *self.uploading.lock().unwrap() = Some(key.clone());

        let snap = Arc::new(snap);
        let result = self.upload_sharded(&key, size, &snap, mtime_ms).await;
        *self.uploading.lock().unwrap() = None;

        let (version, seed) = {
            let mut t = self.tree.lock().unwrap();
            t.busy = false;
            let committed = match result {
                Ok(c) => c,
                Err(e) => {
                    *self.last_error.lock().unwrap() = Some(format!("upload {key}: {e:#}"));
                    if let Ok(f) = t.file_mut(id) {
                        f.uploading = false;
                        if let Some(l) = &mut f.local {
                            l.last_write = Instant::now(); // try again after a pause
                        }
                    }
                    return Err(FsError::Io(format!("{e:#}")));
                }
            };
            *self.last_error.lock().unwrap() = None;
            let version: Arc<str> = committed.version.as_str().into();
            let mut seed: Option<(Arc<File>, PathBuf)> = None;
            if let Ok(f) = t.file_mut(id) {
                f.uploading = false;
                f.remote = Some(Remote { version: version.clone(), size });
                if f.local.as_ref().map(|l| l.gen) == Some(gen) {
                    // Nothing changed while uploading: the journal copy is done.
                    if let Some(l) = f.local.take() {
                        let fully_local = l.extents.covers(0, size);
                        if fully_local && size <= self.cache.cap() / 4 {
                            seed = Some((l.file, l.path));
                        } else {
                            let _ = std::fs::remove_file(&l.path);
                        }
                    }
                }
            }
            (version, seed)
        };
        // Keep the freshly written bytes warm: move them into the block cache
        // under the new version instead of downloading them back later.
        if let Some((file, path)) = seed {
            let cache = self.cache.clone();
            let _ = tokio::task::spawn_blocking(move || {
                let mut buf = vec![0u8; BLOCK_SIZE as usize];
                let mut idx = 0;
                while idx * BLOCK_SIZE < size {
                    let n = (size - idx * BLOCK_SIZE).min(BLOCK_SIZE) as usize;
                    if file.read_exact_at(&mut buf[..n], idx * BLOCK_SIZE).is_err() || cache.put(&version, idx, &buf[..n]).is_err() {
                        break;
                    }
                    idx += 1;
                }
                let _ = std::fs::remove_file(&path);
            })
            .await;
        }
        Ok(())
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

    /// Background loop: upload settled writes and replay namespace changes.
    pub async fn run_uploader(self: Arc<Self>) {
        while !self.stopped.load(Ordering::Relaxed) {
            if !self.sync_once(false).await {
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        }
    }

    /// Background loop: long-poll the cloud for commits and fold them in, so
    /// changes made elsewhere show up in the mount within a second or two.
    pub async fn run_poller(self: Arc<Self>) {
        while !self.stopped.load(Ordering::Relaxed) {
            let known = self.seq();
            match self.client.wait_changes(&self.name, known, 20).await {
                Ok(seq) if seq != known => {
                    match self.refresh().await {
                        // Local changes still uploading; look again shortly.
                        Ok(false) => tokio::time::sleep(Duration::from_millis(500)).await,
                        Ok(true) => {}
                        Err(_) => tokio::time::sleep(Duration::from_secs(2)).await,
                    }
                }
                Ok(_) => {}
                Err(_) => tokio::time::sleep(Duration::from_secs(2)).await,
            }
        }
    }

    pub fn stop(&self) {
        self.stopped.store(true, Ordering::Relaxed);
        let _ = std::fs::remove_dir_all(&self.journal_dir);
    }
}
