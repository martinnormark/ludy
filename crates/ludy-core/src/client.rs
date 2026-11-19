//! HTTP client for the simulated cloud.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use bytes::Bytes;
use serde::Deserialize;

pub use ludy_cloud::store::{DriveInfo, Listing, ObjectInfo, Share, Version};
pub use ludy_cloud::shard::{Manifest, Segment};
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
    /// Shards the cloud stores, and the bytes they take on its disk.
    #[serde(default)]
    pub shards: u64,
    #[serde(default)]
    pub shard_bytes: u64,
    #[serde(default)]
    pub manifests: u64,
}

/// What a sharded upload moved.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct UploadReport {
    /// Shards sent, and their bytes.
    pub sent_shards: u64,
    pub sent_bytes: u64,
    /// Bytes not sent because the cloud already held those shards.
    pub skipped_bytes: u64,
}

#[derive(Deserialize)]
struct MissingBody {
    missing: Vec<String>,
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

    // ------------------------------------------------- shards and manifests

    /// The segments behind a version (the current one if `version` is None).
    /// `Ok(None)` when the version is whole-object content without a manifest.
    pub async fn manifest(&self, drive: &str, key: &str, version: Option<&str>) -> Result<Option<Manifest>> {
        let mut req = self.http.get(format!("{}?manifest", self.url(drive, key)));
        if let Some(v) = version {
            req = req.query(&[("version", v)]);
        }
        let resp = Self::ok(req.send().await?, "manifest").await?;
        if resp.status() == reqwest::StatusCode::NO_CONTENT {
            return Ok(None);
        }
        Ok(Some(resp.json::<Manifest>().await?))
    }

    /// Which of these shards the cloud does not hold yet.
    pub async fn missing_shards(&self, hashes: &[String]) -> Result<Vec<String>> {
        if hashes.is_empty() {
            return Ok(Vec::new());
        }
        let resp = self.http.post(format!("{}/_shards", self.base)).json(hashes).send().await?;
        Ok(Self::ok(resp, "missing shards").await?.json::<MissingBody>().await?.missing)
    }

    /// Upload one shard. The cloud checks that `data` hashes to `hash`.
    pub async fn put_shard(&self, hash: &str, data: Bytes) -> Result<()> {
        let n = data.len() as u64;
        let resp = self.http.put(format!("{}/_shards/{hash}", self.base)).body(data).send().await?;
        Self::ok(resp, "put shard").await?;
        Stats::add(&self.stats.bytes_up, n);
        Ok(())
    }

    /// Commit `manifest` as the new version of `key`. No content moves. If
    /// the cloud lacks some of its shards, they come back as `Err(hashes)`.
    pub async fn commit_manifest(
        &self,
        drive: &str,
        key: &str,
        manifest: &Manifest,
        mtime_ms: Option<u64>,
    ) -> Result<std::result::Result<Committed, Vec<String>>> {
        let mut req = self.http.put(format!("{}?manifest", self.url(drive, key))).json(manifest);
        if let Some(m) = mtime_ms {
            req = req.header("x-ludy-mtime-ms", m);
        }
        let resp = req.send().await?;
        if resp.status() == reqwest::StatusCode::CONFLICT {
            return Ok(Err(resp.json::<MissingBody>().await?.missing));
        }
        let resp = Self::ok(resp, "commit manifest").await?;
        Ok(Ok(Self::committed(&resp)?))
    }

    /// Upload a local file as shards: cut it here, ask which shards the cloud
    /// lacks, send only those, then commit the manifest.
    pub async fn put_file_sharded(&self, drive: &str, key: &str, path: &Path) -> Result<(Committed, UploadReport)> {
        let file = Arc::new(std::fs::File::open(path).with_context(|| format!("open {}", path.display()))?);
        let cut = file.clone();
        let shards: Vec<(u64, u64, String)> = tokio::task::spawn_blocking(move || {
            ludy_cloud::shard::chunks(&*cut).map(|c| c.map(|c| (c.offset, c.data.len() as u64, c.hash))).collect::<std::io::Result<Vec<_>>>()
        })
        .await??;
        let manifest = Manifest {
            size: shards.iter().map(|s| s.1).sum(),
            segments: shards.iter().map(|(_, n, h)| Segment::Shard { h: h.clone(), n: *n }).collect(),
        };
        let mut report = UploadReport::default();
        let hashes: Vec<String> = shards.iter().map(|s| s.2.clone()).collect();
        let mut missing: std::collections::HashSet<String> = self.missing_shards(&hashes).await?.into_iter().collect();
        report.skipped_bytes = shards.iter().filter(|s| !missing.contains(&s.2)).map(|s| s.1).sum();
        Stats::add(&self.stats.bytes_skipped, report.skipped_bytes);
        for _ in 0..2 {
            for (offset, n, hash) in &shards {
                if !missing.remove(hash) {
                    continue;
                }
                use std::os::unix::fs::FileExt;
                let (file, offset, n) = (file.clone(), *offset, *n as usize);
                let data = tokio::task::spawn_blocking(move || {
                    let mut buf = vec![0u8; n];
                    file.read_exact_at(&mut buf, offset).map(|_| buf)
                })
                .await??;
                self.put_shard(hash, Bytes::from(data)).await?;
                report.sent_shards += 1;
                report.sent_bytes += n as u64;
            }
            match self.commit_manifest(drive, key, &manifest, None).await? {
                Ok(committed) => return Ok((committed, report)),
                // A shard the cloud had was cleaned up in between: send it too.
                Err(gone) => missing = gone.into_iter().collect(),
            }
        }
        bail!("put {drive}/{key}: the cloud keeps reporting missing shards")
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
