# The Rivet provider: Ludy's cloud, self-hosted

One binary, `ludy-rivet`, beside a [Rivet](https://rivet.dev) engine. It is what `docker compose up` at the top of the repository starts. The drive logic is [`ludy-server`](../../crates/ludy-server), the same code the Cloudflare Worker runs; this package is what Rivet supplies to it.

Where this fits, and what a provider has to promise: [docs/real-cloud/architecture.md](../../docs/real-cloud/architecture.md). What is known to be wrong or unchecked: [docs/real-cloud/known-issues.md](../../docs/real-cloud/known-issues.md).

## Run it

From the top of the repository, with Docker running and nothing to set:

```bash
docker compose up -d --wait
```

Two services start: the Rivet engine and this server. `--wait` returns when both are healthy, which for the server means a drive answers. It was run with Docker Engine 29.6 and Compose 5.3 and no older Docker was tried. Shards and manifests go to a Docker volume.

The first `up` pulls the server's image, `ghcr.io/martinnormark/ludy-rivet` (140 MB, for amd64 and arm64): with the engine's image already there, a start from nothing took 14 seconds. Where the image cannot be pulled, Compose builds it from the checkout, which took four and a half minutes here and needs BuildKit.

Create a drive and read its head back. A real client sends a sealed name and wrapped keys; these are placeholder bytes:

```bash
curl -X PUT -H 'content-type: application/json' -d '{"root":"0f0e0d0c0b0a09080706050403020100","name":"00","key":"00","dedup":"00"}' http://127.0.0.1:8080/drives/000102030405060708090a0b0c0d0e0f
```

```bash
curl http://127.0.0.1:8080/drives/000102030405060708090a0b0c0d0e0f
```

Both answer the drive's head, with `"seq":1`. If port 8080 is taken, set `LUDY_PORT` in `.env` ([`.env.example`](../../.env.example) lists every setting).

Stop it, keeping the drives:

```bash
docker compose down
```

### The server's image

[A workflow](../../.github/workflows/server-image.yml) builds the image from this repository and publishes it when the server's source changes on `main` (and, until it is merged, on the branch the server lives on). Its tag is the Rivet release it is built against, `rivet-3.1.1`, the same as the engine's.

To run the source in the checkout instead of the published image:

```bash
docker compose up -d --wait --build
```

That build takes the published image's name on this machine, and Compose keeps using it. To go back to the published one, or to get a newer one:

```bash
docker compose pull ludy
```

If Compose builds when you expected it to pull, look for `denied` in its output. A saved login for `ghcr.io` that the registry no longer accepts does that, even though the image is public; `docker logout ghcr.io` clears it.

To keep the blobs in Cloudflare R2, fill in the four `R2_` settings from [`.env.example`](../../.env.example) and add the R2 file:

```bash
docker compose -f docker-compose.yml -f docker-compose.r2.yml up -d
```

To keep them in MinIO on the same machine, which is how the S3 path was tried:

```bash
docker compose -f docker-compose.yml -f docker-compose.minio.yml up -d
```

There is no sign-in yet, so the port is published on the machine itself only. The R2 file has not been started against an R2 account.

### Stopping and starting

| What happens | How long a drive answers nothing |
|---|---|
| `docker compose restart ludy` | Under a second |
| `docker compose down`, then `up -d --wait` | `up` took 12 seconds; the drive answered at once after it |
| `kill -9` of the server, then started again | 6.7 seconds |
| `kill -9` of both containers, then `up -d --wait` | `up` took 21 seconds, the drive 2 more |

Each was done once or twice on this Mac, and each time the drive held what it had. The last two depend on how long the engine waits for a server that went silent: `RIVET_NODE_LOST_MS`, two seconds in the Compose file. With the engine's own default they took 45 and 52 seconds. What the shorter wait costs is in [known problems](../../docs/real-cloud/known-issues.md#the-rivet-provider).

## Without Docker for the server

The engine still runs in a container; the server runs from the checkout.

```bash
docker run -d --name rivet -p 127.0.0.1:6420:6420 -e RIVET__AUTH__ADMIN_TOKEN=dev -e RIVET__STORAGE__LOG_DIRECTORY=/var/lib/rivet/log -e RIVET__STORAGE__AUTO_PROVISION_PARTITIONS=1 -e RIVET__STORAGE__BALANCING__ENABLED=false -e RIVET__OBJECT_STORAGE__PRIMARY__FILE_SYSTEM__PATH=/var/lib/rivet/objects -e RIVET__PEGBOARD__NODE_LOST_THRESHOLD=2000 -e RIVET__PEGBOARD__NODE_PING_TIMEOUT=2000 -v /var/lib/rivet/log -v /var/lib/rivet/objects rivetdev/engine:3.1.1
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

`ludy-rivet health` asks the server on the same machine for a drive that does not exist (`ffff…`, 32 of them) and exits 0 if an actor answered. It is the image's health check, run every three seconds, so the engine holds one actor for that id. Its usual answer is left out of the server's log.

The Compose file reads four more from `.env`: `LUDY_PORT` (8080), `RIVET_TOKEN`, `RIVET_NODE_LOST_MS` (2000) and, with the R2 or MinIO file, that store's settings.

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
