//! Where sealed shards and manifests are kept: any S3 (Cloudflare R2, MinIO,
//! AWS, ...) or a folder.
//!
//! `LUDY_BLOBS` chooses: `s3://<bucket>` reads the usual `AWS_*` variables
//! (`AWS_ENDPOINT`, `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`,
//! `AWS_REGION`); anything else is a path to a folder.

use std::sync::Arc;

use anyhow::Context;
use bytes::Bytes;
use futures_util::stream::BoxStream;
use futures_util::{StreamExt, TryStreamExt};
use object_store::aws::AmazonS3Builder;
use object_store::local::LocalFileSystem;
use object_store::path::Path;
use object_store::{GetOptions, GetRange, ObjectStore, ObjectStoreExt, PutPayload};

pub struct Blobs {
    store: Arc<dyn ObjectStore>,
    what: String,
}

/// A blob, or the part of it that was asked for.
pub struct Found {
    /// Size of the whole object.
    pub size: u64,
    pub bytes: BoxStream<'static, object_store::Result<Bytes>>,
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
        self.store.put(&Path::from(key), PutPayload::from_bytes(body)).await?;
        Ok(())
    }

    pub async fn exists(&self, key: &str) -> anyhow::Result<bool> {
        match self.store.head(&Path::from(key)).await {
            Ok(_) => Ok(true),
            Err(object_store::Error::NotFound { .. }) => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    /// Deleting what is not there is not an error.
    pub async fn delete(&self, key: &str) -> anyhow::Result<()> {
        match self.store.delete(&Path::from(key)).await {
            Ok(()) | Err(object_store::Error::NotFound { .. }) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    /// The object, or bytes `a..=b` of it. `None`: not in the store.
    pub async fn get(&self, key: &str, range: Option<(u64, u64)>) -> anyhow::Result<Option<Found>> {
        let options = GetOptions { range: range.map(|(a, b)| GetRange::Bounded(a..b.saturating_add(1))), ..GetOptions::default() };
        match self.store.get_opts(&Path::from(key), options).await {
            Ok(got) => Ok(Some(Found { size: got.meta.size, bytes: got.into_stream() })),
            Err(object_store::Error::NotFound { .. }) => Ok(None),
            Err(e) => Err(e.into()),
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
pub async fn all(mut bytes: BoxStream<'static, object_store::Result<Bytes>>) -> anyhow::Result<Vec<u8>> {
    let mut out = Vec::new();
    while let Some(chunk) = bytes.next().await {
        out.extend_from_slice(&chunk?);
    }
    Ok(out)
}
