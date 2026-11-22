//! Ludy's cloud on Cloudflare: the provider for Workers.
//!
//! * Sealed shards and manifests live in R2 under `d/<drive>/s/<sha256>` and
//!   `d/<drive>/m/<sha256>`. The Worker stores and serves them and cannot
//!   read them; R2 checks each upload against its name.
//! * Everything else about a drive is one Durable Object per drive, in
//!   [`drive`], running `ludy_server`: the same drive logic as every other
//!   provider. The protocol is `ludy_proto::tree`.
//!
//! Not here yet: sign-in and permissions (anyone who can reach the Worker
//! can do anything), deleting or forking a drive, and pushing changes to
//! clients (they poll the feed). Shard bodies are held in memory whole, at
//! most 16 MiB each.

mod drive;

use ludy_proto::tree::Refusal;
use ludy_server::front::{self, BlobKind, Route};
use ludy_server::log_line;
use worker::*;

pub use drive::Drive;

pub(crate) const BUCKET: &str = "SHARDS";
const DRIVES: &str = "DRIVE";

/// `LUDY_DEV=1`, which only the chaos harness sets, turns on the `_dev`
/// routes: fault switches and consistency checks. Without it they do not exist.
fn dev_mode(env: &Env) -> bool {
    env.var("LUDY_DEV").map(|v| v.to_string() == "1").unwrap_or(false)
}

fn refuse(r: &Refusal) -> Result<Response> {
    respond(ludy_server::Response::refusal(r))
}

pub(crate) fn respond(r: ludy_server::Response) -> Result<Response> {
    let mut resp = Response::from_bytes(r.body)?.with_status(r.status);
    resp.headers_mut().set("content-type", r.content_type)?;
    Ok(resp)
}

#[event(fetch)]
async fn fetch(req: Request, env: Env, _ctx: Context) -> Result<Response> {
    let (method, path) = (req.method().as_ref().to_string(), req.path());
    let started = Date::now().as_millis();
    let result = answer(req, &env, &method, &path).await;
    // A request that fails without an answer is the event worth seeing.
    if let Err(e) = &result {
        let ms = Date::now().as_millis().saturating_sub(started);
        console_log!("{}", log_line("error", "", &[("method", method.into()), ("path", path.into()), ("error", e.to_string().into()), ("ms", ms.into())]));
    }
    result
}

async fn answer(req: Request, env: &Env, method: &str, path: &str) -> Result<Response> {
    match front::route(method, path, dev_mode(env)) {
        Route::Hello => Response::ok("ludy-worker\n"),
        Route::Blob { put: true, kind, drive, hash, key } => blob_put(req, env, kind, &drive, &hash, &key).await,
        Route::Blob { put: false, key, .. } => blob_get(&req, env, &key).await,
        Route::DevBlobs { prefix } => dev_blobs(env, &prefix).await,
        // Lose a blob behind the drive's back, as a failed or over-eager cleanup would.
        Route::DevBlobDelete { key } => {
            env.bucket(BUCKET)?.delete(key).await?;
            Response::ok("deleted\n")
        }
        Route::DevCrypt => Response::ok(front::crypt_selftest()),
        // Everything else about a drive belongs to its Durable Object.
        Route::Drive { drive } => env.durable_object(DRIVES)?.get_by_name(&drive)?.fetch_with_request(req).await,
        Route::Refused(r) => refuse(&r),
    }
}

/// Store a sealed shard or manifest under the SHA-256 it claims. The Worker
/// does not hash the body: R2 is handed the 32 raw bytes and checks them.
async fn blob_put(mut req: Request, env: &Env, kind: BlobKind, drive: &str, hash: &str, key: &str) -> Result<Response> {
    let body = req.bytes().await?;
    if body.len() as u64 > kind.max_bytes() {
        return refuse(&front::too_large());
    }
    let n = body.len() as u64;
    match env.bucket(BUCKET)?.put(key, Data::Bytes(body)).sha256(front::unhex(hash)).execute().await {
        Ok(Some(_)) => {
            console_log!("{}", log_line("blob", drive, &[("kind", kind.letter().into()), ("bytes", n.into())]));
            Ok(Response::ok("stored\n")?.with_status(201))
        }
        // R2 refuses a body that does not hash to the name; nothing is stored.
        // That is the sender's mistake. Told apart by R2's message, since the
        // binding gives no error code.
        Err(e) if e.to_string().to_lowercase().contains("checksum") => refuse(&Refusal::new(ludy_proto::tree::E_MALFORMED, format!("storage refused it: {e}"))),
        // Anything else is storage failing, and the sender should try again: a 5xx.
        Ok(None) => Err(Error::RustError("storage answered nothing".into())),
        Err(e) => Err(e),
    }
}

async fn blob_get(req: &Request, env: &Env, key: &str) -> Result<Response> {
    let range = req.headers().get("range")?.as_deref().and_then(front::parse_range);
    let bucket = env.bucket(BUCKET)?;
    let mut get = bucket.get(key);
    if let Some((a, b)) = range {
        get = get.range(Range::OffsetWithLength { offset: a, length: b - a + 1 });
    }
    let Some(object) = get.execute().await? else {
        return refuse(&front::no_blob());
    };
    let size = object.size();
    let Some(body) = object.body() else {
        return refuse(&Refusal::new("storage", "the object has no body"));
    };
    // Hand the stream to the runtime; the Worker does not copy the bytes.
    let mut resp = Response::from_body(body.response_body()?)?;
    if let Some((a, b)) = range {
        let b = b.min(size.saturating_sub(1));
        resp = resp.with_status(206);
        resp.headers_mut().set("content-range", &format!("bytes {a}-{b}/{size}"))?;
    }
    resp.headers_mut().set("content-type", "application/octet-stream")?;
    Ok(resp)
}

/// What storage holds for a drive, as `s/<sha256>` and `m/<sha256>`. For the chaos harness.
async fn dev_blobs(env: &Env, prefix: &str) -> Result<Response> {
    let bucket = env.bucket(BUCKET)?;
    let (mut names, mut cursor) = (Vec::new(), None::<String>);
    loop {
        let mut list = bucket.list().prefix(prefix.to_string()).limit(1000);
        if let Some(c) = cursor.take() {
            list = list.cursor(c);
        }
        let page = list.execute().await?;
        names.extend(page.objects().iter().map(|o| o.key()[prefix.len()..].to_string()));
        match page.cursor() {
            Some(c) if page.truncated() => cursor = Some(c),
            _ => break,
        }
    }
    Response::from_json(&names)
}
