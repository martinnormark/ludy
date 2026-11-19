//! On-disk state of the simulated cloud.
//!
//! ```text
//! <data>/drives/<drive>.json        catalog: key -> version history (last = current)
//! <data>/manifests/<id>.json        ordered segments of one version's content
//! <data>/shards/<hh>/<sha256>       shard bytes, named by their hash, stored once
//! <data>/blobs/<id>                 whole-object content from before shards (read only)
//! <data>/tmp/                       uploads in flight
//! ```
//!
//! A version never changes after it is committed. Its content is a manifest:
//! a list of shards cut by content-defined chunking. Forks, renames and
//! restores only write catalog entries. A manifest lives while any catalog
//! version (or an upload in flight) refers to it, and a shard lives while
//! any manifest does.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::sync::{watch, Mutex, RwLock};

use crate::shard::{self, Manifest, Segment};
use crate::{now_ms, synth};

/// How many versions of one key are kept (the oldest are dropped first).
pub const MAX_HISTORY: usize = 25;

#[derive(Debug)]
pub enum StoreError {
    NoSuchDrive,
    NoSuchKey,
    NoSuchVersion,
    DriveExists,
    BadName(String),
    Precondition,
    /// A manifest names shards the store does not hold.
    MissingShards(Vec<String>),
    BadManifest(String),
    BadShard(String),
    Io(io::Error),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreError::NoSuchDrive => write!(f, "no such drive"),
            StoreError::NoSuchKey => write!(f, "no such key"),
            StoreError::NoSuchVersion => write!(f, "no such version"),
            StoreError::DriveExists => write!(f, "drive already exists"),
            StoreError::BadName(n) => write!(f, "bad name: {n:?}"),
            StoreError::Precondition => write!(f, "version precondition failed"),
            StoreError::MissingShards(m) => write!(f, "{} shard(s) missing", m.len()),
            StoreError::BadManifest(m) => write!(f, "bad manifest: {m}"),
            StoreError::BadShard(m) => write!(f, "bad shard: {m}"),
            StoreError::Io(e) => write!(f, "io: {e}"),
        }
    }
}

impl From<io::Error> for StoreError {
    fn from(e: io::Error) -> Self {
        StoreError::Io(e)
    }
}

type Result<T> = std::result::Result<T, StoreError>;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Content {
    /// Shards listed in `<data>/manifests/<id>.json`.
    Manifest { id: String },
    /// Bytes generated from the offset; nothing on disk.
    Synthetic { seed: u64 },
    /// Whole-object bytes in `<data>/blobs/<id>`, written before shards existed.
    Blob { id: String },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Version {
    pub id: String,
    pub size: u64,
    pub mtime_ms: u64,
    /// Drive sequence number of the commit that created this version.
    pub seq: u64,
    pub content: Content,
    /// The version this one replaced, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Share {
    pub email: String,
    pub role: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct DriveState {
    created_ms: u64,
    seq: u64,
    #[serde(default)]
    forked_from: Option<String>,
    #[serde(default)]
    shares: Vec<Share>,
    objects: BTreeMap<String, Vec<Version>>,
}

struct Drive {
    state: Mutex<DriveState>,
    seq_tx: watch::Sender<u64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DriveInfo {
    pub name: String,
    pub created_ms: u64,
    pub seq: u64,
    pub objects: u64,
    /// Logical size of all current versions.
    pub bytes: u64,
    /// The part of `bytes` that is synthetic and takes no server disk.
    pub synthetic_bytes: u64,
    pub forked_from: Option<String>,
    pub shares: Vec<Share>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ObjectInfo {
    pub key: String,
    pub size: u64,
    pub mtime_ms: u64,
    pub version: String,
    pub synthetic: bool,
    pub versions: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Listing {
    pub drive: String,
    pub seq: u64,
    pub objects: Vec<ObjectInfo>,
    pub prefixes: Vec<String>,
}

struct ShardEntry {
    len: u64,
    /// How many times live manifests list this shard.
    refs: u32,
}

/// A manifest with the start offset of each segment, for range lookups.
pub struct Loaded {
    pub manifest: Manifest,
    starts: Vec<u64>,
    synthetic: u64,
}

impl Loaded {
    fn new(manifest: Manifest) -> Loaded {
        let mut starts = Vec::with_capacity(manifest.segments.len());
        let mut pos = 0;
        for s in &manifest.segments {
            starts.push(pos);
            pos += s.len();
        }
        Loaded { synthetic: manifest.synthetic_bytes(), manifest, starts }
    }
}

struct ManifestEntry {
    loaded: Arc<Loaded>,
    /// Catalog versions pointing here, plus uploads between storing the
    /// manifest and committing it.
    refs: u32,
}

/// Everything the store knows about shards and manifests. One lock covers
/// "do these shards exist", "count a reference" and "delete at zero", so a
/// commit can never race a cleanup. Lock order: a drive's state first, then
/// this.
#[derive(Default)]
struct Index {
    shards: HashMap<String, ShardEntry>,
    manifests: HashMap<String, ManifestEntry>,
    shard_bytes: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ShardStats {
    pub shards: u64,
    /// Bytes of shard files on the server's disk.
    pub shard_bytes: u64,
    pub manifests: u64,
}

/// What storing an uploaded body produced.
pub struct Ingested {
    pub size: u64,
    pub content: Content,
    pub new_shards: u64,
    pub new_bytes: u64,
}

pub struct Store {
    root: PathBuf,
    drives: RwLock<HashMap<String, Arc<Drive>>>,
    counter: AtomicU64,
    index: StdMutex<Index>,
}

fn valid_drive_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && !name.starts_with(['_', '.'])
        && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ' '))
}

fn valid_key(key: &str) -> bool {
    !key.is_empty() && !key.starts_with('/') && !key.contains("//") && key.len() <= 1024
}

impl Store {
    pub fn open(root: &Path) -> io::Result<Store> {
        for d in ["drives", "blobs", "tmp", "shards", "manifests"] {
            std::fs::create_dir_all(root.join(d))?;
        }
        // Uploads that were in flight when the server stopped are garbage.
        for e in std::fs::read_dir(root.join("tmp"))?.flatten() {
            let _ = std::fs::remove_file(e.path());
        }
        let mut drives = HashMap::new();
        for e in std::fs::read_dir(root.join("drives"))?.flatten() {
            let path = e.path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let Some(name) = path.file_stem().and_then(|s| s.to_str()).map(str::to_string) else { continue };
            let state: DriveState = serde_json::from_slice(&std::fs::read(&path)?)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{}: {e}", path.display())))?;
            let (seq_tx, _) = watch::channel(state.seq);
            drives.insert(name, Arc::new(Drive { state: Mutex::new(state), seq_tx }));
        }
        let index = load_index(root, drives.values().map(|d: &Arc<Drive>| d.state.try_lock().expect("no other user yet")))?;
        Ok(Store { root: root.to_path_buf(), drives: RwLock::new(drives), counter: AtomicU64::new(0), index: StdMutex::new(index) })
    }

    fn new_id(&self) -> String {
        format!("{:012x}{:04x}", now_ms(), self.counter.fetch_add(1, Ordering::Relaxed) & 0xffff)
    }

    fn blob_path(&self, id: &str) -> PathBuf {
        self.root.join("blobs").join(id)
    }

    // ------------------------------------------------- shards and manifests

    /// The hashes in `hashes` that the store does not hold.
    pub fn missing_shards(&self, hashes: &[String]) -> Vec<String> {
        let index = self.index.lock().unwrap();
        let mut seen = BTreeSet::new();
        hashes.iter().filter(|h| !index.shards.contains_key(*h) && seen.insert(*h)).cloned().collect()
    }

    /// Store one shard. The bytes must hash to `hash`. Returns false when
    /// the store already had it.
    pub fn put_shard(&self, hash: &str, data: &[u8]) -> Result<bool> {
        if !shard::valid_hash(hash) {
            return Err(StoreError::BadShard(format!("{hash:?} is not a SHA-256 in lowercase hex")));
        }
        if data.is_empty() || data.len() > shard::MAX_SHARD as usize {
            return Err(StoreError::BadShard(format!("{} bytes is outside the allowed shard size", data.len())));
        }
        let actual = shard::sha256_hex(data);
        if actual != hash {
            return Err(StoreError::BadShard(format!("content hashes to {actual}, not {hash}")));
        }
        let mut index = self.index.lock().unwrap();
        if index.shards.contains_key(hash) {
            return Ok(false);
        }
        let path = shard_path(&self.root, hash);
        std::fs::create_dir_all(path.parent().expect("shard path has a parent"))?;
        let tmp = self.root.join("tmp").join(format!("{hash}.{}", self.new_id()));
        std::fs::write(&tmp, data)?;
        std::fs::rename(&tmp, &path)?;
        index.shards.insert(hash.to_string(), ShardEntry { len: data.len() as u64, refs: 0 });
        index.shard_bytes += data.len() as u64;
        Ok(true)
    }

    /// Store a manifest whose shards are all present, and return its id.
    /// The caller holds one reference to it until it calls [`Store::release`]
    /// (committing a version takes a reference of its own).
    pub fn put_manifest(&self, manifest: &Manifest) -> Result<String> {
        manifest.validate().map_err(StoreError::BadManifest)?;
        let id = manifest.id();
        let mut index = self.index.lock().unwrap();
        if let Some(e) = index.manifests.get_mut(&id) {
            e.refs += 1;
            return Ok(id);
        }
        let mut missing = BTreeSet::new();
        for s in &manifest.segments {
            if let Segment::Shard { h, n } = s {
                match index.shards.get(h) {
                    None => {
                        missing.insert(h.clone());
                    }
                    Some(e) if e.len != *n => {
                        return Err(StoreError::BadManifest(format!("shard {h} is {} bytes, not {n}", e.len)));
                    }
                    Some(_) => {}
                }
            }
        }
        if !missing.is_empty() {
            return Err(StoreError::MissingShards(missing.into_iter().collect()));
        }
        let tmp = self.root.join("tmp").join(format!("{id}.{}.json", self.new_id()));
        std::fs::write(&tmp, serde_json::to_vec(manifest).expect("manifest serializes"))?;
        std::fs::rename(&tmp, manifest_path(&self.root, &id))?;
        for s in &manifest.segments {
            if let Segment::Shard { h, .. } = s {
                index.shards.get_mut(h).expect("checked above").refs += 1;
            }
        }
        index.manifests.insert(id.clone(), ManifestEntry { loaded: Arc::new(Loaded::new(manifest.clone())), refs: 1 });
        Ok(id)
    }

    /// Count one more reference to `content`. False if its manifest is gone.
    fn retain(&self, content: &Content) -> bool {
        let Content::Manifest { id } = content else { return true };
        match self.index.lock().unwrap().manifests.get_mut(id) {
            Some(e) => {
                e.refs += 1;
                true
            }
            None => false,
        }
    }

    /// Give up one reference to `content`. A manifest nobody refers to is
    /// deleted, and so is every shard only it listed.
    pub fn release(&self, content: &Content) {
        let Content::Manifest { id } = content else { return };
        let mut index = self.index.lock().unwrap();
        let Some(e) = index.manifests.get_mut(id) else { return };
        e.refs = e.refs.saturating_sub(1);
        if e.refs > 0 {
            return;
        }
        let dead = index.manifests.remove(id).expect("present above");
        let _ = std::fs::remove_file(manifest_path(&self.root, id));
        for s in &dead.loaded.manifest.segments {
            let Segment::Shard { h, .. } = s else { continue };
            let Some(sh) = index.shards.get_mut(h) else { continue };
            sh.refs = sh.refs.saturating_sub(1);
            if sh.refs == 0 {
                let len = sh.len;
                index.shards.remove(h);
                index.shard_bytes -= len;
                let _ = std::fs::remove_file(shard_path(&self.root, h));
            }
        }
    }

    fn loaded(&self, id: &str) -> Option<Arc<Loaded>> {
        self.index.lock().unwrap().manifests.get(id).map(|e| e.loaded.clone())
    }

    /// The manifest behind a version, with its id. A synthetic object is one
    /// synthetic segment. `None` for whole-object blobs, which have none.
    pub fn manifest_of(&self, v: &Version) -> Option<(Option<String>, Manifest)> {
        match &v.content {
            Content::Manifest { id } => self.loaded(id).map(|l| (Some(id.clone()), l.manifest.clone())),
            Content::Synthetic { seed } => {
                let segments = if v.size == 0 { vec![] } else { vec![Segment::Synth { seed: *seed, off: 0, n: v.size }] };
                Some((None, Manifest { size: v.size, segments }))
            }
            Content::Blob { .. } => None,
        }
    }

    /// How many of a version's bytes are generated rather than stored.
    pub fn synthetic_bytes(&self, v: &Version) -> u64 {
        match &v.content {
            Content::Synthetic { .. } => v.size,
            Content::Manifest { id } => self.loaded(id).map(|l| l.synthetic).unwrap_or(0),
            Content::Blob { .. } => 0,
        }
    }

    /// Delete shards no manifest lists: uploads that were abandoned before
    /// their commit. An upload in flight loses its shards too; its commit is
    /// then refused with the list of what is missing, and the client sends
    /// them again. Returns how many were removed.
    pub fn sweep_orphans(&self) -> usize {
        let mut index = self.index.lock().unwrap();
        let orphans: Vec<String> = index.shards.iter().filter(|(_, e)| e.refs == 0).map(|(h, _)| h.clone()).collect();
        for h in &orphans {
            if let Some(e) = index.shards.remove(h) {
                index.shard_bytes -= e.len;
            }
            let _ = std::fs::remove_file(shard_path(&self.root, h));
        }
        orphans.len()
    }

    pub fn shard_stats(&self) -> ShardStats {
        let index = self.index.lock().unwrap();
        ShardStats { shards: index.shards.len() as u64, shard_bytes: index.shard_bytes, manifests: index.manifests.len() as u64 }
    }

    /// Cut the file at `path` into shards and store them with a manifest.
    /// Blocking: call from `spawn_blocking`. The caller holds a reference to
    /// the returned content, as with [`Store::put_manifest`].
    pub fn ingest(&self, path: &Path) -> Result<Ingested> {
        // A shard that existed when we looked can be cleaned up before the
        // manifest is stored; storing it again is all it takes.
        for _ in 0..3 {
            let mut manifest = Manifest::default();
            let (mut new_shards, mut new_bytes) = (0, 0);
            for chunk in shard::chunks(std::fs::File::open(path)?) {
                let chunk = chunk?;
                let n = chunk.data.len() as u64;
                if self.put_shard(&chunk.hash, &chunk.data)? {
                    new_shards += 1;
                    new_bytes += n;
                }
                manifest.segments.push(Segment::Shard { h: chunk.hash, n });
                manifest.size += n;
            }
            match self.put_manifest(&manifest) {
                Ok(id) => return Ok(Ingested { size: manifest.size, content: Content::Manifest { id }, new_shards, new_bytes }),
                Err(StoreError::MissingShards(_)) => continue,
                Err(e) => return Err(e),
            }
        }
        Err(StoreError::Io(io::Error::other("shards kept disappearing during ingest")))
    }

    async fn drive(&self, name: &str) -> Result<Arc<Drive>> {
        self.drives.read().await.get(name).cloned().ok_or(StoreError::NoSuchDrive)
    }

    pub async fn check_drive(&self, name: &str) -> Result<()> {
        self.drive(name).await.map(|_| ())
    }

    async fn persist(&self, name: &str, state: &DriveState) -> io::Result<()> {
        let path = self.root.join("drives").join(format!("{name}.json"));
        let tmp = self.root.join("tmp").join(format!("{name}.{}.json", self.new_id()));
        tokio::fs::write(&tmp, serde_json::to_vec(state).expect("catalog serializes")).await?;
        tokio::fs::rename(&tmp, &path).await
    }

    /// Bump the sequence number, write the catalog and wake long-pollers.
    async fn commit_state(&self, name: &str, drive: &Drive, state: &mut DriveState) -> io::Result<()> {
        state.seq += 1;
        self.persist(name, state).await?;
        let _ = drive.seq_tx.send(state.seq);
        Ok(())
    }

    // ------------------------------------------------------------ drives

    pub async fn drives(&self) -> Vec<DriveInfo> {
        let drives: Vec<(String, Arc<Drive>)> = self.drives.read().await.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        let mut out = Vec::with_capacity(drives.len());
        for (name, d) in drives {
            let s = d.state.lock().await;
            let mut info = DriveInfo {
                name,
                created_ms: s.created_ms,
                seq: s.seq,
                objects: 0,
                bytes: 0,
                synthetic_bytes: 0,
                forked_from: s.forked_from.clone(),
                shares: s.shares.clone(),
            };
            for v in s.objects.values().filter_map(|h| h.last()) {
                info.objects += 1;
                info.bytes += v.size;
                info.synthetic_bytes += self.synthetic_bytes(v);
            }
            out.push(info);
        }
        out.sort_by(|a, b| a.created_ms.cmp(&b.created_ms).then(a.name.cmp(&b.name)));
        out
    }

    pub async fn create_drive(&self, name: &str) -> Result<()> {
        if !valid_drive_name(name) {
            return Err(StoreError::BadName(name.to_string()));
        }
        let mut drives = self.drives.write().await;
        if drives.contains_key(name) {
            return Err(StoreError::DriveExists);
        }
        let state = DriveState { created_ms: now_ms(), seq: 1, ..Default::default() };
        self.persist(name, &state).await?;
        let (seq_tx, _) = watch::channel(state.seq);
        drives.insert(name.to_string(), Arc::new(Drive { state: Mutex::new(state), seq_tx }));
        Ok(())
    }

    pub async fn delete_drive(&self, name: &str) -> Result<()> {
        let drive = self.drive(name).await?;
        let state = drive.state.lock().await;
        let mut drives = self.drives.write().await;
        if drives.remove(name).is_none() {
            return Err(StoreError::NoSuchDrive);
        }
        let versions: Vec<Version> = state.objects.values().flatten().cloned().collect();
        // Catalog first, content after (see `commit`).
        let _ = tokio::fs::remove_file(self.root.join("drives").join(format!("{name}.json"))).await;
        self.release_all(&versions);
        drop(drives);
        drop(state);
        self.gc(blob_ids(versions.iter())).await;
        Ok(())
    }

    /// A fork is a new catalog pointing at the same versions. No content moves.
    pub async fn fork_drive(&self, from: &str, to: &str) -> Result<()> {
        if !valid_drive_name(to) {
            return Err(StoreError::BadName(to.to_string()));
        }
        let src = self.drive(from).await?;
        // Hold the source until the fork's references are counted, so nothing
        // it shares can be cleaned up in between.
        let src_state = src.state.lock().await;
        let mut state = src_state.clone();
        state.created_ms = now_ms();
        state.seq = 1;
        state.forked_from = Some(from.to_string());
        state.shares.clear();
        let mut drives = self.drives.write().await;
        if drives.contains_key(to) {
            return Err(StoreError::DriveExists);
        }
        self.persist(to, &state).await?;
        for v in state.objects.values().flatten() {
            self.retain(&v.content);
        }
        let (seq_tx, _) = watch::channel(state.seq);
        drives.insert(to.to_string(), Arc::new(Drive { state: Mutex::new(state), seq_tx }));
        Ok(())
    }

    pub async fn rename_drive(&self, from: &str, to: &str) -> Result<()> {
        if !valid_drive_name(to) {
            return Err(StoreError::BadName(to.to_string()));
        }
        let mut drives = self.drives.write().await;
        if drives.contains_key(to) {
            return Err(StoreError::DriveExists);
        }
        let drive = drives.remove(from).ok_or(StoreError::NoSuchDrive)?;
        let dir = self.root.join("drives");
        tokio::fs::rename(dir.join(format!("{from}.json")), dir.join(format!("{to}.json"))).await?;
        drives.insert(to.to_string(), drive);
        Ok(())
    }

    pub async fn share_drive(&self, name: &str, email: &str, role: &str) -> Result<()> {
        if !email.contains('@') {
            return Err(StoreError::BadName(email.to_string()));
        }
        let role = if role.eq_ignore_ascii_case("edit") { "edit" } else { "view" };
        let drive = self.drive(name).await?;
        let mut s = drive.state.lock().await;
        s.shares.retain(|sh| sh.email != email);
        s.shares.push(Share { email: email.to_string(), role: role.to_string() });
        self.commit_state(name, &drive, &mut s).await?;
        Ok(())
    }

    pub async fn unshare_drive(&self, name: &str, email: &str) -> Result<()> {
        let drive = self.drive(name).await?;
        let mut s = drive.state.lock().await;
        s.shares.retain(|sh| sh.email != email);
        self.commit_state(name, &drive, &mut s).await?;
        Ok(())
    }

    /// Resolve once the drive's sequence number exceeds `since`, or at timeout.
    pub async fn wait_for_change(&self, name: &str, since: u64, timeout: Duration) -> Result<u64> {
        let drive = self.drive(name).await?;
        let mut rx = drive.seq_tx.subscribe();
        let wait = async {
            loop {
                let seq = *rx.borrow_and_update();
                if seq > since {
                    return seq;
                }
                if rx.changed().await.is_err() {
                    return seq;
                }
            }
        };
        match tokio::time::timeout(timeout, wait).await {
            Ok(seq) => Ok(seq),
            Err(_) => Ok(*drive.seq_tx.borrow()),
        }
    }

    // ------------------------------------------------------------ objects

    pub async fn list(&self, name: &str, prefix: &str, delimiter: Option<&str>) -> Result<Listing> {
        let drive = self.drive(name).await?;
        let s = drive.state.lock().await;
        let mut objects = Vec::new();
        let mut prefixes = BTreeSet::new();
        for (key, history) in s.objects.range(prefix.to_string()..) {
            if !key.starts_with(prefix) {
                break;
            }
            let Some(v) = history.last() else { continue };
            if let Some(d) = delimiter.filter(|d| !d.is_empty()) {
                if let Some(i) = key[prefix.len()..].find(d) {
                    prefixes.insert(key[..prefix.len() + i + d.len()].to_string());
                    continue;
                }
            }
            objects.push(ObjectInfo {
                key: key.clone(),
                size: v.size,
                mtime_ms: v.mtime_ms,
                version: v.id.clone(),
                synthetic: self.synthetic_bytes(v) > 0,
                versions: history.len(),
            });
        }
        Ok(Listing { drive: name.to_string(), seq: s.seq, objects, prefixes: prefixes.into_iter().collect() })
    }

    pub async fn head(&self, name: &str, key: &str, version: Option<&str>) -> Result<Version> {
        let drive = self.drive(name).await?;
        let s = drive.state.lock().await;
        let history = s.objects.get(key).ok_or(StoreError::NoSuchKey)?;
        match version {
            None => history.last().cloned().ok_or(StoreError::NoSuchKey),
            Some(id) => history.iter().find(|v| v.id == id).cloned().ok_or(StoreError::NoSuchVersion),
        }
    }

    pub async fn versions(&self, name: &str, key: &str) -> Result<Vec<Version>> {
        let drive = self.drive(name).await?;
        let s = drive.state.lock().await;
        s.objects.get(key).cloned().ok_or(StoreError::NoSuchKey)
    }

    /// Commit new content for `key`. With `if_match`, the commit only succeeds
    /// when the current version has that id (`"none"` = the key must not
    /// exist), so of two racing writers exactly one wins.
    pub async fn commit(
        &self,
        name: &str,
        key: &str,
        size: u64,
        content: Content,
        if_match: Option<&str>,
        mtime_ms: Option<u64>,
    ) -> Result<Version> {
        if !valid_key(key) {
            return Err(StoreError::BadName(key.to_string()));
        }
        let drive = self.drive(name).await?;
        let dropped;
        let version = {
            let mut s = drive.state.lock().await;
            let current = s.objects.get(key).and_then(|h| h.last()).map(|v| v.id.as_str());
            if let Some(want) = if_match {
                let ok = match current {
                    None => want == "none",
                    Some(id) => want == id,
                };
                if !ok {
                    return Err(StoreError::Precondition);
                }
            }
            let version = Version {
                id: self.new_id(),
                size,
                mtime_ms: mtime_ms.unwrap_or_else(now_ms),
                seq: s.seq + 1,
                content,
                parent: current.map(str::to_string),
            };
            if !self.retain(&version.content) {
                return Err(StoreError::NoSuchVersion);
            }
            let history = s.objects.entry(key.to_string()).or_default();
            history.push(version.clone());
            let excess = history.len().saturating_sub(MAX_HISTORY);
            dropped = history.drain(..excess).collect::<Vec<_>>();
            // The catalog goes to disk before any content is released: a
            // crash in between leaves unreferenced files (swept at the next
            // start), never a catalog that points at something deleted.
            self.commit_state(name, &drive, &mut s).await?;
            self.release_all(&dropped);
            version
        };
        self.gc(blob_ids(dropped.iter())).await;
        Ok(version)
    }

    /// Make an old version current again. Writes no content.
    pub async fn restore(&self, name: &str, key: &str, version_id: &str) -> Result<Version> {
        let old = self.head(name, key, Some(version_id)).await?;
        self.commit(name, key, old.size, old.content, None, None).await
    }

    /// Delete `key`; a key ending in `/` also deletes everything under it.
    pub async fn delete(&self, name: &str, key: &str) -> Result<usize> {
        let drive = self.drive(name).await?;
        let mut removed = Vec::new();
        {
            let mut s = drive.state.lock().await;
            let doomed: Vec<String> = if key.ends_with('/') {
                s.objects.range(key.to_string()..).map(|(k, _)| k).take_while(|k| k.starts_with(key)).cloned().collect()
            } else {
                s.objects.contains_key(key).then(|| key.to_string()).into_iter().collect()
            };
            if doomed.is_empty() {
                return Err(StoreError::NoSuchKey);
            }
            for k in &doomed {
                if let Some(h) = s.objects.remove(k) {
                    removed.extend(h);
                }
            }
            self.commit_state(name, &drive, &mut s).await?;
            self.release_all(&removed);
        }
        let n = removed.len();
        self.gc(blob_ids(removed.iter())).await;
        Ok(n)
    }

    /// Move `from` to `to`. If `from` names a folder (objects exist under
    /// `from/`), the whole subtree moves in the same commit.
    pub async fn rename(&self, name: &str, from: &str, to: &str) -> Result<usize> {
        let from = from.trim_end_matches('/');
        let to = to.trim_end_matches('/');
        if !valid_key(to) || from.is_empty() {
            return Err(StoreError::BadName(to.to_string()));
        }
        let drive = self.drive(name).await?;
        let mut replaced = Vec::new();
        let moved = {
            let mut s = drive.state.lock().await;
            let from_dir = format!("{from}/");
            let mut moves: Vec<(String, String)> = Vec::new();
            if s.objects.contains_key(from) {
                moves.push((from.to_string(), to.to_string()));
            }
            for k in s.objects.range(from_dir.clone()..).map(|(k, _)| k).take_while(|k| k.starts_with(&from_dir)) {
                moves.push((k.clone(), format!("{to}/{}", &k[from_dir.len()..])));
            }
            if moves.is_empty() {
                return Err(StoreError::NoSuchKey);
            }
            for (old, new) in &moves {
                let history = s.objects.remove(old).unwrap_or_default();
                if let Some(prev) = s.objects.insert(new.clone(), history) {
                    replaced.extend(prev);
                }
            }
            self.commit_state(name, &drive, &mut s).await?;
            self.release_all(&replaced);
            moves.len()
        };
        self.gc(blob_ids(replaced.iter())).await;
        Ok(moved)
    }

    // ------------------------------------------------------------ content

    pub async fn reader(&self, content: &Content) -> Result<Reader> {
        Ok(match content {
            Content::Manifest { id } => {
                let loaded = self.loaded(id).ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("manifest {id} is gone")))?;
                Reader::Manifest { root: self.root.clone(), loaded, open: None }
            }
            Content::Blob { id } => Reader::Blob(tokio::fs::File::open(self.blob_path(id)).await?),
            Content::Synthetic { seed } => Reader::Synthetic { seed: *seed },
        })
    }

    /// Start receiving an upload body into a temporary file.
    pub async fn begin_upload(&self) -> io::Result<Upload> {
        let tmp = self.root.join("tmp").join(self.new_id());
        Ok(Upload { file: tokio::fs::File::create(&tmp).await?, tmp })
    }

    /// Versions that left every catalog they were in: give up their content.
    fn release_all(&self, versions: &[Version]) {
        for v in versions {
            self.release(&v.content);
        }
    }


    /// Delete the blobs in `candidates` that no catalog refers to any more.
    async fn gc(&self, candidates: BTreeSet<String>) {
        if candidates.is_empty() {
            return;
        }
        let drives: Vec<Arc<Drive>> = self.drives.read().await.values().cloned().collect();
        let mut live = BTreeSet::new();
        for d in drives {
            let s = d.state.lock().await;
            for id in blob_ids(s.objects.values().flatten()) {
                if candidates.contains(&id) {
                    live.insert(id);
                }
            }
        }
        for id in candidates.difference(&live) {
            let _ = tokio::fs::remove_file(self.blob_path(id)).await;
        }
    }
}

fn blob_ids<'a>(versions: impl Iterator<Item = &'a Version>) -> BTreeSet<String> {
    versions
        .filter_map(|v| match &v.content {
            Content::Blob { id } => Some(id.clone()),
            Content::Synthetic { .. } | Content::Manifest { .. } => None,
        })
        .collect()
}

fn shard_path(root: &Path, hash: &str) -> PathBuf {
    root.join("shards").join(&hash[..2]).join(hash)
}

fn manifest_path(root: &Path, id: &str) -> PathBuf {
    root.join("manifests").join(format!("{id}.json"))
}

/// Rebuild the index from what is on disk: every shard file, and every
/// manifest a catalog version points at. Files nothing refers to (uploads
/// that never committed) are removed.
fn load_index<G>(root: &Path, catalogs: impl Iterator<Item = G>) -> io::Result<Index>
where
    G: std::ops::Deref<Target = DriveState>,
{
    let mut index = Index::default();
    for dir in std::fs::read_dir(root.join("shards"))?.flatten() {
        let Ok(files) = std::fs::read_dir(dir.path()) else { continue };
        for f in files.flatten() {
            let name = f.file_name().to_string_lossy().into_owned();
            if shard::valid_hash(&name) {
                let len = f.metadata()?.len();
                index.shard_bytes += len;
                index.shards.insert(name, ShardEntry { len, refs: 0 });
            } else {
                let _ = std::fs::remove_file(f.path());
            }
        }
    }
    for state in catalogs {
        for v in state.objects.values().flatten() {
            let Content::Manifest { id } = &v.content else { continue };
            if let Some(e) = index.manifests.get_mut(id) {
                e.refs += 1;
                continue;
            }
            let path = manifest_path(root, id);
            // A version whose manifest cannot be read is damage; say so and
            // keep serving everything else rather than refuse to start.
            let manifest: Manifest = match std::fs::read(&path).map_err(|e| e.to_string()).and_then(|raw| serde_json::from_slice(&raw).map_err(|e| e.to_string())) {
                Ok(m) => m,
                Err(e) => {
                    eprintln!("ludy-cloud: version {} has no readable manifest ({}: {e})", v.id, path.display());
                    continue;
                }
            };
            for s in &manifest.segments {
                if let Segment::Shard { h, .. } = s {
                    if let Some(sh) = index.shards.get_mut(h) {
                        sh.refs += 1;
                    }
                }
            }
            index.manifests.insert(id.clone(), ManifestEntry { loaded: Arc::new(Loaded::new(manifest)), refs: 1 });
        }
    }
    for f in std::fs::read_dir(root.join("manifests"))?.flatten() {
        let name = f.file_name().to_string_lossy().into_owned();
        if !name.strip_suffix(".json").is_some_and(|id| index.manifests.contains_key(id)) {
            let _ = std::fs::remove_file(f.path());
        }
    }
    let orphans: Vec<String> = index.shards.iter().filter(|(_, e)| e.refs == 0).map(|(h, _)| h.clone()).collect();
    for h in orphans {
        if let Some(e) = index.shards.remove(&h) {
            index.shard_bytes -= e.len;
        }
        let _ = std::fs::remove_file(shard_path(root, &h));
    }
    Ok(index)
}

pub enum Reader {
    Manifest {
        root: PathBuf,
        loaded: Arc<Loaded>,
        /// The shard file read last, kept open for the next sequential read.
        open: Option<(usize, tokio::fs::File)>,
    },
    Blob(tokio::fs::File),
    Synthetic { seed: u64 },
}

impl Reader {
    pub async fn read_at(&mut self, offset: u64, len: usize) -> io::Result<Vec<u8>> {
        match self {
            Reader::Manifest { root, loaded, open } => {
                let mut out = Vec::with_capacity(len);
                let end = offset + len as u64;
                let mut pos = offset;
                // The segment containing `pos`: the last one starting at or before it.
                let mut i = loaded.starts.partition_point(|&s| s <= pos).saturating_sub(1);
                while pos < end {
                    let seg = loaded.manifest.segments.get(i).ok_or(io::ErrorKind::UnexpectedEof)?;
                    let within = pos - loaded.starts[i];
                    let take = (seg.len() - within).min(end - pos) as usize;
                    match seg {
                        Segment::Synth { seed, off, .. } => out.extend_from_slice(&synth::bytes(*seed, off + within, take)),
                        Segment::Shard { h, .. } => {
                            if open.as_ref().map(|(at, _)| *at) != Some(i) {
                                *open = Some((i, tokio::fs::File::open(shard_path(root, h)).await?));
                            }
                            let file = &mut open.as_mut().expect("opened above").1;
                            let at = out.len();
                            out.resize(at + take, 0);
                            file.seek(io::SeekFrom::Start(within)).await?;
                            file.read_exact(&mut out[at..]).await?;
                        }
                    }
                    pos += take as u64;
                    i += 1;
                }
                Ok(out)
            }
            Reader::Blob(f) => {
                let mut buf = vec![0u8; len];
                f.seek(io::SeekFrom::Start(offset)).await?;
                f.read_exact(&mut buf).await?;
                Ok(buf)
            }
            Reader::Synthetic { seed } => Ok(synth::bytes(*seed, offset, len)),
        }
    }
}

/// An upload body on its way into a temporary file.
pub struct Upload {
    file: tokio::fs::File,
    tmp: PathBuf,
}

impl Upload {
    pub async fn write(&mut self, data: &[u8]) -> io::Result<()> {
        self.file.write_all(data).await
    }

    /// Close the file and return its path. The caller removes it.
    pub async fn finish(mut self) -> io::Result<PathBuf> {
        self.file.flush().await?;
        Ok(self.tmp)
    }
}
