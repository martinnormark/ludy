# Docker Compose on one machine

One command starts the server on a machine with Docker: a Rivet engine and Ludy's server, with everything kept in Docker volumes. It answers on `http://127.0.0.1:8080`, which is where the app and the CLI look by default. This is the path that has been tested most.

## What you need

- Docker with Compose. It was run with Docker Engine 29.6 and 29.9 and Compose 5.3; nothing older was tried.
- A checkout of this repository, for [`docker-compose.yml`](../../docker-compose.yml).
- Disk: the engine refuses to start unless the disk under its volumes has 10% or 4 GiB free.

Nothing has to be set.

## Start it

From the top of the repository:

```bash
docker compose up -d --wait
```

`--wait` returns when both services are healthy, which for the server means a drive answers. The first start pulls two images: the Rivet engine, and the server, `ghcr.io/martinnormark/ludy-rivet` (140 MB, built for amd64 and arm64). With the engine's image already there, a start from nothing took 11 to 14 seconds.

Where the server's image cannot be pulled, Compose builds it from the checkout instead. That took four and a half minutes here and needs BuildKit. If Compose builds when you expected it to pull, look for `denied` in its output: a saved login for `ghcr.io` that the registry no longer accepts does that even though the image is public, and `docker logout ghcr.io` clears it.

## Check it is up

```bash
curl http://127.0.0.1:8080/
```

That answers `ludy-rivet`. The same through Ludy, which also shows the address it is using:

```bash
cargo run -p ludy-cli -- server
```

## Use it

Nothing to set: `http://127.0.0.1:8080` is the default.

```bash
cargo run -p ludy-cli -- mkdrive Footage
```

```bash
cargo run -p ludy-app
```

[Getting started](../getting-started.md) goes on from here.

## Settings

All optional. Copy [`.env.example`](../../.env.example) to `.env` beside the Compose file and uncomment what you want to change.

| Variable | Default | What it is |
|---|---|---|
| `LUDY_PORT` | `8080` | The port on this machine. If you change it, set `LUDY_SERVER` for the app to match |
| `RIVET_TOKEN` | a fixed string | The engine's admin token. Only the server can reach the engine, so the fixed one does for one machine |
| `GC_GRACE_MS` | `86400000` (a day) | How long a shard no version uses is kept before the collector deletes it |
| `RIVET_NODE_LOST_MS` | `2000` | How long the engine waits for a server that went silent before it gives its drives away. See [stopping and starting](#stopping-and-starting) |

The server's own settings are in [the provider's README](../../providers/rivet/README.md#settings).

## Where the data is

Three Docker volumes, named after the Compose project (`ludy` by default):

| Volume | Holds |
|---|---|
| `ludy_rivet-log`, `ludy_rivet-objects` | The engine's state, which includes every drive's database: the tree, the versions, which shards are in use |
| `ludy_ludy-blobs` | The sealed shards and manifests |

A drive needs all three. Backing them up and restoring them was not tried. What opens a drive is not here at all: the keys are in the Mac's keystore, `~/Library/Application Support/Ludy/drives.json`, and that file needs a backup of its own.

## Stopping and starting

Stop it, keeping the drives:

```bash
docker compose down
```

Remove the drives too. This deletes the three volumes and cannot be undone:

```bash
docker compose down -v
```

| What happens | How long a drive answers nothing |
|---|---|
| `docker compose restart ludy` | Under a second |
| `docker compose down`, then `up -d --wait` | `up` took 12 seconds; the drive answered at once after it |
| `kill -9` of the server, then started again | 6.7 seconds |
| `kill -9` of both containers, then `up -d --wait` | `up` took 21 seconds, the drive 2 more |

Each was done once or twice on this Mac (2026-11-22), and each time the drive held what it had. The last two depend on how long the engine waits for a server that went silent, `RIVET_NODE_LOST_MS`. With the engine's own default they took 45 and 52 seconds. What the two-second wait costs is in [known problems](../known-issues.md#the-rivet-provider).

A mounted drive rides through all of these: writes go to the Mac's journal and are sent when the server is back.

## Running the source in the checkout

[A workflow](../../.github/workflows/server-image.yml) publishes the image when the server's source changes. Its tag is the Rivet release it is built against, `rivet-3.1.1`, the same as the engine's. To run what is in the checkout instead:

```bash
docker compose up -d --wait --build
```

That build takes the published image's name on this machine, and Compose keeps using it. To go back to the published one, or to get a newer one:

```bash
docker compose pull ludy
```

## What was checked

- **Both chaos suites** pass against this provider, the server and the engine killed together at every `kill -9` (see [chaos testing](../evidence/chaos-testing.md)). They run the binary from `target/release`, not the image.
- **The Compose file with the published image, 2026-11-23, arm64:** started from nothing; drives created; files uploaded, listed and downloaded with the CLI; a drive mounted, read and written through the Finder's NFS client; a second keystore read back what the first wrote. Details in [what works](../evidence/verification.md).
- **A real mount through kills of this stack:** [`scripts/chaos-mount.sh`](../../scripts/chaos-mount.sh) starts a Compose project of its own, kills both containers with SIGKILL in the middle of a flood, and kills the mounting process with unsent writes. Result in [what works](../evidence/verification.md#chaos-on-a-real-mount).

Not checked: the amd64 image (built, never started), more than one machine, a backup and restore of the volumes, anything other than loopback. More in [known problems](../known-issues.md#the-rivet-provider).
