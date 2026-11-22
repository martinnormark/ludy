// An independent implementation of Ludy's encryption format, version 1,
// written from the description at the top of src/crypt.rs with nothing but
// Node's built-in crypto. It produces the test vectors in crypt-v1.json;
// the Rust test tests/crypt_vectors.rs must reproduce every one of them.
//
//   node crypt-v1.mjs           check crypt-v1.json against this implementation
//   node crypt-v1.mjs --write   write crypt-v1.json
//
// No dependencies. Node 20 or later.

import crypto from 'node:crypto';
import fs from 'node:fs';
import { fileURLToPath } from 'node:url';

const FRAME = 64 * 1024;
const VERSION = 1;
const enc = (s) => Buffer.from(s, 'utf8');
const hex = (b) => Buffer.from(b).toString('hex');
const sha256 = (b) => crypto.createHash('sha256').update(b).digest();
const hmac = (key, msg) => crypto.createHmac('sha256', key).update(msg).digest();
const subkey = (key, label) => Buffer.from(crypto.hkdfSync('sha256', key, Buffer.alloc(0), enc(label), 32));

function aead(key, nonce, aad, plaintext) {
  const c = crypto.createCipheriv('chacha20-poly1305', key, nonce, { authTagLength: 16 });
  c.setAAD(aad, { plaintextLength: plaintext.length });
  return Buffer.concat([c.update(plaintext), c.final(), c.getAuthTag()]);
}

// ---- shards

function sealShard(plaintext, shardKey) {
  const frames = [];
  for (let i = 0, off = 0; off < plaintext.length; i++, off += FRAME) {
    const nonce = Buffer.alloc(12);
    nonce.writeBigUInt64LE(BigInt(i), 0);
    frames.push(aead(shardKey, nonce, Buffer.alloc(0), plaintext.subarray(off, off + FRAME)));
  }
  return Buffer.concat(frames);
}

const contentKey = (dedup, plaintext) => hmac(subkey(dedup, 'ludy/v1/shard-key'), plaintext);

// ---- sealed boxes

function sealBox(key, label, id, plaintext, nonce) {
  const aad = Buffer.concat([enc(label), id]);
  return Buffer.concat([Buffer.from([VERSION]), nonce, aead(subkey(key, label), nonce, aad, plaintext)]);
}

function padName(name) {
  const bytes = enc(name.normalize('NFC'));
  return Buffer.concat([bytes, Buffer.alloc((32 - (bytes.length % 32)) % 32)]);
}

function encodeManifest(m) {
  const parts = [Buffer.alloc(12)];
  parts[0].writeBigUInt64LE(BigInt(m.size), 0);
  parts[0].writeUInt32LE(m.segments.length, 8);
  for (const s of m.segments) {
    if (s.shard) {
      const n = Buffer.alloc(4);
      n.writeUInt32LE(s.shard.n);
      parts.push(Buffer.from([0]), Buffer.from(s.shard.id, 'hex'), n, Buffer.from(s.shard.key, 'hex'));
    } else {
      const b = Buffer.alloc(24);
      b.writeBigUInt64LE(BigInt(s.synth.seed), 0);
      b.writeBigUInt64LE(BigInt(s.synth.off), 8);
      b.writeBigUInt64LE(BigInt(s.synth.n), 16);
      parts.push(Buffer.from([1]), b);
    }
  }
  return Buffer.concat(parts);
}

// ---- inputs

// Fixed "random" material: the hash of a description of what it is.
const fixed = (what, n = 32) => sha256(enc(`ludy test vector: ${what}`)).subarray(0, n);

// Shard plaintext both sides can make: byte i is (i*131 + (i>>9) + salt) mod 256.
function pattern(len, salt) {
  const b = Buffer.alloc(len);
  for (let i = 0; i < len; i++) b[i] = (i * 131 + (i >>> 9) + salt) & 0xff;
  return b;
}

function build() {
  const dedup = fixed('dedup key');

  const shards = [1, FRAME - 1, FRAME, FRAME + 1, 200000].map((len, salt) => {
    const plaintext = pattern(len, salt);
    const key = contentKey(dedup, plaintext);
    const sealed = sealShard(plaintext, key);
    return {
      dedup: hex(dedup), len, salt,
      key: hex(key), id: hex(sha256(sealed)), sealed_len: sealed.length,
      head: hex(sealed.subarray(0, 48)), tail: hex(sealed.subarray(-32)),
    };
  });

  const cut = [['drive', 'ludy/v1/cut', dedup], ['file', 'ludy/v1/cut-file', fixed('file key')]].map(([kind, label, key]) => {
    const table = Buffer.from(crypto.hkdfSync('sha256', key, Buffer.alloc(0), enc(label), 2048));
    return {
      kind, key: hex(key),
      first: [0, 1, 2, 3].map((i) => table.readBigUInt64LE(i * 8).toString(16).padStart(16, '0')),
      table_sha256: hex(sha256(table)),
    };
  });

  const parent = fixed('parent key'), child = fixed('child key'), childId = fixed('child id', 16);
  const wrapNonce = fixed('wrap nonce', 12);
  const wrap = {
    parent: hex(parent), child_id: hex(childId), child: hex(child), nonce: hex(wrapNonce),
    sealed: hex(sealBox(parent, 'ludy/v1/wrap', childId, child, wrapNonce)),
  };

  const rootId = fixed('root id', 16), dedupNonce = fixed('dedup nonce', 12);
  const dedup_wrap = {
    root: hex(parent), root_id: hex(rootId), dedup: hex(dedup), nonce: hex(dedupNonce),
    sealed: hex(sealBox(parent, 'ludy/v1/dedup', rootId, dedup, dedupNonce)),
  };

  // The first is written decomposed (NFD) on purpose: e + combining acute.
  // Then 30 bytes, exactly 32 (no padding at all), 34 (a second padding
  // block) and a non-Latin name. Escapes keep the exact code points visible.
  const names = [
    'cafe\u0301.txt', 'Footage 2026 \u2014 final (2).mov', 'a'.repeat(28) + '.txt',
    'Footage 2026 \u2014 final cut (2).mov', '\u65e5\u672c\u8a9e',
  ].map((name, i) => {
    const nonce = fixed(`name nonce ${i}`, 12);
    return {
      key: hex(child), id: hex(childId), parent: hex(parent), name, nonce: hex(nonce),
      normalized: name.normalize('NFC'),
      sealed: hex(sealBox(child, 'ludy/v1/name', childId, padName(name), nonce)),
      hash: hex(hmac(subkey(parent, 'ludy/v1/name-hash'), enc(name.normalize('NFC')))),
    };
  });

  const manifestValue = {
    size: 300000 + 53687091200 + 1,
    segments: [
      { shard: { id: hex(fixed('shard id a')), n: 300000, key: hex(fixed('shard key a')) } },
      { synth: { seed: 9, off: 4096, n: 53687091200 } },
      { shard: { id: hex(fixed('shard id b')), n: 1, key: hex(fixed('shard key b')) } },
    ],
  };
  const encoded = encodeManifest(manifestValue), manifestNonce = fixed('manifest nonce', 12);
  const manifest = {
    key: hex(child), id: hex(childId), nonce: hex(manifestNonce), manifest: manifestValue,
    encoded: hex(encoded),
    sealed: hex(sealBox(child, 'ludy/v1/manifest', childId, encoded, manifestNonce)),
  };

  const metaNonce = fixed('meta nonce', 12), mtime = Buffer.alloc(8);
  mtime.writeBigUInt64LE(1795000000123n);
  const meta = {
    key: hex(child), id: hex(childId), nonce: hex(metaNonce), mtime_ms: 1795000000123,
    sealed: hex(sealBox(child, 'ludy/v1/meta', childId, mtime, metaNonce)),
  };

  return { format: VERSION, generated_by: 'crypt-v1.mjs (Node built-in crypto)', shards, cut, wrap, dedup_wrap, names, manifest, meta };
}

const file = fileURLToPath(new URL('./crypt-v1.json', import.meta.url));
const text = JSON.stringify(build(), null, 2) + '\n';
if (process.argv.includes('--write')) {
  fs.writeFileSync(file, text);
  console.log(`wrote ${file}`);
} else if (fs.readFileSync(file, 'utf8') === text) {
  console.log('crypt-v1.json matches this implementation');
} else {
  console.error('crypt-v1.json does NOT match this implementation');
  process.exit(1);
}
