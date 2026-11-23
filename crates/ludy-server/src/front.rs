//! What the HTTP side in front of the drives shares between providers.
//!
//! Sealed shards and manifests live in the blob store under
//! `d/<drive>/s/<sha256>` and `d/<drive>/m/<sha256>`. The front stores and
//! serves them itself: their bytes never pass through a drive's instance.
//! Everything else about a drive goes to that drive.

use ludy_proto::crypt;
use ludy_proto::shard::valid_hash;
use ludy_proto::tree::{valid_id, Refusal, E_MALFORMED, E_TOO_LARGE, MAX_MANIFEST};

/// Where a drive's blob is in the store. `blob` is `s/<sha256>` or `m/<sha256>`.
pub fn blob_key(drive: &str, blob: &str) -> String {
    format!("d/{drive}/{blob}")
}

/// A sealed shard (`s`) or manifest (`m`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlobKind {
    Shard,
    Manifest,
}

impl BlobKind {
    pub fn letter(self) -> &'static str {
        match self {
            BlobKind::Shard => "s",
            BlobKind::Manifest => "m",
        }
    }

    /// The most bytes an upload of this kind may have.
    pub fn max_bytes(self) -> u64 {
        match self {
            BlobKind::Shard => crypt::MAX_SEALED_SHARD,
            BlobKind::Manifest => MAX_MANIFEST,
        }
    }
}

/// Where a request goes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Route {
    /// `GET /`: the provider says its name.
    Hello,
    /// `PUT` or `GET` of a shard or manifest: `key` is where it is in the store.
    Blob { put: bool, kind: BlobKind, drive: String, hash: String, key: String },
    /// `GET /_dev/blobs/:drive`: what the store holds for a drive, as `s/<sha256>` and `m/<sha256>`.
    DevBlobs { prefix: String },
    /// `DELETE /_dev/blobs/:drive/:kind/:hash`: lose a blob behind the drive's back.
    DevBlobDelete { key: String },
    /// `GET /_dev/crypt`: [`crypt_selftest`].
    DevCrypt,
    /// Everything else under `/drives/:drive` belongs to that drive.
    Drive { drive: String },
    Refused(Refusal),
}

fn no_route() -> Route {
    Route::Refused(Refusal::new(E_MALFORMED, "no such route"))
}

/// Decide where a request goes. `dev` is the `LUDY_DEV` setting: without it
/// the `_dev` routes do not exist.
pub fn route(method: &str, path: &str, dev: bool) -> Route {
    let parts: Vec<&str> = path.trim_matches('/').split('/').filter(|p| !p.is_empty()).collect();
    match (method, parts.as_slice()) {
        ("GET", []) => Route::Hello,
        ("PUT" | "GET", ["drives", drive, kind @ ("shards" | "manifests"), hash]) => {
            if !(valid_id(drive) && valid_hash(hash)) {
                return Route::Refused(Refusal::new(E_MALFORMED, "not a drive id and a sha-256 name"));
            }
            let kind = if *kind == "shards" { BlobKind::Shard } else { BlobKind::Manifest };
            let key = blob_key(drive, &format!("{}/{hash}", kind.letter()));
            Route::Blob { put: method == "PUT", kind, drive: drive.to_string(), hash: hash.to_string(), key }
        }
        ("GET", ["_dev", "blobs", drive]) if dev && valid_id(drive) => Route::DevBlobs { prefix: blob_key(drive, "") },
        ("DELETE", ["_dev", "blobs", drive, kind @ ("s" | "m"), hash]) if dev && valid_id(drive) && valid_hash(hash) => {
            Route::DevBlobDelete { key: blob_key(drive, &format!("{kind}/{hash}")) }
        }
        ("GET", ["_dev", "crypt"]) if dev => Route::DevCrypt,
        (_, ["drives", drive, ..]) => {
            if valid_id(drive) {
                Route::Drive { drive: drive.to_string() }
            } else {
                Route::Refused(Refusal::new(E_MALFORMED, "drive id is not 16 bytes of hex"))
            }
        }
        _ => no_route(),
    }
}

/// The refusal for an upload larger than its kind allows.
pub fn too_large() -> Refusal {
    Refusal::new(E_TOO_LARGE, "larger than a sealed shard or manifest can be")
}

/// The refusal for a blob that is not in the store.
pub fn no_blob() -> Refusal {
    Refusal::new("no_blob", "not in storage")
}

/// `bytes=a-b`, both ends given.
pub fn parse_range(header: &str) -> Option<(u64, u64)> {
    let (a, b) = header.strip_prefix("bytes=")?.split_once('-')?;
    let (a, b) = (a.parse().ok()?, b.parse().ok()?);
    (a <= b).then_some((a, b))
}

pub fn unhex(s: &str) -> Vec<u8> {
    (0..s.len() / 2).filter_map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).ok()).collect()
}

/// Run the encryption format where the server runs, on the 200,000-byte
/// input of `ludy-proto/tests/vectors/crypt-v1.json`, and answer the sealed
/// shard's name. The server never holds a real key; this shows the code a
/// browser client will need runs on this target.
pub fn crypt_selftest() -> String {
    // The dedup key of that vector file. A test value, not a secret.
    const DEDUP: &str = "90b1a9a9def6e53d3ac9c640d9a42a30f6ff26bb639e65c21f895b218b51196b";
    let Ok(key) = <[u8; 32]>::try_from(&unhex(DEDUP)[..]) else { return "bad key".into() };
    let plain: Vec<u8> = (0..200_000usize).map(|i| (i * 131 + (i >> 9) + 4) as u8).collect();
    let sealed = crypt::seal_shard(&plain, &crypt::DedupKey::from_bytes(key));
    match crypt::open_shard(&sealed.key, plain.len() as u64, &sealed.bytes) {
        Ok(opened) if opened == plain => sealed.id_hex(),
        _ => "sealed shard did not open".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DRIVE: &str = "000102030405060708090a0b0c0d0e0f";

    fn hash() -> String {
        "ab".repeat(32)
    }

    #[test]
    fn blobs_go_to_the_store_and_the_rest_to_the_drive() {
        let h = hash();
        assert_eq!(route("GET", "/", false), Route::Hello);
        assert!(matches!(route("PUT", &format!("/drives/{DRIVE}/shards/{h}"), false), Route::Blob { put: true, kind: BlobKind::Shard, key, .. } if key == format!("d/{DRIVE}/s/{h}")));
        assert!(matches!(route("GET", &format!("/drives/{DRIVE}/manifests/{h}"), false), Route::Blob { put: false, kind: BlobKind::Manifest, .. }));
        for (method, rest) in [("PUT", ""), ("GET", ""), ("POST", "/commit"), ("GET", "/feed"), ("GET", "/nodes/x/versions"), ("POST", &format!("/shards/{h}")[..])] {
            assert_eq!(route(method, &format!("/drives/{DRIVE}{rest}"), false), Route::Drive { drive: DRIVE.into() }, "{method} {rest}");
        }
    }

    #[test]
    fn bad_names_never_reach_storage_or_a_drive() {
        let h = hash();
        for path in [format!("/drives/nope/shards/{h}"), format!("/drives/{DRIVE}/shards/..%2f..%2fetc"), "/drives/..".into(), format!("/drives/{}", DRIVE.to_uppercase())] {
            assert!(matches!(route("GET", &path, true), Route::Refused(_)), "{path}");
        }
        // A longer path is the drive's to refuse; it is never a key in the store.
        assert_eq!(route("GET", &format!("/drives/{DRIVE}/shards/../../etc"), true), Route::Drive { drive: DRIVE.into() });
        assert!(matches!(route("GET", "/elsewhere", true), Route::Refused(_)));
    }

    #[test]
    fn dev_routes_exist_only_in_dev() {
        let h = hash();
        for (method, path) in [("GET", format!("/_dev/blobs/{DRIVE}")), ("DELETE", format!("/_dev/blobs/{DRIVE}/s/{h}")), ("GET", "/_dev/crypt".into())] {
            assert!(matches!(route(method, &path, false), Route::Refused(_)), "{path}");
            assert!(!matches!(route(method, &path, true), Route::Refused(_)), "{path}");
        }
    }

    #[test]
    fn ranges() {
        assert_eq!(parse_range("bytes=0-0"), Some((0, 0)));
        assert_eq!(parse_range("bytes=5-9"), Some((5, 9)));
        assert_eq!(parse_range("bytes=9-5"), None);
        assert_eq!(parse_range("bytes=5-"), None);
        assert_eq!(parse_range("items=1-2"), None);
    }
}
