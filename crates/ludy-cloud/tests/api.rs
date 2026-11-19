//! The HTTP surface of the simulated cloud, exercised with a plain client.

use std::time::{Duration, Instant};

use ludy_cloud::shard::sha256_hex;
use ludy_cloud::{synth, Config};
use reqwest::{Client, StatusCode};
use serde_json::Value;

struct Server {
    _dir: tempfile::TempDir,
    base: String,
    http: Client,
}

async fn server(latency_ms: u64, mbps: u64) -> Server {
    let dir = tempfile::tempdir().unwrap();
    let cfg = Config { data_dir: dir.path().to_path_buf(), addr: "127.0.0.1:0".parse().unwrap(), latency_ms, mbps, print_log: false };
    let (addr, _cloud, _task) = ludy_cloud::serve(cfg).await.unwrap();
    let s = Server { _dir: dir, base: format!("http://{addr}"), http: Client::new() };
    assert_eq!(s.http.put(format!("{}/d", s.base)).send().await.unwrap().status(), StatusCode::CREATED);
    s
}

impl Server {
    async fn put(&self, key: &str, body: &[u8]) -> reqwest::Response {
        self.http.put(format!("{}/d/{key}", self.base)).body(body.to_vec()).send().await.unwrap()
    }
    async fn get(&self, key: &str, range: Option<&str>) -> reqwest::Response {
        let mut req = self.http.get(format!("{}/d/{key}", self.base));
        if let Some(r) = range {
            req = req.header("range", r);
        }
        req.send().await.unwrap()
    }
    async fn json(&self, path: &str) -> Value {
        self.http.get(format!("{}{path}", self.base)).send().await.unwrap().json().await.unwrap()
    }
    /// Shard files on the server's disk: (count, bytes).
    fn shards(&self) -> (usize, u64) {
        shard_files(self._dir.path())
    }
    fn manifest_count(&self) -> usize {
        std::fs::read_dir(self._dir.path().join("manifests")).unwrap().count()
    }
    async fn manifest(&self, drive: &str, key: &str) -> Value {
        self.json(&format!("/{drive}/{key}?manifest")).await
    }
    async fn put_shard(&self, data: &[u8]) -> String {
        let hash = sha256_hex(data);
        let r = self.http.put(format!("{}/_shards/{hash}", self.base)).body(data.to_vec()).send().await.unwrap();
        assert!(r.status().is_success(), "shard put: {}", r.status());
        hash
    }
    async fn commit(&self, key: &str, manifest: &Value) -> reqwest::Response {
        self.http.put(format!("{}/d/{key}?manifest", self.base)).json(manifest).send().await.unwrap()
    }
}

fn shard_files(data: &std::path::Path) -> (usize, u64) {
    let mut out = (0, 0);
    for dir in std::fs::read_dir(data.join("shards")).unwrap().flatten() {
        for f in std::fs::read_dir(dir.path()).unwrap().flatten() {
            out.0 += 1;
            out.1 += f.metadata().unwrap().len();
        }
    }
    out
}

#[tokio::test]
async fn range_requests_follow_http_semantics() {
    let s = server(0, 0).await;
    let body: Vec<u8> = (0..=255u8).cycle().take(1000).collect();
    assert_eq!(s.put("a.bin", &body).await.status(), StatusCode::CREATED);

    let r = s.get("a.bin", None).await;
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(r.headers()["accept-ranges"], "bytes");
    assert_eq!(&r.bytes().await.unwrap()[..], &body[..]);

    let r = s.get("a.bin", Some("bytes=100-199")).await;
    assert_eq!(r.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(r.headers()["content-range"], "bytes 100-199/1000");
    assert_eq!(r.headers()["content-length"], "100");
    assert_eq!(&r.bytes().await.unwrap()[..], &body[100..200]);

    // open-ended, suffix, and an end past the object
    assert_eq!(&s.get("a.bin", Some("bytes=990-")).await.bytes().await.unwrap()[..], &body[990..]);
    assert_eq!(&s.get("a.bin", Some("bytes=-10")).await.bytes().await.unwrap()[..], &body[990..]);
    let r = s.get("a.bin", Some("bytes=900-5000")).await;
    assert_eq!(r.headers()["content-range"], "bytes 900-999/1000");

    assert_eq!(s.get("a.bin", Some("bytes=1000-1001")).await.status(), StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(s.get("missing", None).await.status(), StatusCode::NOT_FOUND);
    assert_eq!(s.http.get(format!("{}/nodrive/x", s.base)).send().await.unwrap().status(), StatusCode::NOT_FOUND);

    let head = s.http.head(format!("{}/d/a.bin", s.base)).send().await.unwrap();
    assert_eq!(head.headers()["content-length"], "1000");
    assert!(head.headers().contains_key("etag"));
}

#[tokio::test]
async fn a_synthetic_50gb_object_serves_any_range_and_uses_no_disk() {
    let s = server(0, 0).await;
    let size = 50u64 << 30;
    let r = s.http.put(format!("{}/d/huge.bin", s.base)).header("x-ludy-synthetic-size", "50GB").header("x-ludy-synthetic-seed", "7").send().await.unwrap();
    assert_eq!(r.status(), StatusCode::CREATED);
    let head = s.http.head(format!("{}/d/huge.bin", s.base)).send().await.unwrap();
    assert_eq!(head.headers()["content-length"], size.to_string().as_str());
    let off = 25 * (1u64 << 30) + 3;
    let got = s.get("huge.bin", Some(&format!("bytes={off}-{}", off + 4999))).await.bytes().await.unwrap();
    assert_eq!(&got[..], &synth::bytes(7, off, 5000)[..]);
    let tail = s.get("huge.bin", Some("bytes=-16")).await.bytes().await.unwrap();
    assert_eq!(&tail[..], &synth::bytes(7, size - 16, 16)[..]);
    assert_eq!(s.shards(), (0, 0), "a synthetic object stores nothing");
    let drives = s.json("/").await;
    assert_eq!(drives["drives"][0]["bytes"], size);
    assert_eq!(drives["drives"][0]["synthetic_bytes"], size);
}

#[tokio::test]
async fn every_put_is_a_new_version_and_old_ones_can_be_restored() {
    let s = server(0, 0).await;
    let v1 = s.put("doc.txt", b"first").await.headers()["x-ludy-version"].to_str().unwrap().to_string();
    let v2 = s.put("doc.txt", b"second, longer").await.headers()["x-ludy-version"].to_str().unwrap().to_string();
    assert_ne!(v1, v2);
    let versions = s.json("/d/doc.txt?versions").await;
    assert_eq!(versions["versions"].as_array().unwrap().len(), 2);

    // An old version stays readable by id.
    let old = s.http.get(format!("{}/d/doc.txt?version={v1}", s.base)).send().await.unwrap().bytes().await.unwrap();
    assert_eq!(&old[..], b"first");

    // Restore = a new version pointing at the old content; nothing is stored.
    let (shards, manifests) = (s.shards(), s.manifest_count());
    let r = s.http.post(format!("{}/d/doc.txt?restore={v1}", s.base)).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(&s.get("doc.txt", None).await.bytes().await.unwrap()[..], b"first");
    let versions = s.json("/d/doc.txt?versions").await;
    let versions = versions["versions"].as_array().unwrap();
    assert_eq!(versions.len(), 3);
    assert_eq!((s.shards(), s.manifest_count()), (shards, manifests));
    // Every version names the one it replaced.
    assert!(versions[0].get("parent").is_none());
    assert_eq!(versions[1]["parent"], v1.as_str());
    assert_eq!(versions[2]["parent"], v2.as_str());
    assert_eq!(versions[2]["content"], versions[0]["content"]);
}

#[tokio::test]
async fn of_two_racing_writers_exactly_one_wins() {
    let s = server(0, 0).await;
    let v1 = s.put("doc.txt", b"base").await.headers()["x-ludy-version"].to_str().unwrap().to_string();
    let write = |body: &'static str| s.http.put(format!("{}/d/doc.txt", s.base)).header("if-match", v1.clone()).body(body).send();
    let (a, b) = tokio::join!(write("from a"), write("from b"));
    let mut statuses = [a.unwrap().status(), b.unwrap().status()];
    statuses.sort();
    assert_eq!(statuses, [StatusCode::CREATED, StatusCode::PRECONDITION_FAILED]);
    assert_eq!(s.json("/d/doc.txt?versions").await["versions"].as_array().unwrap().len(), 2);
    // The loser's upload left nothing behind: one shard each for the base and the winner.
    assert_eq!(s.shards().0, 2);
    assert_eq!(s.manifest_count(), 2);
}

#[tokio::test]
async fn rename_moves_folders_without_copying_and_delete_frees_blobs() {
    let s = server(0, 0).await;
    s.put("a/one.txt", b"1").await;
    s.put("a/sub/two.txt", b"22").await;
    s.put("b.txt", b"333").await;
    let shards = s.shards();
    let r = s.http.post(format!("{}/d/a?rename=moved/here", s.base)).send().await.unwrap();
    assert_eq!(r.json::<Value>().await.unwrap()["moved"], 2);
    let listing = s.json("/d").await;
    let keys: Vec<&str> = listing["objects"].as_array().unwrap().iter().map(|o| o["key"].as_str().unwrap()).collect();
    assert_eq!(keys, ["b.txt", "moved/here/one.txt", "moved/here/sub/two.txt"]);
    assert_eq!(s.shards(), shards);

    // One folder level at a time, the way a file browser asks.
    let level = s.json("/d?prefix=moved/here/&delimiter=/").await;
    assert_eq!(level["objects"].as_array().unwrap().len(), 1);
    assert_eq!(level["prefixes"][0], "moved/here/sub/");

    assert_eq!(s.http.delete(format!("{}/d/moved/", s.base)).send().await.unwrap().status(), StatusCode::NO_CONTENT);
    assert_eq!(s.json("/d").await["objects"].as_array().unwrap().len(), 1);
    assert_eq!(s.shards(), (1, 3), "deleted objects release their shards");
    assert_eq!(s.manifest_count(), 1);
    assert_eq!(s.http.delete(format!("{}/d/moved/", s.base)).send().await.unwrap().status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn forking_a_drive_copies_no_data_and_the_forks_diverge() {
    let s = server(0, 0).await;
    s.put("film.mov", &vec![7u8; 200_000]).await;
    let shards = s.shards();
    assert_eq!(s.http.post(format!("{}/d?fork=d-fork", s.base)).send().await.unwrap().status(), StatusCode::OK);
    assert_eq!(s.shards(), shards, "a fork shares content");
    let drives = s.json("/").await;
    let fork = drives["drives"].as_array().unwrap().iter().find(|d| d["name"] == "d-fork").unwrap();
    assert_eq!(fork["forked_from"], "d");
    assert_eq!(fork["bytes"], 200_000);

    // Changing the fork leaves the original alone, and vice versa.
    s.http.put(format!("{}/d-fork/film.mov", s.base)).body("recut").send().await.unwrap();
    assert_eq!(s.get("film.mov", None).await.bytes().await.unwrap().len(), 200_000);
    // Deleting the original keeps the shards the fork's history still uses.
    s.http.delete(format!("{}/d/film.mov", s.base)).send().await.unwrap();
    let old = s.json("/d-fork/film.mov?versions").await["versions"][0]["id"].as_str().unwrap().to_string();
    let bytes = s.http.get(format!("{}/d-fork/film.mov?version={old}", s.base)).send().await.unwrap().bytes().await.unwrap();
    assert_eq!(bytes.len(), 200_000);
    // Deleting the fork too leaves only what nothing shares... which is nothing.
    assert_eq!(s.http.delete(format!("{}/d-fork", s.base)).send().await.unwrap().status(), StatusCode::NO_CONTENT);
    assert_eq!(s.shards(), (0, 0));
    assert_eq!(s.manifest_count(), 0);
}

#[tokio::test]
async fn a_long_poll_wakes_on_the_next_commit() {
    let s = server(0, 0).await;
    let seq = s.json("/d").await["seq"].as_u64().unwrap();
    let waiter = {
        let (http, url) = (s.http.clone(), format!("{}/d?changes&since={seq}&wait=10", s.base));
        tokio::spawn(async move { http.get(url).send().await.unwrap().json::<Value>().await.unwrap() })
    };
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(!waiter.is_finished(), "nothing changed yet");
    let t0 = Instant::now();
    s.put("new.txt", b"x").await;
    let woke = waiter.await.unwrap();
    assert!(woke["seq"].as_u64().unwrap() > seq);
    assert!(t0.elapsed() < Duration::from_secs(1));
    // With no change, it returns the same sequence number at the timeout.
    let same = s.json(&format!("/d?changes&since={}&wait=1", woke["seq"])).await;
    assert_eq!(same["seq"], woke["seq"]);
}

#[tokio::test]
async fn latency_and_bandwidth_are_simulated_and_ranges_are_logged() {
    // 8 Mbit/s = 1 MB/s; 300 kB should take about 0.3 s plus 100 ms latency.
    let s = server(100, 8).await;
    s.put("clip.bin", &vec![1u8; 400_000]).await;
    let t0 = Instant::now();
    let got = s.get("clip.bin", Some("bytes=0-299999")).await.bytes().await.unwrap();
    let took = t0.elapsed();
    assert_eq!(got.len(), 300_000);
    assert!(took >= Duration::from_millis(330), "throttle too loose: {took:?}");
    assert!(took < Duration::from_millis(1500), "throttle too tight: {took:?}");

    let log = s.json("/_log").await;
    let entry = log.as_array().unwrap().iter().rev().find(|e| e["method"] == "GET" && e["key"] == "clip.bin").unwrap();
    assert_eq!(entry["range"], serde_json::json!([0, 299_999]));
    assert_eq!(entry["status"], 206);

    // The link can be retuned while running.
    let cfg: Value = s.http.put(format!("{}/_config?latency_ms=0&mbps=0", s.base)).send().await.unwrap().json().await.unwrap();
    assert_eq!((cfg["latency_ms"].as_u64(), cfg["mbps"].as_u64()), (Some(0), Some(0)));
    let t0 = Instant::now();
    s.get("clip.bin", Some("bytes=0-299999")).await.bytes().await.unwrap();
    assert!(t0.elapsed() < Duration::from_millis(200));
}

#[tokio::test]
async fn state_survives_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = Config { data_dir: dir.path().to_path_buf(), addr: "127.0.0.1:0".parse().unwrap(), latency_ms: 0, mbps: 0, print_log: false };
    let http = Client::new();
    {
        let (addr, _c, task) = ludy_cloud::serve(cfg.clone()).await.unwrap();
        http.put(format!("http://{addr}/keep")).send().await.unwrap();
        http.put(format!("http://{addr}/keep/a.txt")).body("persisted").send().await.unwrap();
        http.post(format!("http://{addr}/keep?share")).json(&serde_json::json!({"email": "sam@example.com", "role": "edit"})).send().await.unwrap();
        task.abort();
    }
    let (addr, _c, _task) = ludy_cloud::serve(cfg).await.unwrap();
    let body = http.get(format!("http://{addr}/keep/a.txt")).send().await.unwrap().text().await.unwrap();
    assert_eq!(body, "persisted");
    let drives: Value = http.get(format!("http://{addr}/")).send().await.unwrap().json().await.unwrap();
    assert_eq!(drives["drives"][0]["shares"][0]["role"], "edit");
}

// ---------------------------------------------------------------- shards

const MIB: usize = 1 << 20;

#[tokio::test]
async fn an_object_is_stored_as_shards_and_reads_back_across_their_boundaries() {
    let s = server(0, 0).await;
    let body = synth::bytes(21, 0, 24 * MIB);
    let r = s.put("big.bin", &body).await;
    assert_eq!(r.status(), StatusCode::CREATED);
    let new_shards: usize = r.headers()["x-ludy-new-shards"].to_str().unwrap().parse().unwrap();
    assert_eq!(r.headers()["x-ludy-new-bytes"], (24 * MIB).to_string().as_str());

    let m = s.manifest("d", "big.bin").await;
    let segments = m["segments"].as_array().unwrap();
    assert_eq!(m["size"], 24 * MIB);
    assert_eq!(segments.len(), new_shards);
    assert!(segments.len() >= 4, "24 MiB in {} shard(s)", segments.len());
    assert_eq!(s.shards(), (segments.len(), 24 * MIB as u64));
    // Each shard file is named by the SHA-256 of its bytes.
    let mut pos = 0usize;
    for seg in segments {
        let n = seg["n"].as_u64().unwrap() as usize;
        assert_eq!(seg["h"], sha256_hex(&body[pos..pos + n]).as_str());
        pos += n;
    }

    // Ranges inside a shard, across one boundary, across several, and the tail.
    let first = segments[0]["n"].as_u64().unwrap() as usize;
    let second = first + segments[1]["n"].as_u64().unwrap() as usize;
    for (a, b) in [(0, 99), (first - 10, first + 9), (first - 1, second + 5), (100, 24 * MIB - 1), (24 * MIB - 7, 24 * MIB - 1)] {
        let r = s.get("big.bin", Some(&format!("bytes={a}-{b}"))).await;
        assert_eq!(r.status(), StatusCode::PARTIAL_CONTENT);
        assert!(r.bytes().await.unwrap()[..] == body[a..=b], "range {a}-{b}");
    }
    assert!(s.get("big.bin", None).await.bytes().await.unwrap()[..] == body[..]);

    let stats = s.json("/_stats").await;
    assert_eq!(stats["shards"], segments.len());
    assert_eq!(stats["shard_bytes"], 24 * MIB);
    assert_eq!(stats["manifests"], 1);
}

#[tokio::test]
async fn identical_content_is_stored_once_across_keys_and_drives() {
    let s = server(0, 0).await;
    let body = synth::bytes(22, 0, 6 * MIB);
    s.put("one.bin", &body).await;
    let stored = s.shards();
    assert_eq!(stored.1, 6 * MIB as u64);

    let r = s.put("copies/two.bin", &body).await;
    assert_eq!(r.headers()["x-ludy-new-shards"], "0");
    assert_eq!(r.headers()["x-ludy-new-bytes"], "0");
    s.http.put(format!("{}/other", s.base)).send().await.unwrap();
    let r = s.http.put(format!("{}/other/three.bin", s.base)).body(body.clone()).send().await.unwrap();
    assert_eq!(r.headers()["x-ludy-new-bytes"], "0");
    assert_eq!(s.shards(), stored, "three objects, one copy of the bytes");
    assert_eq!(s.manifest_count(), 1, "and one manifest");
    assert_eq!(s.manifest("d", "one.bin").await["id"], s.manifest("other", "three.bin").await["id"]);

    // The bytes stay while any of the three needs them.
    s.http.delete(format!("{}/d/one.bin", s.base)).send().await.unwrap();
    s.http.delete(format!("{}/other", s.base)).send().await.unwrap();
    assert_eq!(s.shards(), stored);
    assert!(s.get("copies/two.bin", None).await.bytes().await.unwrap()[..] == body[..]);
    s.http.delete(format!("{}/d/copies/two.bin", s.base)).send().await.unwrap();
    assert_eq!(s.shards(), (0, 0));
}

#[tokio::test]
async fn editing_a_few_kilobytes_adds_only_the_shards_around_the_edit() {
    let s = server(0, 0).await;
    let mut body = synth::bytes(23, 0, 24 * MIB);
    s.put("film.bin", &body).await;
    let before = s.shards();

    for b in &mut body[12 * MIB..12 * MIB + 4096] {
        *b ^= 0xff;
    }
    let r = s.put("film.bin", &body).await;
    let new_shards: u64 = r.headers()["x-ludy-new-shards"].to_str().unwrap().parse().unwrap();
    let new_bytes: u64 = r.headers()["x-ludy-new-bytes"].to_str().unwrap().parse().unwrap();
    assert!((1..=3).contains(&new_shards), "{new_shards} new shards for a 4 KiB edit");
    assert!(new_bytes < 8 * MIB as u64, "{new_bytes} new bytes for a 4 KiB edit");
    assert_eq!(s.shards().1, before.1 + new_bytes, "both versions share everything else");
    assert!(s.get("film.bin", None).await.bytes().await.unwrap()[..] == body[..]);
}

#[tokio::test]
async fn a_shard_must_hash_to_its_name() {
    let s = server(0, 0).await;
    let data = b"shard bytes".to_vec();
    let hash = s.put_shard(&data).await;
    // Sending it again is harmless and stores nothing new.
    assert_eq!(s.http.put(format!("{}/_shards/{hash}", s.base)).body(data.clone()).send().await.unwrap().status(), StatusCode::OK);

    let wrong = sha256_hex(b"something else");
    let r = s.http.put(format!("{}/_shards/{wrong}", s.base)).body(data.clone()).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    for name in ["abc", &hash.to_uppercase(), &format!("..%2F{}", &hash[5..])] {
        let r = s.http.put(format!("{}/_shards/{name}", s.base)).body(data.clone()).send().await.unwrap();
        assert_eq!(r.status(), StatusCode::BAD_REQUEST, "{name}");
    }

    let ask = serde_json::json!([hash, wrong, wrong]);
    let r: Value = s.http.post(format!("{}/_shards", s.base)).json(&ask).send().await.unwrap().json().await.unwrap();
    assert_eq!(r["missing"], serde_json::json!([wrong]));
}

#[tokio::test]
async fn a_manifest_commits_only_when_its_shards_are_present_and_add_up() {
    let s = server(0, 0).await;
    let (a, b) = (synth::bytes(31, 0, 300_000), synth::bytes(32, 0, 500_000));
    let ha = s.put_shard(&a).await;
    let hb = sha256_hex(&b);
    let manifest = serde_json::json!({ "size": 800_000, "segments": [{ "h": ha, "n": 300_000 }, { "h": hb, "n": 500_000 }] });

    let r = s.commit("joined.bin", &manifest).await;
    assert_eq!(r.status(), StatusCode::CONFLICT);
    assert_eq!(r.json::<Value>().await.unwrap()["missing"], serde_json::json!([hb]));
    assert_eq!(s.get("joined.bin", None).await.status(), StatusCode::NOT_FOUND);

    s.put_shard(&b).await;
    let r = s.commit("joined.bin", &manifest).await;
    assert_eq!(r.status(), StatusCode::CREATED);
    let version = r.headers()["x-ludy-version"].to_str().unwrap().to_string();
    let body = s.get("joined.bin", None).await.bytes().await.unwrap();
    assert!(body[..300_000] == a[..] && body[300_000..] == b[..]);
    let r = s.get("joined.bin", Some("bytes=299990-300009")).await.bytes().await.unwrap();
    assert!(r[..10] == a[299_990..] && r[10..] == b[..10]);

    // Lengths that do not add up, or do not match the stored shard, are refused.
    let short = serde_json::json!({ "size": 700_000, "segments": [{ "h": ha, "n": 300_000 }, { "h": hb, "n": 500_000 }] });
    assert_eq!(s.commit("bad.bin", &short).await.status(), StatusCode::BAD_REQUEST);
    let lying = serde_json::json!({ "size": 10, "segments": [{ "h": ha, "n": 10 }] });
    assert_eq!(s.commit("bad.bin", &lying).await.status(), StatusCode::BAD_REQUEST);

    // if-match applies to manifest commits like any other.
    let only_a = serde_json::json!({ "size": 300_000, "segments": [{ "h": ha, "n": 300_000 }] });
    let stale = s.http.put(format!("{}/d/joined.bin?manifest", s.base)).header("if-match", "0000").json(&only_a).send().await.unwrap();
    assert_eq!(stale.status(), StatusCode::PRECONDITION_FAILED);
    let fresh = s.http.put(format!("{}/d/joined.bin?manifest", s.base)).header("if-match", version).json(&only_a).send().await.unwrap();
    assert_eq!(fresh.status(), StatusCode::CREATED);
    assert_eq!(s.get("joined.bin", None).await.bytes().await.unwrap().len(), 300_000);
}

#[tokio::test]
async fn a_manifest_can_mix_stored_shards_with_synthetic_ranges() {
    let s = server(0, 0).await;
    // A 50 GB synthetic object with 300 KB of real bytes patched in at 25 GiB + 3.
    let size: u64 = 50 << 30;
    let at: u64 = (25 << 30) + 3;
    s.http.put(format!("{}/d/huge.braw", s.base)).header("x-ludy-synthetic-size", size).header("x-ludy-synthetic-seed", 7).send().await.unwrap();
    let base = s.manifest("d", "huge.braw").await;
    assert_eq!(base["segments"], serde_json::json!([{ "seed": 7, "off": 0, "n": size }]));

    let patch = synth::bytes(99, 0, 300_000);
    let h = s.put_shard(&patch).await;
    let manifest = serde_json::json!({ "size": size, "segments": [
        { "seed": 7, "off": 0, "n": at },
        { "h": h, "n": 300_000 },
        { "seed": 7, "off": at + 300_000, "n": size - at - 300_000 },
    ]});
    assert_eq!(s.commit("huge.braw", &manifest).await.status(), StatusCode::CREATED);
    assert_eq!(s.shards(), (1, 300_000), "the 50 GB around the patch take no disk");

    let r = s.get("huge.braw", Some(&format!("bytes={}-{}", at - 16, at + 300_000 + 15))).await;
    assert_eq!(r.headers()["x-ludy-synthetic"], "1");
    let got = r.bytes().await.unwrap();
    assert!(got[..16] == synth::bytes(7, at - 16, 16)[..]);
    assert!(got[16..300_016] == patch[..]);
    assert!(got[300_016..] == synth::bytes(7, at + 300_000, 16)[..]);
    let tail = s.get("huge.braw", Some("bytes=-8")).await.bytes().await.unwrap();
    assert_eq!(&tail[..], &synth::bytes(7, size - 8, 8)[..]);

    // Listings still count the generated part as synthetic.
    let listing = s.json("/d").await;
    assert_eq!(listing["objects"][0]["size"], size);
    assert_eq!(listing["objects"][0]["synthetic"], true);
    let drives = s.json("/").await;
    assert_eq!(drives["drives"][0]["synthetic_bytes"], size - 300_000);
}

#[tokio::test]
async fn history_beyond_the_cap_releases_its_shards() {
    let s = server(0, 0).await;
    for i in 0..30u32 {
        s.put("log.txt", format!("revision {i}").as_bytes()).await;
    }
    assert_eq!(s.json("/d/log.txt?versions").await["versions"].as_array().unwrap().len(), 25);
    assert_eq!(s.shards().0, 25);
    assert_eq!(s.manifest_count(), 25);
}

#[tokio::test]
async fn a_restart_keeps_shards_in_use_and_sweeps_orphans() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = Config { data_dir: dir.path().to_path_buf(), addr: "127.0.0.1:0".parse().unwrap(), latency_ms: 0, mbps: 0, print_log: false };
    let http = Client::new();
    let body = synth::bytes(41, 0, 5 * MIB);
    let orphan = b"uploaded but never committed".to_vec();
    let kept;
    {
        let (addr, _c, task) = ludy_cloud::serve(cfg.clone()).await.unwrap();
        http.put(format!("http://{addr}/keep")).send().await.unwrap();
        http.put(format!("http://{addr}/keep/a.bin")).body(body.clone()).send().await.unwrap();
        http.post(format!("http://{addr}/keep?fork=copy")).send().await.unwrap();
        kept = shard_files(dir.path());
        let r = http.put(format!("http://{addr}/_shards/{}", sha256_hex(&orphan))).body(orphan.clone()).send().await.unwrap();
        assert_eq!(r.status(), StatusCode::CREATED);
        assert_eq!(shard_files(dir.path()).0, kept.0 + 1);
        task.abort();
    }
    let (addr, _c, _task) = ludy_cloud::serve(cfg).await.unwrap();
    assert_eq!(shard_files(dir.path()), kept, "the orphan is gone, the rest stayed");
    let got = http.get(format!("http://{addr}/keep/a.bin")).send().await.unwrap().bytes().await.unwrap();
    assert!(got[..] == body[..]);

    // Reference counts were rebuilt: the fork keeps the shards alive, then nothing does.
    http.delete(format!("http://{addr}/keep/a.bin")).send().await.unwrap();
    assert_eq!(shard_files(dir.path()), kept);
    http.delete(format!("http://{addr}/copy")).send().await.unwrap();
    assert_eq!(shard_files(dir.path()), (0, 0));
}

#[tokio::test]
async fn a_data_dir_from_before_shards_still_serves_and_migrates_on_write() {
    // What the store wrote before shards existed: a catalog whose versions
    // point at whole-object blobs.
    let dir = tempfile::tempdir().unwrap();
    let body = synth::bytes(71, 0, 300_000);
    std::fs::create_dir_all(dir.path().join("drives")).unwrap();
    std::fs::create_dir_all(dir.path().join("blobs")).unwrap();
    std::fs::write(dir.path().join("blobs/01a1f0eaf4510001"), &body).unwrap();
    let catalog = serde_json::json!({
        "created_ms": 1795043292193u64, "seq": 2, "forked_from": null, "shares": [],
        "objects": { "old.bin": [{ "id": "01a1f0eafa0f0002", "size": 300_000, "mtime_ms": 1795043293711u64, "seq": 2,
                                   "content": { "kind": "blob", "id": "01a1f0eaf4510001" } }] }
    });
    std::fs::write(dir.path().join("drives/d.json"), serde_json::to_vec(&catalog).unwrap()).unwrap();

    let cfg = Config { data_dir: dir.path().to_path_buf(), addr: "127.0.0.1:0".parse().unwrap(), latency_ms: 0, mbps: 0, print_log: false };
    let (addr, _c, _task) = ludy_cloud::serve(cfg).await.unwrap();
    let (http, base) = (Client::new(), format!("http://{addr}"));

    let r = http.get(format!("{base}/d/old.bin")).header("range", "bytes=1000-1999").send().await.unwrap();
    assert_eq!(r.status(), StatusCode::PARTIAL_CONTENT);
    assert!(r.bytes().await.unwrap()[..] == body[1000..2000]);
    // It has no manifest, and says so without an error.
    assert_eq!(http.get(format!("{base}/d/old.bin?manifest")).send().await.unwrap().status(), StatusCode::NO_CONTENT);

    // Writing the key stores shards; the blob version stays readable in history.
    let r = http.put(format!("{base}/d/old.bin")).body("new content").send().await.unwrap();
    assert_eq!(r.status(), StatusCode::CREATED);
    assert_eq!(http.get(format!("{base}/d/old.bin")).send().await.unwrap().text().await.unwrap(), "new content");
    let old = http.get(format!("{base}/d/old.bin?version=01a1f0eafa0f0002")).send().await.unwrap().bytes().await.unwrap();
    assert!(old[..] == body[..]);
    assert_eq!(shard_files(dir.path()).0, 1);

    // Deleting the key removes the blob along with the shard.
    assert_eq!(http.delete(format!("{base}/d/old.bin")).send().await.unwrap().status(), StatusCode::NO_CONTENT);
    assert_eq!(std::fs::read_dir(dir.path().join("blobs")).unwrap().count(), 0);
    assert_eq!(shard_files(dir.path()), (0, 0));
}

// ---------------------------------------------------------------- catalog log and change feed

#[tokio::test]
async fn commits_are_appended_to_a_log_and_replayed_at_the_next_start() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = Config { data_dir: dir.path().to_path_buf(), addr: "127.0.0.1:0".parse().unwrap(), latency_ms: 0, mbps: 0, print_log: false };
    let http = Client::new();
    let snapshot = dir.path().join("drives/d.json");
    let log = dir.path().join("drives/d.log");
    let before;
    {
        let (addr, _c, task) = ludy_cloud::serve(cfg.clone()).await.unwrap();
        let base = format!("http://{addr}");
        http.put(format!("{base}/d")).send().await.unwrap();
        let created = std::fs::metadata(&snapshot).unwrap().len();
        for i in 0..40 {
            http.put(format!("{base}/d/folder/file {i:02}.txt")).body(format!("content {i}")).send().await.unwrap();
        }
        http.put(format!("{base}/d/folder/file 05.txt")).body("changed").send().await.unwrap();
        http.delete(format!("{base}/d/folder/file 06.txt")).send().await.unwrap();
        http.post(format!("{base}/d/folder?rename=moved")).send().await.unwrap();
        // The snapshot was not rewritten for any of that; the log grew by a line per commit.
        assert_eq!(std::fs::metadata(&snapshot).unwrap().len(), created);
        assert_eq!(std::fs::read_to_string(&log).unwrap().lines().count(), 43);
        before = http.get(format!("{base}/d")).send().await.unwrap().json::<Value>().await.unwrap();
        task.abort();
    }
    // A crash in the middle of an append leaves a torn last line.
    {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().append(true).open(&log).unwrap();
        f.write_all(b"{\"seq\": 99, \"set\": {\"torn").unwrap();
    }
    let (addr, _c, _task) = ludy_cloud::serve(cfg).await.unwrap();
    let base = format!("http://{addr}");
    let after = http.get(format!("{base}/d")).send().await.unwrap().json::<Value>().await.unwrap();
    assert_eq!(after["objects"], before["objects"]);
    assert_eq!(after["seq"], before["seq"]);
    assert_eq!(after["objects"].as_array().unwrap().len(), 39);
    assert_eq!(http.get(format!("{base}/d/moved/file 05.txt")).send().await.unwrap().text().await.unwrap(), "changed");

    // Forking, renaming and deleting a drive deal with both files.
    http.post(format!("{base}/d?fork=copy")).send().await.unwrap();
    http.post(format!("{base}/d?rename=renamed")).send().await.unwrap();
    assert!(!log.exists() && !snapshot.exists());
    assert_eq!(http.get(format!("{base}/renamed")).send().await.unwrap().json::<Value>().await.unwrap()["objects"], before["objects"]);
    assert_eq!(http.get(format!("{base}/copy")).send().await.unwrap().json::<Value>().await.unwrap()["objects"], before["objects"]);
    http.put(format!("{base}/renamed/one more.txt")).body("x").send().await.unwrap();
    assert!(dir.path().join("drives/renamed.log").exists());
    http.delete(format!("{base}/renamed")).send().await.unwrap();
    assert!(!dir.path().join("drives/renamed.log").exists() && !dir.path().join("drives/renamed.json").exists());
    assert_eq!(http.get(format!("{base}/copy/moved/file 07.txt")).send().await.unwrap().text().await.unwrap(), "content 7");
}

#[tokio::test]
async fn the_change_feed_reports_what_changed_and_says_when_it_cannot() {
    let s = server(0, 0).await;
    s.put("a.txt", b"1").await;
    s.put("dir/b.txt", b"22").await;
    let listing = s.json("/d").await;
    let (seq, epoch) = (listing["seq"].as_u64().unwrap(), listing["epoch"].as_u64().unwrap());
    assert!(epoch > 0);
    let feed = |since: u64, epoch: u64| {
        let path = format!("/d?feed&since={since}&epoch={epoch}");
        let s = &s;
        async move { s.json(&path).await }
    };

    // Nothing happened yet.
    let none = feed(seq, epoch).await;
    assert_eq!(none["reset"], false);
    assert_eq!(none["objects"].as_array().unwrap().len() + none["deleted"].as_array().unwrap().len(), 0);

    s.put("a.txt", b"one, changed").await;
    s.put("new.txt", b"333").await;
    s.http.delete(format!("{}/d/dir/b.txt", s.base)).send().await.unwrap();
    s.put("gone again.txt", b"x").await;
    s.http.delete(format!("{}/d/gone again.txt", s.base)).send().await.unwrap();
    let changes = feed(seq, epoch).await;
    assert_eq!(changes["reset"], false);
    assert_eq!(changes["seq"], seq + 5);
    let keys: Vec<&str> = changes["objects"].as_array().unwrap().iter().map(|o| o["key"].as_str().unwrap()).collect();
    assert_eq!(keys, ["a.txt", "new.txt"]);
    assert_eq!(changes["objects"][0]["size"], 12);
    assert_eq!(changes["deleted"], serde_json::json!(["dir/b.txt", "gone again.txt"]));

    // A folder rename is its old keys gone and its new keys present.
    s.put("shoot/one.mov", b"1").await;
    s.put("shoot/two.mov", b"2").await;
    let mid = s.json("/d").await["seq"].as_u64().unwrap();
    s.http.post(format!("{}/d/shoot?rename=selects", s.base)).send().await.unwrap();
    let renamed = feed(mid, epoch).await;
    assert_eq!(renamed["deleted"], serde_json::json!(["shoot/one.mov", "shoot/two.mov"]));
    assert_eq!(renamed["objects"].as_array().unwrap().len(), 2);
    // A partial question gets a partial answer.
    assert_eq!(feed(mid + 1, epoch).await["objects"].as_array().unwrap().len(), 0);

    // Questions it cannot answer: another epoch, or a sequence number from the future.
    assert_eq!(feed(seq, epoch + 1).await["reset"], true);
    assert_eq!(feed(seq + 1000, epoch).await["reset"], true);
    // A drive deleted and created again is a new one.
    s.http.delete(format!("{}/d", s.base)).send().await.unwrap();
    s.http.put(format!("{}/d", s.base)).send().await.unwrap();
    let fresh = feed(1, epoch).await;
    assert_eq!(fresh["reset"], true);
    assert_ne!(fresh["epoch"], epoch);
}

#[tokio::test]
async fn a_patch_can_carry_its_shards_in_the_commit_request() {
    let s = server(0, 0).await;
    let (old, new) = (synth::bytes(91, 0, 400_000), synth::bytes(92, 0, 300_000));
    let h_old = s.put_shard(&old).await;
    let h_new = sha256_hex(&new);
    let head = serde_json::to_vec(&serde_json::json!({
        "size": 700_000, "segments": [{ "h": h_old, "n": 400_000 }, { "h": h_new, "n": 300_000 }], "inline": [h_new],
    }))
    .unwrap();
    let send = |shard: &[u8]| {
        let mut body = head.clone();
        body.extend_from_slice(shard);
        s.http.put(format!("{}/d/patched.bin?manifest", s.base)).header("x-ludy-manifest-bytes", head.len()).body(body).send()
    };
    // Bytes that do not hash to the name are refused, and nothing is committed.
    assert_eq!(send(&synth::bytes(93, 0, 300_000)).await.unwrap().status(), StatusCode::BAD_REQUEST);
    assert_eq!(send(&new[..1000]).await.unwrap().status(), StatusCode::BAD_REQUEST);
    assert_eq!(s.get("patched.bin", None).await.status(), StatusCode::NOT_FOUND);

    assert_eq!(send(&new).await.unwrap().status(), StatusCode::CREATED);
    let got = s.get("patched.bin", None).await.bytes().await.unwrap();
    assert!(got[..400_000] == old[..] && got[400_000..] == new[..]);
    assert_eq!(s.shards(), (2, 700_000));
}
