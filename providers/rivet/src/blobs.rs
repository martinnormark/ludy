//! Where sealed shards and manifests are kept: any S3 (Cloudflare R2, MinIO,
//! AWS, ...) or a folder.
//!
//! `LUDY_BLOBS` chooses: `s3://<bucket>` reads the usual `AWS_*` variables
//! (`AWS_ENDPOINT`, `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`,
//! `AWS_REGION`); anything else is a path to a folder.
//!
//! A store on a network can go away, or stay connected and say nothing.
//! Every call here has a limit, so that whoever asked gets a "the store did
//! not answer" in bounded time and can try again, and so that a drive
//! waiting on a lookup is never held for longer than [`QUICK`]:
//!
//! * a lookup or a delete: [`QUICK`];
//! * a read: [`QUICK`] until the store starts answering, and again
//!   between any two pieces of the body;
//! * an upload: [`QUICK`] plus a second for every [`SLOWEST_UPLOAD`] bytes.
//!
//! The S3 client retries inside those limits, a few times and briefly; the
//! senders of requests do the long retrying.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use bytes::Bytes;
use futures_util::stream::BoxStream;
use futures_util::{StreamExt, TryStreamExt};
use object_store::aws::AmazonS3Builder;
use object_store::local::LocalFileSystem;
use object_store::path::Path;
use object_store::{BackoffConfig, ClientConfigKey, GetOptions, GetRange, ObjectStore, ObjectStoreExt, PutPayload, RetryConfig};

/// How long a small call to the store may take, retries included.
pub const QUICK: Duration = Duration::from_secs(8);
/// The slowest upload that is still let finish, in bytes a second.
pub const SLOWEST_UPLOAD: u64 = 250_000;
/// Longer than any limit here, so that the limits here are the ones that fire.
const CLIENT_TIMEOUT: &str = "180s";

/// The store did not answer within the time a call is given.
fn late(what: &str, limit: Duration) -> anyhow::Error {
    anyhow::anyhow!("the store did not answer a {what} within {limit:?}")
}

async fn within<T, E: Into<anyhow::Error>>(what: &str, limit: Duration, call: impl std::future::Future<Output = Result<T, E>>) -> anyhow::Result<T> {
    match tokio::time::timeout(limit, call).await {
        Ok(result) => result.map_err(Into::into),
        Err(_) => Err(late(what, limit)),
    }
}

pub struct Blobs {
    store: Arc<dyn ObjectStore>,
    what: String,
}

/// A blob, or the part of it that was asked for.
pub struct Found {
    /// Size of the whole object.
    pub size: u64,
    pub bytes: BoxStream<'static, std::io::Result<Bytes>>,
}

/// A body that ends in an error if the store stops sending in the middle
/// of it, instead of keeping its reader waiting.
fn impatient(inner: BoxStream<'static, object_store::Result<Bytes>>) -> BoxStream<'static, std::io::Result<Bytes>> {
    futures_util::stream::unfold(Some(inner), |state| async move {
        let mut inner = state?;
        match tokio::time::timeout(QUICK, inner.next()).await {
            Ok(Some(Ok(piece))) => Some((Ok(piece), Some(inner))),
            Ok(Some(Err(e))) => Some((Err(std::io::Error::other(e)), None)),
            Ok(None) => None,
            Err(_) => Some((Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "the store stopped sending in the middle of an object")), None)),
        }
    })
    .boxed()
}

impl Blobs {
    pub fn from_env() -> anyhow::Result<Blobs> {
        let target = std::env::var("LUDY_BLOBS").ok().filter(|v| !v.trim().is_empty()).unwrap_or_else(|| "./ludy-blobs".into());
        if let Some(bucket) = target.strip_prefix("s3://") {
            let bucket = bucket.trim_end_matches('/');
            let mut s3 = AmazonS3Builder::from_env().with_bucket_name(bucket);
            if let Ok(endpoint) = std::env::var("AWS_ENDPOINT") {
                // A named endpoint is R2, MinIO or the like: the bucket goes
                // in the path, and plain HTTP is allowed if the address says so.
                s3 = s3.with_virtual_hosted_style_request(false).with_allow_http(endpoint.starts_with("http://"));
            }
            if std::env::var("AWS_REGION").is_err() && std::env::var("AWS_DEFAULT_REGION").is_err() {
                // What R2 calls its one region; MinIO takes anything.
                s3 = s3.with_region("auto");
            }
            // The limits in this file decide when to give up, not the
            // client's own: a few quick retries, and a request timeout
            // that only fires if those limits somehow do not.
            s3 = s3.with_retry(RetryConfig { backoff: BackoffConfig { init_backoff: Duration::from_millis(100), max_backoff: Duration::from_secs(2), base: 2.0 }, max_retries: 5, retry_timeout: QUICK });
            if std::env::var("AWS_TIMEOUT").is_err() {
                s3 = s3.with_config(object_store::aws::AmazonS3ConfigKey::Client(ClientConfigKey::Timeout), CLIENT_TIMEOUT);
            }
            let store = s3.build().with_context(|| format!("LUDY_BLOBS={target}: setting up the S3 client"))?;
            return Ok(Blobs { store: Arc::new(store), what: format!("s3://{bucket}") });
        }
        std::fs::create_dir_all(&target).with_context(|| format!("LUDY_BLOBS={target}: creating the folder"))?;
        let store = LocalFileSystem::new_with_prefix(&target).with_context(|| format!("LUDY_BLOBS={target}: opening the folder"))?;
        Ok(Blobs { store: Arc::new(store), what: format!("folder {target}") })
    }

    pub fn describe(&self) -> String {
        self.what.clone()
    }

    pub async fn put(&self, key: &str, body: Bytes) -> anyhow::Result<()> {
        let limit = QUICK + Duration::from_secs(body.len() as u64 / SLOWEST_UPLOAD);
        within("put", limit, self.store.put(&Path::from(key), PutPayload::from_bytes(body))).await?;
        Ok(())
    }

    pub async fn exists(&self, key: &str) -> anyhow::Result<bool> {
        match tokio::time::timeout(QUICK, self.store.head(&Path::from(key))).await {
            Ok(Ok(_)) => Ok(true),
            Ok(Err(object_store::Error::NotFound { .. })) => Ok(false),
            Ok(Err(e)) => Err(e.into()),
            Err(_) => Err(late("lookup", QUICK)),
        }
    }

    /// Deleting what is not there is not an error.
    pub async fn delete(&self, key: &str) -> anyhow::Result<()> {
        match tokio::time::timeout(QUICK, self.store.delete(&Path::from(key))).await {
            Ok(Ok(())) | Ok(Err(object_store::Error::NotFound { .. })) => Ok(()),
            Ok(Err(e)) => Err(e.into()),
            Err(_) => Err(late("delete", QUICK)),
        }
    }

    /// The object, or bytes `a..=b` of it. `None`: not in the store.
    pub async fn get(&self, key: &str, range: Option<(u64, u64)>) -> anyhow::Result<Option<Found>> {
        let options = GetOptions { range: range.map(|(a, b)| GetRange::Bounded(a..b.saturating_add(1))), ..GetOptions::default() };
        match tokio::time::timeout(QUICK, self.store.get_opts(&Path::from(key), options)).await {
            Ok(Ok(got)) => Ok(Some(Found { size: got.meta.size, bytes: impatient(got.into_stream()) })),
            Ok(Err(object_store::Error::NotFound { .. })) => Ok(None),
            Ok(Err(e)) => Err(e.into()),
            Err(_) => Err(late("read", QUICK)),
        }
    }

    /// Every key under `prefix`, with the prefix taken off.
    pub async fn list(&self, prefix: &str) -> anyhow::Result<Vec<String>> {
        let under = Path::from(prefix.trim_end_matches('/'));
        let mut names: Vec<String> = self
            .store
            .list(Some(&under))
            .map_ok(|object| object.location.to_string())
            .try_collect()
            .await?;
        for name in &mut names {
            *name = name.strip_prefix(prefix).unwrap_or(name).to_string();
        }
        names.sort();
        Ok(names)
    }
}

/// The whole of a stream of chunks, for the few callers that need it.
#[allow(dead_code)]
pub async fn all(mut bytes: BoxStream<'static, std::io::Result<Bytes>>) -> anyhow::Result<Vec<u8>> {
    let mut out = Vec::new();
    while let Some(chunk) = bytes.next().await {
        out.extend_from_slice(&chunk?);
    }
    Ok(out)
}
