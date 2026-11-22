//! The encryption format against vectors made by another implementation.
//!
//! `tests/vectors/crypt-v1.json` is written by `tests/vectors/crypt-v1.mjs`,
//! which implements the format from its description with Node's built-in
//! crypto and shares no code with this crate. Agreement here means the two
//! read the description the same way; it also pins the format, since a
//! change to what any function produces fails this test.

use ludy_proto::crypt::*;
use ludy_proto::shard::sha256_hex;
use serde_json::Value;

fn unhex(s: &str) -> Vec<u8> {
    assert!(s.len() % 2 == 0, "odd hex length");
    (0..s.len() / 2).map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap()).collect()
}

fn hex_of(v: &Value, field: &str) -> Vec<u8> {
    unhex(v[field].as_str().unwrap_or_else(|| panic!("no string field {field}")))
}

fn key32(v: &Value, field: &str) -> [u8; 32] {
    hex_of(v, field).try_into().expect("32 bytes")
}

fn node_id(v: &Value, field: &str) -> NodeId {
    NodeId(hex_of(v, field).try_into().expect("16 bytes"))
}

/// A `Random` that hands out exactly the bytes the vector names.
fn fixed(bytes: Vec<u8>) -> impl FnMut(&mut [u8]) {
    move |buf: &mut [u8]| buf.copy_from_slice(&bytes)
}

fn pattern(len: usize, salt: usize) -> Vec<u8> {
    (0..len).map(|i| (i * 131 + (i >> 9) + salt) as u8).collect()
}

fn vectors() -> Value {
    let v: Value = serde_json::from_str(include_str!("vectors/crypt-v1.json")).unwrap();
    assert_eq!(v["format"], VERSION as u64);
    v
}

#[test]
fn sealed_shards_match() {
    let v = vectors();
    let shards = v["shards"].as_array().unwrap();
    assert!(shards.len() >= 5);
    for s in shards {
        let len = s["len"].as_u64().unwrap() as usize;
        let plain = pattern(len, s["salt"].as_u64().unwrap() as usize);
        let sealed = seal_shard(&plain, &DedupKey::from_bytes(key32(s, "dedup")));
        assert_eq!(hex(sealed.key.expose()), s["key"], "shard key for {len} bytes");
        assert_eq!(sealed.bytes.len() as u64, s["sealed_len"].as_u64().unwrap());
        assert_eq!(sealed.bytes.len() as u64, sealed_len(len as u64));
        assert_eq!(sealed.id_hex(), s["id"], "name of the sealed shard of {len} bytes");
        assert_eq!(sealed.id_hex(), sha256_hex(&sealed.bytes));
        assert_eq!(hex(&sealed.bytes[..48.min(sealed.bytes.len())]), s["head"]);
        assert_eq!(hex(&sealed.bytes[sealed.bytes.len().saturating_sub(32)..]), s["tail"]);
        // And the other implementation's key opens what this one sealed.
        let key = ShardKey::from_bytes(key32(s, "key"));
        assert_eq!(open_shard(&key, len as u64, &sealed.bytes).unwrap(), plain);
    }
}

#[test]
fn cut_tables_match() {
    let v = vectors();
    for c in v["cut"].as_array().unwrap() {
        let key = match c["kind"].as_str().unwrap() {
            "drive" => CutKey::for_drive(&DedupKey::from_bytes(key32(c, "key"))),
            "file" => CutKey::for_file(&NodeKey::from_bytes(key32(c, "key"))),
            other => panic!("unknown cut key kind {other}"),
        };
        let table = key.table_prefix(256);
        let first: Vec<String> = table[..4].iter().map(|e| format!("{e:016x}")).collect();
        assert_eq!(serde_json::json!(first), c["first"]);
        let bytes: Vec<u8> = table.iter().flat_map(|e| e.to_le_bytes()).collect();
        assert_eq!(sha256_hex(&bytes), c["table_sha256"]);
    }
}

#[test]
fn wrapped_keys_match() {
    let v = vectors();
    let w = &v["wrap"];
    let (parent, child, id) = (NodeKey::from_bytes(key32(w, "parent")), NodeKey::from_bytes(key32(w, "child")), node_id(w, "child_id"));
    let sealed = parent.wrap_child(&id, &child, &mut fixed(hex_of(w, "nonce")));
    assert_eq!(hex(&sealed), w["sealed"]);
    assert_eq!(parent.unwrap_child(&id, &hex_of(w, "sealed")).unwrap(), child);

    let d = &v["dedup_wrap"];
    let (root, dedup, root_id) = (NodeKey::from_bytes(key32(d, "root")), DedupKey::from_bytes(key32(d, "dedup")), node_id(d, "root_id"));
    let sealed = root.wrap_dedup(&root_id, &dedup, &mut fixed(hex_of(d, "nonce")));
    assert_eq!(hex(&sealed), d["sealed"]);
    assert_eq!(root.unwrap_dedup(&root_id, &hex_of(d, "sealed")).unwrap(), dedup);
}

#[test]
fn names_and_name_hashes_match() {
    let v = vectors();
    let names = v["names"].as_array().unwrap();
    // Sealed lengths cover one padding block, a name that exactly fills one, and two blocks.
    let blocks: Vec<usize> = names.iter().map(|n| (hex_of(n, "sealed").len() - 1 - NONCE - TAG) / NAME_PAD).collect();
    assert_eq!(blocks, [1, 1, 1, 2, 1]);
    assert_eq!(names[2]["normalized"].as_str().unwrap().len(), NAME_PAD);
    for n in names {
        let (key, parent, id) = (NodeKey::from_bytes(key32(n, "key")), NodeKey::from_bytes(key32(n, "parent")), node_id(n, "id"));
        let name = n["name"].as_str().unwrap();
        let normalized = n["normalized"].as_str().unwrap();
        assert_eq!(normalize_name(name).unwrap(), normalized);
        let sealed = key.seal_name(&id, name, &mut fixed(hex_of(n, "nonce"))).unwrap();
        assert_eq!(hex(&sealed), n["sealed"], "sealed name {normalized:?}");
        assert_eq!(key.open_name(&id, &hex_of(n, "sealed")).unwrap(), normalized);
        assert_eq!(hex(&parent.name_hash(name).unwrap()), n["hash"], "name hash of {normalized:?}");
    }
    // The first vector is written decomposed; make sure the file kept it that way.
    let first = &names[0];
    assert_ne!(first["name"], first["normalized"]);
}

#[test]
fn manifests_match() {
    let v = vectors();
    let m = &v["manifest"];
    let segments = m["manifest"]["segments"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| {
            if let Some(shard) = s.get("shard") {
                PrivateSegment::Shard { id: key32(shard, "id"), n: shard["n"].as_u64().unwrap() as u32, key: ShardKey::from_bytes(key32(shard, "key")) }
            } else {
                let synth = &s["synth"];
                PrivateSegment::Synth { seed: synth["seed"].as_u64().unwrap(), off: synth["off"].as_u64().unwrap(), n: synth["n"].as_u64().unwrap() }
            }
        })
        .collect();
    let manifest = PrivateManifest { size: m["manifest"]["size"].as_u64().unwrap(), segments };
    manifest.validate().unwrap();
    assert_eq!(hex(&manifest.encode()), m["encoded"]);

    let (key, id) = (NodeKey::from_bytes(key32(m, "key")), node_id(m, "id"));
    let sealed = key.seal_manifest(&id, &manifest, &mut fixed(hex_of(m, "nonce")));
    assert_eq!(hex(&sealed), m["sealed"]);
    assert_eq!(key.open_manifest(&id, &hex_of(m, "sealed")).unwrap(), manifest);
}

/// Not from the other implementation: a pin. The gear table is checked above;
/// this checks that the cutting algorithm around it (the `fastcdc` crate's)
/// still cuts where it did, since a change would silently end dedup against
/// everything already stored.
#[test]
fn keyed_cut_points_stay_where_they_are() {
    let v = vectors();
    let drive = &v["cut"][0];
    assert_eq!(drive["kind"], "drive");
    let key = CutKey::for_drive(&DedupKey::from_bytes(key32(drive, "key")));
    let data = ludy_proto::synth::bytes(31, 0, 32 << 20);
    let lengths: Vec<usize> = cut(&data[..], &key).map(|c| c.unwrap().data.len()).collect();
    assert_eq!(lengths, PINNED_CUTS);
}

const PINNED_CUTS: [usize; 15] = [
    1613485, 3158277, 2419230, 2966710, 3345832, 2115687, 2346013, 2437369, 2170319, 344836, 3084191, 1535302, 364242, 2168338,
    3484601,
];

#[test]
fn version_details_match() {
    let v = vectors();
    let m = &v["meta"];
    let (key, id) = (NodeKey::from_bytes(key32(m, "key")), node_id(m, "id"));
    let meta = FileMeta { mtime_ms: m["mtime_ms"].as_u64().unwrap() };
    assert_eq!(hex(&key.seal_meta(&id, &meta, &mut fixed(hex_of(m, "nonce")))), m["sealed"]);
    assert_eq!(key.open_meta(&id, &hex_of(m, "sealed")).unwrap(), meta);
}
