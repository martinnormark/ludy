# Getting started

Ludy runs from three terminals: the simulated cloud, a script that fills it with demo drives, and the app. Below the three steps are the other ways to start things, the environment variables, and what building needs.

## Run it

Three terminals, all from the repository root (the folder with `Cargo.toml`).

Start the simulated cloud. Data goes to `./data`; the request log prints to the terminal.

```bash
cargo run -p ludy-cloud -- --data ./data --latency-ms 30 --mbps 400
```

Fill it with demo drives. This uploads the 95 MB demo video for real and adds synthetic files of 12 to 180 GB that take no disk.

```bash
scripts/seed-demo.sh ../e58b62c20765eacd37457c1a53d2e97ace458f85.mp4
```

Start the app.

```bash
cargo run -p ludy-app
```

Click **Mount** on a drive. It mounts at `~/Ludy/<drive>` and appears under Locations in the Finder sidebar as the server `ludy.localhost`, with the drive inside it and an eject button. **Open** browses the drive inside the app, mounted or not. Quitting the app, or Ctrl-C in its terminal, ejects mounted drives first.

## Other commands

| Command | What it does |
|---|---|
| `cargo run -p ludy-app -- --mount Footage` | Mount a drive at launch |
| `cargo run -p ludy-app -- --open Footage/raw` | Start in the file browser (`--list` for the list view) |
| `scripts/seed-stress.sh` then `cargo run -p ludy-app -- --open Stress --perf` | Make a drive of 5,000 synthetic files, scroll it by itself for a few seconds, print frame times, quit |
| `cargo run -p ludy-app -- --click 327,345` | Post a click to the app's own window at that point (also `--rclick`, `--dclick`, repeatable; `--demo menu\|entry-menu\|share\|new-drive` opens that surface; `--size 820x560` sets the window size) |
| `cargo run -p ludy-cli -- mount Footage` | Mount from the terminal, no window; Ctrl-C ejects |
| `cargo run -p ludy-cli -- log --follow` | Tail the cloud's range log |
| `cargo run --release -p ludy-cli -- bench` | Time fixed engine scenarios against a cloud of its own and print a table (`--quick` for small sizes; `--latency-ms 0` or `--mbps 0` to see what is round trips and what is bytes) |
| `cargo test -p ludy-core --test chaos` | The chaos tests; add `soak -- --ignored --nocapture` for the randomized run (`CHAOS_SEED`, `CHAOS_STEPS`) |
| `scripts/chaos-mount.sh <work-dir>` | Flood, copy and `kill -9` the cloud and then the mounting process, on a hidden mount (needs the release builds) |
| `curl -X PUT 'http://127.0.0.1:9000/_config?mbps=50&latency_ms=120'` | Slow the link down while running |

## The encrypted cloud, self-hosted

The app does not use this cloud yet; this starts its server. From the repository root, with Docker running:

```bash
docker compose up -d
```

It listens on `http://127.0.0.1:8080`. The first start builds the server's image, which takes several minutes. The settings, and how to keep the blobs in Cloudflare R2 or MinIO, are in [the Rivet provider's README](../providers/rivet/README.md); the Cloudflare Worker is set up as [the overview](real-cloud/overview.md#setting-up-the-worker) says.

## Environment variables

Environment variables: `LUDY_SERVER` (default `http://127.0.0.1:9000`), `LUDY_CACHE_MB` (default 2000), `LUDY_CACHE_DIR` (default `~/Library/Caches/Ludy/blocks`), `LUDY_MOUNT_ROOT` (default `~/Ludy`), `LUDY_READ_ONLY`, `LUDY_NOBROWSE`.

## Building

Building needs only the Xcode Command Line Tools. gpui is built with its `runtime_shaders` feature, so the Metal shader compiler that ships with full Xcode is not required.
