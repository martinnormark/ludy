//! Ludy's simulated cloud: an S3-like object store served over HTTP on localhost.
//!
//! * a drive is a bucket: `PUT /<drive>`, `GET /<drive>` (list), `DELETE /<drive>`
//! * objects: `HEAD`, `GET` (with `Range`), `PUT`, `DELETE` on `/<drive>/<key>`
//! * every commit to a drive goes through that drive's lock (one commit
//!   authority) and bumps its sequence number; `GET /<drive>?changes&since=N`
//!   long-polls for the next commit
//! * every object version is immutable and has an id that doubles as its ETag,
//!   so a client may cache `(drive, key, version, block)` forever
//! * synthetic objects generate their bytes from the offset (see [`synth`])
//! * configurable latency and bandwidth, and a log of every range served

pub mod harness;
pub mod shard;
pub mod store;
pub mod synth;

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::extract::DefaultBodyLimit;
use axum::routing::{get, post, put};
use axum::{Json, Router};
use bytes::Bytes;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::json;

use shard::Manifest;
use store::{Content, Store, StoreError, Version};

/// Size of the pieces a response body is streamed (and throttled) in.
const STREAM_CHUNK: u64 = 64 * 1024;
const LOG_CAPACITY: usize = 4000;

#[derive(Clone, Debug)]
pub struct Config {
    pub data_dir: PathBuf,
    pub addr: SocketAddr,
    /// Delay added before the first byte of every response.
    pub latency_ms: u64,
    /// Shared bandwidth cap in megabits per second; 0 means unlimited.
    pub mbps: u64,
    /// Print each request to stdout.
    pub print_log: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LogEntry {
    pub n: u64,
    pub unix_ms: u64,
    pub method: String,
    pub drive: String,
    pub key: String,
    /// Byte range served or received, when the request carried a body.
    pub range: Option<(u64, u64)>,
    pub status: u16,
    pub bytes: u64,
    pub ms: u64,
}

#[derive(Default)]
struct Counters {
    requests: AtomicU64,
    bytes_out: AtomicU64,
    bytes_in: AtomicU64,
}

/// Token bucket shared by every transfer, so parallel streams split the link.
struct Throttle {
    bytes_per_sec: AtomicU64,
    next_free: Mutex<Instant>,
}

impl Throttle {
    /// Account for `n` bytes and wait until the link would have carried them.
    /// Waits shorter than a few milliseconds are skipped and made up for
    /// later: timers are no finer than a millisecond, so sleeping for every
    /// 64 KiB piece would hold a fast link to half its rate.
    async fn take(&self, n: u64) {
        let rate = self.bytes_per_sec.load(Ordering::Relaxed);
        if rate == 0 {
            return;
        }
        let cost = Duration::from_secs_f64(n as f64 / rate as f64);
        let now = Instant::now();
        let until = {
            let mut next = self.next_free.lock().unwrap();
            let start = (*next).max(now);
            *next = start + cost;
            *next
        };
        if until > now + Duration::from_millis(5) {
            tokio::time::sleep_until(until.into()).await;
        }
    }
}

/// Ways to make the cloud misbehave, for tests of what clients do about it.
/// Set over HTTP with `PUT /_config?stall=1&unavailable=0&hold_commits=0`.
#[derive(Default)]
pub struct Faults {
    /// Accept every request and never answer it.
    pub stall: AtomicBool,
    /// Answer every request with 503 at once.
    pub unavailable: AtomicBool,
    /// Keep manifest commits waiting until this is cleared.
    pub hold_commits: AtomicBool,
    /// How many commits are waiting right now.
    pub commits_held: AtomicU64,
}

pub struct Cloud {
    pub store: Store,
    pub faults: Faults,
    latency_ms: AtomicU64,
    throttle: Throttle,
    counters: Counters,
    log: Mutex<VecDeque<LogEntry>>,
    log_n: AtomicU64,
    print_log: bool,
    started: Instant,
}

type AppState = Arc<Cloud>;

impl Cloud {
    pub fn open(cfg: &Config) -> std::io::Result<Arc<Cloud>> {
        Ok(Arc::new(Cloud {
            store: Store::open(&cfg.data_dir)?,
            faults: Faults::default(),
            latency_ms: AtomicU64::new(cfg.latency_ms),
            throttle: Throttle {
                bytes_per_sec: AtomicU64::new(cfg.mbps * 1_000_000 / 8),
                next_free: Mutex::new(Instant::now()),
            },
            counters: Counters::default(),
            log: Mutex::new(VecDeque::new()),
            log_n: AtomicU64::new(0),
            print_log: cfg.print_log,
            started: Instant::now(),
        }))
    }

    async fn latency(&self) {
        let ms = self.latency_ms.load(Ordering::Relaxed);
        if ms > 0 {
            tokio::time::sleep(Duration::from_millis(ms)).await;
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn record(&self, method: &Method, drive: &str, key: &str, range: Option<(u64, u64)>, status: StatusCode, bytes: u64, t0: Instant) {
        let entry = LogEntry {
            n: self.log_n.fetch_add(1, Ordering::Relaxed) + 1,
            unix_ms: now_ms(),
            method: method.as_str().to_string(),
            drive: drive.to_string(),
            key: key.to_string(),
            range,
            status: status.as_u16(),
            bytes,
            ms: t0.elapsed().as_millis() as u64,
        };
        self.counters.requests.fetch_add(1, Ordering::Relaxed);
        if self.print_log {
            let range = match entry.range {
                Some((a, b)) => format!("bytes={a}-{b} ({})", human(b + 1 - a)),
                None => "-".to_string(),
            };
            println!(
                "[{:9.3}s] {:<6} {} /{}{}{}  {}  {}ms",
                self.started.elapsed().as_secs_f64(),
                entry.method,
                entry.status,
                entry.drive,
                if entry.key.is_empty() { "" } else { "/" },
                entry.key,
                range,
                entry.ms
            );
        }
        let mut log = self.log.lock().unwrap();
        if log.len() >= LOG_CAPACITY {
            log.pop_front();
        }
        log.push_back(entry);
    }

    /// Log entries with a sequence number greater than `since`.
    pub fn log_since(&self, since: u64) -> Vec<LogEntry> {
        self.log.lock().unwrap().iter().filter(|e| e.n > since).cloned().collect()
    }
}

pub fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

pub fn human(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

pub fn router(cloud: Arc<Cloud>) -> Router {
    Router::new()
        .route("/", get(list_drives))
        .route("/_log", get(get_log))
        .route("/_stats", get(get_stats))
        .route("/_config", get(get_stats).put(put_config).post(put_config))
        .route("/_shards", post(shards_missing))
        .route("/_shards/{hash}", put(shard_put))
        .route("/{drive}", get(drive_get).put(drive_put).post(drive_post).delete(drive_delete))
        .route(
            "/{drive}/{*key}",
            get(object_get).head(object_head).put(object_put).post(object_post).delete(object_delete),
        )
        // Shards are up to 16 MiB; object bodies are streamed and unlimited.
        .layer(DefaultBodyLimit::max(shard::MAX_SHARD as usize + 1024))
        .layer(axum::middleware::from_fn_with_state(cloud.clone(), inject_faults))
        .with_state(cloud)
}

/// Bind and serve. Returns the bound address (useful with port 0) and the task.
pub async fn serve(cfg: Config) -> std::io::Result<(SocketAddr, Arc<Cloud>, tokio::task::JoinHandle<()>)> {
    let cloud = Cloud::open(&cfg)?;
    let listener = tokio::net::TcpListener::bind(cfg.addr).await?;
    let addr = listener.local_addr()?;
    let app = router(cloud.clone());
    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Ok((addr, cloud, task))
}

/// Applies [`Faults`] to everything except the admin endpoints, so a test
/// can always switch a fault off again.
async fn inject_faults(State(c): State<AppState>, req: axum::extract::Request, next: axum::middleware::Next) -> Response {
    let path = req.uri().path();
    let admin = path == "/_config" || path == "/_log" || path == "/_stats";
    if !admin {
        if c.faults.stall.load(Ordering::Relaxed) {
            std::future::pending::<()>().await;
        }
        if c.faults.unavailable.load(Ordering::Relaxed) {
            return (StatusCode::SERVICE_UNAVAILABLE, Json(json!({ "error": "the cloud is unavailable" }))).into_response();
        }
    }
    next.run(req).await
}

// ---------------------------------------------------------------- errors

struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "error": self.1 }))).into_response()
    }
}

impl ApiError {
    fn status(e: &StoreError) -> StatusCode {
        match e {
            StoreError::NoSuchDrive | StoreError::NoSuchKey | StoreError::NoSuchVersion => StatusCode::NOT_FOUND,
            StoreError::DriveExists | StoreError::MissingShards(_) => StatusCode::CONFLICT,
            StoreError::BadName(_) | StoreError::BadManifest(_) | StoreError::BadShard(_) => StatusCode::BAD_REQUEST,
            StoreError::Precondition => StatusCode::PRECONDITION_FAILED,
            StoreError::Io(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

impl From<StoreError> for ApiError {
    fn from(e: StoreError) -> Self {
        ApiError(ApiError::status(&e), e.to_string())
    }
}

type ApiResult = Result<Response, ApiError>;
type Params = Query<HashMap<String, String>>;

// ---------------------------------------------------------------- admin

async fn list_drives(State(c): State<AppState>) -> ApiResult {
    let t0 = Instant::now();
    c.latency().await;
    let drives = c.store.drives().await;
    c.record(&Method::GET, "", "", None, StatusCode::OK, 0, t0);
    Ok(Json(json!({ "drives": drives })).into_response())
}

async fn get_log(State(c): State<AppState>, Query(q): Params) -> ApiResult {
    let since = q.get("since").and_then(|s| s.parse().ok()).unwrap_or(0);
    Ok(Json(c.log_since(since)).into_response())
}

async fn get_stats(State(c): State<AppState>) -> ApiResult {
    let shards = c.store.shard_stats();
    Ok(Json(json!({
        "requests": c.counters.requests.load(Ordering::Relaxed),
        "bytes_out": c.counters.bytes_out.load(Ordering::Relaxed),
        "bytes_in": c.counters.bytes_in.load(Ordering::Relaxed),
        "latency_ms": c.latency_ms.load(Ordering::Relaxed),
        "mbps": c.throttle.bytes_per_sec.load(Ordering::Relaxed) * 8 / 1_000_000,
        "uptime_s": c.started.elapsed().as_secs(),
        "shards": shards.shards,
        "shard_bytes": shards.shard_bytes,
        "manifests": shards.manifests,
    }))
    .into_response())
}

async fn put_config(State(c): State<AppState>, Query(q): Params) -> ApiResult {
    if let Some(v) = q.get("latency_ms").and_then(|s| s.parse::<u64>().ok()) {
        c.latency_ms.store(v, Ordering::Relaxed);
    }
    if let Some(v) = q.get("mbps").and_then(|s| s.parse::<u64>().ok()) {
        c.throttle.bytes_per_sec.store(v * 1_000_000 / 8, Ordering::Relaxed);
    }
    let flag = |name: &str| q.get(name).map(|v| v == "1" || v == "true");
    if let Some(on) = flag("stall") {
        c.faults.stall.store(on, Ordering::Relaxed);
    }
    if let Some(on) = flag("unavailable") {
        c.faults.unavailable.store(on, Ordering::Relaxed);
    }
    if let Some(on) = flag("hold_commits") {
        c.faults.hold_commits.store(on, Ordering::Relaxed);
    }
    if flag("sweep") == Some(true) {
        c.store.sweep_orphans();
    }
    get_stats(State(c)).await
}

// ---------------------------------------------------------------- shards

/// `POST /_shards` with a JSON array of hashes: which of them the cloud lacks.
/// A client asks this before uploading, and sends only what is missing.
async fn shards_missing(State(c): State<AppState>, body: Bytes) -> ApiResult {
    let t0 = Instant::now();
    c.latency().await;
    let hashes: Vec<String> = serde_json::from_slice(&body).map_err(|e| ApiError(StatusCode::BAD_REQUEST, e.to_string()))?;
    let missing = c.store.missing_shards(&hashes);
    c.record(&Method::POST, "_shards", &format!("{} of {} missing", missing.len(), hashes.len()), None, StatusCode::OK, 0, t0);
    Ok(Json(json!({ "missing": missing })).into_response())
}

/// `PUT /_shards/<sha256>`: store one shard. The body must hash to the name.
async fn shard_put(State(c): State<AppState>, Path(hash): Path<String>, body: Bytes) -> ApiResult {
    let t0 = Instant::now();
    c.latency().await;
    let n = body.len() as u64;
    c.throttle.take(n).await;
    c.counters.bytes_in.fetch_add(n, Ordering::Relaxed);
    let store = c.clone();
    let name = hash.clone();
    let res = tokio::task::spawn_blocking(move || store.store.put_shard(&name, &body)).await.expect("shard store does not panic");
    let status = match &res {
        Ok(true) => StatusCode::CREATED,
        Ok(false) => StatusCode::OK,
        Err(e) => ApiError::status(e),
    };
    c.record(&Method::PUT, "_shards", &hash, (n > 0).then(|| (0, n - 1)), status, n, t0);
    res?;
    Ok(status.into_response())
}

// ---------------------------------------------------------------- drives

async fn drive_put(State(c): State<AppState>, Path(drive): Path<String>) -> ApiResult {
    let t0 = Instant::now();
    c.latency().await;
    let res = c.store.create_drive(&drive).await;
    let status = if res.is_ok() { StatusCode::CREATED } else { StatusCode::CONFLICT };
    c.record(&Method::PUT, &drive, "", None, status, 0, t0);
    res?;
    Ok(StatusCode::CREATED.into_response())
}

async fn drive_delete(State(c): State<AppState>, Path(drive): Path<String>) -> ApiResult {
    let t0 = Instant::now();
    c.latency().await;
    {
        let (c, drive) = (c.clone(), drive.clone());
        to_the_end(async move { c.store.delete_drive(&drive).await }).await?;
    }
    c.record(&Method::DELETE, &drive, "", None, StatusCode::NO_CONTENT, 0, t0);
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// `GET /<drive>`: list objects. `?changes&since=N[&wait=S]` long-polls for
/// the drive's sequence number to pass N instead.
async fn drive_get(State(c): State<AppState>, Path(drive): Path<String>, Query(q): Params) -> ApiResult {
    let t0 = Instant::now();
    if q.contains_key("changes") {
        let since = q.get("since").and_then(|s| s.parse().ok()).unwrap_or(0);
        let wait = q.get("wait").and_then(|s| s.parse().ok()).unwrap_or(20u64).min(60);
        let seq = c.store.wait_for_change(&drive, since, Duration::from_secs(wait)).await?;
        return Ok(Json(json!({ "seq": seq })).into_response());
    }
    c.latency().await;
    let prefix = q.get("prefix").map(String::as_str).unwrap_or("");
    let listing = c.store.list(&drive, prefix, q.get("delimiter").map(String::as_str)).await?;
    c.record(&Method::GET, &drive, "", None, StatusCode::OK, 0, t0);
    Ok(Json(listing).into_response())
}

#[derive(Deserialize)]
struct ShareBody {
    email: String,
    #[serde(default)]
    role: Option<String>,
}

/// `POST /<drive>?fork=<new>` forks the drive without copying content;
/// `POST /<drive>?share` with `{email, role}` records a share;
/// `POST /<drive>?unshare=<email>` removes one;
/// `POST /<drive>?rename=<new>` renames the drive.
async fn drive_post(State(c): State<AppState>, Path(drive): Path<String>, Query(q): Params, body: Bytes) -> ApiResult {
    let t0 = Instant::now();
    c.latency().await;
    let out = if let Some(new) = q.get("fork") {
        c.store.fork_drive(&drive, new).await?;
        json!({ "forked": new })
    } else if let Some(new) = q.get("rename") {
        c.store.rename_drive(&drive, new).await?;
        json!({ "renamed": new })
    } else if q.contains_key("share") {
        let b: ShareBody = serde_json::from_slice(&body).map_err(|e| ApiError(StatusCode::BAD_REQUEST, e.to_string()))?;
        let role = b.role.unwrap_or_else(|| "view".into());
        c.store.share_drive(&drive, &b.email, &role).await?;
        json!({ "shared": b.email, "role": role })
    } else if let Some(email) = q.get("unshare") {
        c.store.unshare_drive(&drive, email).await?;
        json!({ "unshared": email })
    } else {
        return Err(ApiError(StatusCode::BAD_REQUEST, "expected ?fork=, ?rename=, ?share or ?unshare=".into()));
    };
    c.record(&Method::POST, &drive, "", None, StatusCode::OK, 0, t0);
    Ok(Json(out).into_response())
}

// ---------------------------------------------------------------- objects

fn version_headers(c: &Cloud, v: &Version) -> HeaderMap {
    let mut h = HeaderMap::new();
    h.insert("etag", HeaderValue::from_str(&format!("\"{}\"", v.id)).unwrap());
    h.insert("x-ludy-version", HeaderValue::from_str(&v.id).unwrap());
    h.insert("x-ludy-mtime-ms", HeaderValue::from(v.mtime_ms));
    h.insert("accept-ranges", HeaderValue::from_static("bytes"));
    if c.store.synthetic_bytes(v) > 0 {
        h.insert("x-ludy-synthetic", HeaderValue::from_static("1"));
    }
    h
}

/// Parse a single `Range: bytes=` spec into an inclusive `(start, end)`.
fn parse_range(headers: &HeaderMap, size: u64) -> Result<Option<(u64, u64)>, ApiError> {
    let Some(raw) = headers.get("range").and_then(|v| v.to_str().ok()) else {
        return Ok(None);
    };
    let bad = || ApiError(StatusCode::RANGE_NOT_SATISFIABLE, format!("unsatisfiable range {raw:?} for size {size}"));
    let spec = raw.strip_prefix("bytes=").ok_or_else(bad)?;
    let (a, b) = spec.split_once('-').ok_or_else(bad)?;
    let (start, end) = if a.is_empty() {
        let n: u64 = b.parse().map_err(|_| bad())?;
        if n == 0 {
            return Err(bad());
        }
        (size.saturating_sub(n), size.saturating_sub(1))
    } else {
        let start: u64 = a.parse().map_err(|_| bad())?;
        let end = if b.is_empty() { size.saturating_sub(1) } else { b.parse::<u64>().map_err(|_| bad())?.min(size.saturating_sub(1)) };
        (start, end)
    };
    if size == 0 || start > end || start >= size {
        return Err(bad());
    }
    Ok(Some((start, end)))
}

async fn object_head(State(c): State<AppState>, Path((drive, key)): Path<(String, String)>, Query(q): Params) -> ApiResult {
    let t0 = Instant::now();
    c.latency().await;
    let v = c.store.head(&drive, &key, q.get("version").map(String::as_str)).await;
    let status = if v.is_ok() { StatusCode::OK } else { StatusCode::NOT_FOUND };
    c.record(&Method::HEAD, &drive, &key, None, status, 0, t0);
    let v = v?;
    let mut h = version_headers(&c, &v);
    h.insert("content-length", HeaderValue::from(v.size));
    Ok((StatusCode::OK, h).into_response())
}

async fn object_get(
    State(c): State<AppState>,
    Path((drive, key)): Path<(String, String)>,
    Query(q): Params,
    headers: HeaderMap,
) -> ApiResult {
    let t0 = Instant::now();
    if q.contains_key("versions") {
        c.latency().await;
        let versions = c.store.versions(&drive, &key).await?;
        c.record(&Method::GET, &drive, &format!("{key}?versions"), None, StatusCode::OK, 0, t0);
        return Ok(Json(json!({ "key": key, "versions": versions })).into_response());
    }
    let v = match c.store.head(&drive, &key, q.get("version").map(String::as_str)).await {
        Ok(v) => v,
        Err(e) => {
            c.latency().await;
            c.record(&Method::GET, &drive, &key, None, StatusCode::NOT_FOUND, 0, t0);
            return Err(e.into());
        }
    };
    // `?manifest`: the segments behind this version, for patch uploads.
    if q.contains_key("manifest") {
        c.latency().await;
        let found = c.store.manifest_of(&v);
        let status = if found.is_some() { StatusCode::OK } else { StatusCode::NO_CONTENT };
        c.record(&Method::GET, &drive, &format!("{key}?manifest"), None, status, 0, t0);
        // Whole-object content from before shards has none: 204, not an error.
        let Some((id, manifest)) = found else { return Ok((StatusCode::NO_CONTENT, version_headers(&c, &v)).into_response()) };
        let body = json!({ "id": id, "version": v.id, "size": manifest.size, "segments": manifest.segments });
        return Ok((StatusCode::OK, version_headers(&c, &v), Json(body)).into_response());
    }
    // `If-Match` lets a client insist on the version it has cached blocks for.
    if let Some(want) = headers.get("if-match").and_then(|v| v.to_str().ok()) {
        if want.trim_matches('"') != v.id {
            c.record(&Method::GET, &drive, &key, None, StatusCode::PRECONDITION_FAILED, 0, t0);
            return Err(StoreError::Precondition.into());
        }
    }
    let range = parse_range(&headers, v.size)?;
    let (start, end, status) = match range {
        Some((a, b)) => (a, b, StatusCode::PARTIAL_CONTENT),
        None => (0, v.size.saturating_sub(1), StatusCode::OK),
    };
    let len = if v.size == 0 { 0 } else { end - start + 1 };
    c.latency().await;

    let mut h = version_headers(&c, &v);
    h.insert("content-length", HeaderValue::from(len));
    if status == StatusCode::PARTIAL_CONTENT {
        h.insert("content-range", HeaderValue::from_str(&format!("bytes {start}-{end}/{}", v.size)).unwrap());
    }
    c.record(&Method::GET, &drive, &key, (len > 0).then_some((start, end)), status, len, t0);

    let reader = c.store.reader(&v.content).await?;
    let cloud = c.clone();
    let stream = futures_util::stream::unfold((reader, start, len), move |(mut reader, off, left)| {
        let cloud = cloud.clone();
        async move {
            if left == 0 {
                return None;
            }
            let n = left.min(STREAM_CHUNK);
            cloud.throttle.take(n).await;
            match reader.read_at(off, n as usize).await {
                Ok(buf) => {
                    cloud.counters.bytes_out.fetch_add(buf.len() as u64, Ordering::Relaxed);
                    Some((Ok::<Bytes, std::io::Error>(Bytes::from(buf)), (reader, off + n, left - n)))
                }
                Err(e) => Some((Err(e), (reader, off, 0))),
            }
        }
    });
    Ok((status, h, Body::from_stream(stream)).into_response())
}

/// Run a change to the store to its end even if the request that asked for
/// it is cancelled (the client hung up). A change does two things in order,
/// write the catalog and release what it no longer lists, and must not be
/// dropped between them.
async fn to_the_end<T: Send + 'static>(change: impl std::future::Future<Output = Result<T, StoreError>> + Send + 'static) -> Result<T, StoreError> {
    tokio::spawn(change).await.unwrap_or_else(|e| Err(StoreError::Io(std::io::Error::other(format!("the cloud is shutting down: {e}")))))
}

/// One reference to stored content, given up when this is dropped. A
/// request can be cancelled at any await (the client hung up); holding the
/// reference in a value makes sure it is released whichever way the
/// handler ends.
struct Held {
    cloud: Arc<Cloud>,
    content: Content,
}

impl Drop for Held {
    fn drop(&mut self) {
        self.cloud.store.release(&self.content);
    }
}

async fn object_put(
    State(c): State<AppState>,
    Path((drive, key)): Path<(String, String)>,
    Query(q): Params,
    headers: HeaderMap,
    body: Body,
) -> ApiResult {
    let t0 = Instant::now();
    c.latency().await;
    let if_match = headers.get("if-match").and_then(|v| v.to_str().ok()).map(|s| s.trim_matches('"').to_string());
    let mtime_ms = headers.get("x-ludy-mtime-ms").and_then(|v| v.to_str().ok()).and_then(|s| s.parse().ok());

    // A synthetic object is declared by size alone; no body is stored.
    if let Some(size) = headers.get("x-ludy-synthetic-size").and_then(|v| v.to_str().ok()).and_then(parse_size) {
        let seed = headers.get("x-ludy-synthetic-seed").and_then(|v| v.to_str().ok()).and_then(|s| s.parse().ok()).unwrap_or(0);
        let v = {
            let (c, drive, key) = (c.clone(), drive.clone(), key.clone());
            to_the_end(async move { c.store.commit(&drive, &key, size, Content::Synthetic { seed }, if_match.as_deref(), mtime_ms).await }).await?
        };
        c.record(&Method::PUT, &drive, &key, None, StatusCode::CREATED, 0, t0);
        return Ok((StatusCode::CREATED, version_headers(&c, &v)).into_response());
    }

    c.store.check_drive(&drive).await?;

    // `?manifest`: the body is a manifest whose shards were uploaded already
    // (or were in the cloud to begin with). Committing it moves no content.
    let (size, held, new_shards, new_bytes) = if q.contains_key("manifest") {
        let raw = axum::body::to_bytes(body, 64 << 20).await.map_err(|e| ApiError(StatusCode::BAD_REQUEST, e.to_string()))?;
        if c.faults.hold_commits.load(Ordering::Relaxed) {
            c.faults.commits_held.fetch_add(1, Ordering::Relaxed);
            while c.faults.hold_commits.load(Ordering::Relaxed) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            c.faults.commits_held.fetch_sub(1, Ordering::Relaxed);
        }
        let manifest: Manifest = serde_json::from_slice(&raw).map_err(|e| ApiError(StatusCode::BAD_REQUEST, format!("manifest: {e}")))?;
        match c.store.put_manifest(&manifest) {
            Ok(id) => (manifest.size, Held { cloud: c.clone(), content: Content::Manifest { id } }, 0, 0),
            Err(StoreError::MissingShards(missing)) => {
                c.record(&Method::PUT, &drive, &key, None, StatusCode::CONFLICT, 0, t0);
                return Ok((StatusCode::CONFLICT, Json(json!({ "error": "shards missing", "missing": missing }))).into_response());
            }
            Err(e) => {
                c.record(&Method::PUT, &drive, &key, None, ApiError::status(&e), 0, t0);
                return Err(e.into());
            }
        }
    } else {
        // A plain body: receive it, then cut it into shards here.
        let mut upload = c.store.begin_upload().await.map_err(StoreError::Io)?;
        let mut stream = body.into_data_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| ApiError(StatusCode::BAD_REQUEST, e.to_string()))?;
            c.throttle.take(chunk.len() as u64).await;
            c.counters.bytes_in.fetch_add(chunk.len() as u64, Ordering::Relaxed);
            upload.write(&chunk).await.map_err(StoreError::Io)?;
        }
        let tmp = upload.finish().await.map_err(StoreError::Io)?;
        let (cloud, path) = (c.clone(), tmp.clone());
        // The reference is wrapped inside the blocking task, so it is given
        // up even if nobody is waiting for the result any more.
        let ingested = tokio::task::spawn_blocking(move || {
            let done = cloud.store.ingest(&path);
            let _ = std::fs::remove_file(&path);
            done.map(|i| (i.size, i.new_shards, i.new_bytes, Held { cloud: cloud.clone(), content: i.content }))
        })
        .await
        .unwrap_or_else(|e| Err(StoreError::Io(std::io::Error::other(format!("the cloud is shutting down: {e}")))));
        let (size, new_shards, new_bytes, held) = ingested?;
        (size, held, new_shards, new_bytes)
    };

    let committed = {
        let (c, drive, key, content) = (c.clone(), drive.clone(), key.clone(), held.content.clone());
        to_the_end(async move { c.store.commit(&drive, &key, size, content, if_match.as_deref(), mtime_ms).await }).await
    };
    // The version now holds its own reference (or the commit failed).
    drop(held);
    let v = match committed {
        Ok(v) => v,
        Err(e) => {
            c.record(&Method::PUT, &drive, &key, None, ApiError::status(&e), size, t0);
            return Err(e.into());
        }
    };
    // What crossed the wire in this request: the body of a plain PUT, nothing for a manifest.
    let wire = if q.contains_key("manifest") { 0 } else { size };
    c.record(&Method::PUT, &drive, &key, (wire > 0).then(|| (0, wire - 1)), StatusCode::CREATED, wire, t0);
    let mut h = version_headers(&c, &v);
    h.insert("x-ludy-new-shards", HeaderValue::from(new_shards));
    h.insert("x-ludy-new-bytes", HeaderValue::from(new_bytes));
    Ok((StatusCode::CREATED, h).into_response())
}

/// `POST /<drive>/<key>?rename=<new key>` moves an object (or a whole folder
/// prefix) without touching content; `?restore=<version>` makes an old version
/// current again by committing a new version that points at the old content.
async fn object_post(State(c): State<AppState>, Path((drive, key)): Path<(String, String)>, Query(q): Params) -> ApiResult {
    let t0 = Instant::now();
    c.latency().await;
    if let Some(to) = q.get("rename") {
        let moved = {
            let (c, drive, key, to) = (c.clone(), drive.clone(), key.clone(), to.clone());
            to_the_end(async move { c.store.rename(&drive, &key, &to).await }).await?
        };
        c.record(&Method::POST, &drive, &format!("{key} -> {to}"), None, StatusCode::OK, 0, t0);
        return Ok(Json(json!({ "moved": moved })).into_response());
    }
    if let Some(version) = q.get("restore") {
        let v = {
            let (c, drive, key, version) = (c.clone(), drive.clone(), key.clone(), version.clone());
            to_the_end(async move { c.store.restore(&drive, &key, &version).await }).await?
        };
        c.record(&Method::POST, &drive, &format!("{key} restore {version}"), None, StatusCode::OK, 0, t0);
        return Ok((StatusCode::OK, version_headers(&c, &v)).into_response());
    }
    Err(ApiError(StatusCode::BAD_REQUEST, "expected ?rename= or ?restore=".into()))
}

async fn object_delete(State(c): State<AppState>, Path((drive, key)): Path<(String, String)>) -> ApiResult {
    let t0 = Instant::now();
    c.latency().await;
    let res = {
        let (c, drive, key) = (c.clone(), drive.clone(), key.clone());
        to_the_end(async move { c.store.delete(&drive, &key).await }).await
    };
    let status = if res.is_ok() { StatusCode::NO_CONTENT } else { StatusCode::NOT_FOUND };
    c.record(&Method::DELETE, &drive, &key, None, status, 0, t0);
    res?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// `1048576`, `50GB`, `2GiB`, `300MB` ...
pub fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim();
    let split = s.find(|c: char| !c.is_ascii_digit() && c != '.').unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let n: f64 = num.parse().ok()?;
    let mult: u64 = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "k" | "kb" | "kib" => 1 << 10,
        "m" | "mb" | "mib" => 1 << 20,
        "g" | "gb" | "gib" => 1 << 30,
        "t" | "tb" | "tib" => 1 << 40,
        _ => return None,
    };
    Some((n * mult as f64) as u64)
}
