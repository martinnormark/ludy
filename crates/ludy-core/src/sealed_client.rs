//! HTTP client for the Worker (`app/providers/cloudflare`): the node-tree protocol of an
//! end-to-end encrypted drive. It moves sealed bytes and JSON; nothing here
//! holds a key. [`crate::sealed_drive`] does the sealing and opening.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use bytes::Bytes;
use ludy_proto::tree::{Blobs, Commit, Committed, CreateDrive, DriveHead, Feed, NodeInfo, Refusal};
use serde::de::DeserializeOwned;

use crate::client::{stalled, transfer_timeout, HttpError, RangeBody, SMALL_REQUEST, STALL};
use crate::stats::Stats;

#[derive(Clone)]
pub struct SealedClient {
    http: reqwest::Client,
    base: Arc<str>,
    stats: Arc<Stats>,
}

/// The cloud answered: with what was asked for, or with a refusal it wrote.
/// Anything else (no answer, a timeout, a 5xx) is an `Err`, and may be tried again.
#[derive(Debug)]
pub enum Reply<T> {
    Ok(T),
    Refused(Refusal),
}

impl<T> Reply<T> {
    /// For calls where a refusal is as unexpected as no answer.
    pub fn accepted(self, what: &str) -> Result<T> {
        match self {
            Reply::Ok(value) => Ok(value),
            Reply::Refused(r) => anyhow::bail!("{what}: the cloud refused: {} ({})", r.error, r.detail),
        }
    }
}

impl SealedClient {
    pub fn new(base: &str, stats: Arc<Stats>) -> SealedClient {
        SealedClient::with_pool(base, stats, true)
    }

    /// `pooled: false` opens a connection per request, for tests whose proxy
    /// decides the fate of each connection.
    pub fn with_pool(base: &str, stats: Arc<Stats>, pooled: bool) -> SealedClient {
        let mut builder = reqwest::Client::builder().connect_timeout(Duration::from_secs(5));
        if !pooled {
            builder = builder.pool_max_idle_per_host(0);
        }
        SealedClient { http: builder.build().expect("an HTTP client"), base: base.trim_end_matches('/').into(), stats }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    async fn reply<T: DeserializeOwned>(what: &str, resp: reqwest::Response) -> Result<Reply<T>> {
        let status = resp.status();
        let body = resp.bytes().await.with_context(|| what.to_string())?;
        if status.is_success() {
            return Ok(Reply::Ok(serde_json::from_slice(&body).with_context(|| format!("{what}: an answer that does not parse"))?));
        }
        if status.is_client_error() {
            if let Ok(refusal) = serde_json::from_slice::<Refusal>(&body) {
                return Ok(Reply::Refused(refusal));
            }
        }
        Err(HttpError { status: status.as_u16(), what: what.to_string(), body: String::from_utf8_lossy(&body).chars().take(200).collect() }.into())
    }

    async fn json<T: DeserializeOwned>(&self, what: &str, req: reqwest::RequestBuilder) -> Result<Reply<T>> {
        let resp = req.timeout(SMALL_REQUEST).send().await.with_context(|| what.to_string())?;
        Self::reply(what, resp).await
    }

    pub async fn create_drive(&self, drive: &str, body: &CreateDrive) -> Result<Reply<DriveHead>> {
        self.json("create drive", self.http.put(self.url(&format!("/drives/{drive}"))).json(body)).await
    }

    pub async fn head(&self, drive: &str) -> Result<Reply<DriveHead>> {
        self.json("drive", self.http.get(self.url(&format!("/drives/{drive}")))).await
    }

    /// Send a commit. Sending the same commit again is harmless: the cloud
    /// answers what it answered the first time.
    pub async fn commit(&self, drive: &str, commit: &Commit) -> Result<Reply<Committed>> {
        let body = serde_json::to_vec(commit).expect("a commit serializes");
        self.json("commit", self.http.post(self.url(&format!("/drives/{drive}/commit"))).body(body)).await
    }

    /// One page of what changed after `since`.
    pub async fn feed(&self, drive: &str, since: u64, epoch: Option<u64>, limit: usize) -> Result<Reply<Feed>> {
        let epoch = epoch.map(|e| format!("&epoch={e}")).unwrap_or_default();
        self.json("feed", self.http.get(self.url(&format!("/drives/{drive}/feed?since={since}&limit={limit}{epoch}")))).await
    }

    pub async fn node(&self, drive: &str, id: &str) -> Result<Reply<NodeInfo>> {
        self.json("node", self.http.get(self.url(&format!("/drives/{drive}/nodes/{id}")))).await
    }

    /// Which of these sealed shards and manifests a commit would be refused for.
    pub async fn missing(&self, drive: &str, blobs: &Blobs) -> Result<Blobs> {
        self.json::<Blobs>("which shards are missing", self.http.post(self.url(&format!("/drives/{drive}/missing"))).json(blobs)).await?.accepted("which shards are missing")
    }

    async fn put_blob(&self, what: &str, path: String, bytes: Bytes) -> Result<()> {
        let n = bytes.len() as u64;
        let resp = self.http.put(self.url(&path)).timeout(transfer_timeout(n)).body(bytes).send().await.with_context(|| what.to_string())?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(HttpError { status, what: what.to_string(), body: body.chars().take(200).collect() }.into());
        }
        Stats::add(&self.stats.bytes_up, n);
        Ok(())
    }

    /// Store a sealed shard under the SHA-256 of its bytes.
    pub async fn put_shard(&self, drive: &str, name: &str, bytes: Bytes) -> Result<()> {
        self.put_blob("put shard", format!("/drives/{drive}/shards/{name}"), bytes).await
    }

    pub async fn put_manifest(&self, drive: &str, name: &str, bytes: Bytes) -> Result<()> {
        self.put_blob("put manifest", format!("/drives/{drive}/manifests/{name}"), bytes).await
    }

    /// A sealed manifest, or `None` if storage does not have it.
    pub async fn get_manifest(&self, drive: &str, name: &str) -> Result<Option<Bytes>> {
        let send = self.http.get(self.url(&format!("/drives/{drive}/manifests/{name}"))).timeout(Duration::from_secs(600)).send();
        let resp = tokio::time::timeout(STALL, send).await.map_err(|_| stalled("get manifest"))?.context("get manifest")?;
        if resp.status() == reqwest::StatusCode::CONFLICT || resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !resp.status().is_success() {
            return Err(HttpError { status: resp.status().as_u16(), what: "get manifest".into(), body: String::new() }.into());
        }
        let bytes = tokio::time::timeout(Duration::from_secs(120), resp.bytes()).await.map_err(|_| stalled("get manifest"))??;
        Stats::add(&self.stats.bytes_down, bytes.len() as u64);
        Ok(Some(bytes))
    }

    /// Bytes `start..start + len` of a sealed shard, arriving.
    pub async fn open_shard_range(&self, drive: &str, name: &str, start: u64, len: u64) -> Result<RangeBody> {
        Stats::add(&self.stats.range_requests, 1);
        let send = self
            .http
            .get(self.url(&format!("/drives/{drive}/shards/{name}")))
            .header("range", format!("bytes={start}-{}", start + len - 1))
            .timeout(transfer_timeout(len).max(Duration::from_secs(600)))
            .send();
        let what = format!("GET shard {}", &name[..name.len().min(12)]);
        let resp = tokio::time::timeout(STALL, send).await.map_err(|_| stalled(&what))?.with_context(|| what.clone())?;
        if !resp.status().is_success() {
            return Err(HttpError { status: resp.status().as_u16(), what: format!("{what} bytes={start}+{len}"), body: String::new() }.into());
        }
        Ok(RangeBody::new(resp, self.stats.clone(), what, start, len))
    }
}
