//! Chaos tests for the Worker (`app/providers/cloudflare`): the node-tree protocol under
//! `wrangler dev`, with the process killed with `kill -9`, frozen, and
//! restarted, and with requests and replies lost on the way.
//!
//! The client in these tests does what a real one does: it seals names,
//! keys, shards and manifests with `ludy_proto::crypt`, and it repeats a
//! request until it has an answer. A model of the drive predicts the outcome
//! of every commit, refusals included, and each test ends with the same
//! oracle: the drive's own consistency check passes, the tree read back and
//! decrypted from the root key is exactly the model's, every file's content
//! is byte for byte what was written, and storage holds exactly the blobs it
//! should.
//!
//! These need Node and `app/providers/cloudflare/node_modules`, so they are ignored by
//! default:
//!
//! ```text
//! cargo test -p ludy-core --test worker_chaos -- --ignored --test-threads=1
//! WORKER_CHAOS_SEED=7 WORKER_CHAOS_STEPS=400 cargo test -p ludy-core --test worker_chaos soak -- --ignored --nocapture
//! ```

mod worker_rig;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ludy_proto::crypt::{self, hex, CutKey, DedupKey, FileMeta, NodeId, NodeKey, PrivateManifest, PrivateSegment, Random};
use ludy_proto::shard::sha256_hex;
use ludy_proto::synth;
use ludy_proto::tree::*;
use worker_rig::*;

/// A grace period the collector never reaches: it runs only when a test says so.
const NEVER: i64 = 1_000_000_000_000;

type Id = [u8; 16];

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len() / 2).map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap()).collect()
}

/// Seeded randomness for the test's choices and for the keys and nonces it makes.
struct Rng {
    seed: u64,
    n: u64,
}

impl Rng {
    fn next(&mut self) -> u64 {
        self.n += 1;
        synth::word(self.seed, self.n)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }

    fn bytes(&mut self, len: usize) -> Vec<u8> {
        let mut out = vec![0u8; len];
        self.fill(&mut out);
        out
    }

    fn id(&mut self) -> Id {
        let mut id = [0u8; 16];
        self.fill(&mut id);
        id
    }
}

impl Random for Rng {
    fn fill(&mut self, buf: &mut [u8]) {
        for chunk in buf.chunks_mut(8) {
            let word = self.next().to_le_bytes();
            chunk.copy_from_slice(&word[..chunk.len()]);
        }
    }
}

struct FileState {
    content: Vec<u8>,
    mtime_ms: u64,
    version: u64,
    /// The blobs of each version the cloud still keeps, oldest first.
    history: Vec<Vec<String>>,
}

struct Node {
    parent: Option<Id>,
    kind: Kind,
    /// Normalised, as it is stored.
    name: String,
    key: NodeKey,
    file: Option<FileState>,
}

/// What the test believes the drive is.
struct Model {
    drive: String,
    root: Id,
    dedup: DedupKey,
    cut: CutKey,
    live: BTreeMap<Id, Node>,
    dead: BTreeSet<Id>,
    /// Ids from commits that were refused: they must not exist at all.
    never: BTreeSet<Id>,
    /// The drive's sequence number.
    seq: u64,
    /// Commits the cloud applied (each counted once however often it was sent).
    applied: u64,
    /// Blobs the drive has a row for, and how many versions use each.
    rows: BTreeMap<String, u64>,
    /// Blobs in storage.
    stored: BTreeSet<String>,
    rng: Rng,
    trace: Vec<String>,
}

/// A version ready to commit: its op and the blobs it names.
struct Prepared {
    op: Op,
    blobs: Vec<String>,
    content: Vec<u8>,
    mtime_ms: u64,
}

impl Model {
    async fn new(client: &Client, seed: u64) -> Model {
        let mut rng = Rng { seed, n: 0 };
        let (drive, root) = (hex(&rng.id()), rng.id());
        let (root_key, dedup) = (NodeKey::random(&mut rng), DedupKey::random(&mut rng));
        let body = CreateDrive {
            root: hex(&root),
            name: hex(&root_key.seal_name(&NodeId(root), "Chaos drive", &mut rng).unwrap()),
            // Wrapping the root key for a person is not designed yet; any blob stands in.
            key: hex(&rng.bytes(61)),
            dedup: hex(&root_key.wrap_dedup(&NodeId(root), &dedup, &mut rng)),
        };
        let head = client.create_drive(&drive, &body).await.unwrap();
        assert_eq!((head.seq, head.root.as_str()), (1, body.root.as_str()));
        // Creating it again is a retry, not an error; with another root it is.
        assert_eq!(client.create_drive(&drive, &body).await.unwrap().epoch, head.epoch);
        let other = CreateDrive { root: hex(&rng.id()), ..body.clone() };
        assert_eq!(client.create_drive(&drive, &other).await.refusal().unwrap().error, E_EXISTS);

        let mut live = BTreeMap::new();
        live.insert(root, Node { parent: None, kind: Kind::Folder, name: "Chaos drive".into(), key: root_key, file: None });
        let cut = CutKey::for_drive(&dedup);
        Model { drive, root, dedup, cut, live, dead: BTreeSet::new(), never: BTreeSet::new(), seq: 1, applied: 0, rows: BTreeMap::new(), stored: BTreeSet::new(), rng, trace: vec![] }
    }

    fn commit_id(&mut self) -> String {
        hex(&self.rng.id())
    }

    fn folders(&self) -> Vec<Id> {
        self.live.iter().filter(|(_, n)| n.kind == Kind::Folder).map(|(id, _)| *id).collect()
    }

    fn files(&self) -> Vec<Id> {
        self.live.iter().filter(|(_, n)| n.kind == Kind::File).map(|(id, _)| *id).collect()
    }

    fn pick(&mut self, from: &[Id]) -> Option<Id> {
        (!from.is_empty()).then(|| from[self.rng.below(from.len() as u64) as usize])
    }

    fn name_taken(&self, parent: &Id, name: &str, except: Option<&Id>) -> bool {
        let name = crypt::normalize_name(name).unwrap();
        self.live.iter().any(|(id, n)| n.parent.as_ref() == Some(parent) && n.name == name && Some(id) != except)
    }

    fn subtree(&self, top: &Id) -> Vec<Id> {
        let mut out = vec![*top];
        let mut i = 0;
        while i < out.len() {
            let at = out[i];
            out.extend(self.live.iter().filter(|(_, n)| n.parent == Some(at)).map(|(id, _)| *id));
            i += 1;
        }
        out
    }

    /// The op that creates `name` in `parent`, and the new node as the model will hold it.
    fn create_op(&mut self, parent: &Id, name: &str, kind: Kind) -> (Id, Op, Node) {
        let (id, key) = (self.rng.id(), NodeKey::random(&mut self.rng));
        let folder = &self.live[parent].key;
        let op = Op::Create {
            id: hex(&id),
            parent: hex(parent),
            kind,
            name_hash: hex(&folder.name_hash(name).unwrap()),
            name: hex(&key.seal_name(&NodeId(id), name, &mut self.rng).unwrap()),
            key: hex(&folder.wrap_child(&NodeId(id), &key, &mut self.rng)),
        };
        let file = (kind == Kind::File).then(|| FileState { content: vec![], mtime_ms: 0, version: 0, history: vec![] });
        (id, op, Node { parent: Some(*parent), kind, name: crypt::normalize_name(name).unwrap(), key, file })
    }

    fn move_op(&mut self, node: &Id, parent: &Id, name: &str) -> Op {
        let folder = &self.live[parent].key;
        let key = &self.live[node].key;
        Op::Move {
            node: hex(node),
            parent: hex(parent),
            name_hash: hex(&folder.name_hash(name).unwrap()),
            name: hex(&key.seal_name(&NodeId(*node), name, &mut self.rng).unwrap()),
            key: hex(&folder.wrap_child(&NodeId(*node), key, &mut self.rng)),
        }
    }

    /// Cut, seal and upload `content` as a new version of `node` (whose key
    /// is `key`), and return the op that commits it on top of `base`.
    async fn prepare(&mut self, client: &Client, node: &Id, key: &NodeKey, base: Option<u64>, content: Vec<u8>) -> Prepared {
        let mut segments = Vec::new();
        let mut uploads: HashMap<String, Vec<u8>> = HashMap::new();
        let mut shards = Vec::new();
        for piece in crypt::cut(&content[..], &self.cut) {
            let sealed = crypt::seal_shard(&piece.unwrap().data, &self.dedup);
            segments.push(PrivateSegment::from(&sealed));
            shards.push(sealed.id_hex());
            uploads.insert(sealed.id_hex(), sealed.bytes);
        }
        let manifest = PrivateManifest { size: content.len() as u64, segments };
        let sealed_manifest = key.seal_manifest(&NodeId(*node), &manifest, &mut self.rng);
        let manifest_name = sha256_hex(&sealed_manifest);

        let mut names: Vec<String> = uploads.keys().cloned().collect();
        names.sort();
        let lacking = client.missing(&self.drive, &Blobs { shards: names, manifests: vec![manifest_name.clone()] }).await;
        assert_eq!(lacking.manifests, vec![manifest_name.clone()], "a manifest sealed just now cannot be there already");
        for name in &lacking.shards {
            let blob = format!("s/{name}");
            assert!(!self.stored.contains(&blob), "the cloud asks for shard {name} again, which storage should hold");
            client.put_shard(&self.drive, name, uploads[name].clone()).await;
            self.stored.insert(blob);
        }
        for name in uploads.keys() {
            assert!(lacking.shards.contains(name) || self.stored.contains(&format!("s/{name}")), "the cloud claims to hold shard {name}, which was never uploaded");
        }
        client.put_manifest(&self.drive, &manifest_name, sealed_manifest).await;
        self.stored.insert(format!("m/{manifest_name}"));

        let mut blobs = vec![format!("m/{manifest_name}")];
        blobs.extend(shards.iter().map(|s| format!("s/{s}")));
        // A modification time of the file's own, not the time of the commit.
        let mtime_ms = 1_600_000_000_000 + self.rng.below(100_000_000_000);
        let meta = hex(&key.seal_meta(&NodeId(*node), &FileMeta { mtime_ms }, &mut self.rng));
        let op = Op::Put { node: hex(node), base, size: content.len() as u64, manifest: manifest_name, shards, meta };
        Prepared { op, blobs, content, mtime_ms }
    }

    /// The model's side of a version that was committed as sequence number `version`.
    fn committed_version(&mut self, node: &Id, p: Prepared, version: u64) {
        for blob in &p.blobs {
            *self.rows.entry(blob.clone()).or_default() += 1;
        }
        let file = self.live.get_mut(node).unwrap().file.as_mut().unwrap();
        file.content = p.content;
        file.mtime_ms = p.mtime_ms;
        file.version = version;
        file.history.push(p.blobs);
        let dropped: Vec<Vec<String>> = if file.history.len() > HISTORY { file.history.drain(..file.history.len() - HISTORY).collect() } else { vec![] };
        for blob in dropped.iter().flatten() {
            *self.rows.get_mut(blob).unwrap() -= 1;
        }
    }

    /// The model's side of a delete. Returns how many nodes went.
    fn deleted(&mut self, top: &Id) -> u64 {
        let gone = self.subtree(top);
        for id in &gone {
            let node = self.live.remove(id).unwrap();
            for blob in node.file.iter().flat_map(|f| f.history.iter().flatten()) {
                *self.rows.get_mut(blob).unwrap() -= 1;
            }
            self.dead.insert(*id);
        }
        gone.len() as u64
    }

    /// Send a commit that the model says is fine, and insist on it.
    async fn must_commit(&mut self, client: &Client, ops: Vec<Op>, changes: u64, what: &str) -> u64 {
        let commit = Commit { id: self.commit_id(), ops };
        match client.commit(&self.drive, &commit).await {
            Answer::Ok(done) => {
                self.seq += changes;
                self.applied += 1;
                assert_eq!(done.seq, self.seq, "{what}: the drive's sequence number is not what {changes} changes should make it");
                done.seq
            }
            Answer::Refused(r) => panic!("{what}: refused with {} ({}), which the model did not expect\n{}", r.error, r.detail, self.trace_tail()),
        }
    }

    /// Send a commit that the model says must be refused with `code`.
    async fn must_refuse(&mut self, client: &Client, ops: Vec<Op>, code: &str, what: &str) -> Refusal {
        let commit = Commit { id: self.commit_id(), ops };
        match client.commit(&self.drive, &commit).await {
            Answer::Refused(r) if r.error == code => r,
            Answer::Refused(r) => panic!("{what}: refused with {} ({}), expected {code}\n{}", r.error, r.detail, self.trace_tail()),
            Answer::Ok(_) => panic!("{what}: accepted, expected {code}\n{}", self.trace_tail()),
        }
    }

    fn trace_tail(&self) -> String {
        format!("--- last steps ---\n{}", self.trace[self.trace.len().saturating_sub(25)..].join("\n"))
    }

    /// Run the collector and predict exactly what storage is left with.
    async fn collect(&mut self, client: &Client, when: &str) {
        client.collect(&self.drive).await;
        let unused: Vec<String> = self.rows.iter().filter(|(_, refs)| **refs == 0).map(|(id, _)| id.clone()).collect();
        for id in unused {
            self.rows.remove(&id);
            self.stored.remove(&id);
        }
        let have: BTreeSet<String> = client.stored(&self.drive).await.into_iter().collect();
        let lost: Vec<_> = self.stored.difference(&have).collect();
        let kept: Vec<_> = have.difference(&self.stored).collect();
        assert!(lost.is_empty() && kept.is_empty(), "{when}: after collecting, storage lacks {lost:?} and still holds {kept:?}\n{}", self.trace_tail());
    }

    /// The oracle. `deep` also reads every file's content back.
    async fn verify(&self, client: &Client, deep: bool, when: &str) {
        client.check(&self.drive, when).await;
        let head = client.head(&self.drive).await.unwrap();
        assert_eq!(head.seq, self.seq, "{when}: the drive is at another sequence number than the model\n{}", self.trace_tail());

        // Small pages, so the tree always arrives in several.
        let (seq, nodes) = client.whole_feed(&self.drive, 7).await;
        assert_eq!(seq, self.seq);
        let mut seqs = BTreeSet::new();
        let by_id: BTreeMap<Id, &NodeInfo> = nodes.iter().map(|n| (<Id>::try_from(&unhex(&n.id)[..]).unwrap(), n)).collect();
        assert_eq!(by_id.len(), nodes.len(), "{when}: a node appears twice in the feed");
        for n in &nodes {
            assert!(seqs.insert(n.seq), "{when}: two nodes share sequence number {}", n.seq);
            assert!(n.seq <= self.seq);
        }
        for id in &self.never {
            assert!(!by_id.contains_key(id), "{when}: node {} exists, from a commit that was refused\n{}", hex(id), self.trace_tail());
        }
        for id in &self.dead {
            assert!(by_id.get(id).is_some_and(|n| n.deleted && n.version.is_none()), "{when}: deleted node {} is not a tombstone", hex(id));
        }
        let live: BTreeSet<Id> = by_id.iter().filter(|(_, n)| !n.deleted).map(|(id, _)| *id).collect();
        let want: BTreeSet<Id> = self.live.keys().copied().collect();
        assert!(
            live == want,
            "{when}: the drive has extra nodes {:?} and lacks {:?}\n{}",
            live.difference(&want).map(hex_id).collect::<Vec<_>>(),
            want.difference(&live).map(hex_id).collect::<Vec<_>>(),
            self.trace_tail()
        );
        assert_eq!(by_id.len(), self.live.len() + self.dead.len(), "{when}: the feed has nodes the model never heard of");

        // Read the tree the way a client would: from the root key down.
        for (id, want) in &self.live {
            let got = by_id[id];
            assert_eq!(got.kind, want.kind);
            assert_eq!(got.parent, want.parent.map(|p| hex(&p)), "{when}: {} has another parent", want.name);
            let Some(parent) = want.parent else { continue };
            let folder = &self.live[&parent].key;
            let key = folder.unwrap_child(&NodeId(*id), &unhex(&got.key)).unwrap_or_else(|e| panic!("{when}: the key of {} does not open under its folder: {e}", want.name));
            assert!(key == want.key);
            assert_eq!(key.open_name(&NodeId(*id), &unhex(&got.name)).unwrap(), want.name);
            assert_eq!(got.name_hash, hex(&folder.name_hash(&want.name).unwrap()));
            let Some(file) = &want.file else { continue };
            match &got.version {
                None => assert_eq!(file.version, 0, "{when}: {} lost its version", want.name),
                Some(v) => {
                    assert_eq!((v.id, v.size), (file.version, file.content.len() as u64), "{when}: {} is at another version\n{}", want.name, self.trace_tail());
                    assert_eq!(key.open_meta(&NodeId(*id), &unhex(&v.meta)).unwrap().mtime_ms, file.mtime_ms, "{when}: {} has another modification time", want.name);
                    if deep {
                        let content = self.read(client, id, &key, &v.manifest).await;
                        assert!(content == file.content, "{when}: {} does not read back as written", want.name);
                    }
                }
            }
        }

        // The drive counts its blobs as the model does.
        let stats = client.stats(&self.drive).await;
        let in_use = self.rows.values().filter(|refs| **refs > 0).count() as u64;
        assert_eq!(stats["blobs_in_use"], in_use, "{when}: blobs in use\n{}", self.trace_tail());
        assert_eq!(stats["blobs_unused"], self.rows.len() as u64 - in_use, "{when}: blobs waiting for the collector");
        assert_eq!(stats["blobs_dying"], 0);
        assert_eq!(stats["nodes"], self.live.len() as u64);
        assert_eq!(stats["tombstones"], self.dead.len() as u64);
        assert_eq!(stats["versions"], self.live.values().filter_map(|n| n.file.as_ref()).map(|f| f.history.len() as u64).sum::<u64>());
        assert_eq!(stats["commits_remembered"], self.applied, "{when}: commits the cloud applied against commits the client was told were applied");
        assert_eq!(stats["pinned"], 0);
    }

    /// Fetch and open a file the way a reader would: manifest, then shards.
    async fn read(&self, client: &Client, id: &Id, key: &NodeKey, manifest_name: &str) -> Vec<u8> {
        let sealed = client.get_manifest(&self.drive, manifest_name).await.unwrap();
        assert_eq!(sha256_hex(&sealed), manifest_name);
        let manifest = key.open_manifest(&NodeId(*id), &sealed).unwrap();
        let mut content = Vec::with_capacity(manifest.size as usize);
        for segment in &manifest.segments {
            let PrivateSegment::Shard { id: shard, n, key } = segment else { panic!("no synthetic segments here") };
            let sealed = client.get_shard(&self.drive, &hex(shard), None).await.unwrap();
            let plain = crypt::open_shard(key, *n as u64, &sealed).unwrap();
            // And a ranged read of the same shard: only the frames around its middle.
            let (off, len) = (*n as u64 / 2, 1000.min(*n as u64));
            let span = crypt::span(off, len, *n as u64).unwrap();
            let part = client.get_shard(&self.drive, &hex(shard), Some((span.bytes.start, span.bytes.end - 1))).await.unwrap();
            let end = (off + len).min(*n as u64) as usize;
            assert!(crypt::open_span(key, *n as u64, &span, &part).unwrap() == plain[off as usize..end]);
            content.extend_from_slice(&plain);
        }
        content
    }
}

fn hex_id(id: &Id) -> String {
    hex(id)
}

/// A client that only ever asks "what changed since I last looked".
#[derive(Default)]
struct Observer {
    since: u64,
    epoch: Option<u64>,
    nodes: BTreeMap<String, NodeInfo>,
    pages: u64,
}

impl Observer {
    async fn sync(&mut self, client: &Client, drive: &str, limit: usize) {
        loop {
            let page = client.feed(drive, self.since, self.epoch, limit).await;
            assert!(!page.reset, "the feed asked for a restart");
            self.epoch = Some(page.epoch);
            self.pages += 1;
            for n in page.nodes {
                self.since = self.since.max(n.seq);
                self.nodes.insert(n.id.clone(), n);
            }
            if !page.more {
                return;
            }
        }
    }

    /// Following the feed must give what reading everything gives.
    async fn matches_a_full_read(&mut self, client: &Client, drive: &str) {
        self.sync(client, drive, 5).await;
        let (_, all) = client.whole_feed(drive, 1000).await;
        let all: BTreeMap<String, NodeInfo> = all.into_iter().map(|n| (n.id.clone(), n)).collect();
        assert!(self.nodes == all, "following the feed gives another tree than reading it whole");
    }
}

struct Rig {
    worker: Worker,
    proxy: Proxy,
    /// Through the proxy, so through whatever faults are switched on.
    client: Client,
}

async fn rig(gc_grace_ms: i64, seed: u64) -> Option<Rig> {
    if !available() {
        return None;
    }
    let worker = Worker::start(gc_grace_ms).await;
    let proxy = Proxy::start(worker.port, seed).await;
    let client = Client::new(&proxy.url());
    Some(Rig { worker, proxy, client })
}

fn content(rng: &mut Rng) -> Vec<u8> {
    let len = match rng.below(10) {
        0 => 0,
        1..=6 => 1 + rng.below(3000),
        7 | 8 => 60_000 + rng.below(160_000),
        _ => 500_000 + rng.below(900_000),
    } as usize;
    synth::bytes(rng.next(), 0, len.next_multiple_of(8))[..len].to_vec()
}

fn assert_quiet_log(worker: &Worker, allowed: &[&str]) {
    let troubles: Vec<String> = log_troubles(&worker.log()).into_iter().filter(|l| !allowed.iter().any(|a| l.contains(a))).collect();
    assert!(troubles.is_empty(), "the Worker's log has errors nobody caused on purpose:\n{}", troubles.join("\n"));
}

// --------------------------------------------------------------- scenarios

/// The reply to a commit is lost three times. The client keeps sending the
/// same commit; it must be applied once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs Node and app/providers/cloudflare/node_modules"]
async fn a_lost_reply_does_not_apply_a_commit_twice() {
    let Some(rig) = rig(NEVER, 1).await else { return };
    let mut m = Model::new(&rig.client, 1).await;
    let root = m.root;

    let (id, op, node) = m.create_op(&root, "report.txt", Kind::File);
    let commit = Commit { id: m.commit_id(), ops: vec![op] };
    rig.proxy.faults.lose_next_replies.store(3, Ordering::SeqCst);
    let done = rig.client.commit(&m.drive, &commit).await.unwrap();
    assert!(done.replayed, "the first three answers were thrown away, so the one that arrived is a replay");
    assert_eq!(done.seq, 2);
    assert_eq!(rig.proxy.faults.replies_lost.load(Ordering::SeqCst), 3);
    m.live.insert(id, node);
    m.seq = 2;
    m.applied = 1;

    // The same id with other content is not a retry.
    let (other, op, _) = m.create_op(&root, "other.txt", Kind::File);
    let reused = Commit { id: commit.id.clone(), ops: vec![op] };
    assert_eq!(rig.client.commit(&m.drive, &reused).await.refusal().unwrap().error, E_COMMIT_REUSED);
    m.never.insert(other);

    // And the answer is the same after the Worker has been killed and started again.
    let mut rig = rig;
    rig.worker.restart().await;
    let again = rig.client.commit(&m.drive, &commit).await.unwrap();
    assert_eq!((again.seq, again.replayed), (2, true));
    m.verify(&rig.client, true, "after the replays").await;
    assert_quiet_log(&rig.worker, &[]);
}

/// A commit whose last op is refused must leave nothing of its first ops.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs Node and app/providers/cloudflare/node_modules"]
async fn a_refused_commit_leaves_nothing_behind() {
    let Some(rig) = rig(NEVER, 2).await else { return };
    let mut m = Model::new(&rig.client, 2).await;
    let root = m.root;

    let (folder, op, node) = m.create_op(&root, "Footage", Kind::Folder);
    m.must_commit(&rig.client, vec![op], 1, "create a folder").await;
    m.live.insert(folder, node);
    let (file, op, mut node) = m.create_op(&folder, "clip.mov", Kind::File);
    let key = node.key.clone();
    let data = synth::bytes(5, 0, 700_000);
    let put = m.prepare(&rig.client, &file, &key, Some(0), data).await;
    node.file = Some(FileState { content: vec![], mtime_ms: 0, version: 0, history: vec![] });
    let version = m.must_commit(&rig.client, vec![op, put.op.clone()], 2, "create a file with content").await;
    m.live.insert(file, node);
    m.committed_version(&file, put, version);

    // Good ops first, then one op of each kind that must be refused.
    for code in [E_EXISTS, E_NO_NODE, E_CYCLE, "file-as-folder", "delete-root", E_CONFLICT, E_MISSING] {
        let mut ops = Vec::new();
        for name in ["a.txt", "b.txt"] {
            let (id, op, _) = m.create_op(&root, name, Kind::File);
            m.never.insert(id);
            ops.push(op);
        }
        if !matches!(code, E_CONFLICT | E_MISSING | "file-as-folder") {
            ops.push(Op::Delete { node: hex(&file) });
        }
        let (poison, code) = match code {
            E_EXISTS => (m.create_op(&root, "Footage", Kind::File).1, E_EXISTS),
            E_NO_NODE => (Op::Delete { node: hex(&[9u8; 16]) }, E_NO_NODE),
            // A folder moved into a folder that this same commit put inside it.
            E_CYCLE => {
                let (inner, create_inner, _) = m.create_op(&folder, "inner", Kind::Folder);
                m.never.insert(inner);
                ops.push(create_inner);
                (Op::Move { node: hex(&folder), parent: hex(&inner), name_hash: "ab".repeat(32), name: "aa".into(), key: "bb".into() }, E_CYCLE)
            }
            "file-as-folder" => (m.create_op(&file, "inside-a-file", Kind::File).1, E_WRONG_KIND),
            "delete-root" => (Op::Delete { node: hex(&root) }, E_WRONG_KIND),
            E_CONFLICT => (Op::Put { node: hex(&file), base: Some(0), size: 1, manifest: "ab".repeat(32), shards: vec![], meta: "aa".into() }, E_CONFLICT),
            _ => (Op::Put { node: hex(&file), base: Some(version), size: 1, manifest: "ab".repeat(32), shards: vec!["cd".repeat(32)], meta: "aa".into() }, E_MISSING),
        };
        ops.push(poison);
        let last = ops.len() - 1;
        let r = m.must_refuse(&rig.client, ops, code, code).await;
        assert_eq!(r.op, Some(last), "the refusal should name the op that was refused");
        if code == E_MISSING {
            assert_eq!((r.missing.manifests.len(), r.missing.shards.len()), (1, 1));
        }
        if code == E_CONFLICT {
            assert_eq!(r.current, Some(version));
        }
        m.verify(&rig.client, true, &format!("after a commit refused with {code}")).await;
    }
    assert_quiet_log(&rig.worker, &[]);
}

/// `kill -9` while a burst of commits is in flight. Every one of them is
/// retried until answered, and every one must be there exactly once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs Node and app/providers/cloudflare/node_modules"]
async fn kill_nine_in_a_burst_of_commits() {
    let Some(mut rig) = rig(NEVER, 3).await else { return };
    let mut m = Model::new(&rig.client, 3).await;
    let root = m.root;
    for round in 0..6u64 {
        // Folders with files in them, so that each burst has versions to write and subtrees to delete.
        for f in 0..2 {
            let (folder, op, node) = m.create_op(&root, &format!("folder-{round}-{f}"), Kind::Folder);
            m.must_commit(&rig.client, vec![op], 1, "create a folder").await;
            m.live.insert(folder, node);
            for i in 0..4 {
                let (id, op, node) = m.create_op(&folder, &format!("file-{i}"), Kind::File);
                let key = node.key.clone();
                let data = content(&mut m.rng);
                let put = m.prepare(&rig.client, &id, &key, Some(0), data).await;
                let version = m.must_commit(&rig.client, vec![op, put.op.clone()], 2, "create a file").await;
                m.live.insert(id, node);
                m.committed_version(&id, put, version);
            }
        }
        burst_and_kill(&mut rig, &mut m, 16, round).await;
        m.verify(&rig.client, true, &format!("after kill number {}", round + 1)).await;
    }
    m.collect(&rig.client, "after the kills").await;
    m.verify(&rig.client, true, "after collecting").await;
    eprintln!("burst kills:\n{}", m.trace.join("\n"));
    assert_eq!(rig.worker.kills, 6);
    assert_quiet_log(&rig.worker, &[]);
}

/// What one commit of a burst does, for the model to follow once it is answered.
enum BurstOp {
    Create(Id, Node),
    Put(Id, Prepared),
    Delete(Id),
}

/// Send a burst of commits at once (`creates` new files, new versions of up
/// to four existing files, deletes of up to two files and one whole folder),
/// kill the Worker once some have been answered, start it again, and let
/// every commit get its answer. No two commits touch the same node, so the
/// order they land in does not matter to the model.
async fn burst_and_kill(rig: &mut Rig, m: &mut Model, creates: usize, tag: u64) {
    // A folder to delete with everything under it; nothing else in the burst may touch that subtree.
    let candidates: Vec<Id> = m.folders().into_iter().filter(|f| *f != m.root).collect();
    let doomed = if m.rng.below(2) == 0 { m.pick(&candidates) } else { None };
    let off_limits: BTreeSet<Id> = doomed.map(|d| m.subtree(&d).into_iter().collect()).unwrap_or_default();
    let folders: Vec<Id> = m.folders().into_iter().filter(|f| !off_limits.contains(f)).collect();
    let mut files: Vec<Id> = m.files().into_iter().filter(|f| !off_limits.contains(f)).collect();

    let mut ops: Vec<(Op, BurstOp)> = Vec::new();
    for i in 0..creates {
        let parent = folders[i % folders.len()];
        let (id, op, node) = m.create_op(&parent, &format!("burst-{tag}-{i}"), Kind::File);
        ops.push((op, BurstOp::Create(id, node)));
    }
    // New versions, uploaded before the burst so that only the commits race the kill.
    for _ in 0..4.min(files.len()) {
        let file = files.swap_remove(m.rng.below(files.len() as u64) as usize);
        let (base, key) = (m.live[&file].file.as_ref().unwrap().version, m.live[&file].key.clone());
        let data = content(&mut m.rng);
        let put = m.prepare(&rig.client, &file, &key, Some(base), data).await;
        ops.push((put.op.clone(), BurstOp::Put(file, put)));
    }
    for _ in 0..2.min(files.len()) {
        let file = files.swap_remove(m.rng.below(files.len() as u64) as usize);
        ops.push((Op::Delete { node: hex(&file) }, BurstOp::Delete(file)));
    }
    if let Some(folder) = doomed {
        ops.push((Op::Delete { node: hex(&folder) }, BurstOp::Delete(folder)));
    }

    let n = ops.len();
    let acks = Arc::new(AtomicUsize::new(0));
    let mut tasks = Vec::new();
    let mut follow = Vec::new();
    for (op, then) in ops {
        let commit = Commit { id: m.commit_id(), ops: vec![op] };
        let (client, drive, acks) = (rig.client.clone(), m.drive.clone(), acks.clone());
        tasks.push(tokio::spawn(async move {
            let answer = client.commit(&drive, &commit).await;
            acks.fetch_add(1, Ordering::SeqCst);
            answer
        }));
        follow.push(then);
    }
    // Kill on an observed event: some, not all, have been answered.
    let some = 1 + m.rng.below(n as u64 / 2) as usize;
    let deadline = Instant::now() + Duration::from_secs(5);
    while acks.load(Ordering::SeqCst) < some && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    let answered_before = acks.load(Ordering::SeqCst);
    rig.worker.restart().await;
    let (mut puts, mut deleted) = (0, 0);
    for (task, then) in tasks.into_iter().zip(follow) {
        let done = match task.await.unwrap() {
            Answer::Ok(done) => done,
            Answer::Refused(r) => panic!("a burst commit was refused: {} ({})\n{}", r.error, r.detail, m.trace_tail()),
        };
        m.applied += 1;
        match then {
            BurstOp::Create(id, node) => {
                m.live.insert(id, node);
                m.seq += 1;
            }
            // A commit of one put: the version is the sequence number it was answered with, replayed or not.
            BurstOp::Put(file, put) => {
                m.committed_version(&file, put, done.seq);
                m.seq += 1;
                puts += 1;
            }
            BurstOp::Delete(id) => {
                let gone = m.deleted(&id);
                m.seq += gone;
                deleted += gone;
            }
        }
    }
    m.trace.push(format!("burst of {n} commits ({creates} creates, {puts} new versions, {deleted} nodes deleted), killed after {answered_before} answers"));
    rig.client.check(&m.drive, "right after the restart").await;
}

/// Two writers put a new version on the same base at the same moment.
/// Exactly one wins, and the other is told which version it lost to.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs Node and app/providers/cloudflare/node_modules"]
async fn of_two_writers_on_one_base_exactly_one_wins() {
    let Some(rig) = rig(NEVER, 4).await else { return };
    let mut m = Model::new(&rig.client, 4).await;
    let root = m.root;
    let (file, op, node) = m.create_op(&root, "shared.doc", Kind::File);
    let key = node.key.clone();
    m.must_commit(&rig.client, vec![op], 1, "create").await;
    m.live.insert(file, node);

    for round in 0..12u64 {
        let base = m.live[&file].file.as_ref().unwrap().version;
        let a = m.prepare(&rig.client, &file, &key, Some(base), synth::bytes(100 + round, 0, 4096)).await;
        let b = m.prepare(&rig.client, &file, &key, Some(base), synth::bytes(200 + round, 0, 8192)).await;
        let (ca, cb) = (Commit { id: m.commit_id(), ops: vec![a.op.clone()] }, Commit { id: m.commit_id(), ops: vec![b.op.clone()] });
        let (ra, rb) = tokio::join!(rig.client.commit(&m.drive, &ca), rig.client.commit(&m.drive, &cb));
        let (winner, loser) = match (ra, rb) {
            (Answer::Ok(done), Answer::Refused(r)) => ((a, done), r),
            (Answer::Refused(r), Answer::Ok(done)) => ((b, done), r),
            (Answer::Ok(_), Answer::Ok(_)) => panic!("round {round}: both writers won"),
            (Answer::Refused(x), Answer::Refused(y)) => panic!("round {round}: both writers lost: {} and {}", x.error, y.error),
        };
        assert_eq!(loser.error, E_CONFLICT);
        assert_eq!(loser.current, Some(winner.1.seq), "the loser is told the version it lost to");
        m.seq += 1;
        m.applied += 1;
        m.committed_version(&file, winner.0, winner.1.seq);
    }
    m.verify(&rig.client, true, "after twelve races").await;
    // The losers' uploads are in storage and nothing refers to them: the collector leaves them be.
    m.collect(&rig.client, "after the races").await;
    assert_quiet_log(&rig.worker, &[]);
}

/// A shard that vanishes from storage between upload and commit is named in
/// the refusal; uploading it again makes the same commit go through.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs Node and app/providers/cloudflare/node_modules"]
async fn a_shard_lost_before_the_commit_is_asked_for_again() {
    let Some(rig) = rig(NEVER, 5).await else { return };
    let mut m = Model::new(&rig.client, 5).await;
    let root = m.root;
    let (file, op, node) = m.create_op(&root, "big.bin", Kind::File);
    let key = node.key.clone();
    m.must_commit(&rig.client, vec![op], 1, "create").await;
    m.live.insert(file, node);

    let data = synth::bytes(77, 0, 3 << 20);
    let put = m.prepare(&rig.client, &file, &key, Some(0), data.clone()).await;
    let victim = put.blobs.last().unwrap().clone();
    assert!(put.blobs.len() >= 3, "3 MiB should be more than one shard");
    rig.client.lose_blob(&m.drive, &victim).await;

    let commit = Commit { id: m.commit_id(), ops: vec![put.op.clone()] };
    let r = rig.client.commit(&m.drive, &commit).await.refusal().expect("a commit naming a lost shard must be refused");
    assert_eq!(r.error, E_MISSING);
    assert_eq!(r.missing.shards, vec![victim[2..].to_string()]);
    m.verify(&rig.client, true, "after the refusal").await;

    // The client cuts and seals the same content again, which gives the same shard, and sends it.
    let piece = crypt::cut(&data[..], &m.cut).map(|c| crypt::seal_shard(&c.unwrap().data, &m.dedup)).find(|s| format!("s/{}", s.id_hex()) == victim).unwrap();
    rig.client.put_shard(&m.drive, &piece.id_hex(), piece.bytes).await;
    let done = rig.client.commit(&m.drive, &commit).await.unwrap();
    assert!(!done.replayed, "a refused commit was never applied, so this is the first time");
    m.seq += 1;
    m.applied += 1;
    m.committed_version(&file, put, done.seq);
    m.verify(&rig.client, true, "after sending the shard again").await;
    assert_quiet_log(&rig.worker, &[]);
}

/// With no grace period the collector runs right behind every commit, while
/// writers keep bringing back content whose shards were just let go. No
/// version may ever come to depend on a shard the collector removed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs Node and app/providers/cloudflare/node_modules"]
async fn the_collector_never_takes_what_a_commit_names() {
    let Some(rig) = rig(0, 6).await else { return };
    let mut m = Model::new(&rig.client, 6).await;
    let root = m.root;
    let mut files = Vec::new();
    for i in 0..4 {
        let (id, op, node) = m.create_op(&root, &format!("f{i}"), Kind::File);
        let key = node.key.clone();
        m.must_commit(&rig.client, vec![op], 1, "create").await;
        m.live.insert(id, node);
        files.push((id, key));
    }
    // A small pool of contents, so the same shards keep being let go and wanted again.
    let pool: Vec<Vec<u8>> = (0..5u64).map(|i| synth::bytes(900 + i, 0, 300_000 + 40_000 * i as usize)).collect();
    let mut refused_for_missing = 0;
    for step in 0..120u64 {
        let (file, key) = files[m.rng.below(4) as usize].clone();
        let data = pool[m.rng.below(5) as usize].clone();
        // Delete-and-recreate now and then, so that every version of a file lets go at once.
        if step % 9 == 8 {
            let changes = m.deleted(&file);
            m.must_commit(&rig.client, vec![Op::Delete { node: hex(&file) }], changes, "delete").await;
            let (id, op, node) = m.create_op(&root, &format!("again-{step}"), Kind::File);
            let slot = files.iter().position(|f| f.0 == file).unwrap();
            files[slot] = (id, node.key.clone());
            m.must_commit(&rig.client, vec![op], 1, "create again").await;
            m.live.insert(id, node);
            continue;
        }
        let base = m.live[&file].file.as_ref().unwrap().version;
        // The collector may delete between "which are missing" and the commit; then the commit says so.
        let mut attempts = 0;
        loop {
            attempts += 1;
            assert!(attempts < 50, "step {step}: the commit keeps being refused for missing shards");
            let put = prepare_racing(&rig.client, &mut m, &file, &key, base, data.clone()).await;
            let commit = Commit { id: m.commit_id(), ops: vec![put.op.clone()] };
            match rig.client.commit(&m.drive, &commit).await {
                Answer::Ok(done) => {
                    m.seq += 1;
                    m.applied += 1;
                    let f = m.live.get_mut(&file).unwrap().file.as_mut().unwrap();
                    f.content = put.content;
                    f.version = done.seq;
                    break;
                }
                Answer::Refused(r) if r.error == E_MISSING => refused_for_missing += 1,
                Answer::Refused(r) => panic!("step {step}: {} ({})", r.error, r.detail),
            }
        }
        if step % 10 == 0 {
            rig.client.check(&m.drive, "while the collector runs").await;
        }
    }
    eprintln!("collector race: {refused_for_missing} commits were told a shard had just been removed and sent it again");

    // Whatever the collector did, every current version must read back whole.
    rig.client.check(&m.drive, "at the end").await;
    for (id, key) in &files {
        let node = rig.client.node(&m.drive, &hex(id)).await.unwrap();
        let Some(v) = node.version else { continue };
        let got = m.read(&rig.client, id, key, &v.manifest).await;
        assert!(got == m.live[id].file.as_ref().unwrap().content, "a file lost content to the collector");
    }
    let stats = rig.client.stats(&m.drive).await;
    eprintln!("collector race: {}", stats["since_start"]);
    assert_quiet_log(&rig.worker, &[]);
}

/// `prepare`, for the test where the collector runs by itself: storage is
/// not predictable there, so upload whatever the cloud says it lacks.
async fn prepare_racing(client: &Client, m: &mut Model, node: &Id, key: &NodeKey, base: u64, content: Vec<u8>) -> Prepared {
    let sealed: Vec<_> = crypt::cut(&content[..], &m.cut).map(|c| crypt::seal_shard(&c.unwrap().data, &m.dedup)).collect();
    let manifest = PrivateManifest { size: content.len() as u64, segments: sealed.iter().map(PrivateSegment::from).collect() };
    let sealed_manifest = key.seal_manifest(&NodeId(*node), &manifest, &mut m.rng);
    let manifest_name = sha256_hex(&sealed_manifest);
    let shards: Vec<String> = sealed.iter().map(|s| s.id_hex()).collect();
    let lacking = client.missing(&m.drive, &Blobs { shards: shards.clone(), manifests: vec![] }).await;
    for s in sealed {
        if lacking.shards.contains(&s.id_hex()) {
            client.put_shard(&m.drive, &s.id_hex(), s.bytes).await;
        }
    }
    client.put_manifest(&m.drive, &manifest_name, sealed_manifest).await;
    let meta = hex(&key.seal_meta(&NodeId(*node), &FileMeta { mtime_ms: 1 }, &mut m.rng));
    let op = Op::Put { node: hex(node), base: Some(base), size: content.len() as u64, manifest: manifest_name, shards, meta };
    Prepared { op, blobs: vec![], content, mtime_ms: 1 }
}

/// A panic in the middle of a commit's transaction, after it has written,
/// must leave nothing of it; and the drive must keep working.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs Node and app/providers/cloudflare/node_modules"]
async fn a_panic_inside_a_commit_leaves_no_half_commit() {
    let Some(rig) = rig(NEVER, 7).await else { return };
    let mut m = Model::new(&rig.client, 7).await;
    let root = m.root;
    let (id, op, node) = m.create_op(&root, "before.txt", Kind::File);
    m.must_commit(&rig.client, vec![op], 1, "create").await;
    m.live.insert(id, node);

    // Send the request that panics and do not wait for it: what matters is
    // how long everyone else is kept waiting.
    let (client, path) = (rig.client.clone(), format!("/drives/{}/_dev/panic", m.drive));
    let panicking = tokio::spawn(async move {
        let sent = Instant::now();
        (client.once(reqwest::Method::POST, &path).await, sent.elapsed())
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    let asked = Instant::now();
    // `commits_remembered` would be 2 if the write before the panic had stayed.
    m.verify(&rig.client, true, "after the panic").await;
    let out = asked.elapsed();
    let (status, took) = panicking.await.unwrap();
    eprintln!("panic: the request that panicked got {status:?} after {took:?}; the next requests to the drive were all answered within {out:?}");
    assert!(status.is_none_or(|s| s >= 500), "the panicking request answered {status:?}");
    assert!(out < Duration::from_secs(2), "one panic kept the drive from answering for {out:?}");
    let (id, op, node) = m.create_op(&root, "after.txt", Kind::File);
    m.must_commit(&rig.client, vec![op], 1, "create after the panic").await;
    m.live.insert(id, node);
    m.verify(&rig.client, true, "after the commit that followed the panic").await;
    let log = rig.worker.log();
    assert!(log.contains("panic inside a transaction"), "the panic should be in the log");
    // The panic itself is in the log, and nothing else is.
    assert_quiet_log(&rig.worker, &["ludy-server/src/drive.rs", "panic inside a transaction", "RuntimeError: unreachable", "/_dev/panic"]);
}

/// The fault switches and the consistency check must not exist unless the
/// harness asked for them.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs Node and app/providers/cloudflare/node_modules"]
async fn the_dev_routes_are_off_unless_switched_on() {
    if !available() {
        return;
    }
    let worker = Worker::start_with(vec![]).await;
    let client = Client::new(&worker.url());
    let mut m = Model::new(&client, 8).await;
    let root = m.root;
    let (file, op, node) = m.create_op(&root, "kept.txt", Kind::File);
    let key = node.key.clone();
    m.must_commit(&client, vec![op], 1, "create").await;
    m.live.insert(file, node);
    let put = m.prepare(&client, &file, &key, Some(0), b"still here".to_vec()).await;
    let blob = put.blobs[1].clone();
    let version = m.must_commit(&client, vec![put.op.clone()], 1, "put").await;
    m.committed_version(&file, put, version);

    let d = &m.drive;
    for (method, path) in [
        (reqwest::Method::GET, format!("/drives/{d}/_dev/check")),
        (reqwest::Method::POST, format!("/drives/{d}/_dev/gc")),
        (reqwest::Method::POST, format!("/drives/{d}/_dev/panic")),
        (reqwest::Method::GET, format!("/_dev/blobs/{d}")),
        (reqwest::Method::DELETE, format!("/_dev/blobs/{d}/{blob}")),
        (reqwest::Method::GET, "/_dev/crypt".to_string()),
    ] {
        assert_eq!(client.once(method.clone(), &path).await, Some(400), "{method} {path} should not exist");
    }
    // And the DELETE above deleted nothing.
    assert!(m.read(&client, &file, &key, &client.node(d, &hex(&file)).await.unwrap().version.unwrap().manifest).await == b"still here");
    assert_quiet_log(&worker, &[]);
}

/// The collector is cut short part-way and the Worker killed. What the
/// collector had marked stays marked; a commit that wants one of those
/// shards back is refused even though the object is still in storage; and
/// once the collector has finished, the same commit goes through after a
/// fresh upload.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs Node and app/providers/cloudflare/node_modules"]
async fn a_collector_cut_short_never_hands_out_what_it_marked() {
    let Some(mut rig) = rig(NEVER, 9).await else { return };
    let mut m = Model::new(&rig.client, 9).await;
    let root = m.root;
    let (folder, op, node) = m.create_op(&root, "doomed", Kind::Folder);
    m.must_commit(&rig.client, vec![op], 1, "create a folder").await;
    m.live.insert(folder, node);
    let n = 120usize;
    for i in 0..n {
        let (id, op, node) = m.create_op(&folder, &format!("f{i}"), Kind::File);
        let key = node.key.clone();
        let put = m.prepare(&rig.client, &id, &key, Some(0), synth::bytes(5000 + i as u64, 0, 1024)).await;
        let version = m.must_commit(&rig.client, vec![op, put.op.clone()], 2, "create a file").await;
        m.live.insert(id, node);
        m.committed_version(&id, put, version);
    }
    let changes = m.deleted(&folder);
    m.must_commit(&rig.client, vec![Op::Delete { node: hex(&folder) }], changes, "delete them all").await;
    assert_eq!(rig.client.stats(&m.drive).await["blobs_unused"], 2 * n as u64);

    // The collector marks all 240, deletes 50 of them from storage, and dies.
    rig.client.collect_and_stop_after(&m.drive, 50).await;
    rig.worker.restart().await;
    let stats = rig.client.stats(&m.drive).await;
    let in_storage: BTreeSet<String> = rig.client.stored(&m.drive).await.into_iter().collect();
    assert_eq!((stats["blobs_dying"].as_u64(), in_storage.len()), (Some(2 * n as u64), 2 * n - 50), "the marks must survive the kill");
    rig.client.check(&m.drive, "after the kill").await;

    // Content whose shard is marked and still in storage: the dangerous one.
    let (data, shard) = (0..n as u64)
        .map(|i| synth::bytes(5000 + i, 0, 1024))
        .map(|data| (crypt::seal_shard(&data, &m.dedup).id_hex(), data))
        .find(|(shard, _)| in_storage.contains(&format!("s/{shard}")))
        .map(|(shard, data)| (data, shard))
        .unwrap();
    let asked = rig.client.missing(&m.drive, &Blobs { shards: vec![shard.clone()], manifests: vec![] }).await;
    assert_eq!(asked.shards, vec![shard.clone()], "a shard being deleted must be reported as missing though storage still has it");

    let (file, op, node) = m.create_op(&root, "survivor", Kind::File);
    let key = node.key.clone();
    m.must_commit(&rig.client, vec![op], 1, "create the survivor").await;
    m.live.insert(file, node);
    // A commit that names it without uploading, as a client would that asked before the collector began.
    let manifest = PrivateManifest { size: 1024, segments: vec![PrivateSegment::from(&crypt::seal_shard(&data, &m.dedup))] };
    let sealed_manifest = key.seal_manifest(&NodeId(file), &manifest, &mut m.rng);
    let manifest_name = sha256_hex(&sealed_manifest);
    rig.client.put_manifest(&m.drive, &manifest_name, sealed_manifest).await;
    let commit = Commit { id: m.commit_id(), ops: vec![Op::Put { node: hex(&file), base: Some(0), size: 1024, manifest: manifest_name.clone(), shards: vec![shard.clone()], meta: "aa".into() }] };
    let r = rig.client.commit(&m.drive, &commit).await.refusal().expect("a commit must not come to depend on a shard that is being deleted");
    assert_eq!((r.error.as_str(), &r.missing.shards), (E_MISSING, &vec![shard.clone()]));

    // The refusal woke the collector. The client sends the shard and tries again until it is through.
    let mut refusals = 1;
    let done = loop {
        if refusals >= 200 {
            panic!("the commit keeps being refused; the drive says {}", rig.client.stats(&m.drive).await);
        }
        rig.client.put_shard(&m.drive, &shard, crypt::seal_shard(&data, &m.dedup).bytes).await;
        match rig.client.commit(&m.drive, &commit).await {
            Answer::Ok(done) => break done,
            Answer::Refused(r) if r.error == E_MISSING => refusals += 1,
            Answer::Refused(r) => panic!("{} ({})", r.error, r.detail),
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert!(!done.replayed);
    eprintln!("collector cut short: the commit that wanted a marked shard back was refused {refusals} times before the collector had finished");

    // Finish whatever is left and look at everything.
    rig.client.collect(&m.drive).await;
    rig.client.check(&m.drive, "after collecting again").await;
    let stats = rig.client.stats(&m.drive).await;
    assert_eq!((stats["blobs_dying"].as_u64(), stats["blobs_unused"].as_u64(), stats["blobs_in_use"].as_u64()), (Some(0), Some(0), Some(2)));
    assert!(m.read(&rig.client, &file, &key, &manifest_name).await == data, "the survivor lost its content to the collector");
    let have: BTreeSet<String> = rig.client.stored(&m.drive).await.into_iter().collect();
    assert_eq!(have, BTreeSet::from([format!("m/{manifest_name}"), format!("s/{shard}")]), "storage should hold the survivor and nothing of the 120 files");
    assert_quiet_log(&rig.worker, &[]);
}

/// A folder of three thousand files made in three commits and deleted in one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs Node and app/providers/cloudflare/node_modules"]
async fn a_wide_tree_in_few_commits() {
    let Some(rig) = rig(NEVER, 10).await else { return };
    let mut m = Model::new(&rig.client, 10).await;
    let root = m.root;
    let (folder, op, node) = m.create_op(&root, "wide", Kind::Folder);
    m.must_commit(&rig.client, vec![op], 1, "create a folder").await;
    m.live.insert(folder, node);
    let mut create_ms = Vec::new();
    for batch in 0..3 {
        let mut ops = Vec::new();
        let mut made = Vec::new();
        for i in 0..MAX_OPS {
            let (id, op, node) = m.create_op(&folder, &format!("file-{batch}-{i}"), Kind::File);
            ops.push(op);
            made.push((id, node));
        }
        let t = Instant::now();
        m.must_commit(&rig.client, ops, MAX_OPS as u64, "a thousand files").await;
        create_ms.push(t.elapsed().as_millis());
        m.live.extend(made);
    }
    // One more op than a commit may hold.
    let too_many: Vec<Op> = (0..=MAX_OPS).map(|i| m.create_op(&folder, &format!("extra-{i}"), Kind::File).1).collect();
    m.must_refuse(&rig.client, too_many, E_MALFORMED, "1001 ops").await;

    let t = Instant::now();
    let (_, nodes) = rig.client.whole_feed(&m.drive, FEED_PAGE).await;
    let feed_ms = t.elapsed().as_millis();
    assert_eq!(nodes.len(), 3 * MAX_OPS + 2);
    // A page larger than allowed is cut to the limit, not refused.
    assert_eq!(rig.client.feed(&m.drive, 0, None, 1_000_000).await.nodes.len(), 3 * MAX_OPS + 2);
    let page = rig.client.feed(&m.drive, 0, None, 1000).await;
    assert!(page.more && page.nodes.len() == 1000);
    m.verify(&rig.client, false, "with three thousand files").await;

    let changes = m.deleted(&folder);
    assert_eq!(changes, 3 * MAX_OPS as u64 + 1);
    let t = Instant::now();
    m.must_commit(&rig.client, vec![Op::Delete { node: hex(&folder) }], changes, "delete the folder").await;
    let delete_ms = t.elapsed().as_millis();
    m.verify(&rig.client, false, "after deleting them").await;
    let stats = rig.client.stats(&m.drive).await;
    eprintln!("wide tree: 1,000 creates per commit took {create_ms:?} ms; reading 3,002 nodes took {feed_ms} ms; deleting 3,001 nodes in one commit took {delete_ms} ms; database {} bytes", stats["database_bytes"]);
    assert_quiet_log(&rig.worker, &[]);
}

/// Forty versions of one file: twenty-five are kept, and what the dropped
/// ones used is let go.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs Node and app/providers/cloudflare/node_modules"]
async fn history_is_capped_and_the_dropped_versions_let_go() {
    let Some(rig) = rig(NEVER, 11).await else { return };
    let mut m = Model::new(&rig.client, 11).await;
    let root = m.root;
    let (file, op, node) = m.create_op(&root, "often-saved.txt", Kind::File);
    let key = node.key.clone();
    m.must_commit(&rig.client, vec![op], 1, "create").await;
    m.live.insert(file, node);
    let mut ids = Vec::new();
    for i in 0..40u64 {
        let base = m.live[&file].file.as_ref().unwrap().version;
        // Every third save repeats earlier content, so some shards are shared between versions.
        let data = synth::bytes(300 + i % 3 * (i / 3), 0, 2048);
        let put = m.prepare(&rig.client, &file, &key, Some(base), data).await;
        let version = m.must_commit(&rig.client, vec![put.op.clone()], 1, "save").await;
        m.committed_version(&file, put, version);
        ids.push(version);
    }
    let kept = rig.client.versions(&m.drive, &hex(&file)).await.unwrap();
    assert_eq!(kept.len(), HISTORY);
    assert_eq!(kept.iter().map(|v| v.id).collect::<Vec<_>>(), ids.iter().rev().take(HISTORY).copied().collect::<Vec<_>>(), "the newest twenty-five, newest first");
    m.verify(&rig.client, true, "after forty saves").await;
    m.collect(&rig.client, "after forty saves").await;
    m.verify(&rig.client, true, "after collecting").await;
    // Every kept version still reads: none of its blobs went with the dropped ones.
    for v in &kept {
        let sealed = rig.client.get_manifest(&m.drive, &v.manifest).await.unwrap();
        let manifest = key.open_manifest(&NodeId(file), &sealed).unwrap();
        for segment in &manifest.segments {
            let PrivateSegment::Shard { id, n, key } = segment else { unreachable!() };
            let sealed = rig.client.get_shard(&m.drive, &hex(id), None).await.unwrap();
            crypt::open_shard(key, *n as u64, &sealed).unwrap();
        }
    }
    assert_quiet_log(&rig.worker, &[]);
}

/// What does not fit or does not parse is refused with an answer, never an error.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs Node and app/providers/cloudflare/node_modules"]
async fn oversized_and_malformed_requests_are_refused() {
    let Some(rig) = rig(NEVER, 12).await else { return };
    let mut m = Model::new(&rig.client, 12).await;
    let (c, d) = (&rig.client, m.drive.clone());
    let code = |body: &[u8]| serde_json::from_slice::<Refusal>(body).map(|r| r.error).unwrap_or_else(|_| format!("not a refusal: {}", String::from_utf8_lossy(body)));
    use reqwest::Method as M;

    // A sealed shard of exactly the largest size goes in; one byte more does not.
    let largest = synth::bytes(1, 0, crypt::MAX_SEALED_SHARD as usize);
    let name = sha256_hex(&largest);
    assert_eq!(c.raw(M::PUT, &format!("/drives/{d}/shards/{name}"), largest.clone()).await.0, 201);
    assert!(c.get_shard(&d, &name, None).await.unwrap() == largest);
    let mut over = largest;
    over.push(0);
    let (status, body) = c.raw(M::PUT, &format!("/drives/{d}/shards/{}", sha256_hex(&over)), over).await;
    assert_eq!((status, code(&body).as_str()), (413, E_TOO_LARGE));

    // A body that does not hash to its name is refused and not stored.
    let other = "ab".repeat(32);
    let (status, body) = c.raw(M::PUT, &format!("/drives/{d}/shards/{other}"), b"not that".to_vec()).await;
    assert_eq!((status, code(&body).as_str()), (400, E_MALFORMED));
    assert_eq!(c.get_shard(&d, &other, None).await.refusal().unwrap().error, "no_blob");
    assert_eq!(c.raw(M::PUT, &format!("/drives/{d}/shards/nope"), vec![1]).await.0, 400);
    assert_eq!(c.raw(M::PUT, &format!("/drives/{d}/manifests/{other}"), b"not that".to_vec()).await.0, 400);

    // Commits that are not commits.
    for bad in [&b"not json"[..], b"{}", br#"{"id":"zz","ops":[]}"#, br#"{"id":"00000000000000000000000000000000","ops":[]}"#, br#"{"id":"00000000000000000000000000000000","ops":[{"op":"explode"}]}"#] {
        let (status, body) = c.raw(M::POST, &format!("/drives/{d}/commit"), bad.to_vec()).await;
        assert_eq!((status, code(&body).as_str()), (400, E_MALFORMED), "{}", String::from_utf8_lossy(bad));
    }
    // A drive that does not exist, and an id that is not one.
    let nowhere = hex(&m.rng.id());
    let (id, op, _) = m.create_op(&m.root.clone(), "x", Kind::File);
    m.never.insert(id);
    let commit = serde_json::to_vec(&Commit { id: m.commit_id(), ops: vec![op] }).unwrap();
    let (status, body) = c.raw(M::POST, &format!("/drives/{nowhere}/commit"), commit.clone()).await;
    assert_eq!((status, code(&body).as_str()), (404, E_NO_DRIVE));
    assert_eq!(c.raw(M::POST, "/drives/not-an-id/commit", commit).await.0, 400);
    assert_eq!(c.raw(M::GET, &format!("/drives/{nowhere}/feed?since=0"), vec![]).await.0, 404);
    assert_eq!(c.raw(M::POST, &format!("/drives/{nowhere}/missing"), b"{}".to_vec()).await.0, 404);
    assert_eq!(c.raw(M::GET, &format!("/drives/{d}/nodes/{}", hex(&m.rng.id())), vec![]).await.0, 404);
    assert_eq!(c.raw(M::GET, &format!("/drives/{d}/no-such-thing"), vec![]).await.0, 400);
    assert_eq!(c.raw(M::GET, &format!("/drives/{d}/feed?since=many"), vec![]).await.0, 400);
    assert_eq!(c.raw(M::PUT, &format!("/drives/{nowhere}"), b"{}".to_vec()).await.0, 400);

    // A client that is ahead of the drive, or remembers another drive under this id, starts over.
    let head = c.head(&d).await.unwrap();
    assert!(c.feed(&d, head.seq + 1, None, 10).await.reset);
    assert!(c.feed(&d, 0, Some(head.epoch + 1), 10).await.reset);
    assert!(!c.feed(&d, head.seq, Some(head.epoch), 10).await.reset);

    // The encryption format runs inside the Worker's WebAssembly and gives the vector's answer.
    let vectors: serde_json::Value = serde_json::from_str(include_str!("../../ludy-proto/tests/vectors/crypt-v1.json")).unwrap();
    let want = vectors["shards"].as_array().unwrap().iter().find(|s| s["len"] == 200_000).unwrap()["id"].as_str().unwrap().to_string();
    let (status, body) = c.raw(M::GET, "/_dev/crypt", vec![]).await;
    assert_eq!((status, String::from_utf8_lossy(&body).as_ref()), (200, want.as_str()));

    // None of that changed the drive. (The largest shard is in storage with nothing referring to it.)
    m.stored.insert(format!("s/{name}"));
    m.verify(c, true, "after the refusals").await;
    m.collect(c, "after the refusals").await;
    assert_quiet_log(&rig.worker, &[]);
}

// -------------------------------------------------------------------- soak

/// Random operations on a drive while the Worker is killed, frozen and
/// restarted and the proxy loses requests and replies. The seed is printed.
// ------------------------------------------------- the store taken away
//
// These need the server's blobs in a real bucket behind the rig's store
// proxy (`LUDY_RIG_S3_*`, which `scripts/chaos-r2.sh` sets), and skip
// themselves otherwise. What "right" means when the store fails:
//
// * a request that needs the store gets a 5xx within `STORE_PATIENCE`,
//   never a wrong answer and never silence;
// * what needs no store keeps working, and the drive is never held for
//   longer than one bounded wait;
// * a commit is applied whole or not at all;
// * once the store is back, the next attempt succeeds and storage holds
//   exactly what the commits name.

/// How long the server may take to say that its store did not answer.
const STORE_PATIENCE: Duration = Duration::from_secs(20);

fn skipped_without_a_bucket() {
    eprintln!("skipped: the server's blobs are in a folder. scripts/chaos-r2.sh runs this against a bucket");
}

/// Create a file and commit `data` as its first version.
async fn write_file(rig: &Rig, m: &mut Model, name: &str, data: Vec<u8>) -> Id {
    let root = m.root;
    let (file, op, node) = m.create_op(&root, name, Kind::File);
    let key = node.key.clone();
    let put = m.prepare(&rig.client, &file, &key, Some(0), data).await;
    let version = m.must_commit(&rig.client, vec![op, put.op.clone()], 2, name).await;
    m.live.insert(file, node);
    m.committed_version(&file, put, version);
    file
}

/// A commit whose blobs the drive has never heard of, so that it has to ask the store about them.
fn commit_needing_the_store(m: &mut Model, name: &str) -> Commit {
    let root = m.root;
    let (file, op, _) = m.create_op(&root, name, Kind::File);
    m.never.insert(file);
    let put = Op::Put { node: hex(&file), base: Some(0), size: 10, manifest: hex(&m.rng.bytes(32)), shards: vec![hex(&m.rng.bytes(32))], meta: hex(&m.rng.bytes(37)) };
    Commit { id: m.commit_id(), ops: vec![op, put] }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs a bucket: scripts/chaos-r2.sh"]
async fn a_store_that_is_gone_fails_requests_in_time_and_nothing_is_half_done() {
    let Some(rig) = rig(NEVER, 31).await else { return };
    let Some(store) = &rig.worker.store else { return skipped_without_a_bucket() };
    let mut m = Model::new(&rig.client, 31).await;
    write_file(&rig, &mut m, "before.bin", synth::bytes(1, 0, 300_000)).await;
    let up = store.faults.bytes_up.load(Ordering::SeqCst);
    assert!(store.connections() > 0 && up > 300_000, "the server did not reach its bucket through the rig's proxy ({})", store.summary());

    store.refuse();
    let sealed = crypt::seal_shard(&synth::bytes(2, 0, 200_000), &m.dedup);
    let shard_path = format!("/drives/{}/shards/{}", m.drive, sealed.id_hex());
    // What the sender is told: that the store did not answer, and nothing
    // about where or what the store is.
    let told_only_that = |what: &str, answer: &Option<(u16, String)>, took: Duration| {
        eprintln!("\n{what} with the store gone: {:?} after {took:?}", answer.as_ref().map(|a| a.0));
        let (status, text) = answer.as_ref().unwrap_or_else(|| panic!("{what}: no answer in {took:?}"));
        assert_eq!(*status, 503, "{what} with the store gone");
        assert!(took < STORE_PATIENCE, "{what}: the server took {took:?} to say its store is gone");
        assert_eq!(text.trim(), "the blob store did not answer; try again", "{what}");
        let host = std::env::var("LUDY_RIG_S3_ENDPOINT").unwrap_or_default().replace("https://", "");
        let bucket = std::env::var("LUDY_RIG_S3_BUCKET").unwrap_or_default();
        assert!(!text.contains(&host) && !text.contains(&bucket) && !text.contains("http"), "{what}: the reply names the store: {text}");
    };
    let (answer, took) = rig.client.attempt_text(reqwest::Method::PUT, &shard_path, Some(sealed.bytes.clone()), Duration::from_secs(240)).await;
    told_only_that("an upload", &answer, took);

    // A stored shard cannot be read, and the server says so.
    let stored = m.stored.iter().find(|b| b.starts_with("s/")).unwrap()[2..].to_string();
    let (answer, took) = rig.client.attempt_text(reqwest::Method::GET, &format!("/drives/{}/shards/{stored}", m.drive), None, Duration::from_secs(240)).await;
    told_only_that("a read", &answer, took);

    // A commit that has to ask the store is neither applied nor told its
    // blobs are missing: the server does not know.
    let commit = commit_needing_the_store(&mut m, "asks-the-store.bin");
    let body = serde_json::to_vec(&commit).unwrap();
    let (answer, took) = rig.client.attempt_text(reqwest::Method::POST, &format!("/drives/{}/commit", m.drive), Some(body.clone()), Duration::from_secs(240)).await;
    told_only_that("a commit that has to look its blobs up", &answer, took);

    // What needs no store goes on: a folder, the feed, the drive's own check.
    let started = Instant::now();
    let root = m.root;
    let (folder, op, node) = m.create_op(&root, "made-without-the-store", Kind::Folder);
    m.must_commit(&rig.client, vec![op], 1, "a folder while the store is gone").await;
    m.live.insert(folder, node);
    rig.client.whole_feed(&m.drive, 100).await;
    rig.client.check(&m.drive, "while the store is gone").await;
    assert!(started.elapsed() < Duration::from_secs(5), "a folder, the feed and the check took {:?} with the store gone", started.elapsed());

    // Back: the commit that could not be looked up is now refused for what it is,
    // the upload goes through, and a new file is written and read.
    store.pass();
    let (status, _) = rig.client.attempt(reqwest::Method::POST, &format!("/drives/{}/commit", m.drive), Some(body), Duration::from_secs(60)).await;
    assert_eq!(status, Some(409), "with the store back, a commit naming blobs nobody uploaded is refused as missing");
    let name = sealed.id_hex();
    rig.client.put_shard(&m.drive, &name, sealed.bytes).await;
    m.stored.insert(format!("s/{name}"));
    write_file(&rig, &mut m, "after.bin", synth::bytes(3, 0, 300_000)).await;
    m.verify(&rig.client, true, "after the store came back").await;
    eprintln!("store proxy: {}", store.summary());
    assert_quiet_log(&rig.worker, &[]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs a bucket: scripts/chaos-r2.sh"]
async fn a_store_that_says_nothing_does_not_hang_the_drive() {
    let Some(rig) = rig(NEVER, 32).await else { return };
    let Some(store) = &rig.worker.store else { return skipped_without_a_bucket() };
    let mut m = Model::new(&rig.client, 32).await;
    write_file(&rig, &mut m, "before.bin", synth::bytes(4, 0, 200_000)).await;
    let other = Model::new(&rig.client, 3200).await;

    store.stall();
    let sealed = crypt::seal_shard(&synth::bytes(5, 0, 200_000), &m.dedup);
    let (status, took) = rig.client.attempt(reqwest::Method::PUT, &format!("/drives/{}/shards/{}", m.drive, sealed.id_hex()), Some(sealed.bytes.clone()), Duration::from_secs(240)).await;
    eprintln!("\nan upload with the store silent: {status:?} after {took:?}");
    assert!(status == Some(503) && took < STORE_PATIENCE, "an upload with the store silent: {status:?} after {took:?}");
    // What a mounted drive meets: a read of a shard that is there.
    let stored = m.stored.iter().find(|b| b.starts_with("s/")).unwrap()[2..].to_string();
    let (status, took) = rig.client.attempt(reqwest::Method::GET, &format!("/drives/{}/shards/{stored}", m.drive), None, Duration::from_secs(240)).await;
    eprintln!("a read with the store silent: {status:?} after {took:?}");
    assert!(status == Some(503) && took < STORE_PATIENCE, "a read with the store silent: {status:?} after {took:?}");

    // A commit that waits for the store holds its drive. While it waits,
    // the drive's feed must still be answered within the same bound, and
    // another drive must not notice at all.
    let commit = commit_needing_the_store(&mut m, "asks-the-store.bin");
    let (client, path, body) = (rig.client.clone(), format!("/drives/{}/commit", m.drive), serde_json::to_vec(&commit).unwrap());
    let waiting = tokio::spawn(async move { client.attempt(reqwest::Method::POST, &path, Some(body), Duration::from_secs(240)).await });
    tokio::time::sleep(Duration::from_millis(300)).await;
    let (status, took) = rig.client.attempt(reqwest::Method::GET, &format!("/drives/{}", other.drive), None, Duration::from_secs(60)).await;
    eprintln!("another drive's head while a commit waits for the store: {status:?} after {took:?}");
    assert!(status == Some(200) && took < Duration::from_secs(3), "another drive was held up: {status:?} after {took:?}");
    let (status, took) = rig.client.attempt(reqwest::Method::GET, &format!("/drives/{}/feed?since=0&limit=100", m.drive), None, Duration::from_secs(240)).await;
    eprintln!("the same drive's feed while a commit waits for the store: {status:?} after {took:?}");
    assert!(status == Some(200) && took < STORE_PATIENCE, "the drive's feed was held for {took:?} behind a commit waiting for the store ({status:?})");
    let (status, took) = waiting.await.unwrap();
    eprintln!("the commit itself: {status:?} after {took:?}");
    assert!(status == Some(503) && took < STORE_PATIENCE, "a commit with the store silent: {status:?} after {took:?}");

    store.pass();
    let (status, took) = rig.client.attempt(reqwest::Method::GET, &format!("/drives/{}", m.drive), None, Duration::from_secs(60)).await;
    assert!(status == Some(200) && took < Duration::from_secs(5), "with the store back the drive answered {status:?} after {took:?}");
    write_file(&rig, &mut m, "after.bin", synth::bytes(6, 0, 200_000)).await;
    m.verify(&rig.client, true, "after the store spoke again").await;
    eprintln!("store proxy: {}", store.summary());
    assert_quiet_log(&rig.worker, &[]);
}

/// The store says nothing for 35 seconds: longer than the server lets any
/// call to it wait, longer than a client waits for one attempt, and long
/// enough for attempts to pile up behind one another in the drive. A client
/// that keeps asking, as the engine does, is in the middle of writing a file.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs a bucket: scripts/chaos-r2.sh"]
async fn a_store_silent_for_longer_than_any_request_lives_is_ridden_out() {
    const SILENCE: Duration = Duration::from_secs(35);
    let Some(rig) = rig(NEVER, 35).await else { return };
    let Some(store) = &rig.worker.store else { return skipped_without_a_bucket() };
    let mut m = Model::new(&rig.client, 35).await;
    write_file(&rig, &mut m, "before.bin", synth::bytes(7, 0, 200_000)).await;
    let mut other = Model::new(&rig.client, 3500).await;

    store.stall();
    let healing = store.heal_after(SILENCE.as_millis() as u64);
    let started = Instant::now();
    // Lookups, a shard, a manifest and a commit, each sent again and again
    // until it is answered.
    write_file(&rig, &mut m, "through-the-silence.bin", synth::bytes(8, 0, 400_000)).await;
    let took = started.elapsed();
    eprintln!("\na file written through {SILENCE:?} of silence from the store was in after {took:?}");
    assert!(took >= SILENCE - Duration::from_secs(1), "the write finished after {took:?}, before the store spoke again: it cannot have reached the store");
    assert!(took < SILENCE + STORE_PATIENCE, "the write needed {took:?}: {:?} after the store was back", took.saturating_sub(SILENCE));
    healing.await.unwrap();
    // The drive is not left busy with what piled up, and another drive never noticed.
    let (status, took) = rig.client.attempt(reqwest::Method::GET, &format!("/drives/{}/feed?since=0&limit=100", m.drive), None, Duration::from_secs(60)).await;
    assert!(status == Some(200) && took < Duration::from_secs(3), "after the silence the drive's feed answered {status:?} after {took:?}");
    let root = other.root;
    let (folder, op, node) = other.create_op(&root, "untouched", Kind::Folder);
    other.must_commit(&rig.client, vec![op], 1, "another drive").await;
    other.live.insert(folder, node);
    m.verify(&rig.client, true, "after the silence").await;
    other.verify(&rig.client, true, "the other drive").await;
    let log = rig.worker.log();
    eprintln!("the server gave up on the store {} times and logged each; store proxy: {}", log_count(&log, "store", None), store.summary());
    assert!(log_count(&log, "store", None) >= 3, "35 s of silence should have run several calls into their limit");
    assert_quiet_log(&rig.worker, &[]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs a bucket: scripts/chaos-r2.sh"]
async fn connections_to_the_store_cut_in_mid_flight_lose_nothing() {
    let Some(rig) = rig(NEVER, 33).await else { return };
    let Some(store) = &rig.worker.store else { return skipped_without_a_bucket() };
    let mut m = Model::new(&rig.client, 33).await;
    // Uploads and commits of several megabytes while every connection to
    // the store is cut again and again under them.
    let cutting = {
        let faults = store.faults.clone();
        tokio::spawn(async move {
            for _ in 0..40 {
                tokio::time::sleep(Duration::from_millis(250)).await;
                faults_sever(&faults);
            }
        })
    };
    let mut files = Vec::new();
    for i in 0..4u64 {
        files.push(write_file(&rig, &mut m, &format!("cut-{i}.bin"), synth::bytes(40 + i, 0, 2_500_000)).await);
    }
    // Reads through the same weather: every byte must be the one written.
    m.verify(&rig.client, true, "while connections are being cut").await;
    cutting.await.unwrap();
    assert!(store.faults.cuts.load(Ordering::SeqCst) > 0, "no connection was open when the rig cut: {}", store.summary());
    // Delete two of them and collect, cut again in the middle of the deletes.
    for file in files.drain(..2) {
        let changes = m.deleted(&file);
        m.must_commit(&rig.client, vec![Op::Delete { node: hex(&file) }], changes, "delete").await;
    }
    let cutting = {
        let faults = store.faults.clone();
        tokio::spawn(async move {
            for _ in 0..8 {
                tokio::time::sleep(Duration::from_millis(60)).await;
                faults_sever(&faults);
            }
        })
    };
    m.collect(&rig.client, "with connections cut under the collector").await;
    cutting.await.unwrap();
    m.verify(&rig.client, true, "at the end").await;
    eprintln!("store proxy: {}", store.summary());
    assert_quiet_log(&rig.worker, &[]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs a bucket: scripts/chaos-r2.sh"]
async fn the_collector_gets_through_a_store_that_was_gone() {
    let Some(rig) = rig(NEVER, 34).await else { return };
    let Some(store) = &rig.worker.store else { return skipped_without_a_bucket() };
    let mut m = Model::new(&rig.client, 34).await;
    let mut files = Vec::new();
    for i in 0..12u64 {
        files.push(write_file(&rig, &mut m, &format!("going-{i}.bin"), synth::bytes(60 + i, 0, 20_000 + 1000 * i as usize)).await);
    }
    let kept = write_file(&rig, &mut m, "staying.bin", synth::bytes(90, 0, 150_000)).await;
    for file in files {
        let changes = m.deleted(&file);
        m.must_commit(&rig.client, vec![Op::Delete { node: hex(&file) }], changes, "delete").await;
    }
    // 24 blobs are now unused. The collector is asked to run with the
    // store gone, and goes on being asked until it has run; the store
    // comes back seven seconds in.
    store.refuse();
    let healing = store.heal_after(7000);
    let started = Instant::now();
    m.collect(&rig.client, "after the store came back under the collector").await;
    healing.await.unwrap();
    eprintln!("the collector ran to its end {:?} after it was first asked, with the store gone for 7 s", started.elapsed());
    assert!(started.elapsed() < Duration::from_secs(7) + STORE_PATIENCE, "the collector needed {:?} after a 7 s outage", started.elapsed());
    // What it removed can be written again, and what was in use is intact.
    write_file(&rig, &mut m, "back.bin", synth::bytes(60, 0, 20_000)).await;
    assert!(m.live.contains_key(&kept));
    m.verify(&rig.client, true, "at the end").await;
    eprintln!("store proxy: {}", store.summary());
    assert_quiet_log(&rig.worker, &[]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "minutes long; needs Node and app/providers/cloudflare/node_modules"]
async fn soak() {
    let seed: u64 = std::env::var("WORKER_CHAOS_SEED").ok().and_then(|s| s.parse().ok()).unwrap_or_else(|| ludy_proto::now_ms() % 100_000);
    let steps: u64 = std::env::var("WORKER_CHAOS_STEPS").ok().and_then(|s| s.parse().ok()).unwrap_or(300);
    eprintln!("worker soak: WORKER_CHAOS_SEED={seed} WORKER_CHAOS_STEPS={steps}");
    let Some(mut rig) = rig(NEVER, seed).await else { return };
    let started = Instant::now();
    let mut m = Model::new(&rig.client, seed).await;
    let mut observer = Observer::default();
    let (mut freezes, mut collections, mut refusals) = (0u32, 0u32, BTreeMap::<&str, u32>::new());
    let (mut store_healing, mut store_faults) = (None::<tokio::task::JoinHandle<()>>, 0u32);
    // Six replies and four requests in a hundred never arrive.
    rig.proxy.rates(60, 40);
    let names = ["a", "b", "c", "notes.txt", "clip.mov", "caf\u{e9}", "cafe\u{301}", "Report", "report", "\u{65e5}\u{672c}\u{8a9e}", "x y z", "d", "e", "f"];

    for step in 0..steps {
        // With the blobs in a bucket, the bucket has weather too: about one
        // step in fourteen starts some, once the last has passed.
        if let Some(store) = &rig.worker.store {
            if store_healing.as_ref().is_none_or(|h| h.is_finished()) && m.rng.below(100) < 7 {
                let (what, healing) = store.weather(m.rng.below(100), m.rng.below(1000));
                m.trace.push(format!("{step}: {what}"));
                store_healing = Some(healing);
                store_faults += 1;
            }
        }
        let roll = m.rng.below(100);
        let name = names[m.rng.below(names.len() as u64) as usize];
        let (folders, files) = (m.folders(), m.files());
        let line;
        match roll {
            // A new file, created and filled in one commit.
            0..=17 => {
                let parent = m.pick(&folders).unwrap();
                line = format!("create file {name:?} in {}", hex(&parent));
                let (id, op, node) = m.create_op(&parent, name, Kind::File);
                if m.name_taken(&parent, name, None) {
                    m.never.insert(id);
                    m.must_refuse(&rig.client, vec![op], E_EXISTS, &line).await;
                    *refusals.entry(E_EXISTS).or_default() += 1;
                } else {
                    let data = content(&mut m.rng);
                    let key = node.key.clone();
                    let put = m.prepare(&rig.client, &id, &key, Some(0), data).await;
                    let version = m.must_commit(&rig.client, vec![op, put.op.clone()], 2, &line).await;
                    m.live.insert(id, node);
                    m.committed_version(&id, put, version);
                }
            }
            18..=25 => {
                let parent = m.pick(&folders).unwrap();
                line = format!("create folder {name:?} in {}", hex(&parent));
                let (id, op, node) = m.create_op(&parent, name, Kind::Folder);
                if m.name_taken(&parent, name, None) {
                    m.never.insert(id);
                    m.must_refuse(&rig.client, vec![op], E_EXISTS, &line).await;
                    *refusals.entry(E_EXISTS).or_default() += 1;
                } else {
                    m.must_commit(&rig.client, vec![op], 1, &line).await;
                    m.live.insert(id, node);
                }
            }
            // New content, a small change, or another file's content.
            26..=52 => {
                let Some(file) = m.pick(&files) else { continue };
                let old = m.live[&file].file.as_ref().unwrap();
                let (base, key) = (old.version, m.live[&file].key.clone());
                let data = match roll {
                    26..=40 => content(&mut m.rng),
                    41..=47 if !old.content.is_empty() => {
                        let mut data = old.content.clone();
                        let at = m.rng.below(data.len() as u64) as usize;
                        let end = (at + 1 + m.rng.below(64) as usize).min(data.len());
                        for b in &mut data[at..end] {
                            *b ^= 0x5a;
                        }
                        data
                    }
                    _ => match m.pick(&files) {
                        Some(other) => m.live[&other].file.as_ref().unwrap().content.clone(),
                        None => continue,
                    },
                };
                line = format!("write {} bytes to {}", data.len(), hex(&file));
                let put = m.prepare(&rig.client, &file, &key, Some(base), data).await;
                let version = m.must_commit(&rig.client, vec![put.op.clone()], 1, &line).await;
                m.committed_version(&file, put, version);
            }
            // Rename in place, or move somewhere else.
            53..=68 => {
                let candidates: Vec<Id> = m.live.keys().filter(|id| **id != m.root).copied().collect();
                let Some(node) = m.pick(&candidates) else { continue };
                let target = if roll <= 60 { m.live[&node].parent.unwrap() } else { m.pick(&folders).unwrap() };
                line = format!("move {} to {name:?} in {}", hex(&node), hex(&target));
                if target == node {
                    // Caught before it reaches a drive.
                    let op = m.move_op(&node, &target, name);
                    m.must_refuse(&rig.client, vec![op], E_MALFORMED, &line).await;
                    *refusals.entry(E_MALFORMED).or_default() += 1;
                } else {
                    let op = m.move_op(&node, &target, name);
                    let expect = if m.subtree(&node).contains(&target) {
                        Some(E_CYCLE)
                    } else if m.name_taken(&target, name, Some(&node)) {
                        Some(E_EXISTS)
                    } else {
                        None
                    };
                    match expect {
                        Some(code) => {
                            m.must_refuse(&rig.client, vec![op], code, &line).await;
                            *refusals.entry(code).or_default() += 1;
                        }
                        None => {
                            m.must_commit(&rig.client, vec![op], 1, &line).await;
                            let n = m.live.get_mut(&node).unwrap();
                            n.parent = Some(target);
                            n.name = crypt::normalize_name(name).unwrap();
                        }
                    }
                }
            }
            69..=76 => {
                let candidates: Vec<Id> = m.live.keys().filter(|id| **id != m.root).copied().collect();
                let Some(node) = m.pick(&candidates) else { continue };
                line = format!("delete {} and what is under it", hex(&node));
                let changes = m.subtree(&node).len() as u64;
                m.must_commit(&rig.client, vec![Op::Delete { node: hex(&node) }], changes, &line).await;
                assert_eq!(m.deleted(&node), changes);
            }
            // A write on top of a version that is no longer current.
            77..=80 => {
                let stale: Vec<Id> = files.iter().filter(|f| m.live[*f].file.as_ref().unwrap().version > 0).copied().collect();
                let Some(file) = m.pick(&stale) else { continue };
                let (current, key) = (m.live[&file].file.as_ref().unwrap().version, m.live[&file].key.clone());
                line = format!("write to {} on a stale base", hex(&file));
                let data = content(&mut m.rng);
                let put = m.prepare(&rig.client, &file, &key, Some(current - 1), data).await;
                let r = m.must_refuse(&rig.client, vec![put.op], E_CONFLICT, &line).await;
                assert_eq!(r.current, Some(current));
                *refusals.entry(E_CONFLICT).or_default() += 1;
            }
            // Good ops followed by one that is refused: none of it may stay.
            81..=84 => {
                let parent = m.pick(&folders).unwrap();
                line = format!("a commit with a bad last op in {}", hex(&parent));
                let mut ops = Vec::new();
                for i in 0..3 {
                    let (id, op, _) = m.create_op(&parent, &format!("poisoned-{step}-{i}"), Kind::Folder);
                    m.never.insert(id);
                    ops.push(op);
                }
                if let Some(victim) = m.pick(&files) {
                    ops.push(Op::Delete { node: hex(&victim) });
                }
                ops.push(Op::Delete { node: hex(&m.rng.id()) });
                m.must_refuse(&rig.client, ops, E_NO_NODE, &line).await;
                *refusals.entry(E_NO_NODE).or_default() += 1;
            }
            85..=87 => {
                let dead: Vec<Id> = m.dead.iter().copied().collect();
                let Some(ghost) = m.pick(&dead) else { continue };
                line = format!("delete {} again", hex(&ghost));
                m.must_refuse(&rig.client, vec![Op::Delete { node: hex(&ghost) }], E_NO_NODE, &line).await;
                *refusals.entry(E_NO_NODE).or_default() += 1;
            }
            // A folder and its files in one commit.
            88..=90 => {
                let parent = m.pick(&folders).unwrap();
                let folder_name = format!("batch-{step}");
                line = format!("create folder {folder_name} with files in {}", hex(&parent));
                let (folder, op, node) = m.create_op(&parent, &folder_name, Kind::Folder);
                m.live.insert(folder, node);
                let mut ops = vec![op];
                let mut made = Vec::new();
                for i in 0..(2 + m.rng.below(5)) {
                    let (id, op, node) = m.create_op(&folder, &format!("item-{i}"), Kind::File);
                    ops.push(op);
                    made.push((id, node));
                }
                let changes = ops.len() as u64;
                m.must_commit(&rig.client, ops, changes, &line).await;
                m.live.extend(made);
            }
            91..=92 => {
                line = "run the collector".into();
                m.collect(&rig.client, &format!("step {step}")).await;
                collections += 1;
            }
            // Stop every process of the Worker for a while; requests pile up and time out.
            93..=94 => {
                let ms = 500 + m.rng.below(4000);
                line = format!("freeze the Worker for {ms} ms");
                rig.worker.freeze();
                let pid = rig.worker.pid();
                // Thaw from the side, while this step is already trying to get
                // through. The step waits for the timer before anything can
                // kill the Worker, so the pid is still the Worker's when it fires.
                let thawed = tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(ms)).await;
                    signal_group(pid, libc::SIGCONT);
                });
                freezes += 1;
                let (id, op, node) = m.create_op(&m.root.clone(), &format!("during-freeze-{step}"), Kind::Folder);
                m.must_commit(&rig.client, vec![op], 1, &line).await;
                m.live.insert(id, node);
                thawed.await.unwrap();
            }
            95..=96 => {
                let n = 6 + m.rng.below(10) as usize;
                line = format!("burst of commits ({n} creates, plus writes and deletes) with kill -9 in the middle");
                burst_and_kill(&mut rig, &mut m, n, step).await;
            }
            97 => {
                line = "kill -9 between requests".into();
                rig.worker.restart().await;
                rig.client.check(&m.drive, "after an idle kill").await;
            }
            _ => {
                line = "look at everything".into();
                observer.sync(&rig.client, &m.drive, 9).await;
                m.verify(&rig.client, false, &format!("step {step}")).await;
            }
        }
        m.trace.push(format!("{step}: {line}"));
    }

    // The faults stop; what is there must be exactly what was written.
    if let Some(store) = &rig.worker.store {
        store.pass();
        if let Some(healing) = store_healing.take() {
            healing.abort();
        }
        store.pass();
    }
    rig.proxy.rates(0, 0);
    m.verify(&rig.client, true, "at the end").await;
    observer.matches_a_full_read(&rig.client, &m.drive).await;
    m.collect(&rig.client, "at the end").await;
    m.verify(&rig.client, true, "after the last collection").await;
    let end_stats = rig.client.stats(&m.drive).await;
    let (files_at_end, nodes_at_end) = (m.files().len(), m.live.len());

    // Delete everything: no version left, nothing in use, and storage holds
    // only what no commit ever referred to.
    let top: Vec<Id> = m.live.iter().filter(|(_, n)| n.parent == Some(m.root)).map(|(id, _)| *id).collect();
    for id in top {
        let changes = m.deleted(&id);
        m.must_commit(&rig.client, vec![Op::Delete { node: hex(&id) }], changes, "delete everything").await;
    }
    m.collect(&rig.client, "after deleting everything").await;
    m.verify(&rig.client, true, "after deleting everything").await;
    assert!(m.rows.is_empty(), "the model still counts blobs in use");
    let stats = rig.client.stats(&m.drive).await;
    assert_eq!((stats["versions"].as_u64(), stats["blobs_in_use"].as_u64(), stats["nodes"].as_u64()), (Some(0), Some(0), Some(1)));

    // Telemetry: what the client saw, what the proxy did, what the Worker logged.
    let log = rig.worker.log();
    let seen = &rig.client.seen;
    let f = &rig.proxy.faults;
    let n = |a: &std::sync::atomic::AtomicU64| a.load(Ordering::Relaxed);
    eprintln!("\n| Worker soak, seed {seed}, {steps} steps | |");
    eprintln!("|---|---|");
    eprintln!("| Wall time | {:.0} s |", started.elapsed().as_secs_f64());
    eprintln!("| Commits applied | {} |", m.applied);
    eprintln!("| Commits answered from memory (client saw a replay) | {} |", n(&seen.replays));
    eprintln!("| Commits refused, as the model predicted | {} |", refusals.iter().map(|(k, v)| format!("{k} {v}")).collect::<Vec<_>>().join(", "));
    eprintln!("| Requests sent / retried | {} / {} |", n(&seen.requests), n(&seen.retries));
    eprintln!("| Timeouts / connection errors / 5xx seen by the client | {} / {} / {} |", n(&seen.timeouts), n(&seen.transport_errors), n(&seen.server_errors));
    eprintln!("| Proxy: connections / replies lost / requests lost | {} / {} / {} |", n(&f.connections), n(&f.replies_lost), n(&f.requests_lost));
    eprintln!("| kill -9 / freezes / collector runs | {} / {freezes} / {collections} |", rig.worker.kills);
    if let Some(store) = &rig.worker.store {
        eprintln!("| Store: spells of weather / times the server logged that it did not answer | {store_faults} / {} |", log_count(&log, "store", None));
        eprintln!("| Store weather | {} |", store.weather_report());
        eprintln!("| Store proxy | {} |", store.summary());
    }
    eprintln!("| Worker start, ms (first, then after each kill) | {:?} |", rig.worker.starts);
    eprintln!("| At the end: live nodes / files / tombstones | {nodes_at_end} / {files_at_end} / {} |", end_stats["tombstones"]);
    eprintln!("| At the end: versions / blobs in use / database bytes | {} / {} / {} |", end_stats["versions"], end_stats["blobs_in_use"], end_stats["database_bytes"]);
    eprintln!("| Worker log: commits ok / replay / refused / errors | {} / {} / {} / {} |", log_count(&log, "commit", Some("ok")), log_count(&log, "commit", Some("replay")), log_count(&log, "commit", None) - log_count(&log, "commit", Some("ok")) - log_count(&log, "commit", Some("replay")), log_count(&log, "error", None));
    for what in ["commit", "missing", "put shard", "put manifest", "get shard", "feed", "check"] {
        if let Some((count, p50, p99, max)) = seen.percentiles(what) {
            eprintln!("| {what}: n, p50 / p99 / max ms until answered | {count}, {p50} / {p99} / {max} |");
        }
    }
    assert_quiet_log(&rig.worker, &[]);
}
