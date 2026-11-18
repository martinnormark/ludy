//! HTTP client for the simulated cloud.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use bytes::Bytes;
use serde::Deserialize;

pub use ludy_cloud::store::{DriveInfo, Listing, ObjectInfo, Share, Version};
pub use ludy_cloud::LogEntry;

use crate::stats::Stats;

#[derive(Clone)]
pub struct CloudClient {
    http: reqwest::Client,
    base: Arc<str>,
    stats: Arc<Stats>,
}

/// What a successful PUT reports back.
#[derive(Clone, Debug)]
pub struct Committed {
    pub version: String,
    pub mtime_ms: u64,
}

#[derive(Deserialize)]
struct DrivesBody {
    drives: Vec<DriveInfo>,
}

#[derive(Deserialize)]
struct SeqBody {
    seq: u64,
}

#[derive(Deserialize)]
struct VersionsBody {
    versions: Vec<Version>,
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
pub struct ServerStats {
    pub requests: u64,
    pub bytes_out: u64,
    pub bytes_in: u64,
    pub latency_ms: u64,
    pub mbps: u64,
}

/// Percent-encode a key for use in a URL path, keeping `/` as the separator.
pub fn encode_key(key: &str) -> String {
    let mut out = String::with_capacity(key.len());
    for b in key.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn encode_component(s: &str) -> String {
    encode_key(s).replace('/', "%2F")
}

impl CloudClient {
    pub fn new(base: &str, stats: Arc<Stats>) -> CloudClient {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(3))
            .pool_max_idle_per_host(16)
            .build()
            .expect("http client");
        CloudClient { http, base: base.trim_end_matches('/').into(), stats }
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    fn url(&self, drive: &str, key: &str) -> String {
        if key.is_empty() {
            format!("{}/{}", self.base, encode_component(drive))
        } else {
            format!("{}/{}/{}", self.base, encode_component(drive), encode_key(key))
        }
    }

    async fn ok(resp: reqwest::Response, what: &str) -> Result<reqwest::Response> {
        if resp.status().is_success() {
            return Ok(resp);
        }
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        bail!("{what}: {status} {body}")
    }

    pub async fn drives(&self) -> Result<Vec<DriveInfo>> {
        let resp = self.http.get(format!("{}/", self.base)).timeout(Duration::from_secs(10)).send().await?;
        Ok(Self::ok(resp, "list drives").await?.json::<DrivesBody>().await?.drives)
    }

    pub async fn create_drive(&self, name: &str) -> Result<()> {
        Self::ok(self.http.put(self.url(name, "")).send().await?, "create drive").await.map(|_| ())
    }

    pub async fn delete_drive(&self, name: &str) -> Result<()> {
        Self::ok(self.http.delete(self.url(name, "")).send().await?, "delete drive").await.map(|_| ())
    }

    pub async fn fork_drive(&self, from: &str, to: &str) -> Result<()> {
        let resp = self.http.post(self.url(from, "")).query(&[("fork", to)]).send().await?;
        Self::ok(resp, "fork drive").await.map(|_| ())
    }

    pub async fn rename_drive(&self, from: &str, to: &str) -> Result<()> {
        let resp = self.http.post(self.url(from, "")).query(&[("rename", to)]).send().await?;
        Self::ok(resp, "rename drive").await.map(|_| ())
    }

    pub async fn share_drive(&self, drive: &str, email: &str, role: &str) -> Result<()> {
        let body = serde_json::json!({ "email": email, "role": role });
        let resp = self.http.post(format!("{}?share", self.url(drive, ""))).json(&body).send().await?;
        Self::ok(resp, "share drive").await.map(|_| ())
    }

    pub async fn unshare_drive(&self, drive: &str, email: &str) -> Result<()> {
        let resp = self.http.post(self.url(drive, "")).query(&[("unshare", email)]).send().await?;
        Self::ok(resp, "unshare drive").await.map(|_| ())
    }

    /// Every object in the drive, flat.
    pub async fn list(&self, drive: &str) -> Result<Listing> {
        let resp = self.http.get(self.url(drive, "")).timeout(Duration::from_secs(30)).send().await?;
        Ok(Self::ok(resp, "list").await?.json().await?)
    }

    /// One folder level: objects directly under `prefix` plus sub-prefixes.
    pub async fn list_folder(&self, drive: &str, prefix: &str) -> Result<Listing> {
        let resp = self
            .http
            .get(self.url(drive, ""))
            .query(&[("prefix", prefix), ("delimiter", "/")])
            .timeout(Duration::from_secs(30))
            .send()
            .await?;
        Ok(Self::ok(resp, "list folder").await?.json().await?)
    }

    /// Long-poll until the drive's sequence number passes `since`.
    pub async fn wait_changes(&self, drive: &str, since: u64, wait_s: u64) -> Result<u64> {
        let resp = self
            .http
            .get(format!("{}?changes&since={since}&wait={wait_s}", self.url(drive, "")))
            .timeout(Duration::from_secs(wait_s + 10))
            .send()
            .await?;
        Ok(Self::ok(resp, "changes").await?.json::<SeqBody>().await?.seq)
    }

    /// Fetch `len` bytes at `start` of one specific version. Counted into the
    /// download statistics as the bytes arrive.
    pub async fn get_range(&self, drive: &str, key: &str, version: &str, start: u64, len: u64) -> Result<Bytes> {
        if len == 0 {
            return Ok(Bytes::new());
        }
        Stats::add(&self.stats.range_requests, 1);
        let mut resp = self
            .http
            .get(self.url(drive, key))
            .query(&[("version", version)])
            .header("range", format!("bytes={start}-{}", start + len - 1))
            .send()
            .await
            .with_context(|| format!("GET {drive}/{key}"))?;
        if !resp.status().is_success() {
            bail!("GET {drive}/{key} bytes={start}+{len}: {}", resp.status());
        }
        let mut buf = Vec::with_capacity(len as usize);
        while let Some(chunk) = resp.chunk().await? {
            Stats::add(&self.stats.bytes_down, chunk.len() as u64);
            buf.extend_from_slice(&chunk);
        }
        if buf.len() as u64 != len {
            bail!("GET {drive}/{key}: wanted {len} bytes at {start}, got {}", buf.len());
        }
        Ok(Bytes::from(buf))
    }

    fn committed(resp: &reqwest::Response) -> Result<Committed> {
        let h = resp.headers();
        let version = h.get("x-ludy-version").and_then(|v| v.to_str().ok()).ok_or_else(|| anyhow!("no version in reply"))?;
        let mtime_ms = h.get("x-ludy-mtime-ms").and_then(|v| v.to_str().ok()).and_then(|s| s.parse().ok()).unwrap_or(0);
        Ok(Committed { version: version.to_string(), mtime_ms })
    }

    /// Upload a body of known length. `body` may be a stream.
    pub async fn put(&self, drive: &str, key: &str, body: reqwest::Body, len: u64, mtime_ms: Option<u64>) -> Result<Committed> {
        let mut req = self.http.put(self.url(drive, key)).header("content-length", len).body(body);
        if let Some(m) = mtime_ms {
            req = req.header("x-ludy-mtime-ms", m);
        }
        let resp = Self::ok(req.send().await?, "put").await?;
        Self::committed(&resp)
    }

    pub async fn put_bytes(&self, drive: &str, key: &str, data: Bytes) -> Result<Committed> {
        let len = data.len() as u64;
        Stats::add(&self.stats.bytes_up, len);
        self.put(drive, key, reqwest::Body::from(data), len, None).await
    }

    /// Upload a local file, streamed.
    pub async fn put_file(&self, drive: &str, key: &str, path: &Path) -> Result<Committed> {
        let file = tokio::fs::File::open(path).await.with_context(|| format!("open {}", path.display()))?;
        let len = file.metadata().await?.len();
        let stats = self.stats.clone();
        let stream = futures_util::stream::unfold(file, move |mut file| {
            let stats = stats.clone();
            async move {
                use tokio::io::AsyncReadExt;
                let mut buf = vec![0u8; 256 * 1024];
                match file.read(&mut buf).await {
                    Ok(0) => None,
                    Ok(n) => {
                        buf.truncate(n);
                        Stats::add(&stats.bytes_up, n as u64);
                        Some((Ok::<Bytes, std::io::Error>(Bytes::from(buf)), file))
                    }
                    Err(e) => Some((Err(e), file)),
                }
            }
        });
        self.put(drive, key, reqwest::Body::wrap_stream(stream), len, None).await
    }

    /// Declare a synthetic object: `size` bytes generated from `seed`.
    pub async fn put_synthetic(&self, drive: &str, key: &str, size: u64, seed: u64) -> Result<Committed> {
        let resp = self
            .http
            .put(self.url(drive, key))
            .header("x-ludy-synthetic-size", size)
            .header("x-ludy-synthetic-seed", seed)
            .send()
            .await?;
        let resp = Self::ok(resp, "put synthetic").await?;
        Self::committed(&resp)
    }

    /// Delete a key. A key ending in `/` deletes the folder and its contents.
    /// A key that is already gone is not an error.
    pub async fn delete(&self, drive: &str, key: &str) -> Result<()> {
        let resp = self.http.delete(self.url(drive, key)).send().await?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(());
        }
        Self::ok(resp, "delete").await.map(|_| ())
    }

    /// Move a key or folder. Returns false when there was nothing to move.
    pub async fn rename(&self, drive: &str, from: &str, to: &str) -> Result<bool> {
        let resp = self.http.post(self.url(drive, from)).query(&[("rename", to)]).send().await?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(false);
        }
        Self::ok(resp, "rename").await.map(|_| true)
    }

    pub async fn versions(&self, drive: &str, key: &str) -> Result<Vec<Version>> {
        let resp = self.http.get(format!("{}?versions", self.url(drive, key))).send().await?;
        Ok(Self::ok(resp, "versions").await?.json::<VersionsBody>().await?.versions)
    }

    pub async fn restore(&self, drive: &str, key: &str, version: &str) -> Result<Committed> {
        let resp = self.http.post(self.url(drive, key)).query(&[("restore", version)]).send().await?;
        let resp = Self::ok(resp, "restore").await?;
        Self::committed(&resp)
    }

    pub async fn server_stats(&self) -> Result<ServerStats> {
        let resp = self.http.get(format!("{}/_stats", self.base)).timeout(Duration::from_secs(5)).send().await?;
        Ok(Self::ok(resp, "stats").await?.json().await?)
    }

    pub async fn server_log(&self, since: u64) -> Result<Vec<LogEntry>> {
        let resp = self.http.get(format!("{}/_log?since={since}", self.base)).timeout(Duration::from_secs(5)).send().await?;
        Ok(Self::ok(resp, "log").await?.json().await?)
    }

    pub async fn set_server_config(&self, latency_ms: Option<u64>, mbps: Option<u64>) -> Result<ServerStats> {
        let mut q: Vec<(&str, String)> = Vec::new();
        if let Some(v) = latency_ms {
            q.push(("latency_ms", v.to_string()));
        }
        if let Some(v) = mbps {
            q.push(("mbps", v.to_string()));
        }
        let resp = self.http.put(format!("{}/_config", self.base)).query(&q).send().await?;
        Ok(Self::ok(resp, "config").await?.json().await?)
    }
}
