//! The HTTP surface of the simulated cloud, exercised with a plain client.

use std::time::{Duration, Instant};

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
    fn blob_count(&self) -> usize {
        std::fs::read_dir(self._dir.path().join("blobs")).unwrap().count()
    }
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
    assert_eq!(s.blob_count(), 0, "a synthetic object stores nothing");
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

    // Restore = a new version pointing at the old content; no new blob.
    let blobs = s.blob_count();
    let r = s.http.post(format!("{}/d/doc.txt?restore={v1}", s.base)).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(&s.get("doc.txt", None).await.bytes().await.unwrap()[..], b"first");
    assert_eq!(s.json("/d/doc.txt?versions").await["versions"].as_array().unwrap().len(), 3);
    assert_eq!(s.blob_count(), blobs);
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
    // The loser's upload left no orphan blob behind.
    assert_eq!(s.blob_count(), 2);
}

#[tokio::test]
async fn rename_moves_folders_without_copying_and_delete_frees_blobs() {
    let s = server(0, 0).await;
    s.put("a/one.txt", b"1").await;
    s.put("a/sub/two.txt", b"22").await;
    s.put("b.txt", b"333").await;
    let blobs = s.blob_count();
    let r = s.http.post(format!("{}/d/a?rename=moved/here", s.base)).send().await.unwrap();
    assert_eq!(r.json::<Value>().await.unwrap()["moved"], 2);
    let listing = s.json("/d").await;
    let keys: Vec<&str> = listing["objects"].as_array().unwrap().iter().map(|o| o["key"].as_str().unwrap()).collect();
    assert_eq!(keys, ["b.txt", "moved/here/one.txt", "moved/here/sub/two.txt"]);
    assert_eq!(s.blob_count(), blobs);

    // One folder level at a time, the way a file browser asks.
    let level = s.json("/d?prefix=moved/here/&delimiter=/").await;
    assert_eq!(level["objects"].as_array().unwrap().len(), 1);
    assert_eq!(level["prefixes"][0], "moved/here/sub/");

    assert_eq!(s.http.delete(format!("{}/d/moved/", s.base)).send().await.unwrap().status(), StatusCode::NO_CONTENT);
    assert_eq!(s.json("/d").await["objects"].as_array().unwrap().len(), 1);
    assert_eq!(s.blob_count(), 1, "deleted objects release their blobs");
    assert_eq!(s.http.delete(format!("{}/d/moved/", s.base)).send().await.unwrap().status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn forking_a_drive_copies_no_data_and_the_forks_diverge() {
    let s = server(0, 0).await;
    s.put("film.mov", &vec![7u8; 200_000]).await;
    let blobs = s.blob_count();
    assert_eq!(s.http.post(format!("{}/d?fork=d-fork", s.base)).send().await.unwrap().status(), StatusCode::OK);
    assert_eq!(s.blob_count(), blobs, "a fork shares content");
    let drives = s.json("/").await;
    let fork = drives["drives"].as_array().unwrap().iter().find(|d| d["name"] == "d-fork").unwrap();
    assert_eq!(fork["forked_from"], "d");
    assert_eq!(fork["bytes"], 200_000);

    // Changing the fork leaves the original alone, and vice versa.
    s.http.put(format!("{}/d-fork/film.mov", s.base)).body("recut").send().await.unwrap();
    assert_eq!(s.get("film.mov", None).await.bytes().await.unwrap().len(), 200_000);
    // Deleting the original keeps the blob the fork's history still uses.
    s.http.delete(format!("{}/d/film.mov", s.base)).send().await.unwrap();
    let old = s.json("/d-fork/film.mov?versions").await["versions"][0]["id"].as_str().unwrap().to_string();
    let bytes = s.http.get(format!("{}/d-fork/film.mov?version={old}", s.base)).send().await.unwrap().bytes().await.unwrap();
    assert_eq!(bytes.len(), 200_000);
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
