//! One drive: the tree of nodes, each file's versions, which sealed shards
//! and manifests those versions use, and the only place that orders the
//! drive's commits.
//!
//! A commit is checked and applied inside one transaction, so it happens
//! entirely or not at all and nothing else runs in the middle. Looking in the
//! blob store for shards the drive has not seen happens before the
//! transaction, since that waits. What the commit may use is decided inside
//! the transaction, from the drive's own rows: a shard the collector has
//! marked is refused there whatever was found earlier.
//!
//! The collector chooses and marks inside a transaction too. On a provider
//! whose requests can run side by side that is what keeps a commit from
//! slipping between the choosing and the marking.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard};

use ludy_proto::shard::{sha256_hex, valid_hash};
use ludy_proto::tree::*;
use serde::Serialize;

use crate::front::blob_key;
use crate::ports::{Abort, Fault, Platform, Row, Sql, Value};
use crate::{log_line, Request, Response};

const SCHEMA: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS drive (one INTEGER PRIMARY KEY CHECK (one = 1), id TEXT NOT NULL, epoch INTEGER NOT NULL, seq INTEGER NOT NULL, \
     root TEXT NOT NULL, name TEXT NOT NULL, key TEXT NOT NULL, dedup TEXT NOT NULL)",
    "CREATE TABLE IF NOT EXISTS nodes (id TEXT PRIMARY KEY, parent TEXT, kind INTEGER NOT NULL, name_hash TEXT NOT NULL, \
     name TEXT NOT NULL, key TEXT NOT NULL, seq INTEGER NOT NULL, deleted INTEGER NOT NULL DEFAULT 0, \
     version INTEGER, size INTEGER NOT NULL DEFAULT 0, manifest TEXT, meta TEXT)",
    "CREATE UNIQUE INDEX IF NOT EXISTS nodes_seq ON nodes (seq)",
    // A deleted node keeps its row as a tombstone and gives its name back.
    "CREATE UNIQUE INDEX IF NOT EXISTS nodes_live_name ON nodes (parent, name_hash) WHERE deleted = 0",
    "CREATE INDEX IF NOT EXISTS nodes_parent ON nodes (parent)",
    // `shards` is the version's shard names end to end, 64 characters each.
    "CREATE TABLE IF NOT EXISTS versions (node TEXT NOT NULL, id INTEGER NOT NULL, size INTEGER NOT NULL, manifest TEXT NOT NULL, \
     meta TEXT NOT NULL, shards TEXT NOT NULL, at_ms INTEGER NOT NULL, PRIMARY KEY (node, id))",
    // A sealed shard (`s/<sha256>`) or manifest (`m/<sha256>`) and how many
    // versions use it. `zero_at`: when the last one let go. `dying`: the
    // collector has started deleting it from storage.
    "CREATE TABLE IF NOT EXISTS blobs (id TEXT PRIMARY KEY, refs INTEGER NOT NULL, zero_at INTEGER, dying INTEGER NOT NULL DEFAULT 0)",
    "CREATE INDEX IF NOT EXISTS blobs_zero ON blobs (zero_at) WHERE refs = 0",
    // Commits already applied, so that a retry is answered and not applied twice.
    "CREATE TABLE IF NOT EXISTS commits (id TEXT PRIMARY KEY, digest TEXT NOT NULL, seq INTEGER NOT NULL, at_ms INTEGER NOT NULL)",
    "CREATE INDEX IF NOT EXISTS commits_at ON commits (at_ms)",
];

/// A retry of a commit is recognised for this long.
const COMMIT_MEMORY_MS: i64 = 24 * 60 * 60 * 1000;
/// Objects deleted from storage per run of the collector.
const GC_BATCH: usize = 500;
/// Lookups in, and deletes from, the blob store that wait at once.
const STORE_BATCH: usize = 32;

#[derive(Clone, Debug)]
struct NodeRow {
    id: String,
    parent: Option<String>,
    kind: u64,
    name_hash: String,
    name: String,
    key: String,
    seq: u64,
    deleted: bool,
    version: Option<u64>,
    size: u64,
    manifest: Option<String>,
    meta: Option<String>,
}

const NODE_COLUMNS: &str = "id, parent, kind, name_hash, name, key, seq, deleted, version, size, manifest, meta";
const KIND_FILE: u64 = 0;
const KIND_FOLDER: u64 = 1;

impl NodeRow {
    /// A row selected as [`NODE_COLUMNS`].
    fn read(r: Row<'_>) -> Result<NodeRow, Fault> {
        Ok(NodeRow {
            id: r.text(0)?,
            parent: r.opt_text(1)?,
            kind: r.int(2)?,
            name_hash: r.text(3)?,
            name: r.text(4)?,
            key: r.text(5)?,
            seq: r.int(6)?,
            deleted: r.int(7)? != 0,
            version: r.opt_int(8)?,
            size: r.int(9)?,
            manifest: r.opt_text(10)?,
            meta: r.opt_text(11)?,
        })
    }

    fn info(self) -> NodeInfo {
        let version = match (self.version, self.manifest, self.meta) {
            (Some(id), Some(manifest), Some(meta)) => Some(VersionInfo { id, size: self.size, manifest, meta }),
            _ => None,
        };
        NodeInfo {
            id: self.id,
            parent: self.parent,
            kind: if self.kind == KIND_FOLDER { Kind::Folder } else { Kind::File },
            name_hash: self.name_hash,
            name: self.name,
            key: self.key,
            seq: self.seq,
            deleted: self.deleted,
            version,
        }
    }
}

struct DriveRow {
    epoch: u64,
    seq: u64,
    root: String,
    name: String,
    key: String,
    dedup: String,
}

/// What this instance has done since it started. Lost when it stops; the
/// log lines carry the same events for anyone adding them up across restarts.
#[derive(Default, Serialize, Clone)]
struct Telemetry {
    started_ms: u64,
    commits: u64,
    replays: u64,
    ops: u64,
    refused: BTreeMap<String, u64>,
    feed_pages: u64,
    feed_nodes: u64,
    storage_lookups: u64,
    gc_runs: u64,
    gc_deleted: u64,
}

type Pins = Arc<Mutex<HashMap<String, u32>>>;

/// A poisoned lock only means another request panicked; the map is still a map.
fn locked<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Holds blobs pinned until dropped.
struct Pinned {
    pins: Pins,
    ids: Vec<String>,
}

impl Drop for Pinned {
    fn drop(&mut self) {
        let mut pins = locked(&self.pins);
        for id in &self.ids {
            if let Some(n) = pins.get_mut(id) {
                *n -= 1;
                if *n == 0 {
                    pins.remove(id);
                }
            }
        }
    }
}

/// One drive, on whatever `P` is.
pub struct Drive<P: Platform> {
    p: P,
    telemetry: Mutex<Telemetry>,
    /// Blobs a commit in flight is about to reference. The collector leaves
    /// them alone. This saves the commit a refusal and an upload; it is not
    /// what keeps it safe (the check inside the transaction is).
    pins: Pins,
}

fn text(s: &str) -> Value {
    Value::text(s)
}

fn int(n: u64) -> Value {
    Value::int(n)
}

fn reply<T: Serialize>(value: &T) -> Response {
    Response::json(200, value)
}

fn refused(r: &Refusal) -> Response {
    Response::refusal(r)
}

/// A number in the query string: absent, a number, or not a number.
fn query_number(query: &str, name: &str) -> Result<Option<u64>, ()> {
    for pair in query.split('&') {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        if k == name {
            return v.parse().map(Some).map_err(|_| ());
        }
    }
    Ok(None)
}

impl<P: Platform> Drive<P> {
    pub fn new(p: P) -> Drive<P> {
        let telemetry = Telemetry { started_ms: p.now_ms(), ..Telemetry::default() };
        Drive { p, telemetry: Mutex::new(telemetry), pins: Arc::new(Mutex::new(HashMap::new())) }
    }

    pub fn platform(&self) -> &P {
        &self.p
    }

    /// Create the drive's tables. Call once per start of the instance,
    /// before anything else. Safe to repeat.
    pub async fn open(&self) -> Result<(), Fault> {
        let made = self
            .p
            .transaction("open", |tx| async move {
                for statement in SCHEMA {
                    tx.exec(statement, vec![]).await?;
                }
                Ok(())
            })
            .await?;
        made.map_err(|r| Fault(format!("creating the drive's tables was refused: {}", r.detail)))
    }

    fn log(&self, event: &str, drive: &str, fields: &[(&str, serde_json::Value)]) {
        self.p.log(&log_line(event, drive, fields));
    }

    async fn drive_row(&self) -> Result<Option<DriveRow>, Fault> {
        drive_row(self.p.sql()).await
    }

    /// The drive's id, remembered at creation for the timer, which has no request to read it from.
    async fn drive_id(&self) -> Result<Option<String>, Fault> {
        self.p.sql().exec("SELECT id FROM drive", vec![]).await?.first().map(|r| r.text(0)).transpose()
    }

    fn count_refusal(&self, r: &Refusal) {
        *locked(&self.telemetry).refused.entry(r.error.clone()).or_default() += 1;
    }

    async fn count(&self, query: &str) -> Result<u64, Fault> {
        self.p.sql().exec(query, vec![]).await?.count()
    }

    /// Answer one request. A [`Fault`] is the platform failing: answer it
    /// with a 5xx, and the sender tries again.
    pub async fn handle(&self, req: Request) -> Result<Response, Fault> {
        let mut parts = req.path.trim_matches('/').split('/');
        let (Some("drives"), Some(drive)) = (parts.next(), parts.next()) else {
            return Ok(refused(&Refusal::new(E_MALFORMED, "not a drive path")));
        };
        let drive = drive.to_string();
        let rest: Vec<&str> = parts.collect();
        let dev = self.p.config().dev;
        match (req.method.as_str(), rest.as_slice()) {
            ("PUT", []) => self.create(&drive, &req.body).await,
            ("GET", []) => self.head().await,
            ("POST", ["commit"]) => self.commit(&drive, req.body).await,
            ("POST", ["missing"]) => self.missing(&drive, &req.body).await,
            ("GET", ["feed"]) => self.feed(&req.query).await,
            ("GET", ["nodes", id]) => self.node(id).await,
            ("GET", ["nodes", id, "versions"]) => self.versions(id).await,
            ("GET", ["stats"]) => self.stats().await,
            ("GET", ["_dev", "check"]) if dev => self.dev_check().await,
            ("POST", ["_dev", "gc"]) if dev => self.dev_gc(&drive, &req.query).await,
            ("POST", ["_dev", "panic"]) if dev => self.dev_panic().await,
            _ => Ok(refused(&Refusal::new(E_MALFORMED, "no such route"))),
        }
    }

    /// The drive's timer went off: run the collector.
    pub async fn alarm(&self) -> Result<(), Fault> {
        let Some(drive) = self.drive_id().await? else {
            return Ok(());
        };
        let more = self.collect(&drive, self.p.config().gc_grace_ms, None).await?;
        if more {
            self.p.set_timer(self.p.now_ms() + 1000).await
        } else {
            // Blobs let go of since are not past their grace period yet.
            self.schedule_collector().await
        }
    }

    // ------------------------------------------------------------ the drive

    async fn create(&self, drive: &str, body: &[u8]) -> Result<Response, Fault> {
        let Ok(body) = serde_json::from_slice::<CreateDrive>(body) else {
            return Ok(refused(&Refusal::new(E_MALFORMED, "not a drive description")));
        };
        if let Err(why) = body.validate() {
            return Ok(refused(&Refusal::new(E_MALFORMED, why)));
        }
        if !valid_id(drive) {
            return Ok(refused(&Refusal::new(E_MALFORMED, "drive id is not 16 bytes of hex")));
        }
        let (now, id) = (self.p.now_ms(), drive.to_string());
        let made = self
            .p
            .transaction("create", move |tx| async move {
                if let Some(existing) = drive_row(&tx).await? {
                    // Creating the same drive again is a retry; another root is another drive.
                    return if existing.root == body.root { Ok(false) } else { Err(Refusal::new(E_EXISTS, "a drive with this id exists").into()) };
                }
                // The epoch tells this drive from an earlier one with the same id.
                tx.exec(
                    "INSERT INTO drive (one, id, epoch, seq, root, name, key, dedup) VALUES (1, ?, ?, 1, ?, ?, ?, ?)",
                    vec![text(&id), int(now), text(&body.root), text(&body.name), text(&body.key), text(&body.dedup)],
                )
                .await?;
                tx.exec(
                    "INSERT INTO nodes (id, parent, kind, name_hash, name, key, seq) VALUES (?, NULL, ?, '', ?, ?, 1)",
                    vec![text(&body.root), int(KIND_FOLDER), text(&body.name), text(&body.key)],
                )
                .await?;
                Ok(true)
            })
            .await?;
        match made {
            Err(r) => return Ok(refused(&r)),
            Ok(true) => self.log("drive", drive, &[("event", "created".into())]),
            Ok(false) => {}
        }
        self.head().await
    }

    async fn head(&self) -> Result<Response, Fault> {
        Ok(match self.drive_row().await? {
            Some(d) => reply(&DriveHead { epoch: d.epoch, seq: d.seq, root: d.root, name: d.name, key: d.key, dedup: d.dedup }),
            None => refused(&Refusal::new(E_NO_DRIVE, "no such drive")),
        })
    }

    // -------------------------------------------------------------- commits

    async fn commit(&self, drive: &str, body: Vec<u8>) -> Result<Response, Fault> {
        let started = self.p.now_ms();
        let Ok(commit) = serde_json::from_slice::<Commit>(&body) else {
            return Ok(refused(&Refusal::new(E_MALFORMED, "not a commit")));
        };
        if let Err(why) = commit.validate() {
            return Ok(refused(&Refusal::new(E_MALFORMED, why)));
        }
        let digest = sha256_hex(&body);
        drop(body);

        // Before anything waits: pin what the commit names. Then look in
        // storage for the ones the drive does not already hold.
        let blobs = commit.blobs();
        let ids: Vec<String> = blobs.shards.iter().map(|h| format!("s/{h}")).chain(blobs.manifests.iter().map(|h| format!("m/{h}"))).collect();
        let _pinned = self.pin(&ids);
        let unknown = self.unknown(&ids).await?;
        if unknown.len() > MAX_NEW_BLOBS {
            let r = Refusal::new(E_TOO_LARGE, "too many new shards in one commit");
            self.count_refusal(&r);
            return Ok(refused(&r));
        }
        let found = self.look_up(drive, &unknown).await?;

        let ops = commit.ops.len() as u64;
        // All of it or none of it, and nothing else in between.
        let result = self.p.transaction("commit", move |tx| async move { apply(&tx, &commit, &digest, started, &found).await }).await?;
        let ms = self.p.now_ms().saturating_sub(started);
        match result {
            Ok(done) => {
                {
                    let mut t = locked(&self.telemetry);
                    if done.replayed {
                        t.replays += 1;
                    } else {
                        t.commits += 1;
                        t.ops += ops;
                    }
                }
                let what = if done.replayed { "replay" } else { "ok" };
                self.log("commit", drive, &[("outcome", what.into()), ("seq", done.seq.into()), ("ops", ops.into()), ("ms", ms.into())]);
                // Also after a replay: the first attempt may have been applied
                // and then failed before it got this far.
                self.schedule_collector().await?;
                Ok(reply(&done))
            }
            Err(r) => {
                self.count_refusal(&r);
                self.log("commit", drive, &[("outcome", r.error.clone().into()), ("ops", ops.into()), ("ms", ms.into())]);
                if r.error == E_MISSING {
                    // It may be waiting for a collector that was cut short.
                    self.schedule_collector().await?;
                }
                Ok(refused(&r))
            }
        }
    }

    fn pin(&self, ids: &[String]) -> Pinned {
        let mut pins = locked(&self.pins);
        for id in ids {
            *pins.entry(id.clone()).or_default() += 1;
        }
        Pinned { pins: self.pins.clone(), ids: ids.to_vec() }
    }

    /// Those of `ids` the drive has no usable row for: storage has to be asked.
    async fn unknown(&self, ids: &[String]) -> Result<Vec<String>, Fault> {
        let mut unknown = Vec::new();
        for id in ids {
            if !matches!(blob_state(self.p.sql(), id).await?, Some((_, false))) {
                unknown.push(id.clone());
            }
        }
        Ok(unknown)
    }

    /// Which of `ids` are in storage right now. A blob the collector is
    /// deleting does not count, whatever storage says.
    async fn look_up(&self, drive: &str, ids: &[String]) -> Result<HashSet<String>, Fault> {
        let mut found = HashSet::new();
        for batch in ids.chunks(STORE_BATCH) {
            let keys: Vec<String> = batch.iter().map(|id| blob_key(drive, id)).collect();
            let there = futures_util::future::join_all(keys.iter().map(|key| self.p.blob_exists(key))).await;
            for (id, there) in batch.iter().zip(there) {
                if there? && !matches!(blob_state(self.p.sql(), id).await?, Some((_, true))) {
                    found.insert(id.clone());
                }
            }
        }
        locked(&self.telemetry).storage_lookups += ids.len() as u64;
        Ok(found)
    }

    /// Which of the named shards and manifests would a commit be refused for?
    async fn missing(&self, drive: &str, body: &[u8]) -> Result<Response, Fault> {
        let Ok(asked) = serde_json::from_slice::<Blobs>(body) else {
            return Ok(refused(&Refusal::new(E_MALFORMED, "not a list of shards and manifests")));
        };
        if asked.len() > MAX_VERSION_SHARDS || !asked.shards.iter().chain(&asked.manifests).all(|h| valid_hash(h)) {
            return Ok(refused(&Refusal::new(E_MALFORMED, "a name is not a sha-256, or there are too many")));
        }
        if self.drive_row().await?.is_none() {
            return Ok(refused(&Refusal::new(E_NO_DRIVE, "no such drive")));
        }
        let ids: Vec<String> = asked.shards.iter().map(|h| format!("s/{h}")).chain(asked.manifests.iter().map(|h| format!("m/{h}"))).collect();
        let _pinned = self.pin(&ids);
        let unknown = self.unknown(&ids).await?;
        let found = self.look_up(drive, &unknown).await?;
        let unknown: HashSet<String> = unknown.into_iter().collect();
        let lacking = |kind: &str, names: &[String]| -> Vec<String> {
            names
                .iter()
                .filter(|h| {
                    let id = format!("{kind}/{h}");
                    unknown.contains(&id) && !found.contains(&id)
                })
                .cloned()
                .collect()
        };
        Ok(reply(&Blobs { shards: lacking("s", &asked.shards), manifests: lacking("m", &asked.manifests) }))
    }

    // ----------------------------------------------------------------- reads

    async fn feed(&self, query: &str) -> Result<Response, Fault> {
        let (Ok(since), Ok(epoch), Ok(limit)) = (query_number(query, "since"), query_number(query, "epoch"), query_number(query, "limit")) else {
            return Ok(refused(&Refusal::new(E_MALFORMED, "since, epoch and limit are numbers")));
        };
        let since = since.unwrap_or(0);
        let Some(drive) = self.drive_row().await? else {
            return Ok(refused(&Refusal::new(E_NO_DRIVE, "no such drive")));
        };
        if epoch.is_some_and(|e| e != drive.epoch) || since > drive.seq {
            return Ok(reply(&Feed { epoch: drive.epoch, seq: drive.seq, reset: true, more: false, nodes: vec![] }));
        }
        let limit = limit.map(|l| l as usize).unwrap_or(FEED_PAGE).clamp(1, MAX_FEED_PAGE);
        let rows = self.p.sql().exec(&format!("SELECT {NODE_COLUMNS} FROM nodes WHERE seq > ? ORDER BY seq LIMIT ?"), vec![int(since), int(limit as u64)]).await?;
        let rows: Vec<NodeRow> = rows.iter().map(NodeRow::read).collect::<Result<_, _>>()?;
        let more = rows.len() == limit && rows.last().is_some_and(|r| r.seq < drive.seq);
        {
            let mut t = locked(&self.telemetry);
            t.feed_pages += 1;
            t.feed_nodes += rows.len() as u64;
        }
        Ok(reply(&Feed { epoch: drive.epoch, seq: drive.seq, reset: false, more, nodes: rows.into_iter().map(NodeRow::info).collect() }))
    }

    async fn node(&self, id: &str) -> Result<Response, Fault> {
        Ok(match node(self.p.sql(), id).await? {
            Some(row) => reply(&row.info()),
            None => refused(&Refusal::new(E_NO_NODE, "no such node")),
        })
    }

    async fn versions(&self, id: &str) -> Result<Response, Fault> {
        if !node(self.p.sql(), id).await?.is_some_and(|n| !n.deleted) {
            return Ok(refused(&Refusal::new(E_NO_NODE, "no such node")));
        }
        let rows = self.p.sql().exec("SELECT id, size, manifest, meta, at_ms FROM versions WHERE node = ? ORDER BY id DESC", vec![text(id)]).await?;
        let entries: Vec<VersionEntry> =
            rows.iter().map(|r| Ok(VersionEntry { id: r.int(0)?, size: r.int(1)?, manifest: r.text(2)?, meta: r.text(3)?, at_ms: r.int(4)? })).collect::<Result<_, Fault>>()?;
        Ok(reply(&entries))
    }

    async fn stats(&self) -> Result<Response, Fault> {
        let drive = self.drive_row().await?;
        let pinned = locked(&self.pins).len();
        let since_start = locked(&self.telemetry).clone();
        Ok(reply(&serde_json::json!({
            "exists": drive.is_some(),
            "seq": drive.as_ref().map(|d| d.seq).unwrap_or(0),
            "epoch": drive.as_ref().map(|d| d.epoch).unwrap_or(0),
            "nodes": self.count("SELECT COUNT(*) AS n FROM nodes WHERE deleted = 0").await?,
            "tombstones": self.count("SELECT COUNT(*) AS n FROM nodes WHERE deleted = 1").await?,
            "versions": self.count("SELECT COUNT(*) AS n FROM versions").await?,
            "blobs_in_use": self.count("SELECT COUNT(*) AS n FROM blobs WHERE refs > 0").await?,
            "blobs_unused": self.count("SELECT COUNT(*) AS n FROM blobs WHERE refs = 0 AND dying = 0").await?,
            "blobs_dying": self.count("SELECT COUNT(*) AS n FROM blobs WHERE dying = 1").await?,
            "commits_remembered": self.count("SELECT COUNT(*) AS n FROM commits").await?,
            "pinned": pinned,
            "database_bytes": self.p.database_bytes().await?,
            "since_start": since_start,
        })))
    }

    // ------------------------------------------------------------ collector

    /// Make sure the collector runs once unused blobs are past their grace
    /// period, and at once if an earlier run was cut short: what it marked
    /// is refused to every commit until it has finished.
    ///
    /// A provider also calls this when an instance starts, so that a timer
    /// lost with the instance is set again.
    pub async fn schedule_collector(&self) -> Result<(), Fault> {
        let now = self.p.now_ms();
        if self.count("SELECT COUNT(*) AS n FROM blobs WHERE dying = 1").await? > 0 {
            // Soon, but never later than it is already set for: a client
            // retrying faster than this would otherwise push it back for ever.
            let soon = now + 50;
            if !self.p.timer().await?.is_some_and(|at| at <= soon) {
                self.p.set_timer(soon).await?;
            }
            return Ok(());
        }
        let waiting = self.count("SELECT COUNT(*) AS n FROM blobs WHERE refs = 0").await?;
        if waiting > 0 && self.p.timer().await?.is_none() {
            self.p.set_timer(now + self.p.config().gc_grace_ms.max(50) as u64).await?;
        }
        Ok(())
    }

    /// Delete from storage the blobs no version has used for `grace_ms`.
    /// Returns whether more are waiting.
    ///
    /// A blob is marked dying, durably, before storage is touched. From then
    /// on a commit that names it is told it is missing, so nothing can come
    /// to depend on an object that is about to go. If this instance dies in
    /// the middle the mark survives and the next run finishes the job.
    ///
    /// `stop_after` is for the chaos harness: stop as a dying instance would,
    /// with that many objects deleted and the rest still marked.
    async fn collect(&self, drive: &str, grace_ms: i64, stop_after: Option<usize>) -> Result<bool, Fault> {
        let cutoff = self.p.now_ms() as i64 - grace_ms;
        let pins = self.pins.clone();
        // Choose and mark in one transaction: a commit either ran before it
        // and took its reference (then the row has refs and is not marked),
        // or runs after and sees the mark.
        let marked = self
            .p
            .transaction("collect", move |tx| async move {
                let rows = tx
                    .exec("SELECT id FROM blobs WHERE dying = 1 OR (refs = 0 AND zero_at <= ?) ORDER BY dying DESC LIMIT ?", vec![Value::Int(cutoff), int(GC_BATCH as u64)])
                    .await?;
                let chosen: Vec<String> = rows.iter().map(|r| r.text(0)).collect::<Result<_, _>>()?;
                // Not the ones a commit in flight has named.
                let take: Vec<String> = {
                    let pins = locked(&pins);
                    chosen.into_iter().filter(|id| !pins.contains_key(id)).collect()
                };
                let mut dying = Vec::new();
                for id in &take {
                    let marked = tx.exec("UPDATE blobs SET dying = 1 WHERE id = ? AND refs = 0", vec![text(id)]).await?.changed > 0;
                    if marked || matches!(blob_state(&tx, id).await?, Some((0, true))) {
                        dying.push(id.clone());
                    }
                }
                Ok((take.len(), dying))
            })
            .await?;
        let (taken, dying) = marked.map_err(|r| Fault(format!("the collector was refused: {}", r.detail)))?;
        if taken == 0 {
            return Ok(false);
        }
        // Make the marks durable before deleting anything.
        self.p.flush().await?;
        if let Some(n) = stop_after {
            for id in dying.iter().take(n) {
                self.p.blob_delete(&blob_key(drive, id)).await?;
            }
            return Ok(true);
        }
        for batch in dying.chunks(STORE_BATCH) {
            let keys: Vec<String> = batch.iter().map(|id| blob_key(drive, id)).collect();
            for result in futures_util::future::join_all(keys.iter().map(|key| self.p.blob_delete(key))).await {
                result?;
            }
        }
        let gone = dying.clone();
        let forgotten = self
            .p
            .transaction("forget", move |tx| async move {
                for id in &gone {
                    tx.exec("DELETE FROM blobs WHERE id = ? AND dying = 1", vec![text(id)]).await?;
                }
                Ok(())
            })
            .await?;
        forgotten.map_err(|r| Fault(format!("the collector was refused: {}", r.detail)))?;
        {
            let mut t = locked(&self.telemetry);
            t.gc_runs += 1;
            t.gc_deleted += dying.len() as u64;
        }
        self.log("gc", drive, &[("deleted", (dying.len() as u64).into())]);
        Ok(taken == GC_BATCH)
    }

    // ------------------------------------------------- for the chaos harness

    /// Recount everything that is stored twice and report what disagrees.
    async fn dev_check(&self) -> Result<Response, Fault> {
        let sql = self.p.sql();
        let mut problems: Vec<String> = Vec::new();
        let mut note = |p: String| {
            if problems.len() < 20 {
                problems.push(p)
            }
        };

        // Blob references, recounted from the versions.
        let mut want: HashMap<String, u64> = HashMap::new();
        let versions = sql.exec("SELECT node, manifest, shards FROM versions", vec![]).await?;
        for v in versions.iter() {
            let (node, manifest, shards) = (v.text(0)?, v.text(1)?, v.text(2)?);
            *want.entry(format!("m/{manifest}")).or_default() += 1;
            for i in (0..shards.len()).step_by(64) {
                *want.entry(format!("s/{}", &shards[i..(i + 64).min(shards.len())])).or_default() += 1;
            }
            if shards.len() % 64 != 0 {
                note(format!("a version of {node} has a shard list of {} characters", shards.len()));
            }
        }
        let blobs = sql.exec("SELECT id, refs FROM blobs", vec![]).await?;
        for b in blobs.iter() {
            let (id, refs) = (b.text(0)?, b.int(1)?);
            let expected = want.remove(&id).unwrap_or(0);
            if refs != expected {
                note(format!("blob {id} has {refs} references, its versions say {expected}"));
            }
        }
        for (id, n) in want {
            note(format!("blob {id} is used by {n} versions and has no row"));
        }
        let too_many = format!("file {{}} keeps more than {HISTORY} versions");
        let checks: [(&str, String); 13] = [
            ("blob {} is in use and being deleted", "SELECT id FROM blobs WHERE refs > 0 AND dying = 1".into()),
            ("node {} is live under a deleted parent", "SELECT n.id FROM nodes n JOIN nodes p ON p.id = n.parent WHERE n.deleted = 0 AND p.deleted = 1".into()),
            ("node {} has a parent that does not exist", "SELECT n.id FROM nodes n LEFT JOIN nodes p ON p.id = n.parent WHERE n.parent IS NOT NULL AND p.id IS NULL".into()),
            ("node {} is inside a file", "SELECT n.id FROM nodes n JOIN nodes p ON p.id = n.parent WHERE n.deleted = 0 AND p.kind = 0".into()),
            ("node {} has no parent and is not the root", "SELECT id FROM nodes WHERE parent IS NULL AND id NOT IN (SELECT root FROM drive)".into()),
            (
                "node {} shares its name with a sibling",
                "SELECT MIN(id) AS id FROM nodes WHERE deleted = 0 AND parent IS NOT NULL GROUP BY parent, name_hash HAVING COUNT(*) > 1".into(),
            ),
            ("node {} shares its sequence number with another", "SELECT MIN(id) AS id FROM nodes GROUP BY seq HAVING COUNT(*) > 1".into()),
            ("node {} is ahead of the drive's sequence number", "SELECT id FROM nodes WHERE seq > (SELECT seq FROM drive)".into()),
            // A file's current version is its newest version row, and a deleted node has none.
            (
                "file {} does not point at its newest version",
                "SELECT n.id FROM nodes n WHERE n.deleted = 0 AND n.kind = 0 AND \
                 COALESCE(n.version, 0) != COALESCE((SELECT MAX(v.id) FROM versions v WHERE v.node = n.id), 0)"
                    .into(),
            ),
            (
                "node {} is deleted or a folder and still has versions",
                "SELECT DISTINCT v.node AS id FROM versions v JOIN nodes n ON n.id = v.node WHERE n.deleted = 1 OR n.kind = 1".into(),
            ),
            ("versions of {} have no node", "SELECT DISTINCT v.node AS id FROM versions v LEFT JOIN nodes n ON n.id = v.node WHERE n.id IS NULL".into()),
            (&too_many[..], format!("SELECT node AS id FROM versions GROUP BY node HAVING COUNT(*) > {HISTORY}")),
            // A cycle: a live node from which walking up never reaches the root.
            (
                "node {} is in a loop",
                "WITH RECURSIVE up (start, at, depth) AS ( \
                   SELECT id, parent, 0 FROM nodes WHERE deleted = 0 AND parent IS NOT NULL \
                   UNION ALL SELECT up.start, n.parent, up.depth + 1 FROM up JOIN nodes n ON n.id = up.at WHERE up.depth < 10000 \
                 ) SELECT DISTINCT start AS id FROM up WHERE depth >= 10000"
                    .into(),
            ),
        ];
        for (what, query) in &checks {
            for row in sql.exec(query, vec![]).await?.iter() {
                note(what.replace("{}", &row.text(0)?));
            }
        }
        Ok(reply(&serde_json::json!({ "ok": problems.is_empty(), "problems": problems })))
    }

    /// Run the collector now, with no grace period, until nothing is left;
    /// or with `?stop_after=n`, one run that stops part-way.
    async fn dev_gc(&self, drive: &str, query: &str) -> Result<Response, Fault> {
        match query_number(query, "stop_after").ok().flatten() {
            Some(n) => {
                self.collect(drive, 0, Some(n as usize)).await?;
            }
            None => while self.collect(drive, 0, None).await? {},
        }
        self.stats().await
    }

    /// Write inside a transaction the way a commit does, then panic. The
    /// harness then asks whether the write is there and how long the drive
    /// was out.
    async fn dev_panic(&self) -> Result<Response, Fault> {
        let done: Result<(), Refusal> = self
            .p
            .transaction("panic", |tx| async move {
                tx.exec("INSERT INTO commits (id, digest, seq, at_ms) VALUES ('panic-probe', '', 0, 0)", vec![]).await?;
                panic!("dev: panic inside a transaction, after a write");
            })
            .await?;
        Ok(Response::text(200, &format!("unreachable: {done:?}")))
    }
}

async fn drive_row(sql: &impl Sql) -> Result<Option<DriveRow>, Fault> {
    let rows = sql.exec("SELECT epoch, seq, root, name, key, dedup FROM drive", vec![]).await?;
    rows.first().map(|r| Ok(DriveRow { epoch: r.int(0)?, seq: r.int(1)?, root: r.text(2)?, name: r.text(3)?, key: r.text(4)?, dedup: r.text(5)? })).transpose()
}

async fn node(sql: &impl Sql, id: &str) -> Result<Option<NodeRow>, Fault> {
    sql.exec(&format!("SELECT {NODE_COLUMNS} FROM nodes WHERE id = ?"), vec![text(id)]).await?.first().map(NodeRow::read).transpose()
}

/// `(refs, dying)` of a blob the drive has a row for.
async fn blob_state(sql: &impl Sql, id: &str) -> Result<Option<(u64, bool)>, Fault> {
    sql.exec("SELECT refs, dying FROM blobs WHERE id = ?", vec![text(id)]).await?.first().map(|r| Ok((r.int(0)?, r.int(1)? != 0))).transpose()
}

async fn live(sql: &impl Sql, id: &str, what: &str) -> Result<NodeRow, Abort> {
    match node(sql, id).await? {
        Some(row) if !row.deleted => Ok(row),
        _ => Err(Refusal::new(E_NO_NODE, format!("{what} {id} does not exist")).into()),
    }
}

async fn name_is_free(sql: &impl Sql, parent: &str, name_hash: &str, except: &str) -> Result<(), Abort> {
    let taken = sql
        .exec("SELECT COUNT(*) AS n FROM nodes WHERE parent = ? AND name_hash = ? AND deleted = 0 AND id != ?", vec![text(parent), text(name_hash), text(except)])
        .await?
        .count()?;
    if taken > 0 {
        return Err(Refusal::new(E_EXISTS, "that name is taken in this folder").into());
    }
    Ok(())
}

/// Let go of one version's manifest and shards.
async fn release(sql: &impl Sql, manifest: &str, shards: &str) -> Result<(), Fault> {
    sql.exec("UPDATE blobs SET refs = refs - 1 WHERE id = ?", vec![text(&format!("m/{manifest}"))]).await?;
    for i in (0..shards.len()).step_by(64) {
        sql.exec("UPDATE blobs SET refs = refs - 1 WHERE id = ?", vec![text(&format!("s/{}", &shards[i..i + 64]))]).await?;
    }
    Ok(())
}

/// A version's `(id, manifest, shards)`.
fn old_version(r: Row<'_>) -> Result<(u64, String, String), Fault> {
    Ok((r.int(0)?, r.text(1)?, r.text(2)?))
}

/// Check a commit against the drive as it is now and apply it. Runs inside a
/// transaction: returning an error undoes everything it wrote.
async fn apply(sql: &impl Sql, commit: &Commit, digest: &str, now: u64, found: &HashSet<String>) -> Result<Committed, Abort> {
    let Some(drive) = drive_row(sql).await? else {
        return Err(Refusal::new(E_NO_DRIVE, "no such drive").into());
    };

    // A commit that was applied before is answered, not applied again.
    let seen = sql.exec("SELECT digest, seq FROM commits WHERE id = ?", vec![text(&commit.id)]).await?;
    if let Some(seen) = seen.first() {
        return if seen.text(0)? == digest {
            Ok(Committed { seq: seen.int(1)?, replayed: true })
        } else {
            Err(Refusal::new(E_COMMIT_REUSED, "this commit id was used for a different commit").into())
        };
    }

    let mut seq = drive.seq;
    for (index, op) in commit.ops.iter().enumerate() {
        // Say which op it was: the sender may want to drop just that one.
        apply_op(sql, op, &mut seq, now, found).await.map_err(|stop| match stop {
            Abort::Refused(mut refusal) => {
                refusal.op = Some(index);
                Abort::Refused(refusal)
            }
            fault => fault,
        })?;
    }

    sql.exec("UPDATE blobs SET zero_at = ? WHERE refs = 0 AND zero_at IS NULL", vec![int(now)]).await?;
    sql.exec("UPDATE drive SET seq = ?", vec![int(seq)]).await?;
    sql.exec("INSERT INTO commits (id, digest, seq, at_ms) VALUES (?, ?, ?, ?)", vec![text(&commit.id), text(digest), int(seq), int(now)]).await?;
    sql.exec("DELETE FROM commits WHERE at_ms < ?", vec![Value::Int(now as i64 - COMMIT_MEMORY_MS)]).await?;
    Ok(Committed { seq, replayed: false })
}

async fn apply_op(sql: &impl Sql, op: &Op, seq: &mut u64, now: u64, found: &HashSet<String>) -> Result<(), Abort> {
    match op {
        Op::Create { id, parent, kind, name_hash, name, key } => {
            let folder = live(sql, parent, "parent").await?;
            if folder.kind != KIND_FOLDER {
                return Err(Refusal::new(E_WRONG_KIND, "the parent is a file").into());
            }
            if node(sql, id).await?.is_some() {
                return Err(Refusal::new(E_EXISTS, format!("node {id} exists")).into());
            }
            name_is_free(sql, parent, name_hash, id).await?;
            *seq += 1;
            let kind = if *kind == Kind::Folder { KIND_FOLDER } else { KIND_FILE };
            sql.exec(
                "INSERT INTO nodes (id, parent, kind, name_hash, name, key, seq) VALUES (?, ?, ?, ?, ?, ?, ?)",
                vec![text(id), text(parent), int(kind), text(name_hash), text(name), text(key), int(*seq)],
            )
            .await?;
        }
        Op::Put { node: id, base, size, manifest, shards, meta } => {
            let file = live(sql, id, "file").await?;
            if file.kind != KIND_FILE {
                return Err(Refusal::new(E_WRONG_KIND, "a folder has no content").into());
            }
            let current = file.version.unwrap_or(0);
            if base.is_some_and(|b| b != current) {
                let mut r = Refusal::new(E_CONFLICT, "the file has changed since the version this was based on");
                r.current = Some(current);
                return Err(r.into());
            }
            // Every blob must be held by the drive already, or have been
            // found in storage just now; never one that is being deleted.
            let mut missing = Blobs::default();
            let mut wanted: Vec<String> = Vec::with_capacity(shards.len() + 1);
            for (kind, hash) in std::iter::once(("m", manifest)).chain(shards.iter().map(|h| ("s", h))) {
                let blob = format!("{kind}/{hash}");
                let usable = match blob_state(sql, &blob).await? {
                    Some((_, dying)) => !dying,
                    None => found.contains(&blob),
                };
                if usable {
                    wanted.push(blob);
                } else if kind == "m" {
                    missing.manifests.push(hash.clone());
                } else if !missing.shards.contains(hash) {
                    missing.shards.push(hash.clone());
                }
            }
            if !missing.is_empty() {
                let mut r = Refusal::new(E_MISSING, "upload these and commit again");
                r.missing = missing;
                return Err(r.into());
            }
            for blob in &wanted {
                sql.exec("INSERT INTO blobs (id, refs) VALUES (?, 1) ON CONFLICT (id) DO UPDATE SET refs = refs + 1, zero_at = NULL", vec![text(blob)]).await?;
            }
            *seq += 1;
            sql.exec(
                "INSERT INTO versions (node, id, size, manifest, meta, shards, at_ms) VALUES (?, ?, ?, ?, ?, ?, ?)",
                vec![text(id), int(*seq), int(*size), text(manifest), text(meta), text(&shards.concat()), int(now)],
            )
            .await?;
            sql.exec(
                "UPDATE nodes SET version = ?, size = ?, manifest = ?, meta = ?, seq = ? WHERE id = ?",
                vec![int(*seq), int(*size), text(manifest), text(meta), int(*seq), text(id)],
            )
            .await?;
            // History is capped: the oldest versions beyond it let go.
            let old = sql.exec("SELECT id, manifest, shards FROM versions WHERE node = ? ORDER BY id DESC LIMIT -1 OFFSET ?", vec![text(id), int(HISTORY as u64)]).await?;
            for v in old.iter() {
                let (version, manifest, shards) = old_version(v)?;
                release(sql, &manifest, &shards).await?;
                sql.exec("DELETE FROM versions WHERE node = ? AND id = ?", vec![text(id), int(version)]).await?;
            }
        }
        Op::Move { node: id, parent, name_hash, name, key } => {
            let moving = live(sql, id, "node").await?;
            if moving.parent.is_none() {
                return Err(Refusal::new(E_WRONG_KIND, "the root does not move").into());
            }
            let folder = live(sql, parent, "parent").await?;
            if folder.kind != KIND_FOLDER {
                return Err(Refusal::new(E_WRONG_KIND, "the parent is a file").into());
            }
            // Walking up from the new parent must not pass the node itself.
            let mut at = Some(folder);
            let mut steps = 0;
            while let Some(here) = at {
                if here.id == *id {
                    return Err(Refusal::new(E_CYCLE, "a folder cannot move into itself").into());
                }
                steps += 1;
                if steps > 100_000 {
                    return Err(Refusal::new("storage", "the tree has a loop").into());
                }
                at = match here.parent {
                    Some(up) => node(sql, &up).await?,
                    None => None,
                };
            }
            name_is_free(sql, parent, name_hash, id).await?;
            *seq += 1;
            sql.exec(
                "UPDATE nodes SET parent = ?, name_hash = ?, name = ?, key = ?, seq = ? WHERE id = ?",
                vec![text(parent), text(name_hash), text(name), text(key), int(*seq), text(id)],
            )
            .await?;
        }
        Op::Delete { node: id } => {
            let doomed = live(sql, id, "node").await?;
            if doomed.parent.is_none() {
                return Err(Refusal::new(E_WRONG_KIND, "the root cannot be deleted").into());
            }
            let subtree = sql
                .exec("WITH RECURSIVE sub (id) AS (SELECT ? UNION ALL SELECT n.id FROM nodes n JOIN sub ON n.parent = sub.id WHERE n.deleted = 0) SELECT id FROM sub", vec![text(id)])
                .await?;
            for n in subtree.iter() {
                let n = n.text(0)?;
                let old = sql.exec("SELECT id, manifest, shards FROM versions WHERE node = ?", vec![text(&n)]).await?;
                for v in old.iter() {
                    let (_, manifest, shards) = old_version(v)?;
                    release(sql, &manifest, &shards).await?;
                }
                sql.exec("DELETE FROM versions WHERE node = ?", vec![text(&n)]).await?;
                *seq += 1;
                sql.exec("UPDATE nodes SET deleted = 1, version = NULL, size = 0, manifest = NULL, meta = NULL, seq = ? WHERE id = ?", vec![int(*seq), text(&n)]).await?;
            }
        }
    }
    Ok(())
}
