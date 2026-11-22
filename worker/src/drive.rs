//! One Durable Object per drive: the tree of nodes, each file's versions,
//! which sealed shards and manifests those versions use, and the only place
//! that orders the drive's commits.
//!
//! A commit is checked and applied inside one synchronous storage
//! transaction, so it happens entirely or not at all and nothing else runs
//! in the middle. Looking in R2 for shards the drive has not seen happens
//! before the transaction, since that does wait. What the commit may use is
//! decided inside the transaction, from the drive's own rows: a shard the
//! collector has marked is refused there whatever was found earlier.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::rc::Rc;

use ludy_proto::shard::sha256_hex;
use ludy_proto::tree::*;
use serde::{Deserialize, Serialize};
use worker::wasm_bindgen::prelude::Closure;
use worker::wasm_bindgen::{JsCast, JsValue};
use worker::*;

use crate::{blob_key, dev_mode, log_line, BUCKET};

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
/// How long a shard no version uses is kept, unless `GC_GRACE_MS` says otherwise.
const DEFAULT_GRACE_MS: i64 = 24 * 60 * 60 * 1000;
/// Objects deleted from storage per run of the collector.
const GC_BATCH: usize = 500;

#[derive(Deserialize, Clone, Debug)]
struct NodeRow {
    id: String,
    parent: Option<String>,
    kind: u8,
    name_hash: String,
    name: String,
    key: String,
    seq: u64,
    deleted: u8,
    version: Option<u64>,
    size: u64,
    manifest: Option<String>,
    meta: Option<String>,
}

const NODE_COLUMNS: &str = "id, parent, kind, name_hash, name, key, seq, deleted, version, size, manifest, meta";
const KIND_FILE: u8 = 0;
const KIND_FOLDER: u8 = 1;

impl NodeRow {
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
            deleted: self.deleted != 0,
            version,
        }
    }
}

#[derive(Deserialize)]
struct DriveRow {
    epoch: u64,
    seq: u64,
    root: String,
    name: String,
    key: String,
    dedup: String,
}

#[derive(Deserialize)]
struct Count {
    n: u64,
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

#[durable_object]
pub struct Drive {
    state: State,
    /// The runtime's storage object, for [`transaction_sync`].
    storage: JsValue,
    env: Env,
    telemetry: RefCell<Telemetry>,
    /// Blobs a commit in flight is about to reference. The collector leaves
    /// them alone. This saves the commit a refusal and an upload; it is not
    /// what keeps it safe (the check inside the transaction is).
    pins: Rc<RefCell<HashMap<String, u32>>>,
}

type Sv = SqlStorageValue;

fn text(s: &str) -> Sv {
    Sv::String(s.to_string())
}

fn int(n: u64) -> Sv {
    Sv::Integer(n as i64)
}

fn now_ms() -> u64 {
    Date::now().as_millis()
}

fn reply<T: Serialize>(value: &T) -> Result<Response> {
    Response::from_json(value)
}

fn refused(r: &Refusal) -> Result<Response> {
    Ok(Response::from_json(r)?.with_status(r.status()))
}

/// Holds blobs pinned until dropped.
struct Pinned {
    pins: Rc<RefCell<HashMap<String, u32>>>,
    ids: Vec<String>,
}

impl Drop for Pinned {
    fn drop(&mut self) {
        let mut pins = self.pins.borrow_mut();
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

impl DurableObject for Drive {
    fn new(state: State, env: Env) -> Self {
        // `State` does not hand out the storage object itself; take it apart and put it back.
        let raw = state._inner();
        let storage: JsValue = raw.storage().expect("a Durable Object has storage").into();
        let state = State::from(raw);
        let sql = state.storage().sql();
        for statement in SCHEMA {
            sql.exec(statement, None).expect("create the drive's tables");
        }
        let telemetry = Telemetry { started_ms: now_ms(), ..Telemetry::default() };
        Drive { state, storage, env, telemetry: RefCell::new(telemetry), pins: Rc::new(RefCell::new(HashMap::new())) }
    }

    async fn fetch(&self, mut req: Request) -> Result<Response> {
        let path = req.path();
        let mut parts = path.trim_matches('/').split('/');
        let (Some("drives"), Some(drive)) = (parts.next(), parts.next()) else {
            return refused(&Refusal::new(E_MALFORMED, "not a drive path"));
        };
        let drive = drive.to_string();
        let rest: Vec<&str> = parts.collect();
        match (req.method(), rest.as_slice()) {
            (Method::Put, []) => self.create(&drive, &mut req).await,
            (Method::Get, []) => self.head(),
            (Method::Post, ["commit"]) => self.commit(&drive, &mut req).await,
            (Method::Post, ["missing"]) => self.missing(&drive, &mut req).await,
            (Method::Get, ["feed"]) => self.feed(&req),
            (Method::Get, ["nodes", id]) => self.node(id),
            (Method::Get, ["nodes", id, "versions"]) => self.versions(id),
            (Method::Get, ["stats"]) => self.stats(),
            (Method::Get, ["_dev", "check"]) if dev_mode(&self.env) => self.dev_check(),
            (Method::Post, ["_dev", "gc"]) if dev_mode(&self.env) => self.dev_gc(&drive, &req).await,
            (Method::Post, ["_dev", "panic"]) if dev_mode(&self.env) => self.dev_panic().await,
            _ => refused(&Refusal::new(E_MALFORMED, "no such route")),
        }
    }

    async fn alarm(&self) -> Result<Response> {
        let Some(drive) = self.drive_id()? else {
            return Response::ok("no drive");
        };
        let more = self.collect(&drive, self.grace_ms(), None).await?;
        if more {
            self.state.storage().set_alarm(1000i64).await?;
        } else {
            // Blobs let go of since are not past their grace period yet.
            self.schedule_collector().await?;
        }
        Response::ok("collected")
    }
}

impl Drive {
    fn sql(&self) -> SqlStorage {
        self.state.storage().sql()
    }

    fn drive_row(&self) -> Result<Option<DriveRow>> {
        Ok(self.sql().exec("SELECT epoch, seq, root, name, key, dedup FROM drive", None)?.to_array::<DriveRow>()?.into_iter().next())
    }

    /// The drive's id, remembered at creation for the alarm, which has no request to read it from.
    fn drive_id(&self) -> Result<Option<String>> {
        #[derive(Deserialize)]
        struct Row {
            id: String,
        }
        Ok(self.sql().exec("SELECT id FROM drive", None)?.to_array::<Row>()?.into_iter().next().map(|r| r.id))
    }

    fn grace_ms(&self) -> i64 {
        self.env.var("GC_GRACE_MS").ok().and_then(|v| v.to_string().parse().ok()).unwrap_or(DEFAULT_GRACE_MS)
    }

    fn count_refusal(&self, r: &Refusal) {
        *self.telemetry.borrow_mut().refused.entry(r.error.clone()).or_default() += 1;
    }

    // ------------------------------------------------------------ the drive

    async fn create(&self, drive: &str, req: &mut Request) -> Result<Response> {
        let Ok(body) = req.json::<CreateDrive>().await else {
            return refused(&Refusal::new(E_MALFORMED, "not a drive description"));
        };
        if let Err(why) = body.validate() {
            return refused(&Refusal::new(E_MALFORMED, why));
        }
        if !valid_id(drive) {
            return refused(&Refusal::new(E_MALFORMED, "drive id is not 16 bytes of hex"));
        }
        if let Some(existing) = self.drive_row()? {
            // Creating the same drive again is a retry; another root is another drive.
            return if existing.root == body.root {
                self.head()
            } else {
                refused(&Refusal::new(E_EXISTS, "a drive with this id exists"))
            };
        }
        let (sql, now, id) = (self.sql(), now_ms(), drive.to_string());
        let made = transaction_sync(&self.storage, move || {
            // The epoch tells this drive from an earlier one with the same id.
            sql.exec(
                "INSERT INTO drive (one, id, epoch, seq, root, name, key, dedup) VALUES (1, ?, ?, 1, ?, ?, ?, ?)",
                vec![text(&id), int(now), text(&body.root), text(&body.name), text(&body.key), text(&body.dedup)],
            )
            .map_err(storage)?;
            sql.exec(
                "INSERT INTO nodes (id, parent, kind, name_hash, name, key, seq) VALUES (?, NULL, ?, '', ?, ?, 1)",
                vec![text(&body.root), int(KIND_FOLDER as u64), text(&body.name), text(&body.key)],
            )
            .map_err(storage)?;
            Ok(())
        })?;
        if let Err(r) = made {
            return refused(&r);
        }
        log_line("drive", drive, &[("event", "created".into())]);
        self.head()
    }

    fn head(&self) -> Result<Response> {
        match self.drive_row()? {
            Some(d) => reply(&DriveHead { epoch: d.epoch, seq: d.seq, root: d.root, name: d.name, key: d.key, dedup: d.dedup }),
            None => refused(&Refusal::new(E_NO_DRIVE, "no such drive")),
        }
    }

    // -------------------------------------------------------------- commits

    async fn commit(&self, drive: &str, req: &mut Request) -> Result<Response> {
        let started = now_ms();
        let body = req.bytes().await?;
        let Ok(commit) = serde_json::from_slice::<Commit>(&body) else {
            return refused(&Refusal::new(E_MALFORMED, "not a commit"));
        };
        if let Err(why) = commit.validate() {
            return refused(&Refusal::new(E_MALFORMED, why));
        }
        let digest = sha256_hex(&body);
        drop(body);

        // Before anything waits: pin what the commit names, then look in
        // storage for the ones the drive does not already hold.
        let blobs = commit.blobs();
        let ids: Vec<String> = blobs.shards.iter().map(|h| format!("s/{h}")).chain(blobs.manifests.iter().map(|h| format!("m/{h}"))).collect();
        let _pinned = self.pin(&ids);
        let unknown: Vec<String> = ids.iter().filter(|id| !matches!(self.blob_state(id), Ok(Some((_, false))))).cloned().collect();
        if unknown.len() > MAX_NEW_BLOBS {
            let r = Refusal::new(E_TOO_LARGE, "too many new shards in one commit");
            self.count_refusal(&r);
            return refused(&r);
        }
        let found = self.look_up(drive, &unknown).await?;

        let (sql, ops) = (self.sql(), commit.ops.len() as u64);
        // All of it or none of it, and nothing else in between.
        let result = transaction_sync(&self.storage, move || apply(&sql, &commit, &digest, started, &found))?;
        let ms = now_ms().saturating_sub(started);
        match result {
            Ok(done) => {
                {
                    let mut t = self.telemetry.borrow_mut();
                    if done.replayed {
                        t.replays += 1;
                    } else {
                        t.commits += 1;
                        t.ops += ops;
                    }
                }
                let what = if done.replayed { "replay" } else { "ok" };
                log_line("commit", drive, &[("outcome", what.into()), ("seq", done.seq.into()), ("ops", ops.into()), ("ms", ms.into())]);
                if !done.replayed {
                    self.schedule_collector().await?;
                }
                reply(&done)
            }
            Err(r) => {
                self.count_refusal(&r);
                log_line("commit", drive, &[("outcome", r.error.clone().into()), ("ops", ops.into()), ("ms", ms.into())]);
                if r.error == E_MISSING {
                    // It may be waiting for a collector that was cut short.
                    self.schedule_collector().await?;
                }
                refused(&r)
            }
        }
    }

    fn pin(&self, ids: &[String]) -> Pinned {
        let mut pins = self.pins.borrow_mut();
        for id in ids {
            *pins.entry(id.clone()).or_default() += 1;
        }
        Pinned { pins: self.pins.clone(), ids: ids.to_vec() }
    }

    /// `(refs, dying)` of a blob the drive has a row for.
    fn blob_state(&self, id: &str) -> Result<Option<(u64, bool)>> {
        blob_state(&self.sql(), id)
    }

    /// Which of `ids` are in storage right now. A blob the collector is
    /// deleting does not count, whatever storage says.
    async fn look_up(&self, drive: &str, ids: &[String]) -> Result<HashSet<String>> {
        let bucket = self.env.bucket(BUCKET)?;
        let mut found = HashSet::new();
        for batch in ids.chunks(32) {
            let heads = batch.iter().map(|id| bucket.head(blob_key(drive, id)));
            for (id, head) in batch.iter().zip(futures_util::future::join_all(heads).await) {
                if head?.is_some() && !matches!(self.blob_state(id)?, Some((_, true))) {
                    found.insert(id.clone());
                }
            }
        }
        self.telemetry.borrow_mut().storage_lookups += ids.len() as u64;
        Ok(found)
    }

    /// Which of the named shards and manifests would a commit be refused for?
    async fn missing(&self, drive: &str, req: &mut Request) -> Result<Response> {
        let Ok(asked) = req.json::<Blobs>().await else {
            return refused(&Refusal::new(E_MALFORMED, "not a list of shards and manifests"));
        };
        if asked.len() > MAX_VERSION_SHARDS || !asked.shards.iter().chain(&asked.manifests).all(|h| ludy_proto::shard::valid_hash(h)) {
            return refused(&Refusal::new(E_MALFORMED, "a name is not a sha-256, or there are too many"));
        }
        if self.drive_row()?.is_none() {
            return refused(&Refusal::new(E_NO_DRIVE, "no such drive"));
        }
        let ids: Vec<String> = asked.shards.iter().map(|h| format!("s/{h}")).chain(asked.manifests.iter().map(|h| format!("m/{h}"))).collect();
        let _pinned = self.pin(&ids);
        let unknown: Vec<String> = ids.iter().filter(|id| !matches!(self.blob_state(id), Ok(Some((_, false))))).cloned().collect();
        let found = self.look_up(drive, &unknown).await?;
        let unknown: HashSet<String> = unknown.into_iter().collect();
        let lacking = |kind: &str, names: &[String]| -> Vec<String> {
            names.iter().filter(|h| { let id = format!("{kind}/{h}"); unknown.contains(&id) && !found.contains(&id) }).cloned().collect()
        };
        reply(&Blobs { shards: lacking("s", &asked.shards), manifests: lacking("m", &asked.manifests) })
    }

    // ----------------------------------------------------------------- reads

    fn feed(&self, req: &Request) -> Result<Response> {
        #[derive(Deserialize)]
        struct Query {
            #[serde(default)]
            since: u64,
            #[serde(default)]
            epoch: Option<u64>,
            #[serde(default)]
            limit: Option<usize>,
        }
        let Ok(q) = req.query::<Query>() else {
            return refused(&Refusal::new(E_MALFORMED, "since, epoch and limit are numbers"));
        };
        let Some(drive) = self.drive_row()? else {
            return refused(&Refusal::new(E_NO_DRIVE, "no such drive"));
        };
        if q.epoch.is_some_and(|e| e != drive.epoch) || q.since > drive.seq {
            return reply(&Feed { epoch: drive.epoch, seq: drive.seq, reset: true, more: false, nodes: vec![] });
        }
        let limit = q.limit.unwrap_or(FEED_PAGE).clamp(1, MAX_FEED_PAGE);
        let rows: Vec<NodeRow> = self
            .sql()
            .exec(&format!("SELECT {NODE_COLUMNS} FROM nodes WHERE seq > ? ORDER BY seq LIMIT ?"), vec![int(q.since), int(limit as u64)])?
            .to_array()?;
        let more = rows.len() == limit && rows.last().is_some_and(|r| r.seq < drive.seq);
        {
            let mut t = self.telemetry.borrow_mut();
            t.feed_pages += 1;
            t.feed_nodes += rows.len() as u64;
        }
        reply(&Feed { epoch: drive.epoch, seq: drive.seq, reset: false, more, nodes: rows.into_iter().map(NodeRow::info).collect() })
    }

    fn node(&self, id: &str) -> Result<Response> {
        match node(&self.sql(), id)? {
            Some(row) => reply(&row.info()),
            None => refused(&Refusal::new(E_NO_NODE, "no such node")),
        }
    }

    fn versions(&self, id: &str) -> Result<Response> {
        #[derive(Deserialize)]
        struct Row {
            id: u64,
            size: u64,
            manifest: String,
            meta: String,
            at_ms: u64,
        }
        if !node(&self.sql(), id)?.is_some_and(|n| n.deleted == 0) {
            return refused(&Refusal::new(E_NO_NODE, "no such node"));
        }
        let rows: Vec<Row> = self.sql().exec("SELECT id, size, manifest, meta, at_ms FROM versions WHERE node = ? ORDER BY id DESC", vec![text(id)])?.to_array()?;
        reply(&rows.into_iter().map(|r| VersionEntry { id: r.id, size: r.size, manifest: r.manifest, meta: r.meta, at_ms: r.at_ms }).collect::<Vec<_>>())
    }

    fn stats(&self) -> Result<Response> {
        let sql = self.sql();
        let count = |query: &str| -> Result<u64> { Ok(sql.exec(query, None)?.one::<Count>()?.n) };
        let drive = self.drive_row()?;
        reply(&serde_json::json!({
            "exists": drive.is_some(),
            "seq": drive.as_ref().map(|d| d.seq).unwrap_or(0),
            "epoch": drive.as_ref().map(|d| d.epoch).unwrap_or(0),
            "nodes": count("SELECT COUNT(*) AS n FROM nodes WHERE deleted = 0")?,
            "tombstones": count("SELECT COUNT(*) AS n FROM nodes WHERE deleted = 1")?,
            "versions": count("SELECT COUNT(*) AS n FROM versions")?,
            "blobs_in_use": count("SELECT COUNT(*) AS n FROM blobs WHERE refs > 0")?,
            "blobs_unused": count("SELECT COUNT(*) AS n FROM blobs WHERE refs = 0 AND dying = 0")?,
            "blobs_dying": count("SELECT COUNT(*) AS n FROM blobs WHERE dying = 1")?,
            "commits_remembered": count("SELECT COUNT(*) AS n FROM commits")?,
            "pinned": self.pins.borrow().len(),
            "database_bytes": sql.database_size(),
            "since_start": self.telemetry.borrow().clone(),
        }))
    }

    // ------------------------------------------------------------ collector

    /// Make sure the collector runs once unused blobs are past their grace
    /// period, and at once if an earlier run was cut short: what it marked
    /// is refused to every commit until it has finished.
    async fn schedule_collector(&self) -> Result<()> {
        let sql = self.sql();
        let interrupted = sql.exec("SELECT COUNT(*) AS n FROM blobs WHERE dying = 1", None)?.one::<Count>()?.n;
        if interrupted > 0 {
            // Soon, but never later than it is already set for: a client
            // retrying faster than this would otherwise push it back for ever.
            let soon = now_ms() as i64 + 50;
            if !self.state.storage().get_alarm().await?.is_some_and(|at| at <= soon) {
                self.state.storage().set_alarm(50i64).await?;
            }
            return Ok(());
        }
        let waiting = sql.exec("SELECT COUNT(*) AS n FROM blobs WHERE refs = 0", None)?.one::<Count>()?.n;
        if waiting > 0 && self.state.storage().get_alarm().await?.is_none() {
            self.state.storage().set_alarm(self.grace_ms().max(50)).await?;
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
    async fn collect(&self, drive: &str, grace_ms: i64, stop_after: Option<usize>) -> Result<bool> {
        #[derive(Deserialize)]
        struct Row {
            id: String,
        }
        let sql = self.sql();
        let cutoff = now_ms() as i64 - grace_ms;
        let rows: Vec<Row> = sql
            .exec("SELECT id FROM blobs WHERE dying = 1 OR (refs = 0 AND zero_at <= ?) ORDER BY dying DESC LIMIT ?", vec![Sv::Integer(cutoff), int(GC_BATCH as u64)])?
            .to_array()?;
        // Not the ones a commit in flight has named.
        let take: Vec<String> = rows.into_iter().map(|r| r.id).filter(|id| !self.pins.borrow().contains_key(id)).collect();
        if take.is_empty() {
            return Ok(false);
        }
        // Mark first, in one go, with no await before it: a commit either
        // ran before this and took its reference (then the row has refs and
        // is not marked), or runs after and sees the mark.
        let mut dying = Vec::new();
        for id in &take {
            let marked = sql.exec("UPDATE blobs SET dying = 1 WHERE id = ? AND refs = 0", vec![text(id)])?.rows_written() > 0;
            if marked || matches!(blob_state(&sql, id)?, Some((0, true))) {
                dying.push(id.clone());
            }
        }
        // Make the marks durable before deleting anything.
        self.state.storage().sync().await?;
        let bucket = self.env.bucket(BUCKET)?;
        if let Some(n) = stop_after {
            for id in dying.iter().take(n) {
                bucket.delete(blob_key(drive, id)).await?;
            }
            return Ok(true);
        }
        for batch in dying.chunks(32) {
            let deletes = batch.iter().map(|id| bucket.delete(blob_key(drive, id)));
            for result in futures_util::future::join_all(deletes).await {
                result?;
            }
        }
        for id in &dying {
            sql.exec("DELETE FROM blobs WHERE id = ? AND dying = 1", vec![text(id)])?;
        }
        {
            let mut t = self.telemetry.borrow_mut();
            t.gc_runs += 1;
            t.gc_deleted += dying.len() as u64;
        }
        log_line("gc", drive, &[("deleted", (dying.len() as u64).into())]);
        Ok(take.len() == GC_BATCH)
    }

    // ------------------------------------------------- for the chaos harness

    /// Recount everything that is stored twice and report what disagrees.
    fn dev_check(&self) -> Result<Response> {
        #[derive(Deserialize)]
        struct Version {
            node: String,
            manifest: String,
            shards: String,
        }
        #[derive(Deserialize)]
        struct Blob {
            id: String,
            refs: u64,
        }
        #[derive(Deserialize)]
        struct Id {
            id: String,
        }
        let sql = self.sql();
        let mut problems: Vec<String> = Vec::new();
        let mut note = |p: String| {
            if problems.len() < 20 {
                problems.push(p)
            }
        };

        // Blob references, recounted from the versions.
        let mut want: HashMap<String, u64> = HashMap::new();
        let versions: Vec<Version> = sql.exec("SELECT node, manifest, shards FROM versions", None)?.to_array()?;
        for v in &versions {
            *want.entry(format!("m/{}", v.manifest)).or_default() += 1;
            for i in (0..v.shards.len()).step_by(64) {
                *want.entry(format!("s/{}", &v.shards[i..(i + 64).min(v.shards.len())])).or_default() += 1;
            }
        }
        let blobs: Vec<Blob> = sql.exec("SELECT id, refs FROM blobs", None)?.to_array()?;
        for b in &blobs {
            let expected = want.remove(&b.id).unwrap_or(0);
            if b.refs != expected {
                note(format!("blob {} has {} references, its versions say {expected}", b.id, b.refs));
            }
        }
        for (id, n) in want {
            note(format!("blob {id} is used by {n} versions and has no row"));
        }
        let ids = |query: &str| -> Result<Vec<String>> { Ok(sql.exec(query, None)?.to_array::<Id>()?.into_iter().map(|r| r.id).collect()) };
        for id in ids("SELECT id FROM blobs WHERE refs > 0 AND dying = 1")? {
            note(format!("blob {id} is in use and being deleted"));
        }
        for id in ids("SELECT n.id FROM nodes n JOIN nodes p ON p.id = n.parent WHERE n.deleted = 0 AND p.deleted = 1")? {
            note(format!("node {id} is live under a deleted parent"));
        }
        for id in ids("SELECT n.id FROM nodes n LEFT JOIN nodes p ON p.id = n.parent WHERE n.parent IS NOT NULL AND p.id IS NULL")? {
            note(format!("node {id} has a parent that does not exist"));
        }
        for id in ids("SELECT n.id FROM nodes n JOIN nodes p ON p.id = n.parent WHERE n.deleted = 0 AND p.kind = 0")? {
            note(format!("node {id} is inside a file"));
        }
        for id in ids("SELECT id FROM nodes WHERE parent IS NULL AND id NOT IN (SELECT root FROM drive)")? {
            note(format!("node {id} has no parent and is not the root"));
        }
        for id in ids("SELECT MIN(id) AS id FROM nodes WHERE deleted = 0 AND parent IS NOT NULL GROUP BY parent, name_hash HAVING COUNT(*) > 1")? {
            note(format!("node {id} shares its name with a sibling"));
        }
        for id in ids("SELECT MIN(id) AS id FROM nodes GROUP BY seq HAVING COUNT(*) > 1")? {
            note(format!("node {id} shares its sequence number with another"));
        }
        for id in ids("SELECT id FROM nodes WHERE seq > (SELECT seq FROM drive)")? {
            note(format!("node {id} is ahead of the drive's sequence number"));
        }
        // A file's current version is its newest version row, and a deleted node has none.
        for id in ids(
            "SELECT n.id FROM nodes n WHERE n.deleted = 0 AND n.kind = 0 AND \
             COALESCE(n.version, 0) != COALESCE((SELECT MAX(v.id) FROM versions v WHERE v.node = n.id), 0)",
        )? {
            note(format!("file {id} does not point at its newest version"));
        }
        for id in ids("SELECT DISTINCT v.node AS id FROM versions v JOIN nodes n ON n.id = v.node WHERE n.deleted = 1 OR n.kind = 1")? {
            note(format!("node {id} is deleted or a folder and still has versions"));
        }
        for id in ids("SELECT DISTINCT v.node AS id FROM versions v LEFT JOIN nodes n ON n.id = v.node WHERE n.id IS NULL")? {
            note(format!("versions of {id} have no node"));
        }
        for v in &versions {
            if v.shards.len() % 64 != 0 {
                note(format!("a version of {} has a shard list of {} characters", v.node, v.shards.len()));
            }
        }
        for id in ids(&format!("SELECT node AS id FROM versions GROUP BY node HAVING COUNT(*) > {HISTORY}"))? {
            note(format!("file {id} keeps more than {HISTORY} versions"));
        }
        // A cycle: a live node from which walking up never reaches the root.
        for id in ids(
            "WITH RECURSIVE up (start, at, depth) AS ( \
               SELECT id, parent, 0 FROM nodes WHERE deleted = 0 AND parent IS NOT NULL \
               UNION ALL SELECT up.start, n.parent, up.depth + 1 FROM up JOIN nodes n ON n.id = up.at WHERE up.depth < 10000 \
             ) SELECT DISTINCT start AS id FROM up WHERE depth >= 10000",
        )? {
            note(format!("node {id} is in a loop"));
        }
        reply(&serde_json::json!({ "ok": problems.is_empty(), "problems": problems }))
    }

    /// Run the collector now, with no grace period, until nothing is left;
    /// or with `?stop_after=n`, one run that stops part-way.
    async fn dev_gc(&self, drive: &str, req: &Request) -> Result<Response> {
        #[derive(Deserialize)]
        struct Query {
            stop_after: Option<usize>,
        }
        match req.query::<Query>().ok().and_then(|q| q.stop_after) {
            Some(n) => {
                self.collect(drive, 0, Some(n)).await?;
            }
            None => while self.collect(drive, 0, None).await? {},
        }
        self.stats()
    }

    /// Write inside a transaction the way a commit does, then panic. The
    /// harness then asks whether the write is there and how long the drive
    /// was out.
    async fn dev_panic(&self) -> Result<Response> {
        let sql = self.sql();
        let done: std::result::Result<(), Refusal> = transaction_sync(&self.storage, move || {
            sql.exec("INSERT INTO commits (id, digest, seq, at_ms) VALUES ('panic-probe', '', 0, 0)", None).map_err(storage)?;
            panic!("dev: panic inside a transaction, after a write");
        })?;
        Response::ok(format!("unreachable: {done:?}"))
    }
}

/// Run `body` inside the runtime's synchronous storage transaction
/// (`transactionSync`, which the `worker` crate does not bind): what it
/// writes is kept if it returns `Ok` and undone if it returns a refusal.
///
/// Synchronous matters. The asynchronous `transaction` waits for a promise,
/// and if the body panics that promise never settles: the drive then answers
/// nothing for 30 seconds, until the runtime gives up on the request. Here a
/// panic is an exception thrown straight through the transaction, which
/// undoes it and fails the one request.
fn transaction_sync<T: 'static>(
    storage_object: &JsValue,
    body: impl FnOnce() -> std::result::Result<T, Refusal> + 'static,
) -> Result<std::result::Result<T, Refusal>> {
    let slot: Rc<RefCell<Option<std::result::Result<T, Refusal>>>> = Rc::new(RefCell::new(None));
    let (out, mut body) = (slot.clone(), Some(body));
    // Returning `Err` makes the glue throw, which is what rolls the transaction back.
    let closure = Closure::wrap(Box::new(move || -> std::result::Result<JsValue, JsValue> {
        let Some(body) = body.take() else {
            return Err(JsValue::from_str("the transaction body ran twice"));
        };
        let result = body();
        let refused = result.is_err();
        *out.borrow_mut() = Some(result);
        if refused {
            Err(JsValue::from_str("refused"))
        } else {
            Ok(JsValue::UNDEFINED)
        }
    }) as Box<dyn FnMut() -> std::result::Result<JsValue, JsValue>>);
    let run: js_sys::Function = js_sys::Reflect::get(storage_object, &JsValue::from_str("transactionSync"))?
        .dyn_into()
        .map_err(|_| Error::RustError("this storage has no transactionSync".into()))?;
    let called = run.call1(storage_object, closure.as_ref());
    let result = slot.borrow_mut().take();
    match (result, called) {
        (Some(Ok(value)), Ok(_)) => Ok(Ok(value)),
        (Some(Err(refusal)), _) => Ok(Err(refusal)),
        // The body was fine and storage could not keep what it wrote.
        (Some(Ok(_)), Err(e)) => Err(e.into()),
        (None, Err(e)) => Err(e.into()),
        (None, Ok(_)) => Err(Error::RustError("the transaction did not run".into())),
    }
}

fn node(sql: &SqlStorage, id: &str) -> Result<Option<NodeRow>> {
    Ok(sql.exec(&format!("SELECT {NODE_COLUMNS} FROM nodes WHERE id = ?"), vec![text(id)])?.to_array::<NodeRow>()?.into_iter().next())
}

fn blob_state(sql: &SqlStorage, id: &str) -> Result<Option<(u64, bool)>> {
    #[derive(Deserialize)]
    struct Row {
        refs: u64,
        dying: u8,
    }
    Ok(sql.exec("SELECT refs, dying FROM blobs WHERE id = ?", vec![text(id)])?.to_array::<Row>()?.into_iter().next().map(|r| (r.refs, r.dying != 0)))
}

/// A storage error inside `apply` is a refusal too: the transaction rolls back.
fn storage(e: Error) -> Refusal {
    Refusal::new("storage", e.to_string())
}

fn live(sql: &SqlStorage, id: &str, what: &str) -> std::result::Result<NodeRow, Refusal> {
    match node(sql, id).map_err(storage)? {
        Some(row) if row.deleted == 0 => Ok(row),
        _ => Err(Refusal::new(E_NO_NODE, format!("{what} {id} does not exist"))),
    }
}

fn name_is_free(sql: &SqlStorage, parent: &str, name_hash: &str, except: &str) -> std::result::Result<(), Refusal> {
    let taken = sql
        .exec("SELECT COUNT(*) AS n FROM nodes WHERE parent = ? AND name_hash = ? AND deleted = 0 AND id != ?", vec![text(parent), text(name_hash), text(except)])
        .and_then(|c| c.one::<Count>())
        .map_err(storage)?
        .n;
    if taken > 0 {
        return Err(Refusal::new(E_EXISTS, "that name is taken in this folder"));
    }
    Ok(())
}

/// Let go of one version's manifest and shards.
fn release(sql: &SqlStorage, manifest: &str, shards: &str) -> Result<()> {
    sql.exec("UPDATE blobs SET refs = refs - 1 WHERE id = ?", vec![text(&format!("m/{manifest}"))])?;
    for i in (0..shards.len()).step_by(64) {
        sql.exec("UPDATE blobs SET refs = refs - 1 WHERE id = ?", vec![text(&format!("s/{}", &shards[i..i + 64]))])?;
    }
    Ok(())
}

/// Check a commit against the drive as it is now and apply it. Runs inside a
/// transaction: returning an error undoes everything it wrote.
fn apply(sql: &SqlStorage, commit: &Commit, digest: &str, now: u64, found: &HashSet<String>) -> std::result::Result<Committed, Refusal> {
    #[derive(Deserialize)]
    struct Seen {
        digest: String,
        seq: u64,
    }
    #[derive(Deserialize)]
    struct Old {
        id: u64,
        manifest: String,
        shards: String,
    }
    #[derive(Deserialize)]
    struct Id {
        id: String,
    }

    let drive: Vec<DriveRow> = sql.exec("SELECT epoch, seq, root, name, key, dedup FROM drive", None).and_then(|c| c.to_array()).map_err(storage)?;
    let Some(drive) = drive.into_iter().next() else {
        return Err(Refusal::new(E_NO_DRIVE, "no such drive"));
    };

    // A commit that was applied before is answered, not applied again.
    let seen: Vec<Seen> = sql.exec("SELECT digest, seq FROM commits WHERE id = ?", vec![text(&commit.id)]).and_then(|c| c.to_array()).map_err(storage)?;
    if let Some(seen) = seen.into_iter().next() {
        return if seen.digest == digest {
            Ok(Committed { seq: seen.seq, replayed: true })
        } else {
            Err(Refusal::new(E_COMMIT_REUSED, "this commit id was used for a different commit"))
        };
    }

    let mut seq = drive.seq;
    for (index, op) in commit.ops.iter().enumerate() {
        let mut one = || -> std::result::Result<(), Refusal> {
            match op {
            Op::Create { id, parent, kind, name_hash, name, key } => {
                let folder = live(sql, parent, "parent")?;
                if folder.kind != KIND_FOLDER {
                    return Err(Refusal::new(E_WRONG_KIND, "the parent is a file"));
                }
                if node(sql, id).map_err(storage)?.is_some() {
                    return Err(Refusal::new(E_EXISTS, format!("node {id} exists")));
                }
                name_is_free(sql, parent, name_hash, id)?;
                seq += 1;
                let kind = if *kind == Kind::Folder { KIND_FOLDER } else { KIND_FILE };
                sql.exec(
                    "INSERT INTO nodes (id, parent, kind, name_hash, name, key, seq) VALUES (?, ?, ?, ?, ?, ?, ?)",
                    vec![text(id), text(parent), int(kind as u64), text(name_hash), text(name), text(key), int(seq)],
                )
                .map_err(storage)?;
            }
            Op::Put { node: id, base, size, manifest, shards, meta } => {
                let file = live(sql, id, "file")?;
                if file.kind != KIND_FILE {
                    return Err(Refusal::new(E_WRONG_KIND, "a folder has no content"));
                }
                let current = file.version.unwrap_or(0);
                if base.is_some_and(|b| b != current) {
                    let mut r = Refusal::new(E_CONFLICT, "the file has changed since the version this was based on");
                    r.current = Some(current);
                    return Err(r);
                }
                // Every blob must be held by the drive already, or have been
                // found in storage just now; never one that is being deleted.
                let mut missing = Blobs::default();
                let mut wanted: Vec<String> = Vec::with_capacity(shards.len() + 1);
                for (kind, hash) in std::iter::once(("m", manifest)).chain(shards.iter().map(|h| ("s", h))) {
                    let blob = format!("{kind}/{hash}");
                    let usable = match blob_state(sql, &blob).map_err(storage)? {
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
                    return Err(r);
                }
                for blob in &wanted {
                    sql.exec(
                        "INSERT INTO blobs (id, refs) VALUES (?, 1) ON CONFLICT (id) DO UPDATE SET refs = refs + 1, zero_at = NULL",
                        vec![text(blob)],
                    )
                    .map_err(storage)?;
                }
                seq += 1;
                sql.exec(
                    "INSERT INTO versions (node, id, size, manifest, meta, shards, at_ms) VALUES (?, ?, ?, ?, ?, ?, ?)",
                    vec![text(id), int(seq), int(*size), text(manifest), text(meta), text(&shards.concat()), int(now)],
                )
                .map_err(storage)?;
                sql.exec(
                    "UPDATE nodes SET version = ?, size = ?, manifest = ?, meta = ?, seq = ? WHERE id = ?",
                    vec![int(seq), int(*size), text(manifest), text(meta), int(seq), text(id)],
                )
                    .map_err(storage)?;
                // History is capped: the oldest versions beyond it let go.
                let old: Vec<Old> = sql
                    .exec("SELECT id, manifest, shards FROM versions WHERE node = ? ORDER BY id DESC LIMIT -1 OFFSET ?", vec![text(id), int(HISTORY as u64)])
                    .and_then(|c| c.to_array())
                    .map_err(storage)?;
                for v in old {
                    release(sql, &v.manifest, &v.shards).map_err(storage)?;
                    sql.exec("DELETE FROM versions WHERE node = ? AND id = ?", vec![text(id), int(v.id)]).map_err(storage)?;
                }
            }
            Op::Move { node: id, parent, name_hash, name, key } => {
                let moving = live(sql, id, "node")?;
                if moving.parent.is_none() {
                    return Err(Refusal::new(E_WRONG_KIND, "the root does not move"));
                }
                let folder = live(sql, parent, "parent")?;
                if folder.kind != KIND_FOLDER {
                    return Err(Refusal::new(E_WRONG_KIND, "the parent is a file"));
                }
                // Walking up from the new parent must not pass the node itself.
                let mut at = Some(folder);
                let mut steps = 0;
                while let Some(here) = at {
                    if here.id == *id {
                        return Err(Refusal::new(E_CYCLE, "a folder cannot move into itself"));
                    }
                    steps += 1;
                    if steps > 100_000 {
                        return Err(Refusal::new("storage", "the tree has a loop"));
                    }
                    at = match here.parent {
                        Some(up) => node(sql, &up).map_err(storage)?,
                        None => None,
                    };
                }
                name_is_free(sql, parent, name_hash, id)?;
                seq += 1;
                sql.exec(
                    "UPDATE nodes SET parent = ?, name_hash = ?, name = ?, key = ?, seq = ? WHERE id = ?",
                    vec![text(parent), text(name_hash), text(name), text(key), int(seq), text(id)],
                )
                .map_err(storage)?;
            }
            Op::Delete { node: id } => {
                let doomed = live(sql, id, "node")?;
                if doomed.parent.is_none() {
                    return Err(Refusal::new(E_WRONG_KIND, "the root cannot be deleted"));
                }
                let subtree: Vec<Id> = sql
                    .exec(
                        "WITH RECURSIVE sub (id) AS (SELECT ? UNION ALL SELECT n.id FROM nodes n JOIN sub ON n.parent = sub.id WHERE n.deleted = 0) SELECT id FROM sub",
                        vec![text(id)],
                    )
                    .and_then(|c| c.to_array())
                    .map_err(storage)?;
                for n in subtree {
                    let old: Vec<Old> = sql.exec("SELECT id, manifest, shards FROM versions WHERE node = ?", vec![text(&n.id)]).and_then(|c| c.to_array()).map_err(storage)?;
                    for v in &old {
                        release(sql, &v.manifest, &v.shards).map_err(storage)?;
                    }
                    sql.exec("DELETE FROM versions WHERE node = ?", vec![text(&n.id)]).map_err(storage)?;
                    seq += 1;
                    sql.exec("UPDATE nodes SET deleted = 1, version = NULL, size = 0, manifest = NULL, meta = NULL, seq = ? WHERE id = ?", vec![int(seq), text(&n.id)])
                        .map_err(storage)?;
                }
            }
            }
            Ok(())
        };
        // Say which op it was: the sender may want to drop just that one.
        one().map_err(|mut refusal| {
            refusal.op = Some(index);
            refusal
        })?;
    }

    sql.exec("UPDATE blobs SET zero_at = ? WHERE refs = 0 AND zero_at IS NULL", vec![int(now)]).map_err(storage)?;
    sql.exec("UPDATE drive SET seq = ?", vec![int(seq)]).map_err(storage)?;
    sql.exec("INSERT INTO commits (id, digest, seq, at_ms) VALUES (?, ?, ?, ?)", vec![text(&commit.id), text(digest), int(seq), int(now)]).map_err(storage)?;
    sql.exec("DELETE FROM commits WHERE at_ms < ?", vec![Sv::Integer(now as i64 - COMMIT_MEMORY_MS)]).map_err(storage)?;
    Ok(Committed { seq, replayed: false })
}
