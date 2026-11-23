# The Rivet provider: Ludy's server on Rivet actors

One binary, `ludy-rivet`, beside a [Rivet](https://rivet.dev) engine. It is what `docker compose up` at the top of the repository starts. The drive logic is [`ludy-server`](../../crates/ludy-server), the same code the Cloudflare Worker runs; this package is what Rivet supplies to it.

## Running it

The steps are in the hosting pages, one per way of running it:

| | |
|---|---|
| [Docker Compose](../../docs/hosting/compose.md) | The default: `docker compose up -d --wait`, everything in Docker volumes |
| [Compose with R2, MinIO or another S3](../../docs/hosting/object-storage.md) | The same, with the sealed blobs in a bucket |
| [The binary beside an engine](../../docs/hosting/binary.md) | `cargo run --release -p ludy-rivet`, for working on this package |

Where this fits, and what a provider has to promise: [the server](../../docs/how-it-works/server.md). What is known to be wrong or unchecked: [known problems](../../docs/known-issues.md#the-rivet-provider).

## Settings

| Variable | Default | What it is |
|---|---|---|
| `RIVET_ENDPOINT` | none, required | The engine, as `http://<namespace>:<token>@<host>:6420`. `RIVET_NAMESPACE`, `RIVET_TOKEN` and `RIVET_POOL` can be given apart instead |
| `LUDY_LISTEN` | `0.0.0.0:8080` | Where the HTTP front listens |
| `LUDY_BLOBS` | `./ludy-blobs` | A folder, or `s3://<bucket>` |
| `AWS_ENDPOINT`, `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, `AWS_REGION` | | For `s3://`. With an endpoint the bucket goes in the path, which R2 and MinIO want. Region defaults to `auto` |
| `GC_GRACE_MS` | a day | How long a shard no version uses is kept |
| `LUDY_DEV` | off | `1` turns on the `_dev` routes. Only the chaos suites set it |

`ludy-rivet health` asks the server on the same machine for a drive that does not exist (`ffff…`, 32 of them) and exits 0 if an actor answered. It is the image's health check, run every three seconds, so the engine holds one actor for that id. Its usual answer is left out of the server's log.

These are the server's own. The settings the Compose files read from `.env` are in [the Compose page](../../docs/hosting/compose.md#settings).

## How it is put together

```text
client ── HTTP ──> front (axum) ──┬── shards, manifests ──> blob store (folder, R2, any S3)
                                  └── everything else ──> Rivet gateway ──> the drive's actor
                                                                              │
                                                             ludy-server on the actor's SQLite
```

- **The front** speaks the protocol of [`ludy_proto::tree`](../../crates/ludy-proto/src/tree.rs). It hashes every uploaded shard and manifest and refuses one that does not match the SHA-256 in its path, since no store here does that for us. Blob bytes never reach an actor.
- **A drive is an actor**, found by the drive's id and created on first use. The Rust runtime hands an actor its HTTP requests one at a time. The collector arrives as a scheduled action beside them, and does its choosing and marking inside a transaction. In Rivet's source a transaction takes the actor's SQL exclusively and any other statement waits for it; no test here has shown a commit and the collector actually overlapping.
- **SQLite runs inside this process** (`rivetkit`'s `sqlite-local`), with its pages kept by the engine. Measured in a transaction on this Mac: about 61,000 statements a second, against 2,300 with the default, where each statement goes to the engine and back.
- **The engine and the crate are pinned to one release** (3.1.1). They speak a versioned protocol; move both together.

## Check it

The two chaos suites are the test of a provider. They start the engine in a container of their own and this binary from `target/release`:

```bash
cargo build --release -p ludy-rivet
```

```bash
LUDY_PROVIDER=rivet cargo test -p ludy-core --test worker_chaos -- --ignored --test-threads=1
```

```bash
LUDY_PROVIDER=rivet cargo test -p ludy-core --test sealed_engine -- --ignored --test-threads=1
```

`LUDY_RIVET_KILL=server` or `engine` makes a `kill -9` take only that one; by default it takes both.

The rig gives its engine the same two-second wait for a silent server as `docker-compose.yml` does, so the suites test what Compose runs.
