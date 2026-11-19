//! Disk cache of fixed-size blocks, keyed by `(version id, block index)`.
//!
//! A version's bytes never change, so a cached block is never stale. The
//! cache has a byte cap; when a new block would exceed it, the least recently
//! used blocks are evicted. Blocks of pinned versions are never evicted.
//!
//! Layout: `<dir>/<version id>/<block index>`.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use bytes::Bytes;

/// Blocks are 256 KiB: small enough that reading a header moves a header's
/// worth of data, large enough that sequential reads are not chatty.
pub const BLOCK_SIZE: u64 = 256 * 1024;

type Key = (Arc<str>, u64);

struct Entry {
    size: u64,
    tick: u64,
}

#[derive(Default)]
struct Index {
    entries: HashMap<Key, Entry>,
    /// tick -> key, oldest first.
    lru: BTreeMap<u64, Key>,
    per_version: HashMap<Arc<str>, u64>,
    pinned: HashSet<Arc<str>>,
    bytes: u64,
    tick: u64,
}

impl Index {
    fn touch(&mut self, key: &Key) -> bool {
        let Some(e) = self.entries.get_mut(key) else { return false };
        self.lru.remove(&e.tick);
        self.tick += 1;
        e.tick = self.tick;
        self.lru.insert(self.tick, key.clone());
        true
    }

    fn insert(&mut self, key: Key, size: u64) {
        self.remove(&key);
        self.tick += 1;
        self.lru.insert(self.tick, key.clone());
        *self.per_version.entry(key.0.clone()).or_default() += size;
        self.bytes += size;
        self.entries.insert(key, Entry { size, tick: self.tick });
    }

    fn remove(&mut self, key: &Key) -> Option<u64> {
        let e = self.entries.remove(key)?;
        self.lru.remove(&e.tick);
        self.bytes -= e.size;
        if let Some(v) = self.per_version.get_mut(&key.0) {
            *v -= e.size;
            if *v == 0 {
                self.per_version.remove(&key.0);
            }
        }
        Some(e.size)
    }
}

pub struct BlockCache {
    dir: PathBuf,
    cap: AtomicU64,
    index: Mutex<Index>,
    pub evicted_bytes: AtomicU64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CacheSnapshot {
    pub bytes: u64,
    pub cap: u64,
    pub blocks: u64,
    pub pinned_bytes: u64,
    pub evicted_bytes: u64,
}

impl BlockCache {
    /// Open the cache directory and index whatever blocks are already there.
    pub fn open(dir: &Path, cap: u64) -> io::Result<BlockCache> {
        std::fs::create_dir_all(dir)?;
        let mut found: Vec<(std::time::SystemTime, Key, u64)> = Vec::new();
        for vdir in std::fs::read_dir(dir)?.flatten() {
            if !vdir.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let version: Arc<str> = vdir.file_name().to_string_lossy().as_ref().into();
            for f in std::fs::read_dir(vdir.path())?.flatten() {
                let name = f.file_name();
                let Some(idx) = name.to_str().and_then(|s| s.parse::<u64>().ok()) else {
                    // a partially written block from an earlier run
                    let _ = std::fs::remove_file(f.path());
                    continue;
                };
                let Ok(meta) = f.metadata() else { continue };
                let used = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
                found.push((used, (version.clone(), idx), meta.len()));
            }
        }
        found.sort_by_key(|(t, _, _)| *t);
        let mut index = Index::default();
        for (_, key, size) in found {
            index.insert(key, size);
        }
        let pins = dir.join("pins.json");
        if let Ok(raw) = std::fs::read(&pins) {
            if let Ok(list) = serde_json::from_slice::<Vec<String>>(&raw) {
                index.pinned = list.into_iter().map(Into::into).collect();
            }
        }
        let cache = BlockCache { dir: dir.to_path_buf(), cap: AtomicU64::new(cap), index: Mutex::new(index), evicted_bytes: AtomicU64::new(0) };
        cache.evict_to_fit(0);
        Ok(cache)
    }

    fn path(&self, version: &str, idx: u64) -> PathBuf {
        self.dir.join(version).join(idx.to_string())
    }

    pub fn contains(&self, version: &Arc<str>, idx: u64) -> bool {
        self.index.lock().unwrap().entries.contains_key(&(version.clone(), idx))
    }

    /// Read a block if it is cached, marking it recently used.
    pub fn get(&self, version: &Arc<str>, idx: u64) -> Option<Bytes> {
        let key = (version.clone(), idx);
        if !self.index.lock().unwrap().touch(&key) {
            return None;
        }
        match std::fs::read(self.path(version, idx)) {
            Ok(data) => Some(Bytes::from(data)),
            Err(_) => {
                // The file vanished behind our back; forget it.
                self.index.lock().unwrap().remove(&key);
                None
            }
        }
    }

    /// Forget a block and delete its file (it turned out to be damaged).
    pub fn discard(&self, version: &Arc<str>, idx: u64) {
        self.index.lock().unwrap().remove(&(version.clone(), idx));
        let _ = std::fs::remove_file(self.path(version, idx));
    }

    /// Store a block, evicting older blocks if the cap would be exceeded.
    pub fn put(&self, version: &Arc<str>, idx: u64, data: &[u8]) -> io::Result<()> {
        self.evict_to_fit(data.len() as u64);
        let dir = self.dir.join(&**version);
        std::fs::create_dir_all(&dir)?;
        let tmp = dir.join(format!("{idx}.part"));
        std::fs::write(&tmp, data)?;
        std::fs::rename(&tmp, dir.join(idx.to_string()))?;
        self.index.lock().unwrap().insert((version.clone(), idx), data.len() as u64);
        Ok(())
    }

    /// Evict least-recently-used unpinned blocks until `incoming` more bytes fit.
    fn evict_to_fit(&self, incoming: u64) {
        let cap = self.cap.load(Ordering::Relaxed);
        let mut doomed: Vec<Key> = Vec::new();
        {
            let mut index = self.index.lock().unwrap();
            if index.bytes + incoming <= cap {
                return;
            }
            let mut freed = 0;
            let over = index.bytes + incoming - cap;
            for key in index.lru.values() {
                if freed >= over {
                    break;
                }
                if index.pinned.contains(&key.0) {
                    continue;
                }
                freed += index.entries[key].size;
                doomed.push(key.clone());
            }
            for key in &doomed {
                index.remove(key);
            }
            self.evicted_bytes.fetch_add(freed, Ordering::Relaxed);
        }
        for (version, idx) in doomed {
            let _ = std::fs::remove_file(self.path(&version, idx));
            // Remove the version directory once its last block is gone.
            let _ = std::fs::remove_dir(self.dir.join(&*version));
        }
    }

    pub fn set_cap(&self, cap: u64) {
        self.cap.store(cap, Ordering::Relaxed);
        self.evict_to_fit(0);
    }

    pub fn cap(&self) -> u64 {
        self.cap.load(Ordering::Relaxed)
    }

    /// Bytes of `version` currently held locally.
    pub fn version_bytes(&self, version: &str) -> u64 {
        self.index.lock().unwrap().per_version.get(version).copied().unwrap_or(0)
    }

    pub fn is_pinned(&self, version: &str) -> bool {
        self.index.lock().unwrap().pinned.contains(version)
    }

    /// Pin or unpin a version. Pinned blocks survive eviction.
    pub fn set_pinned(&self, version: &Arc<str>, pinned: bool) {
        let list: Vec<String> = {
            let mut index = self.index.lock().unwrap();
            if pinned {
                index.pinned.insert(version.clone());
            } else {
                index.pinned.remove(version);
            }
            index.pinned.iter().map(|s| s.to_string()).collect()
        };
        let _ = std::fs::write(self.dir.join("pins.json"), serde_json::to_vec(&list).unwrap_or_default());
        if !pinned {
            self.evict_to_fit(0);
        }
    }

    /// Drop every unpinned block.
    pub fn clear(&self) {
        let doomed: Vec<Key> = {
            let mut index = self.index.lock().unwrap();
            let keys: Vec<Key> = index.entries.keys().filter(|k| !index.pinned.contains(&k.0)).cloned().collect();
            for k in &keys {
                index.remove(k);
            }
            keys
        };
        for (version, idx) in doomed {
            let _ = std::fs::remove_file(self.path(&version, idx));
            let _ = std::fs::remove_dir(self.dir.join(&*version));
        }
    }

    pub fn snapshot(&self) -> CacheSnapshot {
        let index = self.index.lock().unwrap();
        let pinned_bytes = index.pinned.iter().filter_map(|v| index.per_version.get(v)).sum();
        CacheSnapshot {
            bytes: index.bytes,
            cap: self.cap.load(Ordering::Relaxed),
            blocks: index.entries.len() as u64,
            pinned_bytes,
            evicted_bytes: self.evicted_bytes.load(Ordering::Relaxed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Arc<str> {
        s.into()
    }

    #[test]
    fn stores_and_reads_back_blocks() {
        let dir = tempfile::tempdir().unwrap();
        let cache = BlockCache::open(dir.path(), 1 << 20).unwrap();
        assert!(cache.get(&v("a"), 0).is_none());
        cache.put(&v("a"), 0, b"hello").unwrap();
        cache.put(&v("a"), 7, b"world!").unwrap();
        assert_eq!(&cache.get(&v("a"), 0).unwrap()[..], b"hello");
        assert_eq!(&cache.get(&v("a"), 7).unwrap()[..], b"world!");
        assert_eq!(cache.snapshot().bytes, 11);
        assert_eq!(cache.version_bytes("a"), 11);
    }

    #[test]
    fn evicts_least_recently_used_when_over_cap() {
        let dir = tempfile::tempdir().unwrap();
        let cache = BlockCache::open(dir.path(), 300).unwrap();
        cache.put(&v("a"), 0, &[1u8; 100]).unwrap();
        cache.put(&v("a"), 1, &[2u8; 100]).unwrap();
        cache.put(&v("b"), 0, &[3u8; 100]).unwrap();
        // touch a/0 so a/1 is the oldest
        assert!(cache.get(&v("a"), 0).is_some());
        cache.put(&v("b"), 1, &[4u8; 100]).unwrap();
        assert!(cache.get(&v("a"), 1).is_none(), "oldest block should be evicted");
        assert!(cache.get(&v("a"), 0).is_some());
        assert!(cache.get(&v("b"), 0).is_some());
        assert!(cache.get(&v("b"), 1).is_some());
        let snap = cache.snapshot();
        assert_eq!(snap.bytes, 300);
        assert_eq!(snap.evicted_bytes, 100);
        assert!(!dir.path().join("a").join("1").exists());
    }

    #[test]
    fn pinned_versions_survive_eviction() {
        let dir = tempfile::tempdir().unwrap();
        let cache = BlockCache::open(dir.path(), 200).unwrap();
        cache.put(&v("keep"), 0, &[1u8; 100]).unwrap();
        cache.set_pinned(&v("keep"), true);
        cache.put(&v("x"), 0, &[2u8; 100]).unwrap();
        cache.put(&v("x"), 1, &[3u8; 100]).unwrap();
        assert!(cache.get(&v("keep"), 0).is_some());
        assert!(cache.get(&v("x"), 0).is_none());
        assert_eq!(cache.snapshot().pinned_bytes, 100);
    }

    #[test]
    fn reopening_finds_existing_blocks_and_pins() {
        let dir = tempfile::tempdir().unwrap();
        {
            let cache = BlockCache::open(dir.path(), 1 << 20).unwrap();
            cache.put(&v("a"), 3, b"persist").unwrap();
            cache.set_pinned(&v("a"), true);
        }
        let cache = BlockCache::open(dir.path(), 1 << 20).unwrap();
        assert_eq!(&cache.get(&v("a"), 3).unwrap()[..], b"persist");
        assert!(cache.is_pinned("a"));
        assert_eq!(cache.snapshot().blocks, 1);
    }

    #[test]
    fn shrinking_the_cap_evicts() {
        let dir = tempfile::tempdir().unwrap();
        let cache = BlockCache::open(dir.path(), 1000).unwrap();
        for i in 0..5 {
            cache.put(&v("a"), i, &[0u8; 100]).unwrap();
        }
        cache.set_cap(250);
        assert_eq!(cache.snapshot().bytes, 200);
    }
}
