//! The HTTP side: the Ludy protocol, as every client speaks it.
//!
//! Shards and manifests go straight between the client and the blob store.
//! Nothing checks an upload for us here the way R2 does, so the front
//! hashes each body and refuses one that does not match its name.
//! Everything else about a drive is forwarded to that drive's actor through
//! the Rivet engine's gateway, which starts the actor if it is asleep.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::State;
use axum::http::{header, HeaderMap, Method, StatusCode, Uri};
use axum::response::Response;
use bytes::Bytes;
use ludy_proto::shard::sha256_hex;
use ludy_proto::tree::{Refusal, E_MALFORMED};
use ludy_server::front::{self, BlobKind, Route};
use ludy_server::log_line;

use crate::{actor, log, Env};

/// The largest body of a request to a drive. A commit of 100,000 shard
/// names is about 6.7 MB.
const MAX_DRIVE_BODY: usize = 32 << 20;
/// How long the gateway may take. It holds a request while it starts an
/// actor, or waits for a node to come back.
const GATEWAY_TIMEOUT: Duration = Duration::from_secs(30);

pub async fn serve(listener: tokio::net::TcpListener, env: Arc<Env>) -> anyhow::Result<()> {
    let app = axum::Router::new().fallback(handle).with_state(env);
    axum::serve(listener, app).await?;
    Ok(())
}

fn text(status: u16, body: impl Into<String>) -> Response {
    let mut r = Response::new(Body::from(body.into()));
    *r.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    r.headers_mut().insert(header::CONTENT_TYPE, header::HeaderValue::from_static("text/plain; charset=utf-8"));
    r
}

fn refuse(r: &Refusal) -> Response {
    let mut resp = Response::new(Body::from(serde_json::to_vec(r).unwrap_or_default()));
    *resp.status_mut() = StatusCode::from_u16(r.status()).unwrap_or(StatusCode::BAD_REQUEST);
    resp.headers_mut().insert(header::CONTENT_TYPE, header::HeaderValue::from_static("application/json"));
    resp
}

/// Storage or the engine failed: a 5xx, and the sender tries again. `event`
/// is `error` for what should not happen and `upstream` for the engine or a
/// node being away, which a restart of either causes. A store that did not
/// answer has its own, [`store_failed`].
fn failed(event: &str, status: u16, method: &Method, path: &str, error: String, started: Instant) -> Response {
    log(&log_line(event, "", &[("method", method.as_str().into()), ("path", path.into()), ("error", error.clone().into()), ("ms", (started.elapsed().as_millis() as u64).into())]));
    text(status, error)
}

/// The blob store failed or did not answer in time: a 503, and the sender
/// tries again. The log gets the store's own words, which name the bucket
/// and its address; the sender gets none of that.
fn store_failed(method: &Method, path: &str, error: &anyhow::Error, started: Instant) -> Response {
    crate::store_trouble(method.as_str(), path, &format!("{error:#}"), started.elapsed().as_millis() as u64);
    text(503, crate::STORE_AWAY.to_string())
}

async fn handle(State(env): State<Arc<Env>>, method: Method, uri: Uri, headers: HeaderMap, body: Body) -> Response {
    let started = Instant::now();
    let path = uri.path().to_string();
    let response = match front::route(method.as_str(), &path, env.config.dev) {
        Route::Hello if env.ready.load(Ordering::SeqCst) => text(200, "ludy-rivet\n"),
        Route::Hello => text(503, "waiting for the Rivet engine\n"),
        Route::Blob { put: true, kind, drive, hash, key } => blob_put(&env, &method, &path, body, kind, &drive, &hash, &key, started).await,
        Route::Blob { put: false, key, .. } => blob_get(&env, &method, &path, &headers, &key, started).await,
        Route::DevBlobs { prefix } => match env.blobs.list(&prefix).await {
            Ok(names) => json(&names),
            Err(e) => store_failed(&method, &path, &e, started),
        },
        // Lose a blob behind the drive's back, as a failed or over-eager cleanup would.
        Route::DevBlobDelete { key } => match env.blobs.delete(&key).await {
            Ok(()) => text(200, "deleted\n"),
            Err(e) => store_failed(&method, &path, &e, started),
        },
        Route::DevCrypt => text(200, front::crypt_selftest()),
        Route::Drive { drive } => to_drive(&env, &method, &path, uri.query().unwrap_or_default(), body, &drive, started).await,
        Route::Refused(r) => refuse(&r),
    };
    // One line per request, the way `wrangler dev` writes them. Not for the
    // health check's question when it got its usual answer: that is asked
    // every few seconds for as long as the server runs.
    let status = response.status().as_u16();
    if !(status == 404 && path.strip_prefix("/drives/") == Some(crate::PROBE_DRIVE)) {
        log(&format!("{method} {path} {status} ({}ms)", started.elapsed().as_millis()));
    }
    response
}

fn json<T: serde::Serialize>(value: &T) -> Response {
    let mut r = Response::new(Body::from(serde_json::to_vec(value).unwrap_or_default()));
    r.headers_mut().insert(header::CONTENT_TYPE, header::HeaderValue::from_static("application/json"));
    r
}

/// Read a body of at most `limit` bytes. `None`: it is larger, or the sender went away.
async fn read(body: Body, limit: usize) -> Option<Bytes> {
    axum::body::to_bytes(body, limit).await.ok()
}

/// Store a sealed shard or manifest under the SHA-256 it claims.
#[allow(clippy::too_many_arguments)]
async fn blob_put(env: &Env, method: &Method, path: &str, body: Body, kind: BlobKind, drive: &str, hash: &str, key: &str, started: Instant) -> Response {
    let Some(body) = read(body, kind.max_bytes() as usize).await else {
        return refuse(&front::too_large());
    };
    // Hash it here: a store that is a folder checks nothing, and each S3
    // words its own refusal differently.
    if sha256_hex(&body) != hash {
        return refuse(&Refusal::new(E_MALFORMED, "the body does not hash to the sha-256 in its name"));
    }
    let n = body.len() as u64;
    match env.blobs.put(key, body).await {
        Ok(()) => {
            log(&log_line("blob", drive, &[("kind", kind.letter().into()), ("bytes", n.into())]));
            text(201, "stored\n")
        }
        Err(e) => store_failed(method, path, &e, started),
    }
}

async fn blob_get(env: &Env, method: &Method, path: &str, headers: &HeaderMap, key: &str, started: Instant) -> Response {
    let range = headers.get(header::RANGE).and_then(|v| v.to_str().ok()).and_then(front::parse_range);
    let found = match env.blobs.get(key, range).await {
        Ok(Some(found)) => found,
        Ok(None) => return refuse(&front::no_blob()),
        Err(e) => return store_failed(method, path, &e, started),
    };
    // The store's stream goes to the client as it comes; nothing is copied here.
    let mut resp = Response::new(Body::from_stream(found.bytes));
    if let Some((a, b)) = range {
        let b = b.min(found.size.saturating_sub(1));
        *resp.status_mut() = StatusCode::PARTIAL_CONTENT;
        if let Ok(value) = header::HeaderValue::from_str(&format!("bytes {a}-{b}/{}", found.size)) {
            resp.headers_mut().insert(header::CONTENT_RANGE, value);
        }
    }
    resp.headers_mut().insert(header::CONTENT_TYPE, header::HeaderValue::from_static("application/octet-stream"));
    resp
}

/// Hand a request to the drive's actor, through the engine's gateway. The
/// actor is found by the drive's id and created the first time.
async fn to_drive(env: &Env, method: &Method, path: &str, query: &str, body: Body, drive: &str, started: Instant) -> Response {
    // The gateway reads `rvt-` parameters as its own: which actor, how to
    // create it, what to do when it crashes. Those are ours to set.
    if query.split('&').any(|pair| pair.starts_with("rvt-") || pair.starts_with("rvt%2D") || pair.starts_with("rvt%2d")) {
        return refuse(&Refusal::new(E_MALFORMED, "no such parameter"));
    }
    let Some(body) = read(body, MAX_DRIVE_BODY).await else {
        return refuse(&front::too_large());
    };
    let engine = &env.engine;
    let and = if query.is_empty() { "" } else { "&" };
    let url = format!(
        "{}/gateway/{}/request{path}?{query}{and}rvt-namespace={}&rvt-method=getOrCreate&rvt-key={drive}&rvt-pool={}",
        engine.endpoint,
        actor::NAME,
        engine.namespace,
        engine.pool
    );
    let mut request = engine.http.request(method.clone(), url).timeout(GATEWAY_TIMEOUT).body(body);
    if let Some(token) = &engine.token {
        request = request.header("x-rivet-token", token);
    }
    let answer = match request.send().await {
        Ok(answer) => answer,
        Err(e) => return failed("upstream", 502, method, path, format!("the Rivet engine did not answer: {e}"), started),
    };
    let status = answer.status().as_u16();
    let ours = answer.headers().contains_key(actor::OURS);
    let content_type = answer.headers().get(header::CONTENT_TYPE).cloned();
    let body = match answer.bytes().await {
        Ok(body) => body,
        Err(e) => return failed("upstream", 502, method, path, format!("the Rivet engine's answer broke off: {e}"), started),
    };
    if !ours {
        // The gateway answered for itself: the actor crashed, or no node has it.
        let said: String = String::from_utf8_lossy(&body).chars().take(300).collect();
        return failed("upstream", 502, method, path, format!("the drive did not answer; the gateway said {status}: {said}"), started);
    }
    let mut resp = Response::new(Body::from(body));
    *resp.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY);
    if let Some(content_type) = content_type {
        resp.headers_mut().insert(header::CONTENT_TYPE, content_type);
    }
    resp
}
