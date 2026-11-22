//! End-to-end encryption, format version 1.
//!
//! The cloud stores what this module produces and can read none of it: not
//! file content, not names. Everything here is pure computation. Randomness
//! comes in through [`Random`], so the crate needs no system RNG and the same
//! inputs always give the same bytes (see `tests/vectors`).
//!
//! # Keys
//!
//! Every key is 32 bytes. A key is never used directly for two purposes; a
//! purpose key is `HKDF-SHA256(ikm = key, salt = none, info = label)`.
//!
//! * A **node key** belongs to one file or folder and is random. A folder's
//!   key wraps the keys of its children, so whoever holds a folder's key can
//!   read everything below it and nothing above or beside it.
//! * A **dedup key** belongs to one drive and is random. It makes shard keys
//!   and cut points a function of the content, so the same bytes written
//!   twice are stored once. Whoever holds it can test whether content they
//!   already know is somewhere in the drive; it goes to people with the whole
//!   drive, not to someone sharing one folder (they seal with fresh keys).
//! * A **shard key** encrypts one shard and only ever that shard's bytes.
//!
//! # Shards
//!
//! A shard's plaintext is split into frames of [`FRAME`] bytes (the last one
//! shorter). Frame `i` is ChaCha20-Poly1305 under the shard key with the
//! nonce `i` as 12 little-endian bytes and no associated data; its 16-byte tag
//! follows it. A sealed shard is the frames end to end, so any plaintext
//! range maps to a ciphertext range ([`span`]) and a reader fetches only the
//! frames it needs.
//!
//! The shard key is `HMAC-SHA256(subkey(dedup, "ludy/v1/shard-key"), plaintext)`,
//! or random when the writer has no dedup key. Counter nonces are safe only
//! because a shard key never seals two different plaintexts; that is why
//! sealing takes a dedup key or a [`Random`], never a shard key.
//!
//! A sealed shard is named by the SHA-256 of its sealed bytes, which the
//! cloud can check without being able to read them. A writer therefore has to
//! seal a shard to learn its name, even one the cloud already has.
//!
//! # Cut points
//!
//! FastCDC with a gear table derived from a key ([`CutKey`]), so that the
//! lengths of a file's shards, which the cloud sees, are not a function of
//! the content alone. This makes recognising a known file by its shard
//! lengths harder. It is a mitigation, not a guarantee.
//!
//! # Sealed boxes: wrapped keys, names, manifests
//!
//! `0x01 || nonce (12 random bytes) || ChaCha20-Poly1305(subkey(key, label),
//! nonce, plaintext, aad = label || node id)`.
//!
//! The node id in the associated data means a box moved to another node does
//! not open. With random 96-bit nonces a purpose key must seal fewer than
//! 2^32 boxes; the keys here seal one per child added to a folder, per rename
//! of a node, or per version of a file.
//!
//! | Box | Key | Label | Plaintext |
//! |---|---|---|---|
//! | Wrapped child key | parent's node key | `ludy/v1/wrap` | the child's node key |
//! | Name | the node's own key | `ludy/v1/name` | NFC name, zero-padded to 32 bytes |
//! | Manifest | the file's node key | `ludy/v1/manifest` | [`PrivateManifest::encode`] |
//! | Version details | the file's node key | `ludy/v1/meta` | modification time, ms since 1970, 8 bytes little-endian |
//! | Dedup key | the drive root's key | `ludy/v1/dedup` | the dedup key |
//!
//! A folder cannot hold two children with the same name, and the cloud
//! enforces that on the **name hash**:
//! `HMAC-SHA256(subkey(parent key, "ludy/v1/name-hash"), NFC name)`.
//! Names are compared after Unicode NFC normalisation and are case-sensitive.
//! That rule is part of every stored hash and cannot change later.
//!
//! # What this does not do
//!
//! Wrapping a key for a person (public keys), paging very large manifests,
//! and protecting against a cloud that serves an older, genuine version of a
//! file.

use std::fmt;
use std::io::{self, Read};
use std::ops::Range;

use chacha20poly1305::aead::{AeadInPlace, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce, Tag};
use fastcdc::v2020::{cut_gear, logarithm2, Normalization, MASKS};
use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use unicode_normalization::UnicodeNormalization;
use zeroize::Zeroize;

use crate::shard::{AVG_SHARD, MAX_SHARD, MIN_SHARD};

/// First byte of every sealed box.
pub const VERSION: u8 = 1;
/// Plaintext bytes per frame of a sealed shard.
pub const FRAME: usize = 64 * 1024;
/// Bytes of authentication tag after each frame.
pub const TAG: usize = 16;
/// Bytes of nonce in a sealed box.
pub const NONCE: usize = 12;
/// The largest sealed shard: [`MAX_SHARD`] bytes of plaintext and its tags.
pub const MAX_SEALED_SHARD: u64 = sealed_len(MAX_SHARD as u64);
/// Names are padded to a multiple of this before sealing, to hide their exact length.
pub const NAME_PAD: usize = 32;
/// The longest name, in bytes of UTF-8 after normalisation.
pub const MAX_NAME: usize = 255;

const L_SHARD_KEY: &[u8] = b"ludy/v1/shard-key";
const L_CUT: &[u8] = b"ludy/v1/cut";
const L_CUT_FILE: &[u8] = b"ludy/v1/cut-file";
const L_WRAP: &[u8] = b"ludy/v1/wrap";
const L_NAME: &[u8] = b"ludy/v1/name";
const L_NAME_HASH: &[u8] = b"ludy/v1/name-hash";
const L_MANIFEST: &[u8] = b"ludy/v1/manifest";
const L_DEDUP: &[u8] = b"ludy/v1/dedup";
const L_META: &[u8] = b"ludy/v1/meta";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CryptError {
    /// A sealed box of a format version this code does not know.
    Version(u8),
    /// Fewer bytes than the format requires.
    Truncated,
    /// The bytes do not authenticate: wrong key, wrong node, or changed in storage.
    Tampered,
    /// Authentic bytes that do not decode, or an input the format refuses.
    Malformed(&'static str),
}

impl fmt::Display for CryptError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CryptError::Version(v) => write!(f, "unknown encryption format version {v}"),
            CryptError::Truncated => write!(f, "sealed data is cut short"),
            CryptError::Tampered => write!(f, "sealed data does not authenticate"),
            CryptError::Malformed(what) => write!(f, "malformed: {what}"),
        }
    }
}

impl std::error::Error for CryptError {}

pub type Result<T> = std::result::Result<T, CryptError>;

/// Where random bytes come from. The caller supplies the system's generator;
/// tests supply fixed bytes.
pub trait Random {
    fn fill(&mut self, buf: &mut [u8]);
}

impl<F: FnMut(&mut [u8])> Random for F {
    fn fill(&mut self, buf: &mut [u8]) {
        self(buf)
    }
}

macro_rules! secret_key {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Clone, PartialEq, Eq)]
        pub struct $name([u8; 32]);

        impl $name {
            pub fn from_bytes(bytes: [u8; 32]) -> Self {
                $name(bytes)
            }

            /// The raw key. Only for storing it sealed, never for logs.
            pub fn expose(&self) -> &[u8; 32] {
                &self.0
            }
        }

        impl Drop for $name {
            fn drop(&mut self) {
                self.0.zeroize();
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, concat!(stringify!($name), "(..)"))
            }
        }
    };
}

secret_key!(
    /// The key of one file or folder.
    NodeKey
);
secret_key!(
    /// A drive's secret for content-derived shard keys and cut points.
    DedupKey
);
secret_key!(
    /// The key of one sealed shard. Carried in the file's manifest.
    ShardKey
);

/// A node's id: 16 bytes chosen at random by whoever creates the node.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NodeId(pub [u8; 16]);

impl NodeId {
    pub fn random(rng: &mut impl Random) -> NodeId {
        let mut id = [0u8; 16];
        rng.fill(&mut id);
        NodeId(id)
    }
}

fn random_key(rng: &mut impl Random) -> [u8; 32] {
    let mut key = [0u8; 32];
    rng.fill(&mut key);
    key
}

/// `HKDF-SHA256(ikm = key, salt = none, info = label)`, 32 bytes.
fn subkey(key: &[u8; 32], label: &[u8]) -> [u8; 32] {
    let mut out = [0u8; 32];
    Hkdf::<Sha256>::new(None, key).expand(label, &mut out).expect("32 bytes is a valid HKDF length");
    out
}

fn hmac(key: &[u8; 32], message: &[u8]) -> [u8; 32] {
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(key).expect("HMAC takes a key of any length");
    mac.update(message);
    mac.finalize().into_bytes().into()
}

pub fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(DIGITS[(b >> 4) as usize] as char);
        out.push(DIGITS[(b & 15) as usize] as char);
    }
    out
}

// ------------------------------------------------------------------ shards

/// Bytes a shard of `n` plaintext bytes takes once sealed.
pub const fn sealed_len(n: u64) -> u64 {
    n + TAG as u64 * n.div_ceil(FRAME as u64)
}

fn frame_nonce(index: u64) -> Nonce {
    let mut nonce = [0u8; NONCE];
    nonce[..8].copy_from_slice(&index.to_le_bytes());
    nonce.into()
}

/// A shard ready to upload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SealedShard {
    /// SHA-256 of `bytes`: the shard's name in the cloud.
    pub id: [u8; 32],
    /// Goes into the file's manifest; a reader needs it to open the shard.
    pub key: ShardKey,
    /// Plaintext length.
    pub n: u32,
    pub bytes: Vec<u8>,
}

impl SealedShard {
    pub fn id_hex(&self) -> String {
        hex(&self.id)
    }
}

/// Seal a shard with a key derived from its content: the same plaintext under
/// the same dedup key always gives the same bytes and the same name.
pub fn seal_shard(plaintext: &[u8], dedup: &DedupKey) -> SealedShard {
    let key = ShardKey(hmac(&subkey(&dedup.0, L_SHARD_KEY), plaintext));
    seal_with(plaintext, key)
}

/// Seal a shard with a random key, for a writer without the drive's dedup
/// key. The same plaintext sealed twice gives unrelated bytes.
pub fn seal_shard_fresh(plaintext: &[u8], rng: &mut impl Random) -> SealedShard {
    seal_with(plaintext, ShardKey(random_key(rng)))
}

/// Private on purpose: `key` must not have sealed any other plaintext.
fn seal_with(plaintext: &[u8], key: ShardKey) -> SealedShard {
    assert!(plaintext.len() <= MAX_SHARD as usize, "a shard is at most MAX_SHARD bytes");
    let cipher = ChaCha20Poly1305::new(Key::from_slice(&key.0));
    let mut bytes = Vec::with_capacity(sealed_len(plaintext.len() as u64) as usize);
    for (i, frame) in plaintext.chunks(FRAME).enumerate() {
        let start = bytes.len();
        bytes.extend_from_slice(frame);
        let tag = cipher
            .encrypt_in_place_detached(&frame_nonce(i as u64), b"", &mut bytes[start..])
            .expect("a frame is far below the cipher's length limit");
        bytes.extend_from_slice(&tag);
    }
    let id = Sha256::digest(&bytes).into();
    SealedShard { id, key, n: plaintext.len() as u32, bytes }
}

/// The part of a sealed shard that holds a plaintext range.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    /// Index of the first frame in `bytes`.
    pub first_frame: u64,
    /// The range of the sealed shard to fetch: whole frames.
    pub bytes: Range<u64>,
    /// Plaintext bytes to drop from the front of the opened frames.
    pub skip: usize,
    /// Plaintext bytes to keep after that.
    pub take: usize,
}

/// Which sealed bytes hold plaintext `off..off + len` of a shard of `n`
/// plaintext bytes. The range is clipped to the shard; `None` if nothing of
/// it lies inside.
pub fn span(off: u64, len: u64, n: u64) -> Option<Span> {
    let end = off.saturating_add(len).min(n);
    if off >= end {
        return None;
    }
    let (frame, sealed_frame) = (FRAME as u64, (FRAME + TAG) as u64);
    let first = off / frame;
    let last = (end - 1) / frame;
    Some(Span {
        first_frame: first,
        bytes: first * sealed_frame..((last + 1) * sealed_frame).min(sealed_len(n)),
        skip: (off - first * frame) as usize,
        take: (end - off) as usize,
    })
}

/// Open consecutive whole frames of a shard of `n` plaintext bytes, starting
/// at frame `first_frame`. `sealed` must be exactly those frames: any
/// change, a frame out of place, a missing tail or a frame of another shard
/// is an error.
pub fn open_frames(key: &ShardKey, n: u64, first_frame: u64, sealed: &[u8]) -> Result<Vec<u8>> {
    let cipher = ChaCha20Poly1305::new(Key::from_slice(&key.0));
    let total_frames = n.div_ceil(FRAME as u64);
    let mut out = Vec::with_capacity(sealed.len());
    let (mut pos, mut index) = (0usize, first_frame);
    while pos < sealed.len() {
        if index >= total_frames {
            return Err(CryptError::Malformed("more frames than the shard has"));
        }
        let plain = (n - index * FRAME as u64).min(FRAME as u64) as usize;
        let Some(frame) = sealed.get(pos..pos + plain + TAG) else {
            return Err(CryptError::Truncated);
        };
        let start = out.len();
        out.extend_from_slice(&frame[..plain]);
        cipher
            .decrypt_in_place_detached(&frame_nonce(index), b"", &mut out[start..], Tag::from_slice(&frame[plain..]))
            .map_err(|_| CryptError::Tampered)?;
        pos += plain + TAG;
        index += 1;
    }
    Ok(out)
}

/// Open a whole sealed shard of `n` plaintext bytes.
pub fn open_shard(key: &ShardKey, n: u64, sealed: &[u8]) -> Result<Vec<u8>> {
    if (sealed.len() as u64) < sealed_len(n) {
        return Err(CryptError::Truncated);
    }
    if sealed.len() as u64 > sealed_len(n) {
        return Err(CryptError::Malformed("sealed shard is longer than its length says"));
    }
    open_frames(key, n, 0, sealed)
}

/// Open the sealed bytes fetched for `span` and return exactly the plaintext
/// range that was asked for.
pub fn open_span(key: &ShardKey, n: u64, span: &Span, sealed: &[u8]) -> Result<Vec<u8>> {
    if (sealed.len() as u64) < span.bytes.end - span.bytes.start {
        return Err(CryptError::Truncated);
    }
    let mut plain = open_frames(key, n, span.first_frame, sealed)?;
    if plain.len() < span.skip + span.take {
        return Err(CryptError::Truncated);
    }
    plain.truncate(span.skip + span.take);
    plain.drain(..span.skip);
    Ok(plain)
}

// -------------------------------------------------------------- cut points

/// A FastCDC gear table derived from a key. Two writers cut the same way,
/// and so share shards, only with the same `CutKey`. A file must be cut with
/// the same one for all of its versions, or an edit re-cuts the whole file.
#[derive(Clone)]
pub struct CutKey {
    gear: Box<[u64; 256]>,
    gear_ls: Box<[u64; 256]>,
}

impl CutKey {
    /// For a writer with the drive's dedup key.
    pub fn for_drive(dedup: &DedupKey) -> CutKey {
        CutKey::derive(&dedup.0, L_CUT)
    }

    /// For a writer without it: cut each file by that file's own key.
    pub fn for_file(file: &NodeKey) -> CutKey {
        CutKey::derive(&file.0, L_CUT_FILE)
    }

    /// The table is `HKDF-SHA256(ikm, info = label)` expanded to 2048 bytes,
    /// read as 256 little-endian `u64`s. The label is `ludy/v1/cut` for a
    /// drive and `ludy/v1/cut-file` for a file.
    fn derive(ikm: &[u8; 32], label: &[u8]) -> CutKey {
        let mut bytes = [0u8; 256 * 8];
        Hkdf::<Sha256>::new(None, ikm).expand(label, &mut bytes).expect("2048 bytes is a valid HKDF length");
        let mut gear = Box::new([0u64; 256]);
        for (entry, chunk) in gear.iter_mut().zip(bytes.chunks_exact(8)) {
            *entry = u64::from_le_bytes(chunk.try_into().unwrap());
        }
        bytes.zeroize();
        CutKey::from_table(gear)
    }

    fn from_table(gear: Box<[u64; 256]>) -> CutKey {
        let mut gear_ls = Box::new([0u64; 256]);
        for (shifted, entry) in gear_ls.iter_mut().zip(gear.iter()) {
            *shifted = entry << 1;
        }
        CutKey { gear, gear_ls }
    }

    /// The first entries of the table, for test vectors.
    #[doc(hidden)]
    pub fn table_prefix(&self, n: usize) -> Vec<u64> {
        self.gear[..n].to_vec()
    }
}

impl Drop for CutKey {
    fn drop(&mut self) {
        self.gear.zeroize();
        self.gear_ls.zeroize();
    }
}

impl fmt::Debug for CutKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "CutKey(..)")
    }
}

/// One plaintext shard cut from a stream, not yet sealed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cut {
    /// Offset within the stream.
    pub offset: u64,
    pub data: Vec<u8>,
}

/// Cuts a stream into shards of [`MIN_SHARD`] to [`MAX_SHARD`] bytes at
/// points that depend on the content and the [`CutKey`]. Memory use is
/// bounded by `MAX_SHARD`.
pub struct Cutter<R> {
    source: R,
    key: CutKey,
    buffer: Vec<u8>,
    filled: usize,
    eof: bool,
    offset: u64,
    mask_s: u64,
    mask_l: u64,
}

pub fn cut<R: Read>(source: R, key: &CutKey) -> Cutter<R> {
    // The same parameters as `shard::chunks`: normalisation level 1.
    let bits = logarithm2(AVG_SHARD);
    let level = Normalization::Level1.bits();
    Cutter {
        source,
        key: key.clone(),
        buffer: vec![0; MAX_SHARD as usize],
        filled: 0,
        eof: false,
        offset: 0,
        mask_s: MASKS[(bits + level) as usize],
        mask_l: MASKS[(bits - level) as usize],
    }
}

impl<R: Read> Cutter<R> {
    fn fill(&mut self) -> io::Result<()> {
        while !self.eof && self.filled < self.buffer.len() {
            match self.source.read(&mut self.buffer[self.filled..]) {
                Ok(0) => self.eof = true,
                Ok(n) => self.filled += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
}

impl<R: Read> Iterator for Cutter<R> {
    type Item = io::Result<Cut>;

    fn next(&mut self) -> Option<Self::Item> {
        if let Err(e) = self.fill() {
            return Some(Err(e));
        }
        if self.filled == 0 {
            return None;
        }
        let (_, count) = cut_gear(
            &self.buffer[..self.filled],
            MIN_SHARD as usize,
            AVG_SHARD as usize,
            MAX_SHARD as usize,
            self.mask_s,
            self.mask_l,
            self.mask_s << 1,
            self.mask_l << 1,
            &self.key.gear,
            &self.key.gear_ls,
        );
        let data = self.buffer[..count].to_vec();
        self.buffer.copy_within(count..self.filled, 0);
        self.filled -= count;
        let offset = self.offset;
        self.offset += count as u64;
        Some(Ok(Cut { offset, data }))
    }
}

// ------------------------------------------------------------ sealed boxes

fn seal_box(key: &[u8; 32], label: &[u8], id: &NodeId, plaintext: &[u8], rng: &mut impl Random) -> Vec<u8> {
    let mut purpose = subkey(key, label);
    let cipher = ChaCha20Poly1305::new(Key::from_slice(&purpose));
    purpose.zeroize();
    let mut nonce = [0u8; NONCE];
    rng.fill(&mut nonce);
    let aad = [label, &id.0[..]].concat();
    let mut out = Vec::with_capacity(1 + NONCE + plaintext.len() + TAG);
    out.push(VERSION);
    out.extend_from_slice(&nonce);
    out.extend_from_slice(plaintext);
    let tag = cipher
        .encrypt_in_place_detached(Nonce::from_slice(&nonce), &aad, &mut out[1 + NONCE..])
        .expect("a sealed box is far below the cipher's length limit");
    out.extend_from_slice(&tag);
    out
}

fn open_box(key: &[u8; 32], label: &[u8], id: &NodeId, sealed: &[u8]) -> Result<Vec<u8>> {
    let Some((&version, rest)) = sealed.split_first() else {
        return Err(CryptError::Truncated);
    };
    if version != VERSION {
        return Err(CryptError::Version(version));
    }
    if rest.len() < NONCE + TAG {
        return Err(CryptError::Truncated);
    }
    let (nonce, rest) = rest.split_at(NONCE);
    let (body, tag) = rest.split_at(rest.len() - TAG);
    let mut purpose = subkey(key, label);
    let cipher = ChaCha20Poly1305::new(Key::from_slice(&purpose));
    purpose.zeroize();
    let aad = [label, &id.0[..]].concat();
    let mut plain = body.to_vec();
    cipher
        .decrypt_in_place_detached(Nonce::from_slice(nonce), &aad, &mut plain, Tag::from_slice(tag))
        .map_err(|_| CryptError::Tampered)?;
    Ok(plain)
}

fn key_from(mut bytes: Vec<u8>) -> Result<[u8; 32]> {
    let key = <[u8; 32]>::try_from(&bytes[..]).map_err(|_| CryptError::Malformed("a key is 32 bytes"));
    bytes.zeroize();
    key
}

/// The form of a name that is stored and hashed: Unicode NFC. Refuses what
/// cannot be a file name anywhere: empty, `.`, `..`, a slash or a NUL, or
/// more than [`MAX_NAME`] bytes.
pub fn normalize_name(name: &str) -> Result<String> {
    let name: String = name.nfc().collect();
    if name.is_empty() || name == "." || name == ".." {
        return Err(CryptError::Malformed("not a file name"));
    }
    if name.contains(['/', '\0']) {
        return Err(CryptError::Malformed("a name cannot contain a slash or a NUL"));
    }
    if name.len() > MAX_NAME {
        return Err(CryptError::Malformed("name longer than 255 bytes"));
    }
    Ok(name)
}

impl NodeKey {
    pub fn random(rng: &mut impl Random) -> NodeKey {
        NodeKey(random_key(rng))
    }

    /// Seal a child's key under this folder's key. Done once when the child
    /// is created and again whenever it moves to another folder.
    pub fn wrap_child(&self, child_id: &NodeId, child: &NodeKey, rng: &mut impl Random) -> Vec<u8> {
        seal_box(&self.0, L_WRAP, child_id, &child.0, rng)
    }

    pub fn unwrap_child(&self, child_id: &NodeId, sealed: &[u8]) -> Result<NodeKey> {
        Ok(NodeKey(key_from(open_box(&self.0, L_WRAP, child_id, sealed)?)?))
    }

    /// Seal this node's name under its own key, so whoever is given the node
    /// can read what it is called. A rename seals again; a move does not.
    pub fn seal_name(&self, id: &NodeId, name: &str, rng: &mut impl Random) -> Result<Vec<u8>> {
        let mut padded = normalize_name(name)?.into_bytes();
        padded.resize(padded.len().div_ceil(NAME_PAD) * NAME_PAD, 0);
        Ok(seal_box(&self.0, L_NAME, id, &padded, rng))
    }

    pub fn open_name(&self, id: &NodeId, sealed: &[u8]) -> Result<String> {
        let mut padded = open_box(&self.0, L_NAME, id, sealed)?;
        while padded.last() == Some(&0) {
            padded.pop();
        }
        let name = String::from_utf8(padded).map_err(|_| CryptError::Malformed("name is not UTF-8"))?;
        if normalize_name(&name)? != name {
            return Err(CryptError::Malformed("name is not normalised"));
        }
        Ok(name)
    }

    /// What the cloud sees in place of a child's name, with `self` the
    /// folder's key: equal for equal names in this folder, and telling
    /// nothing about names in any other folder.
    pub fn name_hash(&self, name: &str) -> Result<[u8; 32]> {
        Ok(hmac(&subkey(&self.0, L_NAME_HASH), normalize_name(name)?.as_bytes()))
    }

    /// Seal a version's manifest under the file's key.
    pub fn seal_manifest(&self, id: &NodeId, manifest: &PrivateManifest, rng: &mut impl Random) -> Vec<u8> {
        let mut encoded = manifest.encode();
        let sealed = seal_box(&self.0, L_MANIFEST, id, &encoded, rng);
        encoded.zeroize();
        sealed
    }

    pub fn open_manifest(&self, id: &NodeId, sealed: &[u8]) -> Result<PrivateManifest> {
        let mut encoded = open_box(&self.0, L_MANIFEST, id, sealed)?;
        let manifest = PrivateManifest::decode(&encoded);
        encoded.zeroize();
        manifest
    }

    /// Seal what a listing shows about a version besides its size: for now
    /// the modification time. It travels with the node, so a folder can be
    /// listed without fetching any manifest, and the cloud cannot read it.
    pub fn seal_meta(&self, id: &NodeId, meta: &FileMeta, rng: &mut impl Random) -> Vec<u8> {
        seal_box(&self.0, L_META, id, &meta.mtime_ms.to_le_bytes(), rng)
    }

    pub fn open_meta(&self, id: &NodeId, sealed: &[u8]) -> Result<FileMeta> {
        let plain = open_box(&self.0, L_META, id, sealed)?;
        let mtime = <[u8; 8]>::try_from(&plain[..]).map_err(|_| CryptError::Malformed("version details are 8 bytes"))?;
        Ok(FileMeta { mtime_ms: u64::from_le_bytes(mtime) })
    }

    /// Seal the drive's dedup key under the drive root's key (`self`).
    pub fn wrap_dedup(&self, root_id: &NodeId, dedup: &DedupKey, rng: &mut impl Random) -> Vec<u8> {
        seal_box(&self.0, L_DEDUP, root_id, &dedup.0, rng)
    }

    pub fn unwrap_dedup(&self, root_id: &NodeId, sealed: &[u8]) -> Result<DedupKey> {
        Ok(DedupKey(key_from(open_box(&self.0, L_DEDUP, root_id, sealed)?)?))
    }
}

impl DedupKey {
    pub fn random(rng: &mut impl Random) -> DedupKey {
        DedupKey(random_key(rng))
    }
}

/// What a listing shows about a file's version besides its size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileMeta {
    /// Milliseconds since the Unix epoch.
    pub mtime_ms: u64,
}

// --------------------------------------------------------------- manifests

/// One piece of a file, as only its readers see it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrivateSegment {
    /// `n` plaintext bytes in the sealed shard named `id`, opened with `key`.
    Shard { id: [u8; 32], n: u32, key: ShardKey },
    /// `n` generated bytes: the synthetic stream for `seed`, from `off`.
    /// Made by the reader, not fetched. For tests and demos of very large files.
    Synth { seed: u64, off: u64, n: u64 },
}

impl PrivateSegment {
    pub fn len(&self) -> u64 {
        match self {
            PrivateSegment::Shard { n, .. } => *n as u64,
            PrivateSegment::Synth { n, .. } => *n,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl From<&SealedShard> for PrivateSegment {
    fn from(shard: &SealedShard) -> Self {
        PrivateSegment::Shard { id: shard.id, n: shard.n, key: shard.key.clone() }
    }
}

/// The content of one version of a file: its size and, in order, the
/// segments that add up to it, with the key of each shard. Stored sealed;
/// the cloud is told only [`PrivateManifest::shard_ids`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PrivateManifest {
    pub size: u64,
    pub segments: Vec<PrivateSegment>,
}

const SEG_SHARD: u8 = 0;
const SEG_SYNTH: u8 = 1;

impl PrivateManifest {
    /// No empty segments, no shard over [`MAX_SHARD`], and lengths that add up to `size`.
    pub fn validate(&self) -> Result<()> {
        let mut total = 0u64;
        for segment in &self.segments {
            if segment.is_empty() {
                return Err(CryptError::Malformed("empty segment"));
            }
            if matches!(segment, PrivateSegment::Shard { n, .. } if *n > MAX_SHARD) {
                return Err(CryptError::Malformed("shard over the maximum size"));
            }
            total = total.checked_add(segment.len()).ok_or(CryptError::Malformed("size overflows"))?;
        }
        if total != self.size {
            return Err(CryptError::Malformed("segments do not add up to the size"));
        }
        Ok(())
    }

    /// The sealed shards this version needs, in order, as the cloud names
    /// them. This is what the cloud is told so it can keep them.
    pub fn shard_ids(&self) -> Vec<String> {
        self.segments
            .iter()
            .filter_map(|s| match s {
                PrivateSegment::Shard { id, .. } => Some(hex(id)),
                PrivateSegment::Synth { .. } => None,
            })
            .collect()
    }

    /// `size` (u64), segment count (u32), then each segment: a tag byte, and
    /// for a shard its id (32), length (u32) and key (32), for a synthetic
    /// range its seed, offset and length (u64 each). Integers little-endian.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(12 + self.segments.len() * 69);
        out.extend_from_slice(&self.size.to_le_bytes());
        out.extend_from_slice(&(self.segments.len() as u32).to_le_bytes());
        for segment in &self.segments {
            match segment {
                PrivateSegment::Shard { id, n, key } => {
                    out.push(SEG_SHARD);
                    out.extend_from_slice(id);
                    out.extend_from_slice(&n.to_le_bytes());
                    out.extend_from_slice(&key.0);
                }
                PrivateSegment::Synth { seed, off, n } => {
                    out.push(SEG_SYNTH);
                    out.extend_from_slice(&seed.to_le_bytes());
                    out.extend_from_slice(&off.to_le_bytes());
                    out.extend_from_slice(&n.to_le_bytes());
                }
            }
        }
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<PrivateManifest> {
        let mut reader = Reader { bytes };
        let size = reader.u64()?;
        let count = reader.u32()? as usize;
        // The smallest segment is 25 bytes; refuse a count the input cannot hold.
        if count > reader.bytes.len() / 25 {
            return Err(CryptError::Truncated);
        }
        let mut segments = Vec::with_capacity(count);
        for _ in 0..count {
            segments.push(match reader.take(1)?[0] {
                SEG_SHARD => {
                    let id = reader.array()?;
                    let n = reader.u32()?;
                    PrivateSegment::Shard { id, n, key: ShardKey(reader.array()?) }
                }
                SEG_SYNTH => PrivateSegment::Synth { seed: reader.u64()?, off: reader.u64()?, n: reader.u64()? },
                _ => return Err(CryptError::Malformed("unknown segment kind")),
            });
        }
        if !reader.bytes.is_empty() {
            return Err(CryptError::Malformed("bytes after the last segment"));
        }
        let manifest = PrivateManifest { size, segments };
        manifest.validate()?;
        Ok(manifest)
    }
}

struct Reader<'a> {
    bytes: &'a [u8],
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.bytes.len() < n {
            return Err(CryptError::Truncated);
        }
        let (head, tail) = self.bytes.split_at(n);
        self.bytes = tail;
        Ok(head)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N]> {
        Ok(self.take(N)?.try_into().unwrap())
    }

    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.array()?))
    }

    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.array()?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{shard, synth};

    /// Random bytes for tests: a counter, so every draw differs.
    struct Counter(u64);

    impl Random for Counter {
        fn fill(&mut self, buf: &mut [u8]) {
            for b in buf {
                self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                *b = (self.0 >> 56) as u8;
            }
        }
    }

    fn dedup(n: u8) -> DedupKey {
        DedupKey::from_bytes([n; 32])
    }

    const LENGTHS: [usize; 8] = [1, FRAME - 1, FRAME, FRAME + 1, 2 * FRAME, 3 * FRAME + 5, MIN_SHARD as usize + 7, MAX_SHARD as usize];

    #[test]
    fn a_shard_of_any_length_seals_and_opens() {
        let d = dedup(1);
        for n in LENGTHS {
            let plain = synth::bytes(n as u64, 0, n);
            let sealed = seal_shard(&plain, &d);
            assert_eq!(sealed.bytes.len() as u64, sealed_len(n as u64), "sealed length for {n}");
            assert_eq!(sealed.n as usize, n);
            assert_eq!(sealed.id_hex(), shard::sha256_hex(&sealed.bytes));
            assert_eq!(open_shard(&sealed.key, n as u64, &sealed.bytes).unwrap(), plain, "round trip for {n}");
        }
        assert_eq!(sealed_len(0), 0);
        assert_eq!(MAX_SEALED_SHARD, MAX_SHARD as u64 + 256 * TAG as u64);
        assert!(seal_shard(b"", &d).bytes.is_empty());
    }

    #[test]
    fn sealed_bytes_do_not_contain_the_plaintext() {
        let plain = vec![0x41u8; 3 * FRAME];
        let sealed = seal_shard(&plain, &dedup(1));
        assert!(!sealed.bytes.windows(8).any(|w| w == [0x41; 8]));
    }

    #[test]
    fn the_same_content_dedups_only_under_the_same_dedup_key() {
        let plain = synth::bytes(5, 0, 300_000);
        let a = seal_shard(&plain, &dedup(1));
        assert_eq!(a, seal_shard(&plain, &dedup(1)));
        let other_drive = seal_shard(&plain, &dedup(2));
        assert_ne!(a.id, other_drive.id);
        assert_ne!(a.key, other_drive.key);

        let mut changed = plain.clone();
        changed[299_999] ^= 1;
        let b = seal_shard(&changed, &dedup(1));
        assert_ne!(a.key, b.key, "one changed byte must give another key");
        // Different keys, so the unchanged leading frames differ too.
        assert_ne!(a.bytes[..FRAME], b.bytes[..FRAME]);

        let mut rng = Counter(1);
        let fresh1 = seal_shard_fresh(&plain, &mut rng);
        let fresh2 = seal_shard_fresh(&plain, &mut rng);
        assert_ne!(fresh1.id, fresh2.id);
        assert_ne!(fresh1.id, a.id);
        assert_eq!(open_shard(&fresh1.key, plain.len() as u64, &fresh1.bytes).unwrap(), plain);
    }

    #[test]
    fn any_range_opens_from_only_its_frames() {
        let d = dedup(3);
        for n in [1usize, FRAME, FRAME + 1, 5 * FRAME + 123] {
            let plain = synth::bytes(9, 0, n);
            let sealed = seal_shard(&plain, &d);
            let mut rng = Counter(n as u64);
            let mut cases = vec![(0u64, n as u64), (0, 1), (n as u64 - 1, 1), (0, u64::MAX)];
            for _ in 0..200 {
                let mut b = [0u8; 16];
                rng.fill(&mut b);
                let off = u64::from_le_bytes(b[..8].try_into().unwrap()) % n as u64;
                let len = 1 + u64::from_le_bytes(b[8..].try_into().unwrap()) % (2 * FRAME as u64 + 50);
                cases.push((off, len));
            }
            for (off, len) in cases {
                let s = span(off, len, n as u64).unwrap();
                let end = (off.saturating_add(len)).min(n as u64) as usize;
                assert!(s.bytes.end <= sealed.bytes.len() as u64);
                // Never more than the frames the range touches.
                let frames = (end as u64 - 1) / FRAME as u64 - off / FRAME as u64 + 1;
                assert!(s.bytes.end - s.bytes.start <= frames * (FRAME + TAG) as u64);
                let fetched = &sealed.bytes[s.bytes.start as usize..s.bytes.end as usize];
                assert_eq!(open_span(&sealed.key, n as u64, &s, fetched).unwrap(), plain[off as usize..end], "{off}+{len} of {n}");
            }
            assert_eq!(span(n as u64, 10, n as u64), None);
            assert_eq!(span(0, 0, n as u64), None);
        }
        // A 64 KiB read at an arbitrary offset costs at most two frames.
        let s = span(1_000_000, 65536, 4 << 20).unwrap();
        assert_eq!(s.bytes.end - s.bytes.start, 2 * (FRAME + TAG) as u64);
    }

    #[test]
    fn damage_to_a_sealed_shard_is_refused() {
        let n = 3 * FRAME + 500;
        let plain = synth::bytes(21, 0, n);
        let sealed = seal_shard(&plain, &dedup(4));
        let (key, n64) = (&sealed.key, n as u64);
        let sf = FRAME + TAG;

        for at in [0, FRAME - 1, FRAME, FRAME + TAG - 1, sf + 7, sealed.bytes.len() - 1] {
            let mut bad = sealed.bytes.clone();
            bad[at] ^= 1;
            assert_eq!(open_shard(key, n64, &bad), Err(CryptError::Tampered), "flipped byte at {at}");
        }

        let mut swapped = sealed.bytes.clone();
        let (first, second) = (sealed.bytes[..sf].to_vec(), sealed.bytes[sf..2 * sf].to_vec());
        swapped[..sf].copy_from_slice(&second);
        swapped[sf..2 * sf].copy_from_slice(&first);
        assert_eq!(open_shard(key, n64, &swapped), Err(CryptError::Tampered), "two frames swapped");

        // Frame 1 served where frame 0 was asked for.
        assert_eq!(open_frames(key, n64, 0, &sealed.bytes[sf..2 * sf]), Err(CryptError::Tampered));
        assert_eq!(open_frames(key, n64, 1, &sealed.bytes[sf..2 * sf]).unwrap(), plain[FRAME..2 * FRAME]);

        let without_last = &sealed.bytes[..3 * sf];
        assert_eq!(open_shard(key, n64, without_last), Err(CryptError::Truncated), "last frame dropped");
        let s = span(0, n64, n64).unwrap();
        assert_eq!(open_span(key, n64, &s, without_last), Err(CryptError::Truncated));
        assert_eq!(open_shard(key, n64, &sealed.bytes[..sealed.bytes.len() - 1]), Err(CryptError::Truncated));
        let mut longer = sealed.bytes.clone();
        longer.extend_from_slice(&[0; 20]);
        assert!(open_shard(key, n64, &longer).is_err());

        // A frame of another shard, and the right bytes under another key.
        let other = seal_shard(&synth::bytes(22, 0, n), &dedup(4));
        let mut mixed = sealed.bytes.clone();
        mixed[sf..2 * sf].copy_from_slice(&other.bytes[sf..2 * sf]);
        assert_eq!(open_shard(key, n64, &mixed), Err(CryptError::Tampered));
        assert_eq!(open_shard(&other.key, n64, &sealed.bytes), Err(CryptError::Tampered));

        // A manifest that lies about the length does not get the tail cut off silently.
        assert!(open_shard(key, n64 - 1, &sealed.bytes).is_err());
    }

    fn lens(data: &[u8], key: &CutKey) -> Vec<usize> {
        cut(data, key).map(|c| c.unwrap().data.len()).collect()
    }

    #[test]
    fn the_cutter_with_the_stock_table_matches_the_existing_one() {
        let (stock, _) = fastcdc::v2020::get_gear_with_seed(0);
        let key = CutKey::from_table(stock);
        for (seed, n) in [(11u64, 24usize << 20), (12, 300_000), (13, 1), (14, (MAX_SHARD as usize) + 1)] {
            let data = synth::bytes(seed, 0, n);
            let old: Vec<(u64, Vec<u8>)> = shard::chunks(&data[..]).map(|c| c.unwrap()).map(|c| (c.offset, c.data)).collect();
            let new: Vec<(u64, Vec<u8>)> = cut(&data[..], &key).map(|c| c.unwrap()).map(|c| (c.offset, c.data)).collect();
            assert_eq!(new.len(), old.len(), "{n} bytes");
            assert!(new == old, "cuts differ for {n} bytes");
        }
        assert_eq!(cut(&b""[..], &key).count(), 0);
    }

    #[test]
    fn cut_points_depend_on_the_key() {
        let data = synth::bytes(31, 0, 32 << 20);
        let a = lens(&data, &CutKey::for_drive(&dedup(1)));
        assert_eq!(a, lens(&data, &CutKey::for_drive(&dedup(1))));
        let b = lens(&data, &CutKey::for_drive(&dedup(2)));
        let (stock, _) = fastcdc::v2020::get_gear_with_seed(0);
        let c = lens(&data, &CutKey::from_table(stock));
        let file = lens(&data, &CutKey::for_file(&NodeKey::from_bytes([1; 32])));
        for (name, other) in [("another drive", &b), ("the stock table", &c), ("a file key of the same bytes", &file)] {
            let ends = |v: &[usize]| v.iter().scan(0usize, |p, n| { *p += n; Some(*p) }).collect::<std::collections::HashSet<_>>();
            let shared = ends(&a).intersection(&ends(other)).count();
            // Only the end of the data, and forced cuts at the maximum size, can coincide.
            assert!(shared <= 2, "{shared} cut points shared with {name}");
        }
        for v in [&a, &b] {
            assert_eq!(v.iter().sum::<usize>(), data.len());
            assert!(v.len() >= 6, "32 MiB should give several shards, got {}", v.len());
            assert!(v.iter().all(|n| *n <= MAX_SHARD as usize));
            assert!(v[..v.len() - 1].iter().all(|n| *n >= MIN_SHARD as usize));
        }
    }

    #[test]
    fn a_keyed_cut_still_moves_only_the_shards_around_an_edit() {
        let d = dedup(7);
        let key = CutKey::for_drive(&d);
        let mut data = synth::bytes(12, 0, 24 << 20);
        let names = |data: &[u8]| -> Vec<[u8; 32]> { cut(data, &key).map(|c| seal_shard(&c.unwrap().data, &d).id).collect() };
        let before = names(&data);
        for b in &mut data[12 << 20..(12 << 20) + 4096] {
            *b ^= 0x5a;
        }
        let after = names(&data);
        let old: std::collections::HashSet<_> = before.iter().collect();
        let fresh = after.iter().filter(|id| !old.contains(id)).count();
        assert!((1..=3).contains(&fresh), "{fresh} new sealed shards for a 4 KiB edit");

        let mut shifted = data[..1 << 20].to_vec();
        shifted.extend_from_slice(b"seventeen bytes!!");
        shifted.extend_from_slice(&data[1 << 20..]);
        let inserted = names(&shifted);
        let old: std::collections::HashSet<_> = after.iter().collect();
        let kept = inserted.iter().filter(|id| old.contains(id)).count();
        assert!(kept + 3 >= inserted.len(), "only {kept} of {} sealed shards survived an insert", inserted.len());
    }

    #[test]
    fn a_child_key_opens_only_under_its_parent_and_its_own_id() {
        let mut rng = Counter(2);
        let (parent, child) = (NodeKey::random(&mut rng), NodeKey::random(&mut rng));
        let (id, other_id) = (NodeId::random(&mut rng), NodeId::random(&mut rng));
        let wrapped = parent.wrap_child(&id, &child, &mut rng);
        assert_eq!(wrapped.len(), 1 + NONCE + 32 + TAG);
        assert_eq!(parent.unwrap_child(&id, &wrapped).unwrap(), child);
        assert_ne!(wrapped, parent.wrap_child(&id, &child, &mut rng), "a second wrap uses another nonce");

        assert_eq!(parent.unwrap_child(&other_id, &wrapped), Err(CryptError::Tampered), "moved to another node");
        assert_eq!(child.unwrap_child(&id, &wrapped), Err(CryptError::Tampered), "wrong parent");
        let mut bad = wrapped.clone();
        *bad.last_mut().unwrap() ^= 1;
        assert_eq!(parent.unwrap_child(&id, &bad), Err(CryptError::Tampered));
        bad = wrapped.clone();
        bad[0] = 2;
        assert_eq!(parent.unwrap_child(&id, &bad), Err(CryptError::Version(2)));
        assert_eq!(parent.unwrap_child(&id, &wrapped[..20]), Err(CryptError::Truncated));
        assert_eq!(parent.unwrap_child(&id, &[]), Err(CryptError::Truncated));
        // A box sealed for another purpose under the same key is not a wrapped key.
        let as_dedup = parent.wrap_dedup(&id, &DedupKey::from_bytes(*child.expose()), &mut rng);
        assert_eq!(parent.unwrap_child(&id, &as_dedup), Err(CryptError::Tampered));
        assert_eq!(parent.unwrap_dedup(&id, &as_dedup).unwrap().expose(), child.expose());
    }

    #[test]
    fn a_chain_of_folders_opens_from_the_top_only() {
        let mut rng = Counter(3);
        let root = NodeKey::random(&mut rng);
        let mut wrapped = Vec::new();
        let mut parent = root.clone();
        for _ in 0..4 {
            let (id, key) = (NodeId::random(&mut rng), NodeKey::random(&mut rng));
            wrapped.push((id, parent.wrap_child(&id, &key, &mut rng), key.clone()));
            parent = key;
        }
        let mut key = root;
        for (id, sealed, expected) in &wrapped {
            key = key.unwrap_child(id, sealed).unwrap();
            assert_eq!(&key, expected);
        }
        // The key of the second folder opens the third, not the first.
        let second = &wrapped[1].2;
        assert!(second.unwrap_child(&wrapped[2].0, &wrapped[2].1).is_ok());
        assert!(second.unwrap_child(&wrapped[0].0, &wrapped[0].1).is_err());
    }

    #[test]
    fn names_seal_open_and_hide_their_length() {
        let mut rng = Counter(4);
        let (key, id, other_id) = (NodeKey::random(&mut rng), NodeId::random(&mut rng), NodeId::random(&mut rng));
        for name in ["a", "shot-list.txt", "Footage 2026 — final (2).mov", "😀 100%'\".txt", &"x".repeat(255), &"x".repeat(32)] {
            let sealed = key.seal_name(&id, name, &mut rng).unwrap();
            assert_eq!(key.open_name(&id, &sealed).unwrap(), name);
            assert_eq!((sealed.len() - 1 - NONCE - TAG) % NAME_PAD, 0);
            assert_eq!(key.open_name(&other_id, &sealed), Err(CryptError::Tampered), "name moved to another node");
        }
        let short = key.seal_name(&id, "a", &mut rng).unwrap();
        let longer = key.seal_name(&id, "a-much-longer-name.txt", &mut rng).unwrap();
        assert_eq!(short.len(), longer.len(), "names up to 32 bytes seal to one length");
        assert!(!longer.windows(6).any(|w| w == b"longer"));

        for bad in ["", ".", "..", "a/b", "a\0b", &"x".repeat(256)] {
            assert!(key.seal_name(&id, bad, &mut rng).is_err(), "{bad:?} should be refused");
            assert!(key.name_hash(bad).is_err());
        }
    }

    #[test]
    fn name_hashes_follow_normalisation_and_the_folder() {
        let mut rng = Counter(5);
        let (folder, other) = (NodeKey::random(&mut rng), NodeKey::random(&mut rng));
        let (nfc, nfd) = ("caf\u{e9}.txt", "cafe\u{301}.txt");
        assert_ne!(nfc, nfd);
        assert_eq!(folder.name_hash(nfc).unwrap(), folder.name_hash(nfd).unwrap());
        assert_ne!(folder.name_hash("Report.txt").unwrap(), folder.name_hash("report.txt").unwrap(), "case-sensitive");
        assert_ne!(folder.name_hash(nfc).unwrap(), other.name_hash(nfc).unwrap(), "another folder, another hash");

        // A name written in NFD is stored, and read back, in NFC.
        let id = NodeId::random(&mut rng);
        let sealed = folder.seal_name(&id, nfd, &mut rng).unwrap();
        assert_eq!(folder.open_name(&id, &sealed).unwrap(), nfc);
    }

    fn sample_manifest() -> (PrivateManifest, Vec<SealedShard>) {
        let d = dedup(6);
        let shards: Vec<SealedShard> = [300_000usize, 70_000, 1].iter().map(|n| seal_shard(&synth::bytes(*n as u64, 0, *n), &d)).collect();
        let mut segments: Vec<PrivateSegment> = shards.iter().map(PrivateSegment::from).collect();
        segments.insert(1, PrivateSegment::Synth { seed: 9, off: 4096, n: 50 << 30 });
        let size = segments.iter().map(|s| s.len()).sum();
        (PrivateManifest { size, segments }, shards)
    }

    #[test]
    fn a_manifest_seals_under_the_file_key_and_tells_the_cloud_only_shard_names() {
        let mut rng = Counter(6);
        let (key, id, other_id) = (NodeKey::random(&mut rng), NodeId::random(&mut rng), NodeId::random(&mut rng));
        let (manifest, shards) = sample_manifest();
        manifest.validate().unwrap();
        assert_eq!(manifest.shard_ids(), shards.iter().map(|s| s.id_hex()).collect::<Vec<_>>());
        assert!(manifest.shard_ids().iter().all(|h| shard::valid_hash(h)));

        let encoded = manifest.encode();
        assert_eq!(encoded.len(), 12 + 3 * 69 + 25);
        assert_eq!(PrivateManifest::decode(&encoded).unwrap(), manifest);

        let sealed = key.seal_manifest(&id, &manifest, &mut rng);
        assert_eq!(key.open_manifest(&id, &sealed).unwrap(), manifest);
        assert_eq!(key.open_manifest(&other_id, &sealed), Err(CryptError::Tampered), "manifest moved to another file");
        assert_eq!(NodeKey::random(&mut rng).open_manifest(&id, &sealed), Err(CryptError::Tampered));
        // Neither a shard key nor a shard name is readable in the sealed manifest.
        for s in &shards {
            assert!(!sealed.windows(32).any(|w| w == s.key.expose() || w == s.id));
        }
        // A name sealed under the same key and id is not a manifest.
        let name = key.seal_name(&id, "x", &mut rng).unwrap();
        assert_eq!(key.open_manifest(&id, &name), Err(CryptError::Tampered));

        let empty = PrivateManifest::default();
        assert_eq!(key.open_manifest(&id, &key.seal_manifest(&id, &empty, &mut rng)).unwrap(), empty);
    }

    #[test]
    fn version_details_seal_under_the_file_key() {
        let mut rng = Counter(8);
        let (key, id, other_id) = (NodeKey::random(&mut rng), NodeId::random(&mut rng), NodeId::random(&mut rng));
        let meta = FileMeta { mtime_ms: 1_795_000_000_123 };
        let sealed = key.seal_meta(&id, &meta, &mut rng);
        assert_eq!(sealed.len(), 1 + NONCE + 8 + TAG);
        assert_eq!(key.open_meta(&id, &sealed).unwrap(), meta);
        assert_eq!(key.open_meta(&other_id, &sealed), Err(CryptError::Tampered));
        // Not interchangeable with the other boxes under the same key and id.
        assert_eq!(key.open_name(&id, &sealed), Err(CryptError::Tampered));
        assert_eq!(key.open_meta(&id, &key.seal_name(&id, "12345678", &mut rng).unwrap()), Err(CryptError::Tampered));
    }

    #[test]
    fn a_malformed_manifest_is_refused() {
        let (manifest, _) = sample_manifest();
        let encoded = manifest.encode();
        assert_eq!(PrivateManifest::decode(&encoded[..encoded.len() - 1]), Err(CryptError::Truncated));
        assert_eq!(PrivateManifest::decode(&encoded[..5]), Err(CryptError::Truncated));
        let mut extra = encoded.clone();
        extra.push(0);
        assert!(matches!(PrivateManifest::decode(&extra), Err(CryptError::Malformed(_))));
        let mut wrong_size = encoded.clone();
        wrong_size[0] ^= 1;
        assert!(matches!(PrivateManifest::decode(&wrong_size), Err(CryptError::Malformed(_))));
        let mut unknown_kind = encoded.clone();
        unknown_kind[12] = 9;
        assert!(PrivateManifest::decode(&unknown_kind).is_err());
        let mut huge_count = encoded.clone();
        huge_count[8..12].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(PrivateManifest::decode(&huge_count), Err(CryptError::Truncated));

        let key = ShardKey::from_bytes([1; 32]);
        let empty_segment = PrivateManifest { size: 0, segments: vec![PrivateSegment::Shard { id: [0; 32], n: 0, key: key.clone() }] };
        assert!(empty_segment.validate().is_err());
        assert!(PrivateManifest::decode(&empty_segment.encode()).is_err());
        let too_big = PrivateManifest { size: MAX_SHARD as u64 + 1, segments: vec![PrivateSegment::Shard { id: [0; 32], n: MAX_SHARD + 1, key }] };
        assert!(too_big.validate().is_err());
    }

    /// `cargo test --release -p ludy-proto throughput -- --ignored --nocapture`
    #[test]
    #[ignore = "a measurement, not a check"]
    fn throughput() {
        let d = dedup(1);
        let key = CutKey::for_drive(&d);
        let data = synth::bytes(77, 0, 256 << 20);
        let mb = (data.len() >> 20) as f64;

        let t = std::time::Instant::now();
        let cuts: Vec<Cut> = cut(&data[..], &key).map(|c| c.unwrap()).collect();
        println!("cut:  {:6.0} MB/s ({} shards)", mb / t.elapsed().as_secs_f64(), cuts.len());

        let t = std::time::Instant::now();
        let sealed: Vec<SealedShard> = cuts.iter().map(|c| seal_shard(&c.data, &d)).collect();
        println!("seal: {:6.0} MB/s (derive the key, encrypt, name)", mb / t.elapsed().as_secs_f64());

        let t = std::time::Instant::now();
        let opened: usize = sealed.iter().map(|s| open_shard(&s.key, s.n as u64, &s.bytes).unwrap().len()).sum();
        println!("open: {:6.0} MB/s", mb / t.elapsed().as_secs_f64());
        assert_eq!(opened, data.len());
    }

    #[test]
    fn keys_do_not_print() {
        let key = NodeKey::from_bytes([0xab; 32]);
        assert_eq!(format!("{key:?}"), "NodeKey(..)");
        assert_eq!(format!("{:?}", CutKey::for_file(&key)), "CutKey(..)");
    }
}
