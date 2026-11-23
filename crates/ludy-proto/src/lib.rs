//! What a Ludy client and the server agree on.
//!
//! * [`tree`]: the node-tree protocol of an end-to-end encrypted drive:
//!   commits of ops on nodes, and the feed of what changed. Every provider
//!   of the server speaks it, and the engine in `ludy-core` is its client
//! * [`crypt`]: the encryption format: sealed shards, keyed cut points, node
//!   keys, sealed names and manifests. Only clients use it; the server holds
//!   no key
//! * [`shard`]: the cutting parameters and the segment types the format
//!   builds on, and SHA-256 names
//! * [`synth`]: the bytes of a synthetic file
//!
//! Nothing here does I/O beyond reading a byte stream to cut it, so every
//! client (the desktop engine, a phone, a tool) and the server can use it.

pub mod crypt;
pub mod shard;
pub mod synth;
pub mod tree;

use std::time::{SystemTime, UNIX_EPOCH};

pub fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

pub fn human(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}
