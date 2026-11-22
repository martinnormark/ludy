# Ludy

Cloud drives that mount in the Finder and stream on demand. A drive holding hundreds of gigabytes is listed in full, opens in any app, and takes almost no local disk: bytes are fetched from the cloud only when an application reads them.

The "cloud" here is simulated: a local S3-like server with configurable latency and bandwidth. The Mac app is written in Rust with [gpui](https://gpui.rs/).

## Status

- **The simulated cloud, the engine and the app** are built, covered by automated tests, and checked against real mounts (2026-11-19).
- **A real, end-to-end encrypted cloud** is being built beside the simulated one. Its server can be self-hosted (`docker compose up`, on Rivet actors, with blobs in a folder, Cloudflare R2 or any S3) or run as a Cloudflare Worker; both pass the same chaos tests locally. The app does not use it yet and nothing has been deployed (2026-11-22).
- **Not checked:** many of the app's controls, and the Quick Look panel itself.

More in [docs/status.md](docs/status.md).

## Run it

Three terminals, all from this folder. Building needs only the Xcode Command Line Tools.

Start the simulated cloud.

```bash
cargo run -p ludy-cloud -- --data ./data --latency-ms 30 --mbps 400
```

Fill it with demo drives.

```bash
scripts/seed-demo.sh ../e58b62c20765eacd37457c1a53d2e97ace458f85.mp4
```

Start the app, then click **Mount** on a drive. It mounts at `~/Ludy/<drive>` and appears in the Finder sidebar.

```bash
cargo run -p ludy-app
```

Every other command, and the environment variables, are in [docs/getting-started.md](docs/getting-started.md).

## If an eject gets stuck

Look for a macOS permission prompt (*"…" would like to access files on a network volume*) and answer it. Do not `ls -la` the mount root in the meantime. Why this happens and what Ludy does about it: [the mount](docs/how-it-works/mount.md#eject-and-the-network-volume-prompt).

## Read more

| If you want to know | Read |
|---|---|
| How a read, a write and a remote change travel | [The engine](docs/how-it-works/engine.md) |
| Why it mounts over loopback NFS | [The mount](docs/how-it-works/mount.md) |
| How the cloud stores content and what an edit uploads | [The simulated cloud and shards](docs/how-it-works/simulated-cloud.md) |
| What the app looks like | [The app](docs/how-it-works/app.md) |
| Which crate is what | [The workspace](docs/how-it-works/workspace.md) |
| What was actually checked, and how | [What works](docs/evidence/verification.md) |
| How fast it is | [Benchmarks](docs/evidence/benchmarks.md) |
| What happens when things die mid-work | [Chaos testing](docs/evidence/chaos-testing.md) |
| Where the encrypted cloud stands | [The real cloud](docs/real-cloud/overview.md) |
| What is broken, fragile or unchecked there | [Known problems](docs/real-cloud/known-issues.md) |
| How it could run self-hosted | [Architecture](docs/real-cloud/architecture.md) |

The full map is [docs/README.md](docs/README.md).

## Layout

| Folder | What it is |
|---|---|
| [`crates/ludy-proto`](crates/ludy-proto) | What a client and the cloud agree on, including the encryption format |
| [`crates/ludy-core`](crates/ludy-core) | The engine |
| [`crates/ludy-cloud`](crates/ludy-cloud) | The simulated cloud |
| [`crates/ludy-app`](crates/ludy-app) | The gpui app |
| [`crates/ludy-cli`](crates/ludy-cli) | `ludy`, the engine without a window |
| [`worker`](worker) | The real cloud's server, a Cloudflare Worker |
| [`bench`](bench) | Raw benchmark tables |
| [`docs`](docs) | Everything else |
