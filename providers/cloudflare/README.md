# The Cloudflare provider: Ludy's cloud as a Worker

A Worker, a Durable Object per drive, and an R2 bucket. The drive logic is [`ludy-server`](../../crates/ludy-server), the same code the [Rivet provider](../rivet) runs; this package is what Cloudflare supplies to it. It has only run under `wrangler dev` on this Mac. It has never been deployed.

Where this fits, and what a provider has to promise: [docs/real-cloud/architecture.md](../../docs/real-cloud/architecture.md). What is known to be wrong or unchecked: [docs/real-cloud/known-issues.md](../../docs/real-cloud/known-issues.md).

## Run it

It builds for WebAssembly only, so it is a cargo workspace of its own, outside the one at the top of the repository. Once:

```bash
rustup target add wasm32-unknown-unknown
```

```bash
cargo install worker-build
```

```bash
npm install
```

Then, in this folder:

```bash
npm run dev
```

```bash
curl http://127.0.0.1:8787/
```

That answers `ludy-worker`. It runs under workerd, with R2 and the Durable Objects' storage kept in `.wrangler` here.

## Settings

They are in [`wrangler.toml`](wrangler.toml).

| Name | What it is |
|---|---|
| `SHARDS` | The R2 bucket binding. The bucket is named `ludy-shards` |
| `DRIVE` | The Durable Object binding: class `Drive`, with SQLite storage |
| `GC_GRACE_MS` | A variable. How long a shard no version uses is kept; a day if it is not set |
| `LUDY_DEV` | A variable. `1` turns on the `_dev` routes. Only the chaos suites set it |

## How it is put together

```text
client ── HTTP ──> Worker ──┬── shards, manifests ──> R2
                            └── everything else ──> the drive's Durable Object
                                                      │
                                       ludy-server on the object's SQLite
```

- **The Worker** ([`lib.rs`](src/lib.rs)) speaks the protocol of [`ludy_proto::tree`](../../crates/ludy-proto/src/tree.rs). It hands R2 the SHA-256 in an upload's path and R2 refuses a body that does not match. Blob bytes never reach a Durable Object.
- **A drive is a Durable Object** ([`drive.rs`](src/drive.rs)), found by the drive's id. A commit runs inside the runtime's synchronous transaction, and the collector runs from the object's alarm.

## Check it

The two chaos suites are the test of a provider, and this one is their default. From the top of the repository:

```bash
cargo test -p ludy-core --test worker_chaos -- --ignored --test-threads=1
```

```bash
cargo test -p ludy-core --test sealed_engine -- --ignored --test-threads=1
```

They start `wrangler dev` from this folder, so `npm install` has to have been run here.
