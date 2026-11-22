//! Ludy's cloud as a Cloudflare Worker.
//!
//! * Sealed shards and manifests live in R2 under `d/<drive>/s/<sha256>` and
//!   `d/<drive>/m/<sha256>`. The Worker stores and serves them and cannot
//!   read them; R2 checks each upload against its name.
//! * Everything else about a drive (the tree of nodes, versions, which
//!   shards are in use, the order of commits) is one Durable Object per
//!   drive, in [`drive`]. The protocol is `ludy_proto::tree`.
//!
//! Not here yet: sign-in and permissions (anyone who can reach the Worker
//! can do anything), deleting or forking a drive, and pushing changes to
//! clients (they poll the feed). Shard bodies are held in memory whole, at
//! most 16 MiB each.

mod drive;

use ludy_proto::crypt;
use ludy_proto::shard::valid_hash;
use ludy_proto::tree::{valid_id, Refusal, E_MALFORMED, E_TOO_LARGE, MAX_MANIFEST};
use worker::*;

pub use drive::Drive;

pub(crate) const BUCKET: &str = "SHARDS";
const DRIVES: &str = "DRIVE";

/// `LUDY_DEV=1`, which only the chaos harness sets, turns on the `_dev`
/// routes: fault switches and consistency checks. Without it they do not exist.
pub(crate) fn dev_mode(env: &Env) -> bool {
    env.var("LUDY_DEV").map(|v| v.to_string() == "1").unwrap_or(false)
}

/// Where a drive's blob is in the bucket. `blob` is `s/<sha256>` or `m/<sha256>`.
pub(crate) fn blob_key(drive: &str, blob: &str) -> String {
    format!("d/{drive}/{blob}")
}

/// One line of JSON per event, for whoever reads the Worker's log.
pub(crate) fn log_line(event: &str, drive: &str, fields: &[(&str, serde_json::Value)]) {
    let mut line = serde_json::Map::new();
    line.insert("t".into(), event.into());
    line.insert("drive".into(), drive.into());
    for (k, v) in fields {
        line.insert((*k).into(), v.clone());
    }
    console_log!("{}", serde_json::Value::Object(line));
}

pub(crate) fn refuse(error: &str, detail: &str) -> Result<Response> {
    let r = Refusal::new(error, detail);
    Ok(Response::from_json(&r)?.with_status(r.status()))
}

#[event(fetch)]
async fn fetch(req: Request, env: Env, _ctx: Context) -> Result<Response> {
    let (method, path) = (req.method(), req.path());
    let started = Date::now().as_millis();
    let result = Router::new()
        .get("/", |_, _| Response::ok("ludy-worker\n"))
        .put_async("/drives/:drive/shards/:hash", |req, ctx| blob_put(req, ctx, "s"))
        .get_async("/drives/:drive/shards/:hash", |req, ctx| blob_get(req, ctx, "s"))
        .put_async("/drives/:drive/manifests/:hash", |req, ctx| blob_put(req, ctx, "m"))
        .get_async("/drives/:drive/manifests/:hash", |req, ctx| blob_get(req, ctx, "m"))
        .get_async("/_dev/blobs/:drive", dev_blobs)
        .delete_async("/_dev/blobs/:drive/:kind/:hash", dev_blob_delete)
        .get("/_dev/crypt", |_, ctx| if dev_mode(&ctx.env) { Response::ok(crypt_selftest()) } else { refuse(E_MALFORMED, "no such route") })
        // Everything else about a drive belongs to its Durable Object.
        .on_async("/drives/:drive", to_drive)
        .on_async("/drives/:drive/*rest", to_drive)
        .run(req, env)
        .await;
    // A request that fails without an answer is the event worth seeing.
    if let Err(e) = &result {
        let ms = Date::now().as_millis().saturating_sub(started);
        log_line("error", "", &[("method", method.to_string().into()), ("path", path.into()), ("error", e.to_string().into()), ("ms", ms.into())]);
    }
    result
}

async fn to_drive(req: Request, ctx: RouteContext<()>) -> Result<Response> {
    let drive = ctx.param("drive").cloned().unwrap_or_default();
    if !valid_id(&drive) {
        return refuse(E_MALFORMED, "drive id is not 16 bytes of hex");
    }
    ctx.env.durable_object(DRIVES)?.get_by_name(&drive)?.fetch_with_request(req).await
}

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len() / 2).filter_map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).ok()).collect()
}

fn blob_path(ctx: &RouteContext<()>, kind: &str) -> Option<(String, String, String)> {
    let (drive, hash) = (ctx.param("drive")?.clone(), ctx.param("hash")?.clone());
    (valid_id(&drive) && valid_hash(&hash)).then(|| (blob_key(&drive, &format!("{kind}/{hash}")), drive, hash))
}

/// Store a sealed shard or manifest under the SHA-256 it claims. The Worker
/// does not hash the body: R2 is handed the 32 raw bytes and checks them.
async fn blob_put(mut req: Request, ctx: RouteContext<()>, kind: &str) -> Result<Response> {
    let Some((key, drive, hash)) = blob_path(&ctx, kind) else {
        return refuse(E_MALFORMED, "not a drive id and a sha-256 name");
    };
    let limit = if kind == "s" { crypt::MAX_SEALED_SHARD } else { MAX_MANIFEST };
    let body = req.bytes().await?;
    if body.len() as u64 > limit {
        return refuse(E_TOO_LARGE, "larger than a sealed shard or manifest can be");
    }
    let n = body.len() as u64;
    let bucket = ctx.env.bucket(BUCKET)?;
    match bucket.put(&key, Data::Bytes(body)).sha256(unhex(&hash)).execute().await {
        Ok(Some(_)) => {
            log_line("blob", &drive, &[("kind", kind.into()), ("bytes", n.into())]);
            Ok(Response::ok("stored\n")?.with_status(201))
        }
        // R2 refuses a body that does not hash to the name; nothing is stored.
        // That is the sender's mistake. Told apart by R2's message, since the
        // binding gives no error code.
        Err(e) if e.to_string().to_lowercase().contains("checksum") => refuse(E_MALFORMED, &format!("storage refused it: {e}")),
        // Anything else is storage failing, and the sender should try again: a 5xx.
        Ok(None) => Err(Error::RustError("storage answered nothing".into())),
        Err(e) => Err(e),
    }
}

/// `bytes=a-b`, both ends given.
fn parse_range(header: &str) -> Option<(u64, u64)> {
    let (a, b) = header.strip_prefix("bytes=")?.split_once('-')?;
    let (a, b) = (a.parse().ok()?, b.parse().ok()?);
    (a <= b).then_some((a, b))
}

async fn blob_get(req: Request, ctx: RouteContext<()>, kind: &str) -> Result<Response> {
    let Some((key, _, _)) = blob_path(&ctx, kind) else {
        return refuse(E_MALFORMED, "not a drive id and a sha-256 name");
    };
    let range = req.headers().get("range")?.as_deref().and_then(parse_range);
    let bucket = ctx.env.bucket(BUCKET)?;
    let mut get = bucket.get(&key);
    if let Some((a, b)) = range {
        get = get.range(Range::OffsetWithLength { offset: a, length: b - a + 1 });
    }
    let Some(object) = get.execute().await? else {
        return refuse("no_blob", "not in storage");
    };
    let size = object.size();
    let Some(body) = object.body() else {
        return refuse("storage", "the object has no body");
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

// ------------------------------------------------------ for the chaos harness

/// What storage holds for a drive, as `s/<sha256>` and `m/<sha256>`.
async fn dev_blobs(_req: Request, ctx: RouteContext<()>) -> Result<Response> {
    if !dev_mode(&ctx.env) {
        return refuse(E_MALFORMED, "no such route");
    }
    let drive = ctx.param("drive").cloned().unwrap_or_default();
    let prefix = blob_key(&drive, "");
    let bucket = ctx.env.bucket(BUCKET)?;
    let (mut names, mut cursor) = (Vec::new(), None::<String>);
    loop {
        let mut list = bucket.list().prefix(prefix.clone()).limit(1000);
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

/// Lose a blob behind the drive's back, as a failed or over-eager cleanup would.
async fn dev_blob_delete(_req: Request, ctx: RouteContext<()>) -> Result<Response> {
    if !dev_mode(&ctx.env) {
        return refuse(E_MALFORMED, "no such route");
    }
    let (Some(drive), Some(kind), Some(hash)) = (ctx.param("drive"), ctx.param("kind"), ctx.param("hash")) else {
        return refuse(E_MALFORMED, "drive, kind and hash");
    };
    ctx.env.bucket(BUCKET)?.delete(blob_key(drive, &format!("{kind}/{hash}"))).await?;
    Response::ok("deleted\n")
}

/// Run the encryption format inside the Worker's WebAssembly, on the
/// 200,000-byte input of `ludy-proto/tests/vectors/crypt-v1.json`, and answer
/// the sealed shard's name. The server never holds a real key; this shows
/// the code a browser client will need runs on this target.
fn crypt_selftest() -> String {
    // The dedup key of that vector file. A test value, not a secret.
    const DEDUP: &str = "90b1a9a9def6e53d3ac9c640d9a42a30f6ff26bb639e65c21f895b218b51196b";
    let Ok(key) = <[u8; 32]>::try_from(&unhex(DEDUP)[..]) else { return "bad key".into() };
    let plain: Vec<u8> = (0..200_000usize).map(|i| (i * 131 + (i >> 9) + 4) as u8).collect();
    let sealed = crypt::seal_shard(&plain, &crypt::DedupKey::from_bytes(key));
    match crypt::open_shard(&sealed.key, plain.len() as u64, &sealed.bytes) {
        Ok(opened) if opened == plain => sealed.id_hex(),
        _ => "sealed shard did not open".into(),
    }
}
