//! The node-tree protocol: what a client and the cloud exchange about a
//! drive once names and content are end-to-end encrypted.
//!
//! A drive is a tree of **nodes** (files and folders), each with a random
//! 16-byte id chosen by the client that creates it. The cloud stores, per
//! node, three things it cannot read (the sealed name, the node's key wrapped
//! by its parent's key, and for a file the sealed manifest, see
//! [`crate::crypt`]) and what it needs to keep order: the parent, the name
//! hash that makes names unique within a folder, and the sealed shards each
//! version uses.
//!
//! Every change is a [`Commit`]: a list of [`Op`]s applied together or not at
//! all. Each node a commit changes gets the drive's next **sequence number**,
//! so a node's `seq` is unique in the drive and [`Feed`] can hand a client
//! "every node that changed after `since`" in pages.
//!
//! A commit carries an id chosen by the client. Sending the same commit again
//! (the reply was lost, the cloud restarted) returns the first answer and
//! changes nothing, so a client retries until it gets an answer.
//!
//! Ids, hashes and sealed blobs travel as lowercase hex in JSON.

use serde::{Deserialize, Serialize};

use crate::shard::valid_hash;

/// Versions kept per file.
pub const HISTORY: usize = 25;
/// Ops in one commit.
pub const MAX_OPS: usize = 1000;
/// Shards one version may list (about 180 GB of content at the average shard size).
pub const MAX_VERSION_SHARDS: usize = 100_000;
/// Shards and manifests one commit may introduce that the drive does not
/// already hold: each costs the cloud a lookup in storage before it commits.
pub const MAX_NEW_BLOBS: usize = 8000;
/// Bytes of a sealed name: version, nonce, up to 256 padded bytes, tag.
pub const MAX_NAME_BLOB: usize = 1 + 12 + 256 + 16;
/// Bytes of a wrapped key. A node key wrapped by its parent is 61; the root's
/// key is wrapped for people, which is not designed yet, so it gets room.
pub const MAX_KEY_BLOB: usize = 4096;
/// Bytes of a version's sealed details (today: 37, the modification time).
pub const MAX_META_BLOB: usize = 256;
/// Bytes of a sealed manifest.
pub const MAX_MANIFEST: u64 = 32 << 20;
/// Nodes in one page of the feed.
pub const FEED_PAGE: usize = 1000;
pub const MAX_FEED_PAGE: usize = 5000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    File,
    Folder,
}

/// A file's current version, as the cloud knows it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionInfo {
    /// The sequence number the version was committed at.
    pub id: u64,
    /// Plaintext size in bytes, as the writer stated it.
    pub size: u64,
    /// Name (SHA-256) of the sealed manifest.
    pub manifest: String,
    /// Sealed details of the version (its modification time), which a
    /// listing shows without fetching the manifest.
    pub meta: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeInfo {
    pub id: String,
    /// `None` only for the drive's root.
    pub parent: Option<String>,
    pub kind: Kind,
    pub name_hash: String,
    /// Sealed name.
    pub name: String,
    /// The node's key, wrapped by its parent's.
    pub key: String,
    /// When this node last changed. Unique within the drive.
    pub seq: u64,
    /// A deleted node stays in the feed as a tombstone.
    #[serde(default)]
    pub deleted: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<VersionInfo>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "lowercase")]
pub enum Op {
    /// A new file or folder. Refused if the id exists or the name is taken.
    Create { id: String, parent: String, kind: Kind, name_hash: String, name: String, key: String },
    /// A new version of a file. With `base`, refused unless the file's
    /// current version is `base` (0: no version yet). Refused with the list
    /// of what is missing unless the manifest and every shard are in storage.
    Put {
        node: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        base: Option<u64>,
        size: u64,
        manifest: String,
        shards: Vec<String>,
        /// Sealed details of the version; see [`VersionInfo::meta`].
        meta: String,
    },
    /// Rename, move, or both: the node's new parent, name and re-wrapped key.
    Move { node: String, parent: String, name_hash: String, name: String, key: String },
    /// Delete a node and everything under it.
    Delete { node: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Commit {
    /// Chosen at random by the client; makes a retry harmless.
    pub id: String,
    pub ops: Vec<Op>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Committed {
    /// The drive's sequence number after the commit.
    pub seq: u64,
    /// This commit had been applied before; nothing changed this time.
    #[serde(default)]
    pub replayed: bool,
}

/// Nodes changed after `since`, oldest change first. A page can end in the
/// middle of a commit and a child can come before its parent; a client
/// applies pages until `more` is false before it trusts the tree.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Feed {
    pub epoch: u64,
    /// The drive's sequence number when the page was made.
    pub seq: u64,
    /// The question cannot be answered (another drive now has this id, or
    /// `since` is ahead of the drive): start again from 0.
    #[serde(default)]
    pub reset: bool,
    #[serde(default)]
    pub more: bool,
    pub nodes: Vec<NodeInfo>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateDrive {
    /// Id of the root folder.
    pub root: String,
    /// Sealed name of the drive.
    pub name: String,
    /// The root's key, wrapped for whoever may open the drive.
    pub key: String,
    /// The drive's dedup key, wrapped by the root's key.
    pub dedup: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DriveHead {
    pub epoch: u64,
    pub seq: u64,
    pub root: String,
    pub name: String,
    pub key: String,
    pub dedup: String,
}

/// Names of sealed shards and manifests: a question ("which of these do you
/// lack?") and its answer.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Blobs {
    #[serde(default)]
    pub shards: Vec<String>,
    #[serde(default)]
    pub manifests: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionEntry {
    pub id: u64,
    pub size: u64,
    pub manifest: String,
    pub meta: String,
    /// When the cloud committed it.
    pub at_ms: u64,
}

/// Why the cloud said no.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Refusal {
    /// One of the `E_*` codes.
    pub error: String,
    pub detail: String,
    /// For [`E_MISSING`]: what to upload before trying again.
    #[serde(default, skip_serializing_if = "Blobs::is_empty")]
    pub missing: Blobs,
    /// For [`E_CONFLICT`]: the file's current version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current: Option<u64>,
    /// Which op of the commit was refused, counting from 0. None of the
    /// commit was applied, whichever it was.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub op: Option<usize>,
}

pub const E_MALFORMED: &str = "malformed";
pub const E_NO_DRIVE: &str = "no_drive";
pub const E_NO_NODE: &str = "no_node";
/// The id or the name is taken.
pub const E_EXISTS: &str = "exists";
/// `base` is not the file's current version.
pub const E_CONFLICT: &str = "conflict";
/// A shard or manifest the commit names is not in storage.
pub const E_MISSING: &str = "missing";
/// The move would put a folder inside itself.
pub const E_CYCLE: &str = "cycle";
/// The op needs a folder and got a file, or the other way round, or the root.
pub const E_WRONG_KIND: &str = "wrong_kind";
/// The commit id was used before for a different commit.
pub const E_COMMIT_REUSED: &str = "commit_reused";
pub const E_TOO_LARGE: &str = "too_large";

impl Refusal {
    pub fn new(error: &str, detail: impl Into<String>) -> Refusal {
        Refusal { error: error.into(), detail: detail.into(), missing: Blobs::default(), current: None, op: None }
    }

    /// The HTTP status that goes with the code.
    pub fn status(&self) -> u16 {
        match self.error.as_str() {
            E_MALFORMED => 400,
            E_NO_DRIVE | E_NO_NODE => 404,
            E_CONFLICT => 412,
            E_TOO_LARGE => 413,
            _ => 409,
        }
    }
}

impl Blobs {
    pub fn is_empty(&self) -> bool {
        self.shards.is_empty() && self.manifests.is_empty()
    }

    pub fn len(&self) -> usize {
        self.shards.len() + self.manifests.len()
    }
}

/// 32 lowercase hex characters: a node, drive or commit id.
pub fn valid_id(s: &str) -> bool {
    s.len() == 32 && is_hex(s)
}

/// A non-empty sealed blob of at most `max_bytes`, as lowercase hex.
pub fn valid_blob(s: &str, max_bytes: usize) -> bool {
    !s.is_empty() && s.len() % 2 == 0 && s.len() <= max_bytes * 2 && is_hex(s)
}

fn is_hex(s: &str) -> bool {
    s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn check(ok: bool, what: &'static str) -> Result<(), &'static str> {
    if ok {
        Ok(())
    } else {
        Err(what)
    }
}

fn check_placement(parent: &str, name_hash: &str, name: &str, key: &str) -> Result<(), &'static str> {
    check(valid_id(parent), "parent is not an id")?;
    check(valid_hash(name_hash), "name hash is not 32 bytes of hex")?;
    check(valid_blob(name, MAX_NAME_BLOB), "sealed name is empty, too long or not hex")?;
    check(valid_blob(key, MAX_KEY_BLOB), "wrapped key is empty, too long or not hex")
}

impl Op {
    /// Everything that can be checked without the drive's state.
    pub fn validate(&self) -> Result<(), &'static str> {
        match self {
            Op::Create { id, parent, name_hash, name, key, .. } => {
                check(valid_id(id), "node id is not 16 bytes of hex")?;
                check(id != parent, "a node cannot be its own parent")?;
                check_placement(parent, name_hash, name, key)
            }
            Op::Put { node, manifest, shards, meta, .. } => {
                check(valid_id(node), "node id is not 16 bytes of hex")?;
                check(valid_hash(manifest), "manifest name is not a sha-256")?;
                check(valid_blob(meta, MAX_META_BLOB), "sealed version details are empty, too long or not hex")?;
                check(shards.len() <= MAX_VERSION_SHARDS, "too many shards in one version")?;
                check(shards.iter().all(|s| valid_hash(s)), "a shard name is not a sha-256")
            }
            Op::Move { node, parent, name_hash, name, key } => {
                check(valid_id(node), "node id is not 16 bytes of hex")?;
                check(node != parent, "a node cannot be its own parent")?;
                check_placement(parent, name_hash, name, key)
            }
            Op::Delete { node } => check(valid_id(node), "node id is not 16 bytes of hex"),
        }
    }
}

impl Commit {
    pub fn validate(&self) -> Result<(), &'static str> {
        check(valid_id(&self.id), "commit id is not 16 bytes of hex")?;
        check(!self.ops.is_empty(), "a commit needs at least one op")?;
        check(self.ops.len() <= MAX_OPS, "too many ops in one commit")?;
        self.ops.iter().try_for_each(Op::validate)
    }

    /// The shards and manifests this commit's versions name, each once.
    pub fn blobs(&self) -> Blobs {
        let (mut shards, mut manifests) = (std::collections::BTreeSet::new(), std::collections::BTreeSet::new());
        for op in &self.ops {
            if let Op::Put { manifest, shards: list, .. } = op {
                manifests.insert(manifest.clone());
                shards.extend(list.iter().cloned());
            }
        }
        Blobs { shards: shards.into_iter().collect(), manifests: manifests.into_iter().collect() }
    }
}

impl CreateDrive {
    pub fn validate(&self) -> Result<(), &'static str> {
        check(valid_id(&self.root), "root id is not 16 bytes of hex")?;
        check(valid_blob(&self.name, MAX_NAME_BLOB), "sealed name is empty, too long or not hex")?;
        check(valid_blob(&self.key, MAX_KEY_BLOB), "wrapped key is empty, too long or not hex")?;
        check(valid_blob(&self.dedup, MAX_KEY_BLOB), "wrapped dedup key is empty, too long or not hex")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u8) -> String {
        format!("{n:02x}").repeat(16)
    }

    fn hash(n: u8) -> String {
        format!("{n:02x}").repeat(32)
    }

    fn create() -> Op {
        Op::Create { id: id(2), parent: id(1), kind: Kind::File, name_hash: hash(3), name: "01ab".into(), key: "02cd".into() }
    }

    #[test]
    fn ops_round_trip_as_tagged_json() {
        let commit = Commit {
            id: id(9),
            ops: vec![
                create(),
                Op::Put { node: id(2), base: Some(0), size: 5, manifest: hash(4), shards: vec![hash(5), hash(6)], meta: "0a".into() },
                Op::Move { node: id(2), parent: id(7), name_hash: hash(8), name: "aa".into(), key: "bb".into() },
                Op::Delete { node: id(2) },
            ],
        };
        commit.validate().unwrap();
        let json = serde_json::to_string(&commit).unwrap();
        assert!(json.contains(r#""op":"create""#) && json.contains(r#""kind":"file""#) && json.contains(r#""op":"delete""#));
        assert_eq!(serde_json::from_str::<Commit>(&json).unwrap(), commit);
        // `base` left out means no precondition.
        let put: Op = serde_json::from_str(&format!(r#"{{"op":"put","node":"{}","size":1,"manifest":"{}","shards":[],"meta":"0a"}}"#, id(2), hash(4))).unwrap();
        assert!(matches!(put, Op::Put { base: None, .. }));
        assert_eq!(commit.blobs(), Blobs { shards: vec![hash(5), hash(6)], manifests: vec![hash(4)] });
    }

    #[test]
    fn malformed_commits_are_caught_before_they_reach_a_drive() {
        let ok = Commit { id: id(9), ops: vec![create()] };
        assert!(ok.validate().is_ok());
        assert!(Commit { id: "nope".into(), ..ok.clone() }.validate().is_err());
        assert!(Commit { id: id(0xab).to_uppercase(), ..ok.clone() }.validate().is_err());
        assert!(Commit { ops: vec![], ..ok.clone() }.validate().is_err());
        assert!(Commit { ops: vec![create(); MAX_OPS + 1], ..ok.clone() }.validate().is_err());

        let bad = |op: Op| assert!(op.validate().is_err(), "{op:?} should be refused");
        bad(Op::Create { id: id(1), parent: id(1), kind: Kind::Folder, name_hash: hash(3), name: "01".into(), key: "02".into() });
        bad(Op::Create { id: id(2), parent: id(1), kind: Kind::File, name_hash: "zz".into(), name: "01".into(), key: "02".into() });
        bad(Op::Create { id: id(2), parent: id(1), kind: Kind::File, name_hash: hash(3), name: "".into(), key: "02".into() });
        bad(Op::Create { id: id(2), parent: id(1), kind: Kind::File, name_hash: hash(3), name: "0".into(), key: "02".into() });
        bad(Op::Create { id: id(2), parent: id(1), kind: Kind::File, name_hash: hash(3), name: "ab".repeat(MAX_NAME_BLOB + 1), key: "02".into() });
        bad(Op::Create { id: id(2), parent: id(1), kind: Kind::File, name_hash: hash(3), name: "01".into(), key: "../x".into() });
        bad(Op::Put { node: id(2), base: None, size: 1, manifest: "m".into(), shards: vec![], meta: "0a".into() });
        bad(Op::Put { node: id(2), base: None, size: 1, manifest: hash(4), shards: vec!["../../etc".into()], meta: "0a".into() });
        bad(Op::Put { node: id(2), base: None, size: 1, manifest: hash(4), shards: vec![], meta: "".into() });
        bad(Op::Move { node: id(2), parent: id(2), name_hash: hash(3), name: "01".into(), key: "02".into() });
        bad(Op::Delete { node: "".into() });
    }

    #[test]
    fn refusals_carry_a_status() {
        assert_eq!(Refusal::new(E_CONFLICT, "x").status(), 412);
        assert_eq!(Refusal::new(E_MISSING, "x").status(), 409);
        assert_eq!(Refusal::new(E_NO_NODE, "x").status(), 404);
        assert_eq!(Refusal::new(E_MALFORMED, "x").status(), 400);
        let json = serde_json::to_string(&Refusal::new(E_EXISTS, "name taken")).unwrap();
        assert!(!json.contains("missing") && !json.contains("current") && !json.contains("\"op\""));
    }
}
