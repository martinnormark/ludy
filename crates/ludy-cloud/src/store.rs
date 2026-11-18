//! On-disk state of the simulated cloud.
//!
//! ```text
//! <data>/drives/<drive>.json   catalog: key -> version history (last = current)
//! <data>/blobs/<id>            immutable content of one uploaded version
//! <data>/tmp/                  uploads in flight
//! ```
//!
//! A version never changes after it is committed, and its id is unique, so
//! forks and renames only copy catalog entries. A blob is deleted once no
//! catalog refers to it.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::sync::{watch, Mutex, RwLock};

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
    /// Bytes stored in `<data>/blobs/<id>`.
    Blob { id: String },
    /// Bytes generated from the offset; nothing on disk.
    Synthetic { seed: u64 },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Version {
    pub id: String,
    pub size: u64,
    pub mtime_ms: u64,
    /// Drive sequence number of the commit that created this version.
    pub seq: u64,
    pub content: Content,
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

pub struct Store {
    root: PathBuf,
    drives: RwLock<HashMap<String, Arc<Drive>>>,
    counter: AtomicU64,
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
        for d in ["drives", "blobs", "tmp"] {
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
        Ok(Store { root: root.to_path_buf(), drives: RwLock::new(drives), counter: AtomicU64::new(0) })
    }

    fn new_id(&self) -> String {
        format!("{:012x}{:04x}", now_ms(), self.counter.fetch_add(1, Ordering::Relaxed) & 0xffff)
    }

    fn blob_path(&self, id: &str) -> PathBuf {
        self.root.join("blobs").join(id)
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
                if matches!(v.content, Content::Synthetic { .. }) {
                    info.synthetic_bytes += v.size;
                }
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
        let drive = self.drives.write().await.remove(name).ok_or(StoreError::NoSuchDrive)?;
        let _ = tokio::fs::remove_file(self.root.join("drives").join(format!("{name}.json"))).await;
        let blobs = blob_ids(drive.state.lock().await.objects.values().flatten());
        self.gc(blobs).await;
        Ok(())
    }

    /// A fork is a new catalog pointing at the same versions. No content moves.
    pub async fn fork_drive(&self, from: &str, to: &str) -> Result<()> {
        if !valid_drive_name(to) {
            return Err(StoreError::BadName(to.to_string()));
        }
        let src = self.drive(from).await?;
        let mut state = src.state.lock().await.clone();
        state.created_ms = now_ms();
        state.seq = 1;
        state.forked_from = Some(from.to_string());
        state.shares.clear();
        let mut drives = self.drives.write().await;
        if drives.contains_key(to) {
            return Err(StoreError::DriveExists);
        }
        self.persist(to, &state).await?;
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
                synthetic: matches!(v.content, Content::Synthetic { .. }),
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
            let version = Version { id: self.new_id(), size, mtime_ms: mtime_ms.unwrap_or_else(now_ms), seq: s.seq + 1, content };
            let history = s.objects.entry(key.to_string()).or_default();
            history.push(version.clone());
            let excess = history.len().saturating_sub(MAX_HISTORY);
            dropped = blob_ids(history.drain(..excess).collect::<Vec<_>>().iter());
            self.commit_state(name, &drive, &mut s).await?;
            version
        };
        self.gc(dropped).await;
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
            moves.len()
        };
        self.gc(blob_ids(replaced.iter())).await;
        Ok(moved)
    }

    // ------------------------------------------------------------ content

    pub async fn reader(&self, content: &Content) -> Result<Reader> {
        Ok(match content {
            Content::Blob { id } => Reader::Blob(tokio::fs::File::open(self.blob_path(id)).await?),
            Content::Synthetic { seed } => Reader::Synthetic { seed: *seed },
        })
    }

    pub async fn begin_upload(&self) -> io::Result<Upload> {
        let id = self.new_id();
        let tmp = self.root.join("tmp").join(&id);
        Ok(Upload { file: tokio::fs::File::create(&tmp).await?, tmp, dest: self.blob_path(&id), id, size: 0 })
    }

    /// Drop content that was uploaded but never committed.
    pub async fn discard(&self, content: &Content) {
        if let Content::Blob { id } = content {
            let _ = tokio::fs::remove_file(self.blob_path(id)).await;
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
            Content::Synthetic { .. } => None,
        })
        .collect()
}

pub enum Reader {
    Blob(tokio::fs::File),
    Synthetic { seed: u64 },
}

impl Reader {
    pub async fn read_at(&mut self, offset: u64, len: usize) -> io::Result<Vec<u8>> {
        match self {
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

pub struct Upload {
    file: tokio::fs::File,
    tmp: PathBuf,
    dest: PathBuf,
    id: String,
    size: u64,
}

impl Upload {
    pub async fn write(&mut self, data: &[u8]) -> io::Result<()> {
        self.size += data.len() as u64;
        self.file.write_all(data).await
    }

    pub async fn finish(mut self) -> io::Result<(u64, Content)> {
        self.file.flush().await?;
        drop(self.file);
        tokio::fs::rename(&self.tmp, &self.dest).await?;
        Ok((self.size, Content::Blob { id: self.id }))
    }
}
