//! The JSON a client and the cloud exchange.

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Content {
    /// Shards listed in `<data>/manifests/<id>.json`.
    Manifest { id: String },
    /// Bytes generated from the offset; nothing on disk.
    Synthetic { seed: u64 },
    /// Whole-object bytes in `<data>/blobs/<id>`, written before shards existed.
    Blob { id: String },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Version {
    pub id: String,
    pub size: u64,
    pub mtime_ms: u64,
    /// Drive sequence number of the commit that created this version.
    pub seq: u64,
    pub content: Content,
    /// The version this one replaced, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Share {
    pub email: String,
    pub role: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DriveInfo {
    pub name: String,
    pub created_ms: u64,
    pub seq: u64,
    pub objects: u64,
    /// Logical size of all current versions.
    pub bytes: u64,
    /// The part of `bytes` that is synthetic and takes no server disk.
    pub synthetic_bytes: u64,
    pub forked_from: Option<String>,
    pub shares: Vec<Share>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ObjectInfo {
    pub key: String,
    pub size: u64,
    pub mtime_ms: u64,
    pub version: String,
    pub synthetic: bool,
    pub versions: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Listing {
    pub drive: String,
    pub seq: u64,
    /// See [`Changes::epoch`]; pass it back when asking for changes.
    #[serde(default)]
    pub epoch: u64,
    pub objects: Vec<ObjectInfo>,
    pub prefixes: Vec<String>,
}

/// What changed in a drive since a client last looked.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Changes {
    pub drive: String,
    pub seq: u64,
    pub epoch: u64,
    /// The question cannot be answered (the server restarted, the drive is
    /// a new one, or `since` is too old): fetch the full listing instead.
    pub reset: bool,
    /// Objects added or changed, in their current state.
    pub objects: Vec<ObjectInfo>,
    /// Keys that no longer exist.
    pub deleted: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LogEntry {
    pub n: u64,
    pub unix_ms: u64,
    pub method: String,
    pub drive: String,
    pub key: String,
    /// Byte range served or received, when the request carried a body.
    pub range: Option<(u64, u64)>,
    pub status: u16,
    pub bytes: u64,
    pub ms: u64,
}
