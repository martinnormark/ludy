# The Cloudflare provider: Ludy's server as a Worker

A Worker, a Durable Object per drive, and an R2 bucket. The drive logic is [`ludy-server`](../../crates/ludy-server), the same code the [Rivet provider](../rivet) runs; this package is what Cloudflare supplies to it. It has only run under `wrangler dev`. It has never been deployed.

## Running it

The steps, what it needs installed, its settings and the warnings that go with deploying it are in [the Cloudflare hosting page](../../docs/hosting/cloudflare.md). In short, in this folder:

```bash
npm install
```

```bash
npm run dev
```

That answers on `http://127.0.0.1:8787`. It builds for WebAssembly only, so it is a cargo workspace of its own, outside the one at the top of the repository.

Where this fits, and what a provider has to promise: [the server](../../docs/how-it-works/server.md). What is known to be wrong or unchecked: [known problems](../../docs/known-issues.md#cloudflare).

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
