//! What a Ludy client and the cloud agree on.
//!
//! * [`wire`]: the JSON types of the HTTP API (listings, the change feed,
//!   versions, the request log)
//! * [`shard`]: how file content is cut into shards, and the manifest that
//!   lists them. Two clients share shards only if they cut the same way.
//! * [`synth`]: the bytes of a synthetic object
//!
//! Nothing here does I/O beyond reading a byte stream to cut it, so every
//! client (the desktop engine, a phone, a tool) and the server can use it.

pub mod shard;
pub mod synth;
pub mod wire;

pub use wire::{Changes, Content, DriveInfo, Listing, LogEntry, ObjectInfo, Share, Version};

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
