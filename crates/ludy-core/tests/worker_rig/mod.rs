//! What the chaos tests of the server and the engine stand on: the server as a
//! process that can be killed, frozen and restarted; a proxy in front of it
//! that loses requests and replies; and a client that keeps trying until it
//! has an answer, counting what it saw on the way.
//!
//! The server is whichever provider `LUDY_PROVIDER` names:
//!
//! * `cloudflare` (the default): the Worker under `wrangler dev`.
//! * `rivet`: `target/release/ludy-rivet` with a Rivet engine in a Docker
//!   container of its own and a folder for the blobs. Build the binary first
//!   (`cargo build --release -p ludy-rivet`).
//!
//! The type is still called [`Worker`]: the suites were written against the
//! Cloudflare Worker, and they are the conformance test for every provider.
//!
//! On Rivet the server keeps its blobs in a folder of the rig's own. With
//! `LUDY_RIG_S3_BUCKET` set it keeps them in that bucket instead (see
//! [`s3_store`]), reached through [`StoreProxy`], which the suites use to
//! take the store away. `scripts/chaos-r2.sh` sets these; nothing else does,
//! so an ordinary run never touches anyone's bucket.
//!
//! Everything the rig starts is in a process group of its own, on ports of
//! its own, with its state in a temporary folder, and it signals nothing
//! else. A Rivet engine container is named `ludy-rig-<port>` and removed
//! when the rig is dropped; if a test run is itself killed, remove what it
//! left with `docker rm -fv $(docker ps -aq --filter label=ludy-rig)`.

#![allow(dead_code)]

use std::net::SocketAddr;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ludy_proto::synth;
use ludy_proto::tree::*;
use serde::de::DeserializeOwned;
use tokio::io::AsyncReadExt;
use tokio::net::{TcpListener, TcpStream};

/// Which server the suites run against.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Provider {
    Cloudflare,
    Rivet,
}

pub fn provider() -> Provider {
    match std::env::var("LUDY_PROVIDER").unwrap_or_default().as_str() {
        "" | "cloudflare" => Provider::Cloudflare,
        "rivet" => Provider::Rivet,
        other => panic!("LUDY_PROVIDER={other}: the providers are cloudflare and rivet"),
    }
}

/// What `kill -9` takes on Rivet: `LUDY_RIVET_KILL` is `both` (the default:
/// the server and the engine's container, as when their machine loses
/// power), `server` or `engine`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kill {
    Both,
    Server,
    Engine,
}

fn kill_mode() -> Kill {
    match std::env::var("LUDY_RIVET_KILL").unwrap_or_default().as_str() {
        "" | "both" => Kill::Both,
        "server" => Kill::Server,
        "engine" => Kill::Engine,
        other => panic!("LUDY_RIVET_KILL={other}: both, server or engine"),
    }
}

/// The Rivet engine image. The same release as the `rivetkit` crate in
/// `providers/rivet/Cargo.toml` and the image in its `docker-compose.yml`.
const RIVET_ENGINE_IMAGE: &str = "rivetdev/engine:3.1.1";
/// The engine's admin token inside the rig. Nothing outside the rig can reach that engine.
const RIVET_TOKEN: &str = "rig";
/// A drive nobody creates, asked for to see whether drives answer yet.
const PROBE_DRIVE: &str = "ffffffffffffffffffffffffffffffff";

pub fn worker_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../providers/cloudflare")
}

fn rivet_binary() -> PathBuf {
    match std::env::var("LUDY_RIVET_BIN") {
        Ok(path) => PathBuf::from(path),
        Err(_) => PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/release/ludy-rivet"),
    }
}

fn docker(args: &[&str]) -> bool {
    Command::new("docker").args(args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status().map(|s| s.success()).unwrap_or(false)
}

/// Whether the chosen provider can be started here. A suite that finds it
/// cannot skips itself and says what is missing.
pub fn available() -> bool {
    match provider() {
        Provider::Cloudflare => {
            let there = worker_dir().join("node_modules/.bin/wrangler").exists();
            if !there {
                eprintln!("skipped: app/providers/cloudflare/node_modules is not installed (run `npm install` in app/providers/cloudflare)");
            }
            there
        }
        Provider::Rivet => {
            if !rivet_binary().exists() {
                eprintln!("skipped: {} is not built (run `cargo build --release -p ludy-rivet`)", rivet_binary().display());
                return false;
            }
            if !docker(&["version"]) {
                eprintln!("skipped: Docker is not running, and the Rivet engine runs in a container");
                return false;
            }
            true
        }
    }
}

/// A bucket for the Rivet server's blobs, from the environment:
/// `LUDY_RIG_S3_BUCKET`, `LUDY_RIG_S3_ENDPOINT` (an `https://` address),
/// `LUDY_RIG_S3_KEY_ID`, `LUDY_RIG_S3_SECRET`, and `LUDY_RIG_S3_REGION`
/// (`auto` if not given). `None`, the usual case: a folder.
struct S3 {
    bucket: String,
    endpoint: String,
    key_id: String,
    secret: String,
    region: String,
}

fn s3_store() -> Option<S3> {
    let var = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
    let bucket = var("LUDY_RIG_S3_BUCKET")?;
    assert_eq!(provider(), Provider::Rivet, "LUDY_RIG_S3_BUCKET is for LUDY_PROVIDER=rivet: the Worker under wrangler dev has its own emulated R2");
    let need = |name: &str| var(name).unwrap_or_else(|| panic!("LUDY_RIG_S3_BUCKET is set, so {name} must be too"));
    let endpoint = need("LUDY_RIG_S3_ENDPOINT");
    assert!(endpoint.starts_with("https://"), "LUDY_RIG_S3_ENDPOINT must be https: the store proxy tunnels TLS and nothing else");
    Some(S3 { bucket, endpoint, key_id: need("LUDY_RIG_S3_KEY_ID"), secret: need("LUDY_RIG_S3_SECRET"), region: var("LUDY_RIG_S3_REGION").unwrap_or_else(|| "auto".into()) })
}

/// Whether this run keeps the server's blobs in a real bucket.
pub fn store_is_remote() -> bool {
    std::env::var("LUDY_RIG_S3_BUCKET").is_ok_and(|v| !v.is_empty())
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

// ------------------------------------------------------------ the process

/// The Rivet engine's container, beside the server's own process.
struct Engine {
    name: String,
    port: u16,
    created: bool,
}

pub struct Worker {
    pub dir: tempfile::TempDir,
    pub port: u16,
    inspector: u16,
    vars: Vec<(String, String)>,
    child: Option<Child>,
    /// Only with the Rivet provider.
    engine: Option<Engine>,
    s3: Option<S3>,
    /// Between the server and its bucket, when the blobs are in one.
    pub store: Option<StoreProxy>,
    /// How long each start took, in milliseconds.
    pub starts: Vec<u64>,
    pub kills: u32,
}

impl Worker {
    /// Start the server with the `_dev` routes on and the collector's grace
    /// period as given (use a huge one to keep the collector from running by itself).
    pub async fn start(gc_grace_ms: i64) -> Worker {
        Worker::start_with(vec![("LUDY_DEV".into(), "1".into()), ("GC_GRACE_MS".into(), gc_grace_ms.to_string())]).await
    }

    pub async fn start_with(vars: Vec<(String, String)>) -> Worker {
        let port = free_port();
        let engine = (provider() == Provider::Rivet).then(|| Engine { name: format!("ludy-rig-{port}"), port: free_port(), created: false });
        let s3 = s3_store();
        let store = match &s3 {
            Some(_) => Some(StoreProxy::start().await),
            None => None,
        };
        let mut w = Worker { dir: tempfile::tempdir().unwrap(), port, inspector: free_port(), vars, child: None, engine, s3, store, starts: vec![], kills: 0 };
        w.boot().await;
        w
    }

    pub fn log_path(&self) -> PathBuf {
        self.dir.path().join("wrangler.log")
    }

    pub fn log(&self) -> String {
        String::from_utf8_lossy(&std::fs::read(self.log_path()).unwrap_or_default()).into_owned()
    }

    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// The server's own process, not started yet.
    fn command(&self) -> Command {
        let Some(engine) = &self.engine else {
            let mut cmd = Command::new(worker_dir().join("node_modules/.bin/wrangler"));
            cmd.current_dir(worker_dir())
                .args(["dev", "--port", &self.port.to_string(), "--inspector-port", &self.inspector.to_string()])
                .args(["--persist-to", self.dir.path().join("state").to_str().unwrap(), "--show-interactive-dev-session=false"])
                .env("WRANGLER_SEND_METRICS", "false");
            for (k, v) in &self.vars {
                cmd.args(["--var", &format!("{k}:{v}")]);
            }
            return cmd;
        };
        let mut cmd = Command::new(rivet_binary());
        cmd.env("RIVET_ENDPOINT", format!("http://default:{RIVET_TOKEN}@127.0.0.1:{}", engine.port))
            .env("LUDY_LISTEN", format!("127.0.0.1:{}", self.port))
            .env("LUDY_BLOBS", self.dir.path().join("blobs"))
            // Without the two the harness sets, the server must behave as it does for anyone.
            .env_remove("LUDY_DEV")
            .env_remove("GC_GRACE_MS");
        if let (Some(s3), Some(store)) = (&self.s3, &self.store) {
            cmd.env("LUDY_BLOBS", format!("s3://{}", s3.bucket))
                .env("AWS_ENDPOINT", &s3.endpoint)
                .env("AWS_ACCESS_KEY_ID", &s3.key_id)
                .env("AWS_SECRET_ACCESS_KEY", &s3.secret)
                .env("AWS_REGION", &s3.region)
                // Every connection to the bucket goes through the rig.
                .env("AWS_PROXY_URL", store.url());
        }
        for (k, v) in &self.vars {
            cmd.env(k, v);
        }
        cmd
    }

    /// Start the Rivet engine's container, or start it again after a kill.
    /// Its state is in the container, so it is what the engine had on disk.
    fn boot_engine(&mut self) {
        let Some(engine) = &mut self.engine else { return };
        if engine.created {
            assert!(docker(&["start", &engine.name]), "docker start {}", engine.name);
            return;
        }
        let publish = format!("127.0.0.1:{}:6420", engine.port);
        let token = format!("RIVET__AUTH__ADMIN_TOKEN={RIVET_TOKEN}");
        #[rustfmt::skip]
        let run = [
            "run", "-d", "--name", &engine.name, "--label", "ludy-rig=1", "-p", &publish,
            "-e", &token,
            // One instance, everything on its own disk, and the same wait for
            // a silent node: what docker-compose.yml runs.
            "-e", "RIVET__STORAGE__LOG_DIRECTORY=/var/lib/rivet/log",
            "-e", "RIVET__STORAGE__AUTO_PROVISION_PARTITIONS=1",
            "-e", "RIVET__STORAGE__BALANCING__ENABLED=false",
            "-e", "RIVET__OBJECT_STORAGE__PRIMARY__FILE_SYSTEM__PATH=/var/lib/rivet/objects",
            // How long the engine waits before it gives a dead node's actors
            // to another. The default is about 45 s, which a suite that
            // kills the node dozens of times cannot afford.
            "-e", "RIVET__PEGBOARD__NODE_LOST_THRESHOLD=2000",
            "-e", "RIVET__PEGBOARD__NODE_PING_TIMEOUT=2000",
            "-v", "/var/lib/rivet/log", "-v", "/var/lib/rivet/objects",
            RIVET_ENGINE_IMAGE,
        ];
        assert!(docker(&run), "docker run {RIVET_ENGINE_IMAGE} as {}", engine.name);
        engine.created = true;
    }

    async fn boot(&mut self) {
        let started = Instant::now();
        self.boot_engine();
        // After a kill of the engine alone the server is still running.
        if self.child.is_none() {
            let log = std::fs::OpenOptions::new().create(true).append(true).open(self.log_path()).unwrap();
            let mut cmd = self.command();
            cmd.stdin(Stdio::null())
                .stdout(log.try_clone().unwrap())
                .stderr(log)
                // A group of its own, so that killing it takes its children along and nothing else.
                .process_group(0);
            self.child = Some(cmd.spawn().expect("start the server"));
        }
        let http = reqwest::Client::builder().timeout(Duration::from_secs(2)).build().unwrap();
        let deadline = Instant::now() + Duration::from_secs(180);
        // Up means a drive answers. On Rivet the front is up before the
        // engine has given the node its actors back.
        let probe = match self.engine {
            Some(_) => format!("{}/drives/{PROBE_DRIVE}", self.url()),
            None => self.url(),
        };
        loop {
            if let Ok(r) = http.get(&probe).send().await {
                if r.status().is_success() || r.status().as_u16() == 404 {
                    break;
                }
            }
            if let Some(status) = self.child.as_mut().unwrap().try_wait().unwrap() {
                panic!("the server exited with {status}:\n{}", tail(&self.log(), 40));
            }
            assert!(Instant::now() < deadline, "the server did not come up:\n{}", tail(&self.log(), 40));
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        self.starts.push(started.elapsed().as_millis() as u64);
    }

    pub fn pid(&self) -> i32 {
        self.child.as_ref().map(|c| c.id() as i32).unwrap_or(0)
    }

    fn signal(&self, signal: i32) {
        signal_group(self.pid(), signal);
    }

    /// Stop every process of the server where it stands; requests hang.
    /// On Rivet that is the node, front and actors together; the engine
    /// keeps running and sees a node that has stopped answering.
    pub fn freeze(&self) {
        self.signal(libc::SIGSTOP);
    }

    pub fn thaw(&self) {
        self.signal(libc::SIGCONT);
    }

    /// `kill -9` the whole group: nothing gets to flush or say goodbye.
    /// On Rivet the engine's container is killed the same way right after,
    /// as when the machine both run on loses power.
    pub fn kill9(&mut self) {
        if let (Some(engine), Kill::Engine) = (&self.engine, kill_mode()) {
            if self.child.is_some() && docker(&["kill", "-s", "KILL", &engine.name]) {
                self.kills += 1;
            }
            return;
        }
        self.kill_server(kill_mode() == Kill::Both);
    }

    fn kill_server(&mut self, engine_too: bool) {
        if let Some(mut child) = self.child.take() {
            let pid = child.id() as i32;
            // A stopped process does not die until it is continued on some systems; be sure.
            signal_group(pid, libc::SIGCONT);
            signal_group(pid, libc::SIGKILL);
            let _ = child.wait();
            if let (Some(engine), true) = (&self.engine, engine_too) {
                docker(&["kill", "-s", "KILL", &engine.name]);
            }
            self.kills += 1;
            // The port must really be free before anything is started on it again.
            let deadline = Instant::now() + Duration::from_secs(20);
            while std::net::TcpStream::connect(("127.0.0.1", self.port)).is_ok() {
                assert!(Instant::now() < deadline, "something still listens on port {} after the kill", self.port);
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }

    pub fn is_up(&self) -> bool {
        self.child.is_some()
    }

    pub async fn restart(&mut self) {
        self.kill9();
        self.boot().await;
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.kill_server(true);
        if let Some(engine) = &self.engine {
            docker(&["rm", "-f", "-v", &engine.name]);
        }
        if std::thread::panicking() {
            eprintln!("--- the end of the server's log ---\n{}", tail(&self.log(), 60));
        }
        // The log names every drive this server was asked about. A run
        // against a real bucket keeps it, to clean up by and to look at.
        if let Ok(keep) = std::env::var("LUDY_RIG_LOG_DIR") {
            let name = format!("server-{}-{}.log", self.port, ludy_proto::now_ms());
            let _ = std::fs::create_dir_all(&keep);
            let _ = std::fs::copy(self.log_path(), PathBuf::from(keep).join(name));
        }
    }
}

/// Signal a process group the rig started. Never anything else.
pub fn signal_group(pid: i32, signal: i32) {
    if pid > 1 {
        unsafe {
            libc::kill(-pid, signal);
        }
    }
}

pub fn tail(text: &str, lines: usize) -> String {
    let all: Vec<&str> = text.lines().collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}

/// What the Worker's log says went wrong by itself, as opposed to what the
/// test did to it: panics, traps, requests the runtime gave up on, and
/// requests that ended in an error instead of an answer.
pub fn log_troubles(log: &str) -> Vec<String> {
    log.lines()
        .filter(|l| {
            l.contains("panicked") || l.contains("RuntimeError") || l.contains("had hung") || l.contains("\"t\":\"error\"") || l.contains("Uncaught")
        })
        .map(|l| l.chars().take(300).collect())
        .collect()
}

/// Count the Worker's own event lines of one kind and outcome.
pub fn log_count(log: &str, event: &str, outcome: Option<&str>) -> usize {
    let event = format!("\"t\":\"{event}\"");
    let outcome = outcome.map(|o| format!("\"outcome\":\"{o}\""));
    log.lines().filter(|l| l.contains(&event) && outcome.as_ref().is_none_or(|o| l.contains(o))).count()
}

// -------------------------------------------------- the store's proxy

/// What stands between the server and its bucket: an HTTP proxy the
/// server's S3 client is told to use. The client asks it to `CONNECT` to
/// the bucket and speaks TLS through the tunnel, so the proxy sees no
/// request, only bytes, and what it can do to them is what a network does:
/// refuse the connection, carry nothing while holding it open, or cut it.
///
/// The S3 client keeps its connections, so a fault has to reach tunnels
/// that are already open: every tunnel checks the switches before each
/// piece it carries.
pub struct StoreProxy {
    pub addr: SocketAddr,
    pub faults: Arc<StoreFaults>,
    task: tokio::task::JoinHandle<()>,
}

const STORE_PASS: u32 = 0;
const STORE_REFUSE: u32 = 1;
const STORE_STALL: u32 = 2;

#[derive(Default)]
pub struct StoreFaults {
    /// Every spell of weather a soak asked for: what, and for how long.
    spells: Mutex<Vec<(&'static str, u64)>>,
    mode: AtomicU32,
    /// Raised to end every tunnel that is open.
    cut: AtomicU64,
    pub connections: AtomicU64,
    pub refused: AtomicU64,
    pub cuts: AtomicU64,
    pub bytes_up: AtomicU64,
    pub bytes_down: AtomicU64,
}

impl StoreProxy {
    pub async fn start() -> StoreProxy {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let faults = Arc::new(StoreFaults::default());
        let shared = faults.clone();
        let task = tokio::spawn(async move {
            loop {
                let Ok((conn, _)) = listener.accept().await else { break };
                tokio::spawn(tunnel(conn, shared.clone()));
            }
        });
        StoreProxy { addr, faults, task }
    }

    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// The store is reachable again.
    pub fn pass(&self) {
        self.faults.mode.store(STORE_PASS, Ordering::SeqCst);
    }

    /// The store is gone: open connections end, new ones are turned away.
    pub fn refuse(&self) {
        self.faults.mode.store(STORE_REFUSE, Ordering::SeqCst);
        self.faults.cut.fetch_add(1, Ordering::SeqCst);
    }

    /// The store says nothing: connections stay open and carry no byte.
    pub fn stall(&self) {
        self.faults.mode.store(STORE_STALL, Ordering::SeqCst);
    }

    /// Every open connection to the store ends in the middle of whatever
    /// it was doing; the next one gets through.
    pub fn sever(&self) {
        self.faults.cut.fetch_add(1, Ordering::SeqCst);
    }

    pub fn connections(&self) -> u64 {
        self.faults.connections.load(Ordering::SeqCst)
    }

    /// Let the store back in after `ms`, from the side, while the test
    /// goes on asking the server for things.
    pub fn heal_after(&self, ms: u64) -> tokio::task::JoinHandle<()> {
        let faults = self.faults.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(ms)).await;
            faults.mode.store(STORE_PASS, Ordering::SeqCst);
        })
    }

    /// Some weather for a soak, chosen by two rolls of its dice (`kind`
    /// below 100, `length` below 1000): the store turns connections away,
    /// says nothing, or drops what is open, mostly for a second or two,
    /// sometimes for longer than a client waits (3 s here, 8 s in the
    /// engine), and now and then for longer than any single request lives.
    /// Returns what it did and the task that lets the store back in.
    pub fn weather(&self, kind: u64, length: u64) -> (String, tokio::task::JoinHandle<()>) {
        let short = 300 + length * 5 / 2;
        let medium = 4000 + length * 4;
        let long = 32_000 + length * 6;
        let (what, ms) = match kind {
            0..=34 => ("is gone", short),
            35..=64 => ("says nothing", short),
            65..=79 => ("drops its connections", 0),
            80..=89 => ("says nothing", medium),
            90..=96 => ("is gone", medium),
            _ => ("says nothing", long),
        };
        match what {
            "is gone" => self.refuse(),
            "says nothing" => self.stall(),
            _ => self.sever(),
        }
        self.faults.spells.lock().unwrap().push((what, ms));
        (if ms == 0 { format!("the store {what}") } else { format!("the store {what} for {ms} ms") }, self.heal_after(ms))
    }

    /// The weather a soak had, for its report: how often each kind, the
    /// longest of each, and how many spells outlasted 3, 8 and 30 seconds
    /// (the rig client's patience, the engine's, and a request's life).
    pub fn weather_report(&self) -> String {
        let spells = self.faults.spells.lock().unwrap();
        let of = |what: &str| {
            let ms: Vec<u64> = spells.iter().filter(|s| s.0 == what).map(|s| s.1).collect();
            (ms.len(), ms.iter().copied().max().unwrap_or(0))
        };
        let (gone, silent, dropped) = (of("is gone"), of("says nothing"), of("drops its connections"));
        let over = |limit: u64| spells.iter().filter(|s| s.1 > limit).count();
        format!("gone {} times (longest {} ms), silent {} times (longest {} ms), connections dropped {} times; {} spells over 3 s, {} over 8 s, {} over 30 s", gone.0, gone.1, silent.0, silent.1, dropped.0, over(3000), over(8000), over(30_000))
    }

    /// One line for a report: what went through and what was done to it.
    pub fn summary(&self) -> String {
        let n = |a: &AtomicU64| a.load(Ordering::SeqCst);
        let f = &self.faults;
        format!("{} connections, {} turned away, {} cut; {} bytes up, {} down", n(&f.connections), n(&f.refused), n(&f.cuts), n(&f.bytes_up), n(&f.bytes_down))
    }
}

/// Cut every open connection to the store, from a task that only holds the switches.
pub fn faults_sever(faults: &StoreFaults) {
    faults.cut.fetch_add(1, Ordering::SeqCst);
}

impl Drop for StoreProxy {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Carry one direction of a tunnel until it ends, the switches say stop,
/// or the tunnel is cut. Returns whether it was the rig that ended it.
async fn pump(mut from: tokio::net::tcp::OwnedReadHalf, mut to: tokio::net::tcp::OwnedWriteHalf, faults: Arc<StoreFaults>, born: u64, count: fn(&StoreFaults) -> &AtomicU64) -> bool {
    use tokio::io::AsyncWriteExt;
    let ended = |f: &StoreFaults| f.cut.load(Ordering::SeqCst) != born || f.mode.load(Ordering::SeqCst) == STORE_REFUSE;
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = tokio::select! {
            read = from.read(&mut buf) => match read { Ok(n) if n > 0 => n, _ => return false },
            _ = async { while !ended(&faults) { tokio::time::sleep(Duration::from_millis(10)).await } } => return true,
        };
        // A stalled store has the bytes and does nothing with them.
        while faults.mode.load(Ordering::SeqCst) == STORE_STALL && !ended(&faults) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        if ended(&faults) {
            return true;
        }
        if to.write_all(&buf[..n]).await.is_err() {
            return false;
        }
        count(&faults).fetch_add(n as u64, Ordering::Relaxed);
    }
}

async fn tunnel(mut client: TcpStream, faults: Arc<StoreFaults>) {
    use tokio::io::AsyncWriteExt;
    faults.connections.fetch_add(1, Ordering::SeqCst);
    let born = faults.cut.load(Ordering::SeqCst);
    // `CONNECT host:port HTTP/1.1`, some headers, an empty line.
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        match client.read(&mut byte).await {
            Ok(1) if head.len() < 8192 => head.push(byte[0]),
            _ => return,
        }
    }
    let head = String::from_utf8_lossy(&head).into_owned();
    let mut words = head.split_whitespace();
    let (Some("CONNECT"), Some(target)) = (words.next(), words.next()) else {
        let _ = client.write_all(b"HTTP/1.1 405 Method Not Allowed\r\n\r\n").await;
        return;
    };
    // A store that says nothing does not answer the knock either.
    while faults.mode.load(Ordering::SeqCst) == STORE_STALL && faults.cut.load(Ordering::SeqCst) == born {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    if faults.mode.load(Ordering::SeqCst) == STORE_REFUSE || faults.cut.load(Ordering::SeqCst) != born {
        faults.refused.fetch_add(1, Ordering::SeqCst);
        return;
    }
    let Ok(upstream) = TcpStream::connect(target).await else {
        let _ = client.write_all(b"HTTP/1.1 502 Bad Gateway\r\n\r\n").await;
        return;
    };
    if client.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n").await.is_err() {
        return;
    }
    let (from_client, to_client) = client.into_split();
    let (from_upstream, to_upstream) = upstream.into_split();
    let by_the_rig = tokio::select! {
        r = pump(from_client, to_upstream, faults.clone(), born, |f| &f.bytes_up) => r,
        r = pump(from_upstream, to_client, faults.clone(), born, |f| &f.bytes_down) => r,
    };
    if by_the_rig {
        faults.cuts.fetch_add(1, Ordering::SeqCst);
    }
}

// -------------------------------------------------------------- the proxy

/// A TCP proxy in front of the Worker. Each connection carries one request
/// (the test client does not reuse connections), and each may be let
/// through, dropped before it reaches the Worker, or passed on with the
/// reply thrown away: the Worker did the work and the client never hears.
pub struct Proxy {
    pub addr: SocketAddr,
    pub faults: Arc<Faults>,
    task: tokio::task::JoinHandle<()>,
}

#[derive(Default)]
pub struct Faults {
    upstream: AtomicU32,
    seed: AtomicU64,
    /// Per thousand connections.
    pub lose_reply: AtomicU32,
    pub lose_request: AtomicU32,
    /// Accept and say nothing, until switched off.
    pub hold: AtomicBool,
    pub connections: AtomicU64,
    pub replies_lost: AtomicU64,
    pub requests_lost: AtomicU64,
    /// Lose the next this-many replies regardless of the rates.
    pub lose_next_replies: AtomicU32,
}

impl Proxy {
    pub async fn start(upstream_port: u16, seed: u64) -> Proxy {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let faults = Arc::new(Faults::default());
        faults.upstream.store(upstream_port as u32, Ordering::SeqCst);
        faults.seed.store(seed, Ordering::SeqCst);
        let shared = faults.clone();
        let task = tokio::spawn(async move {
            loop {
                let Ok((conn, _)) = listener.accept().await else { break };
                tokio::spawn(carry(conn, shared.clone()));
            }
        });
        Proxy { addr, faults, task }
    }

    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub fn rates(&self, lose_reply: u32, lose_request: u32) {
        self.faults.lose_reply.store(lose_reply, Ordering::SeqCst);
        self.faults.lose_request.store(lose_request, Ordering::SeqCst);
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn carry(mut client: TcpStream, faults: Arc<Faults>) {
    let n = faults.connections.fetch_add(1, Ordering::SeqCst);
    while faults.hold.load(Ordering::SeqCst) {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let roll = (synth::word(faults.seed.load(Ordering::SeqCst), n) % 1000) as u32;
    let lose_request = faults.lose_request.load(Ordering::SeqCst);
    if roll < lose_request {
        faults.requests_lost.fetch_add(1, Ordering::SeqCst);
        return;
    }
    let Ok(mut upstream) = TcpStream::connect(("127.0.0.1", faults.upstream.load(Ordering::SeqCst) as u16)).await else {
        return;
    };
    let forced = faults.lose_next_replies.try_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1)).is_ok();
    if forced || roll < lose_request + faults.lose_reply.load(Ordering::SeqCst) {
        // Pass the request on, wait for the Worker to start answering, and hang up.
        let (mut from_client, _to_client) = client.split();
        let (mut from_upstream, mut to_upstream) = upstream.split();
        let mut first = [0u8; 1];
        tokio::select! {
            _ = async { let _ = tokio::io::copy(&mut from_client, &mut to_upstream).await; std::future::pending::<()>().await } => {}
            _ = from_upstream.read(&mut first) => {}
        }
        faults.replies_lost.fetch_add(1, Ordering::SeqCst);
        return;
    }
    let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
}

// ------------------------------------------------------------- the client

/// What the client saw, for the report at the end of a run.
#[derive(Default)]
pub struct Seen {
    pub requests: AtomicU64,
    pub retries: AtomicU64,
    pub transport_errors: AtomicU64,
    pub timeouts: AtomicU64,
    pub server_errors: AtomicU64,
    pub replays: AtomicU64,
    /// (what, milliseconds until an answer, attempts)
    pub timings: Mutex<Vec<(&'static str, u64, u32)>>,
}

impl Seen {
    pub fn percentiles(&self, what: &str) -> Option<(usize, u64, u64, u64)> {
        let mut ms: Vec<u64> = self.timings.lock().unwrap().iter().filter(|t| t.0 == what).map(|t| t.1).collect();
        if ms.is_empty() {
            return None;
        }
        ms.sort_unstable();
        let at = |p: usize| ms[(ms.len() * p / 100).min(ms.len() - 1)];
        Some((ms.len(), at(50), at(99), *ms.last().unwrap()))
    }
}

#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    base: String,
    pub seen: Arc<Seen>,
    /// Give up on a request that has had no answer for this long, however often it was retried.
    pub patience: Duration,
}

pub enum Answer<T> {
    Ok(T),
    Refused(Refusal),
}

impl<T> Answer<T> {
    pub fn unwrap(self) -> T {
        match self {
            Answer::Ok(v) => v,
            Answer::Refused(r) => panic!("refused: {} ({})", r.error, r.detail),
        }
    }

    pub fn refusal(self) -> Option<Refusal> {
        match self {
            Answer::Ok(_) => None,
            Answer::Refused(r) => Some(r),
        }
    }
}

impl Client {
    pub fn new(base: &str) -> Client {
        // No connection reuse: the proxy decides the fate of one request per connection.
        let http = reqwest::Client::builder().pool_max_idle_per_host(0).timeout(Duration::from_secs(3)).build().unwrap();
        Client { http, base: base.trim_end_matches('/').to_string(), seen: Arc::new(Seen::default()), patience: Duration::from_secs(240) }
    }

    /// Send until there is an answer: a 2xx, or a refusal the Worker wrote.
    /// A lost request, a lost reply, a timeout, a dead or restarting Worker
    /// and a 5xx all mean "ask again".
    async fn ask(&self, what: &'static str, method: reqwest::Method, path: &str, body: Option<Vec<u8>>, range: Option<(u64, u64)>) -> Answer<Vec<u8>> {
        let (started, mut attempts) = (Instant::now(), 0u32);
        loop {
            attempts += 1;
            self.seen.requests.fetch_add(1, Ordering::Relaxed);
            let mut req = self.http.request(method.clone(), format!("{}{path}", self.base));
            if let Some(body) = &body {
                req = req.body(body.clone());
            }
            if let Some((a, b)) = range {
                req = req.header("range", format!("bytes={a}-{b}"));
            }
            let outcome = match req.send().await {
                Ok(resp) => {
                    let status = resp.status();
                    match resp.bytes().await {
                        Ok(bytes) if status.is_success() => Some(Answer::Ok(bytes.to_vec())),
                        Ok(bytes) if status.is_client_error() => match serde_json::from_slice::<Refusal>(&bytes) {
                            Ok(r) => Some(Answer::Refused(r)),
                            Err(_) => panic!("{what}: {status} without a refusal: {}", String::from_utf8_lossy(&bytes)),
                        },
                        Ok(_) => {
                            self.seen.server_errors.fetch_add(1, Ordering::Relaxed);
                            None
                        }
                        Err(_) => {
                            self.seen.transport_errors.fetch_add(1, Ordering::Relaxed);
                            None
                        }
                    }
                }
                Err(e) => {
                    let counter = if e.is_timeout() { &self.seen.timeouts } else { &self.seen.transport_errors };
                    counter.fetch_add(1, Ordering::Relaxed);
                    None
                }
            };
            if let Some(answer) = outcome {
                self.seen.timings.lock().unwrap().push((what, started.elapsed().as_millis() as u64, attempts));
                return answer;
            }
            assert!(started.elapsed() < self.patience, "{what} {path}: no answer in {:?} and {attempts} attempts", self.patience);
            self.seen.retries.fetch_add(1, Ordering::Relaxed);
            tokio::time::sleep(Duration::from_millis((20 * attempts as u64).min(400))).await;
        }
    }

    async fn json<T: DeserializeOwned>(&self, what: &'static str, method: reqwest::Method, path: &str, body: Option<Vec<u8>>) -> Answer<T> {
        match self.ask(what, method, path, body, None).await {
            Answer::Ok(bytes) => Answer::Ok(serde_json::from_slice(&bytes).unwrap_or_else(|e| panic!("{what}: {e}: {}", String::from_utf8_lossy(&bytes)))),
            Answer::Refused(r) => Answer::Refused(r),
        }
    }

    pub async fn create_drive(&self, drive: &str, body: &CreateDrive) -> Answer<DriveHead> {
        self.json("create", reqwest::Method::PUT, &format!("/drives/{drive}"), Some(serde_json::to_vec(body).unwrap())).await
    }

    pub async fn head(&self, drive: &str) -> Answer<DriveHead> {
        self.json("head", reqwest::Method::GET, &format!("/drives/{drive}"), None).await
    }

    /// The same bytes on every attempt, so that a retry is recognised.
    pub async fn commit(&self, drive: &str, commit: &Commit) -> Answer<Committed> {
        let answer = self.json::<Committed>("commit", reqwest::Method::POST, &format!("/drives/{drive}/commit"), Some(serde_json::to_vec(commit).unwrap())).await;
        if matches!(&answer, Answer::Ok(c) if c.replayed) {
            self.seen.replays.fetch_add(1, Ordering::Relaxed);
        }
        answer
    }

    pub async fn missing(&self, drive: &str, blobs: &Blobs) -> Blobs {
        self.json("missing", reqwest::Method::POST, &format!("/drives/{drive}/missing"), Some(serde_json::to_vec(blobs).unwrap())).await.unwrap()
    }

    pub async fn put_shard(&self, drive: &str, hash: &str, bytes: Vec<u8>) {
        self.ask("put shard", reqwest::Method::PUT, &format!("/drives/{drive}/shards/{hash}"), Some(bytes), None).await.unwrap();
    }

    pub async fn put_manifest(&self, drive: &str, hash: &str, bytes: Vec<u8>) {
        self.ask("put manifest", reqwest::Method::PUT, &format!("/drives/{drive}/manifests/{hash}"), Some(bytes), None).await.unwrap();
    }

    pub async fn get_shard(&self, drive: &str, hash: &str, range: Option<(u64, u64)>) -> Answer<Vec<u8>> {
        self.ask("get shard", reqwest::Method::GET, &format!("/drives/{drive}/shards/{hash}"), None, range).await
    }

    pub async fn get_manifest(&self, drive: &str, hash: &str) -> Answer<Vec<u8>> {
        self.ask("get manifest", reqwest::Method::GET, &format!("/drives/{drive}/manifests/{hash}"), None, None).await
    }

    pub async fn feed(&self, drive: &str, since: u64, epoch: Option<u64>, limit: usize) -> Feed {
        let epoch = epoch.map(|e| format!("&epoch={e}")).unwrap_or_default();
        self.json("feed", reqwest::Method::GET, &format!("/drives/{drive}/feed?since={since}&limit={limit}{epoch}"), None).await.unwrap()
    }

    /// Every node the drive has ever had, by pages of `limit`.
    pub async fn whole_feed(&self, drive: &str, limit: usize) -> (u64, Vec<NodeInfo>) {
        let (mut since, mut nodes) = (0, Vec::new());
        loop {
            let page = self.feed(drive, since, None, limit).await;
            assert!(!page.reset);
            since = page.nodes.last().map(|n| n.seq).unwrap_or(since);
            nodes.extend(page.nodes);
            if !page.more {
                return (page.seq, nodes);
            }
        }
    }

    pub async fn node(&self, drive: &str, id: &str) -> Answer<NodeInfo> {
        self.json("node", reqwest::Method::GET, &format!("/drives/{drive}/nodes/{id}"), None).await
    }

    pub async fn versions(&self, drive: &str, id: &str) -> Answer<Vec<VersionEntry>> {
        self.json("versions", reqwest::Method::GET, &format!("/drives/{drive}/nodes/{id}/versions"), None).await
    }

    pub async fn stats(&self, drive: &str) -> serde_json::Value {
        self.json("stats", reqwest::Method::GET, &format!("/drives/{drive}/stats"), None).await.unwrap()
    }

    /// The Worker's own recount of everything it stores twice. Panics with its findings.
    pub async fn check(&self, drive: &str, when: &str) {
        let report: serde_json::Value = self.json("check", reqwest::Method::GET, &format!("/drives/{drive}/_dev/check"), None).await.unwrap();
        assert!(report["ok"] == true, "{when}: the drive's own check failed: {}", report["problems"]);
    }

    pub async fn collect(&self, drive: &str) -> serde_json::Value {
        self.json("collect", reqwest::Method::POST, &format!("/drives/{drive}/_dev/gc"), None).await.unwrap()
    }

    /// One run of the collector that stops as a dying instance would: `n`
    /// objects deleted from storage, the rest still marked as being deleted.
    pub async fn collect_and_stop_after(&self, drive: &str, n: usize) -> serde_json::Value {
        self.json("collect", reqwest::Method::POST, &format!("/drives/{drive}/_dev/gc?stop_after={n}"), None).await.unwrap()
    }

    /// What storage holds for the drive: `s/<sha256>` and `m/<sha256>`.
    pub async fn stored(&self, drive: &str) -> Vec<String> {
        self.json("stored", reqwest::Method::GET, &format!("/_dev/blobs/{drive}"), None).await.unwrap()
    }

    pub async fn lose_blob(&self, drive: &str, blob: &str) {
        self.ask("lose blob", reqwest::Method::DELETE, &format!("/_dev/blobs/{drive}/{blob}"), None, None).await.unwrap();
    }

    /// One attempt with a body, no retry: status and what came back.
    pub async fn raw(&self, method: reqwest::Method, path: &str, body: Vec<u8>) -> (u16, Vec<u8>) {
        let resp = self.http.request(method, format!("{}{path}", self.base)).timeout(Duration::from_secs(30)).body(body).send().await.expect("the Worker is up");
        (resp.status().as_u16(), resp.bytes().await.unwrap().to_vec())
    }

    /// One attempt, no retry, with a limit of its own: the status if an
    /// answer came within `limit`, and how long that took.
    pub async fn attempt(&self, method: reqwest::Method, path: &str, body: Option<Vec<u8>>, limit: Duration) -> (Option<u16>, Duration) {
        let started = Instant::now();
        let mut req = self.http.request(method, format!("{}{path}", self.base)).timeout(limit);
        if let Some(body) = body {
            req = req.body(body);
        }
        let status = match req.send().await {
            Ok(resp) => {
                let status = resp.status().as_u16();
                resp.bytes().await.ok().map(|_| status)
            }
            Err(_) => None,
        };
        (status, started.elapsed())
    }

    /// The same, keeping what came back: the status and the body as text.
    pub async fn attempt_text(&self, method: reqwest::Method, path: &str, body: Option<Vec<u8>>, limit: Duration) -> (Option<(u16, String)>, Duration) {
        let started = Instant::now();
        let mut req = self.http.request(method, format!("{}{path}", self.base)).timeout(limit);
        if let Some(body) = body {
            req = req.body(body);
        }
        let answer = match req.send().await {
            Ok(resp) => {
                let status = resp.status().as_u16();
                resp.text().await.ok().map(|text| (status, text))
            }
            Err(_) => None,
        };
        (answer, started.elapsed())
    }

    /// One attempt, no retry: for requests that are expected to fail.
    pub async fn once(&self, method: reqwest::Method, path: &str) -> Option<u16> {
        self.http.request(method, format!("{}{path}", self.base)).send().await.ok().map(|r| r.status().as_u16())
    }
}

// ------------------------------------------------------- an outside reader

/// One file or folder as someone holding the root key reads it.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    /// `None` for a folder.
    pub content: Option<Vec<u8>>,
    pub mtime_ms: u64,
    pub version: u64,
}

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len() / 2).map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap()).collect()
}

/// Read a whole drive the way a stranger with the root key would: the feed,
/// each key unwrapped from the root down, each name opened, each file's
/// manifest and shards fetched and opened. It shares the encryption format
/// with the engine and none of the engine's code, so it is what the engine's
/// tests compare against. Panics on anything that does not open.
pub async fn read_drive(client: &Client, drive: &str, root: [u8; 16], root_key: &ludy_proto::crypt::NodeKey) -> std::collections::BTreeMap<String, Entry> {
    use ludy_proto::crypt::{self, NodeId, PrivateSegment};
    use std::collections::{BTreeMap, HashMap};
    let (_, nodes) = client.whole_feed(drive, 500).await;
    let live: Vec<&NodeInfo> = nodes.iter().filter(|n| !n.deleted).collect();
    let mut keys: HashMap<String, (crypt::NodeKey, String)> = HashMap::from([(crypt::hex(&root), (root_key.clone(), String::new()))]);
    let mut out = BTreeMap::new();
    let mut left: Vec<&NodeInfo> = live.iter().copied().filter(|n| n.parent.is_some()).collect();
    while !left.is_empty() {
        let before = left.len();
        let mut next = Vec::new();
        for n in left {
            let Some((folder_key, folder_path)) = keys.get(n.parent.as_ref().unwrap()).cloned() else {
                next.push(n);
                continue;
            };
            let id = NodeId(<[u8; 16]>::try_from(&unhex(&n.id)[..]).unwrap());
            let key = folder_key.unwrap_child(&id, &unhex(&n.key)).expect("a node's key opens under its folder's");
            let name = key.open_name(&id, &unhex(&n.name)).expect("a node's name opens under its key");
            assert_eq!(n.name_hash, crypt::hex(&folder_key.name_hash(&name).unwrap()), "the name hash of {name}");
            let path = if folder_path.is_empty() { name.clone() } else { format!("{folder_path}/{name}") };
            let entry = match (&n.kind, &n.version) {
                (Kind::Folder, _) => Entry { content: None, mtime_ms: 0, version: 0 },
                (Kind::File, None) => Entry { content: Some(Vec::new()), mtime_ms: 0, version: 0 },
                (Kind::File, Some(v)) => {
                    let sealed = client.get_manifest(drive, &v.manifest).await.unwrap();
                    let manifest = key.open_manifest(&id, &sealed).expect("a manifest opens under the file's key");
                    assert_eq!(manifest.size, v.size, "{path}: the manifest and the node disagree about the size");
                    let mut content = Vec::with_capacity(v.size as usize);
                    for segment in &manifest.segments {
                        match segment {
                            PrivateSegment::Shard { id, n, key } => {
                                let sealed = client.get_shard(drive, &crypt::hex(id), None).await.unwrap();
                                content.extend_from_slice(&crypt::open_shard(key, *n as u64, &sealed).expect("a shard opens with the key in the manifest"));
                            }
                            PrivateSegment::Synth { seed, off, n } => content.extend_from_slice(&synth::bytes(*seed, *off, *n as usize)),
                        }
                    }
                    Entry { content: Some(content), mtime_ms: key.open_meta(&id, &unhex(&v.meta)).expect("version details open").mtime_ms, version: v.id }
                }
            };
            assert!(out.insert(path.clone(), entry).is_none(), "two nodes at {path}");
            keys.insert(n.id.clone(), (key, path));
        }
        assert!(next.len() < before, "nodes whose folder is not in the drive: {:?}", next.iter().map(|n| &n.id).collect::<Vec<_>>());
        left = next;
    }
    out
}
