# The Rivet provider: Ludy's cloud, self-hosted

One binary, `ludy-rivet`, beside a [Rivet](https://rivet.dev) engine. It is what `docker compose up` at the top of the repository starts. The drive logic is [`ludy-server`](../../crates/ludy-server), the same code the Cloudflare Worker runs; this package is what Rivet supplies to it.

Where this fits, and what a provider has to promise: [docs/real-cloud/architecture.md](../../docs/real-cloud/architecture.md). What is known to be wrong or unchecked: [docs/real-cloud/known-issues.md](../../docs/real-cloud/known-issues.md).

## Run it

```bash
docker compose up -d
```

```bash
curl http://127.0.0.1:8080/
```

That answers `ludy-rivet` once the engine is up. Two services start: the Rivet engine and this server. Shards and manifests go to a Docker volume.

To keep the blobs in Cloudflare R2, fill in the four `R2_` settings from [`.env.example`](../../.env.example) and add the R2 file:

```bash
docker compose -f docker-compose.yml -f docker-compose.r2.yml up -d
```

To keep them in MinIO on the same machine, which is how the S3 path was tried:

```bash
docker compose -f docker-compose.yml -f docker-compose.minio.yml up -d
```

There is no sign-in yet, so the port is published on the machine itself only.

The first `up` builds the server's image: four and a half minutes here. After `kill -9` of both containers a drive answered again after about 48 seconds, with what it held; that wait is the engine's default before it gives up on a dead node (see [Check it](#check-it)). The R2 file has not been started against an R2 account.

## Without Docker for the server

The engine still runs in a container; the server runs from the checkout.

```bash
docker run -d --name rivet -p 127.0.0.1:6420:6420 -e RIVET__AUTH__ADMIN_TOKEN=dev -e RIVET__STORAGE__LOG_DIRECTORY=/var/lib/rivet/log -e RIVET__STORAGE__AUTO_PROVISION_PARTITIONS=1 -e RIVET__STORAGE__BALANCING__ENABLED=false -e RIVET__OBJECT_STORAGE__PRIMARY__FILE_SYSTEM__PATH=/var/lib/rivet/objects -v /var/lib/rivet/log -v /var/lib/rivet/objects rivetdev/engine:3.1.1
```

```bash
RIVET_ENDPOINT=http://default:dev@127.0.0.1:6420 cargo run --release -p ludy-rivet
```

The package is left out of a plain `cargo build` and `cargo test` because it is a large build (about three and a half minutes from nothing); name it with `-p ludy-rivet`.

## Settings

| Variable | Default | What it is |
|---|---|---|
| `RIVET_ENDPOINT` | none, required | The engine, as `http://<namespace>:<token>@<host>:6420`. `RIVET_NAMESPACE`, `RIVET_TOKEN` and `RIVET_POOL` can be given apart instead |
| `LUDY_LISTEN` | `0.0.0.0:8080` | Where the HTTP front listens |
| `LUDY_BLOBS` | `./ludy-blobs` | A folder, or `s3://<bucket>` |
| `AWS_ENDPOINT`, `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, `AWS_REGION` | | For `s3://`. With an endpoint the bucket goes in the path, which R2 and MinIO want. Region defaults to `auto` |
| `GC_GRACE_MS` | a day | How long a shard no version uses is kept |
| `LUDY_DEV` | off | `1` turns on the `_dev` routes. Only the chaos suites set it |

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

In them the rig sets the engine's `pegboard.node_lost_threshold` to two seconds. That is how long the engine waits before it gives a dead node's actors to another node; the default left a drive unanswered for 46 seconds after `kill -9` of the server, and two seconds brought it to under nine. `docker-compose.yml` leaves the default alone.
