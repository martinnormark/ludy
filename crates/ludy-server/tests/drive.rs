//! The drive's rules, on SQLite in memory with a blob store, a timer and a
//! clock that are plain values. No server is started, so these run in
//! milliseconds, and the ports can be made to fail, which no real provider
//! can be asked to do.
//!
//! The chaos suites in `ludy-core/tests` check the same logic on a real
//! provider under kills and lost messages; they are the conformance test.

use std::collections::BTreeSet;
use std::future::Future;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use futures_executor::block_on;
use ludy_proto::tree::*;
use ludy_server::{Abort, Config, Drive, Fault, Platform, Request, Response, Rows, Sql, Value};

// ------------------------------------------------------------ the platform

#[derive(Clone)]
struct MemSql {
    db: Arc<Mutex<rusqlite::Connection>>,
    /// Statements left until one fails; negative: none will.
    fail_in: Arc<AtomicI64>,
}

impl Sql for MemSql {
    async fn exec(&self, sql: &str, params: Vec<Value>) -> Result<Rows, Fault> {
        if self.fail_in.fetch_sub(1, Ordering::SeqCst) == 0 {
            return Err(Fault::new("the database failed, as asked"));
        }
        let db = self.db.lock().unwrap();
        let fault = |e: rusqlite::Error| Fault(e.to_string());
        let mut statement = db.prepare_cached(sql).map_err(fault)?;
        let bound: Vec<rusqlite::types::Value> = params
            .into_iter()
            .map(|v| match v {
                Value::Null => rusqlite::types::Value::Null,
                Value::Int(n) => rusqlite::types::Value::Integer(n),
                Value::Text(s) => rusqlite::types::Value::Text(s),
            })
            .collect();
        let columns = statement.column_count();
        let read_only = statement.readonly();
        let mut out = Rows::default();
        let mut rows = statement.query(rusqlite::params_from_iter(bound)).map_err(fault)?;
        while let Some(row) = rows.next().map_err(fault)? {
            let mut values = Vec::with_capacity(columns);
            for i in 0..columns {
                values.push(match row.get_ref(i).map_err(fault)? {
                    rusqlite::types::ValueRef::Null => Value::Null,
                    rusqlite::types::ValueRef::Integer(n) => Value::Int(n),
                    rusqlite::types::ValueRef::Text(t) => Value::Text(String::from_utf8_lossy(t).into_owned()),
                    other => return Err(Fault(format!("a column of type {:?}", other.data_type()))),
                });
            }
            out.rows.push(values);
        }
        drop(rows);
        if !read_only {
            out.changed = db.changes();
        }
        Ok(out)
    }
}

/// Rolls the transaction back unless told it was committed: also when the body panics.
struct Open<'a> {
    db: &'a Mutex<rusqlite::Connection>,
    done: bool,
}

impl Drop for Open<'_> {
    fn drop(&mut self) {
        if !self.done {
            let _ = self.db.lock().unwrap_or_else(|e| e.into_inner()).execute_batch("ROLLBACK");
        }
    }
}

#[derive(Clone)]
struct Mem {
    sql: MemSql,
    blobs: Arc<Mutex<BTreeSet<String>>>,
    timer: Arc<Mutex<Option<u64>>>,
    clock: Arc<AtomicU64>,
    grace_ms: Arc<AtomicI64>,
    log: Arc<Mutex<Vec<String>>>,
    /// The blob store answers nothing but failure.
    store_down: Arc<Mutex<bool>>,
}

impl Mem {
    fn new() -> Mem {
        Mem {
            sql: MemSql { db: Arc::new(Mutex::new(rusqlite::Connection::open_in_memory().unwrap())), fail_in: Arc::new(AtomicI64::new(-1)) },
            blobs: Arc::default(),
            timer: Arc::default(),
            clock: Arc::new(AtomicU64::new(1_000_000)),
            grace_ms: Arc::new(AtomicI64::new(60_000)),
            log: Arc::default(),
            store_down: Arc::default(),
        }
    }
}

impl Platform for Mem {
    type Sql = MemSql;
    type Tx = MemSql;

    fn sql(&self) -> &MemSql {
        &self.sql
    }

    async fn transaction<T, F, Fut>(&self, _name: &'static str, body: F) -> Result<Result<T, Refusal>, Fault>
    where
        T: 'static,
        F: FnOnce(MemSql) -> Fut + 'static,
        Fut: Future<Output = Result<T, Abort>> + 'static,
    {
        let fault = |e: rusqlite::Error| Fault(e.to_string());
        self.sql.db.lock().unwrap().execute_batch("BEGIN IMMEDIATE").map_err(fault)?;
        let mut open = Open { db: &self.sql.db, done: false };
        match body(self.sql.clone()).await {
            Ok(value) => {
                self.sql.db.lock().unwrap().execute_batch("COMMIT").map_err(fault)?;
                open.done = true;
                Ok(Ok(value))
            }
            Err(Abort::Refused(r)) => Ok(Err(r)),
            Err(Abort::Fault(f)) => Err(f),
        }
    }

    fn now_ms(&self) -> u64 {
        self.clock.load(Ordering::SeqCst)
    }

    fn config(&self) -> Config {
        Config { gc_grace_ms: self.grace_ms.load(Ordering::SeqCst), dev: true }
    }

    fn log(&self, line: &str) {
        self.log.lock().unwrap().push(line.to_string());
    }

    async fn blob_exists(&self, key: &str) -> Result<bool, Fault> {
        if *self.store_down.lock().unwrap() {
            return Err(Fault::new("the blob store failed, as asked"));
        }
        Ok(self.blobs.lock().unwrap().contains(key))
    }

    async fn blob_delete(&self, key: &str) -> Result<(), Fault> {
        if *self.store_down.lock().unwrap() {
            return Err(Fault::new("the blob store failed, as asked"));
        }
        self.blobs.lock().unwrap().remove(key);
        Ok(())
    }

    async fn timer(&self) -> Result<Option<u64>, Fault> {
        Ok(*self.timer.lock().unwrap())
    }

    async fn set_timer(&self, at_ms: u64) -> Result<(), Fault> {
        *self.timer.lock().unwrap() = Some(at_ms);
        Ok(())
    }

    async fn flush(&self) -> Result<(), Fault> {
        Ok(())
    }

    async fn database_bytes(&self) -> Result<u64, Fault> {
        Ok(0)
    }
}

// ------------------------------------------------------------- the rig

const DRIVE: &str = "d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0";

fn id(n: u8) -> String {
    format!("{n:02x}").repeat(16)
}

fn hash(n: u8) -> String {
    format!("{n:02x}").repeat(32)
}

struct Rig {
    mem: Mem,
    drive: Drive<Mem>,
    commits: u8,
}

impl Rig {
    /// A drive whose root is `id(1)`.
    fn new() -> Rig {
        let mem = Mem::new();
        let drive = Drive::new(mem.clone());
        block_on(drive.open()).unwrap();
        let mut rig = Rig { mem, drive, commits: 0 };
        let made = rig.send("PUT", "", &CreateDrive { root: id(1), name: "aa".into(), key: "bb".into(), dedup: "cc".into() });
        assert_eq!(made.status, 200);
        rig
    }

    fn request(&self, method: &str, rest: &str, body: Vec<u8>) -> Result<Response, Fault> {
        let (path, query) = rest.split_once('?').unwrap_or((rest, ""));
        block_on(self.drive.handle(Request { method: method.into(), path: format!("/drives/{DRIVE}{path}"), query: query.into(), body }))
    }

    fn send<T: serde::Serialize>(&mut self, method: &str, rest: &str, body: &T) -> Response {
        self.request(method, rest, serde_json::to_vec(body).unwrap()).expect("the platform holds")
    }

    fn get<T: serde::de::DeserializeOwned>(&self, rest: &str) -> T {
        let r = self.request("GET", rest, vec![]).unwrap();
        assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
        serde_json::from_slice(&r.body).unwrap()
    }

    /// Commit `ops` under a fresh commit id.
    fn commit(&mut self, ops: Vec<Op>) -> Result<Committed, Refusal> {
        self.commits += 1;
        let commit = Commit { id: format!("c0{:02x}", self.commits).repeat(8), ops };
        self.commit_as(&commit)
    }

    fn commit_as(&mut self, commit: &Commit) -> Result<Committed, Refusal> {
        let r = self.send("POST", "/commit", commit);
        if r.status == 200 {
            Ok(serde_json::from_slice(&r.body).unwrap())
        } else {
            let refusal: Refusal = serde_json::from_slice(&r.body).unwrap();
            assert_eq!(r.status, refusal.status());
            Err(refusal)
        }
    }

    fn store(&self, blob: &str) {
        self.mem.blobs.lock().unwrap().insert(format!("d/{DRIVE}/{blob}"));
    }

    fn stored(&self) -> Vec<String> {
        let prefix = format!("d/{DRIVE}/");
        self.mem.blobs.lock().unwrap().iter().map(|k| k[prefix.len()..].to_string()).collect()
    }

    /// The drive's own recount of everything it stores twice.
    fn check(&self) {
        let report: serde_json::Value = self.get("/_dev/check");
        assert!(report["ok"] == true, "the drive's own check failed: {}", report["problems"]);
    }

    fn stats(&self) -> serde_json::Value {
        self.get("/stats")
    }

    fn seq(&self) -> u64 {
        self.get::<DriveHead>("").seq
    }
}

fn file(n: u8, parent: u8, name: u8) -> Op {
    Op::Create { id: id(n), parent: id(parent), kind: Kind::File, name_hash: hash(name), name: "01".into(), key: "02".into() }
}

fn folder(n: u8, parent: u8, name: u8) -> Op {
    Op::Create { id: id(n), parent: id(parent), kind: Kind::Folder, name_hash: hash(name), name: "01".into(), key: "02".into() }
}

fn put(n: u8, base: Option<u64>, manifest: u8, shards: &[u8]) -> Op {
    Op::Put { node: id(n), base, size: 10, manifest: hash(manifest), shards: shards.iter().map(|s| hash(*s)).collect(), meta: "0a".into() }
}

// ------------------------------------------------------------- the tests

#[test]
fn creating_a_drive_twice_is_a_retry_and_another_root_is_refused() {
    let mut rig = Rig::new();
    let again = rig.send("PUT", "", &CreateDrive { root: id(1), name: "aa".into(), key: "bb".into(), dedup: "cc".into() });
    assert_eq!(again.status, 200);
    let head: DriveHead = serde_json::from_slice(&again.body).unwrap();
    assert_eq!((head.seq, head.root), (1, id(1)));
    let other = rig.send("PUT", "", &CreateDrive { root: id(9), name: "aa".into(), key: "bb".into(), dedup: "cc".into() });
    assert_eq!(other.status, 409);
    assert_eq!(rig.mem.log.lock().unwrap().iter().filter(|l| l.contains("\"created\"")).count(), 1);
    rig.check();
}

#[test]
fn a_commit_is_refused_until_its_blobs_are_stored_and_then_applied_once() {
    let mut rig = Rig::new();
    let commit = Commit { id: id(0xc1), ops: vec![file(2, 1, 3), put(2, Some(0), 10, &[11, 12, 11])] };
    let refusal = rig.commit_as(&commit).unwrap_err();
    assert_eq!((refusal.error.as_str(), refusal.op), (E_MISSING, Some(1)));
    assert_eq!(refusal.missing, Blobs { shards: vec![hash(11), hash(12)], manifests: vec![hash(10)] });
    // Nothing of it was applied, the create included.
    assert_eq!(rig.seq(), 1);
    assert_eq!(rig.request("GET", &format!("/nodes/{}", id(2)), vec![]).unwrap().status, 404);

    let lacking: Blobs = serde_json::from_slice(&rig.send("POST", "/missing", &Blobs { shards: vec![hash(11), hash(12)], manifests: vec![hash(10)] }).body).unwrap();
    assert_eq!(lacking.len(), 3);
    for blob in [format!("m/{}", hash(10)), format!("s/{}", hash(11)), format!("s/{}", hash(12))] {
        rig.store(&blob);
    }
    assert_eq!(rig.commit_as(&commit).unwrap(), Committed { seq: 3, replayed: false });
    // The same bytes again are answered, not applied.
    assert_eq!(rig.commit_as(&commit).unwrap(), Committed { seq: 3, replayed: true });
    assert_eq!(rig.seq(), 3);
    // The same id for something else is refused.
    let reused = rig.commit_as(&Commit { id: id(0xc1), ops: vec![file(5, 1, 6)] }).unwrap_err();
    assert_eq!(reused.error, E_COMMIT_REUSED);

    let node: NodeInfo = rig.get(&format!("/nodes/{}", id(2)));
    assert_eq!(node.version.unwrap().id, 3);
    // A shard used twice by one version is counted twice.
    assert_eq!(rig.stats()["blobs_in_use"], 3);
    rig.check();
}

#[test]
fn the_rules_of_the_tree() {
    let mut rig = Rig::new();
    rig.commit(vec![folder(2, 1, 20), folder(3, 2, 30), file(4, 3, 40)]).unwrap();
    let refused = |r: Result<Committed, Refusal>| r.unwrap_err().error;
    assert_eq!(refused(rig.commit(vec![file(5, 1, 20)])), E_EXISTS, "a taken name");
    assert_eq!(refused(rig.commit(vec![file(4, 1, 21)])), E_EXISTS, "a taken id");
    assert_eq!(refused(rig.commit(vec![file(5, 4, 21)])), E_WRONG_KIND, "a file as a parent");
    assert_eq!(refused(rig.commit(vec![file(5, 9, 21)])), E_NO_NODE, "no such parent");
    assert_eq!(refused(rig.commit(vec![Op::Move { node: id(2), parent: id(3), name_hash: hash(22), name: "01".into(), key: "02".into() }])), E_CYCLE);
    assert_eq!(refused(rig.commit(vec![Op::Delete { node: id(1) }])), E_WRONG_KIND, "the root stays");
    assert_eq!(refused(rig.commit(vec![put(2, None, 10, &[])])), E_WRONG_KIND, "a folder has no content");
    // The second op is refused, so the first is not applied either.
    let both = rig.commit(vec![file(6, 1, 60), file(7, 1, 20)]).unwrap_err();
    assert_eq!((both.error.as_str(), both.op), (E_EXISTS, Some(1)));
    assert_eq!(rig.request("GET", &format!("/nodes/{}", id(6)), vec![]).unwrap().status, 404);
    assert_eq!(rig.seq(), 4);

    // A rename frees the old name; a deleted folder takes everything under it.
    rig.commit(vec![Op::Move { node: id(4), parent: id(1), name_hash: hash(41), name: "01".into(), key: "02".into() }, file(8, 3, 40)]).unwrap();
    rig.commit(vec![Op::Delete { node: id(2) }]).unwrap();
    let feed: Feed = rig.get("/feed?since=0");
    let state: Vec<(String, bool)> = feed.nodes.iter().map(|n| (n.id.clone(), n.deleted)).collect();
    assert_eq!(state, vec![(id(1), false), (id(4), false), (id(2), true), (id(3), true), (id(8), true)]);
    assert_eq!(rig.stats()["tombstones"], 3);
    rig.check();
}

#[test]
fn a_put_with_a_stale_base_is_a_conflict_and_history_is_capped() {
    let mut rig = Rig::new();
    rig.commit(vec![file(2, 1, 3)]).unwrap();
    for n in 0..=HISTORY as u8 {
        rig.store(&format!("m/{}", hash(100 + n)));
        rig.store(&format!("s/{}", hash(50 + n)));
    }
    let first = rig.commit(vec![put(2, Some(0), 100, &[50])]).unwrap().seq;
    let conflict = rig.commit(vec![put(2, Some(0), 101, &[51])]).unwrap_err();
    assert_eq!((conflict.error.as_str(), conflict.current, conflict.status()), (E_CONFLICT, Some(first), 412));
    for n in 1..=HISTORY as u8 {
        rig.commit(vec![put(2, None, 100 + n, &[50 + n])]).unwrap();
    }
    let versions: Vec<VersionEntry> = rig.get(&format!("/nodes/{}/versions", id(2)));
    assert_eq!(versions.len(), HISTORY);
    assert!(versions.iter().all(|v| v.id != first), "the oldest version let go");
    let stats = rig.stats();
    assert_eq!((stats["blobs_in_use"].as_u64(), stats["blobs_unused"].as_u64()), (Some(2 * HISTORY as u64), Some(2)));
    rig.check();
}

#[test]
fn the_feed_pages_and_resets() {
    let mut rig = Rig::new();
    rig.commit((2..12).map(|n| file(n, 1, n)).collect()).unwrap();
    let page: Feed = rig.get("/feed?since=0&limit=4");
    assert_eq!((page.nodes.len(), page.more, page.reset, page.seq), (4, true, false, 11));
    let rest: Feed = rig.get(&format!("/feed?since={}&limit=100&epoch={}", page.nodes[3].seq, page.epoch));
    assert_eq!((rest.nodes.len(), rest.more), (7, false));
    assert!(rig.get::<Feed>("/feed?since=99").reset, "ahead of the drive");
    assert!(rig.get::<Feed>("/feed?since=0&epoch=5").reset, "another drive's epoch");
    assert_eq!(rig.request("GET", "/feed?since=soon", vec![]).unwrap().status, 400);
}

#[test]
fn the_collector_deletes_what_no_version_uses_after_the_grace_period() {
    let mut rig = Rig::new();
    for blob in [format!("m/{}", hash(10)), format!("s/{}", hash(11)), format!("m/{}", hash(20)), format!("s/{}", hash(21))] {
        rig.store(&blob);
    }
    rig.commit(vec![file(2, 1, 3), put(2, None, 10, &[11]), file(4, 1, 5), put(4, None, 20, &[21])]).unwrap();
    assert_eq!(*rig.mem.timer.lock().unwrap(), None, "nothing is unused yet");
    rig.commit(vec![Op::Delete { node: id(2) }]).unwrap();
    let due = rig.mem.timer.lock().unwrap().expect("the collector is set");
    assert_eq!(due, rig.mem.now_ms() + 60_000);

    // Too early: nothing goes.
    *rig.mem.timer.lock().unwrap() = None;
    block_on(rig.drive.alarm()).unwrap();
    assert_eq!(rig.stored().len(), 4);
    assert!(rig.mem.timer.lock().unwrap().is_some(), "and it is set again");

    rig.mem.clock.fetch_add(60_001, Ordering::SeqCst);
    *rig.mem.timer.lock().unwrap() = None;
    block_on(rig.drive.alarm()).unwrap();
    assert_eq!(rig.stored(), vec![format!("m/{}", hash(20)), format!("s/{}", hash(21))]);
    assert_eq!(*rig.mem.timer.lock().unwrap(), None);
    let stats = rig.stats();
    assert_eq!((stats["blobs_in_use"].as_u64(), stats["blobs_unused"].as_u64(), stats["blobs_dying"].as_u64()), (Some(2), Some(0), Some(0)));
    rig.check();
}

#[test]
fn a_blob_the_collector_has_marked_is_refused_until_it_is_gone() {
    let mut rig = Rig::new();
    for blob in [format!("m/{}", hash(10)), format!("s/{}", hash(11)), format!("s/{}", hash(12))] {
        rig.store(&blob);
    }
    rig.commit(vec![file(2, 1, 3), put(2, None, 10, &[11, 12])]).unwrap();
    rig.commit(vec![Op::Delete { node: id(2) }]).unwrap();
    // The collector marks all three and dies after deleting one.
    let stats: serde_json::Value = serde_json::from_slice(&rig.request("POST", "/_dev/gc?stop_after=1", vec![]).unwrap().body).unwrap();
    assert_eq!(stats["blobs_dying"], 3);
    assert_eq!(rig.stored().len(), 2);

    // Two of them are still in storage, and a commit may not have them back.
    *rig.mem.timer.lock().unwrap() = None;
    let refusal = rig.commit(vec![file(4, 1, 5), put(4, None, 10, &[11, 12])]).unwrap_err();
    assert_eq!(refusal.error, E_MISSING);
    assert_eq!(refusal.missing.len(), 3);
    let soon = rig.mem.now_ms() + 50;
    assert_eq!(*rig.mem.timer.lock().unwrap(), Some(soon), "the refusal wakes the collector");
    // A client retrying faster than that does not push it back.
    rig.mem.clock.fetch_add(20, Ordering::SeqCst);
    rig.commit(vec![file(4, 1, 5), put(4, None, 10, &[11, 12])]).unwrap_err();
    assert_eq!(*rig.mem.timer.lock().unwrap(), Some(soon));

    block_on(rig.drive.alarm()).unwrap();
    assert!(rig.stored().is_empty());
    assert_eq!(rig.stats()["blobs_dying"], 0);
    // Uploaded again, they are as good as new.
    for blob in [format!("m/{}", hash(10)), format!("s/{}", hash(11)), format!("s/{}", hash(12))] {
        rig.store(&blob);
    }
    rig.commit(vec![file(4, 1, 5), put(4, None, 10, &[11, 12])]).unwrap();
    rig.check();
}

#[test]
fn a_panic_inside_a_transaction_leaves_nothing_behind() {
    let rig = Rig::new();
    let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| rig.request("POST", "/_dev/panic", vec![])));
    assert!(panicked.is_err());
    assert_eq!(rig.stats()["commits_remembered"], 0);
    rig.check();
}

#[test]
fn when_storage_fails_the_request_fails_and_nothing_is_kept() {
    let mut rig = Rig::new();
    rig.store(&format!("m/{}", hash(10)));
    let commit = Commit { id: id(0xc1), ops: vec![file(2, 1, 3), put(2, None, 10, &[])] };
    let body = serde_json::to_vec(&commit).unwrap();

    // The blob store is down: a fault, not a refusal, so the sender tries again.
    *rig.mem.store_down.lock().unwrap() = true;
    assert!(rig.request("POST", "/commit", body.clone()).is_err());
    *rig.mem.store_down.lock().unwrap() = false;

    // The database fails at every statement of the commit in turn. Whatever
    // was written before the failure is undone.
    let mut failures = 0;
    for at in 0..200 {
        rig.mem.sql.fail_in.store(at, Ordering::SeqCst);
        let outcome = rig.request("POST", "/commit", body.clone());
        let left = rig.mem.sql.fail_in.swap(-1, Ordering::SeqCst);
        match outcome {
            Err(_) => {
                failures += 1;
                rig.check();
            }
            // The commit got through before the failure was due.
            Ok(r) if left >= 0 => {
                assert_eq!(r.status, 200);
                break;
            }
            // The failure came after the transaction, while setting the
            // timer or counting: the commit stands and the answer is lost.
            Ok(r) => panic!("a failed statement was answered with {}", r.status),
        }
        // Either all of the commit or none of it.
        let seq = rig.seq();
        assert!(seq == 1 || seq == 3, "half a commit: seq {seq}");
        if seq == 3 {
            // Applied, and the failure was after it: a retry is a replay.
            assert_eq!(rig.commit_as(&commit).unwrap(), Committed { seq: 3, replayed: true });
            break;
        }
    }
    assert!(failures > 5, "the commit runs more statements than that");
    assert_eq!(rig.commit_as(&commit).unwrap(), Committed { seq: 3, replayed: true });
    rig.check();
}

/// The Rivet provider runs a drive on a thread pool, so with a platform that
/// can move between threads the whole request must be able to as well.
#[test]
fn a_request_can_move_between_threads_when_the_platform_can() {
    fn movable<T: Send>(_: &T) {}
    let rig = Rig::new();
    let request = rig.drive.handle(Request { method: "GET".into(), path: format!("/drives/{DRIVE}"), query: String::new(), body: vec![] });
    movable(&request);
    assert_eq!(block_on(request).unwrap().status, 200);
    let commit = rig.drive.handle(Request { method: "POST".into(), path: format!("/drives/{DRIVE}/commit"), query: String::new(), body: vec![] });
    movable(&commit);
    drop(commit);
    let alarm = rig.drive.alarm();
    movable(&alarm);
    let open = rig.drive.open();
    movable(&open);
    let wake = rig.drive.schedule_collector();
    movable(&wake);
}
