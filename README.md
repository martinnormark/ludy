# Ludy

End-to-end encrypted cloud drives that mount in the Finder and stream on demand. A drive is listed in full, opens in any app, and takes almost no local disk: bytes are fetched from the server only when an application reads them. File content and names are sealed on the Mac; the server stores what it cannot read.

The server is yours to run: one `docker compose up` on a machine with Docker, or a Cloudflare Worker. The Mac app is written in Rust with [gpui](https://gpui.rs/).

## Status

- **It runs on a real server.** The app, the CLI and the mount work against either provider of the server, and both providers pass the same chaos suites (2026-11-23). The local simulated server the project started on is gone.
- **The server has only run on one Mac.** With Docker Compose it was tried with its blobs in a volume, in MinIO and in a real Cloudflare R2 bucket. The Worker has never been deployed to Cloudflare, and no second machine has been involved.
- **There is no sign-in.** Whoever can reach a server can use it, so it has to stay on a machine or network you trust. A drive is opened by its key, which is kept in a file on the Mac.
- **Not checked on the current engine:** the Finder, Quick Look and video playback from a mount, and most of the app's controls.

More in [docs/status.md](docs/status.md).

## Run it

Three commands, all from this folder. You need Docker running, a Rust toolchain and the Xcode Command Line Tools.

Start the server. It answers on `http://127.0.0.1:8080` when this returns.

```bash
docker compose up -d --wait
```

Make three demo drives on it.

```bash
scripts/seed-demo.sh ../e58b62c20765eacd37457c1a53d2e97ace458f85.mp4
```

Start the app, then click **Mount** on a drive. It mounts at `~/Ludy/<drive>` and appears in the Finder sidebar.

```bash
cargo run -p ludy-app
```

Every other command, and the environment variables, are in [docs/getting-started.md](docs/getting-started.md). Other ways to run the server are in [docs/hosting](docs/hosting/README.md).

## If an eject gets stuck

Look for a macOS permission prompt (*"…" would like to access files on a network volume*) and answer it. Do not `ls -la` the mount root in the meantime. Why this happens and what Ludy does about it: [the mount](docs/how-it-works/mount.md#eject-and-the-network-volume-prompt).

## Read more

| If you want to know | Read |
|---|---|
| How to run the server: Compose, R2 or another S3, Cloudflare | [Hosting](docs/hosting/README.md) |
| How a read, a write and a remote change travel, and where keys are kept | [The engine](docs/how-it-works/engine.md) |
| What is encrypted, how, and what the server still sees | [Encryption and shards](docs/how-it-works/encryption.md) |
| How one server runs on two providers | [The server](docs/how-it-works/server.md) |
| Why it mounts over loopback NFS | [The mount](docs/how-it-works/mount.md) |
| What the app looks like | [The app](docs/how-it-works/app.md) |
| Which crate is what | [The workspace](docs/how-it-works/workspace.md) |
| What was actually checked, and how | [What works](docs/evidence/verification.md) |
| What happens when things die mid-work | [Chaos testing](docs/evidence/chaos-testing.md) |
| How fast it is | [Benchmarks](docs/evidence/benchmarks.md) |
| What is broken, fragile, unchecked or not built | [Known problems](docs/known-issues.md) |

The full map is [docs/README.md](docs/README.md).

## Layout

| Folder | What it is |
|---|---|
| [`crates/ludy-proto`](crates/ludy-proto) | What a client and the server agree on: the protocol and the encryption format |
| [`crates/ludy-core`](crates/ludy-core) | The engine |
| [`crates/ludy-app`](crates/ludy-app) | The gpui app |
| [`crates/ludy-cli`](crates/ludy-cli) | `ludy`, the engine without a window |
| [`crates/ludy-server`](crates/ludy-server) | The server's logic, with no platform in it |
| [`providers/rivet`](providers/rivet) | The provider `docker compose up` starts: Rivet actors |
| [`providers/cloudflare`](providers/cloudflare) | The Cloudflare provider: a Worker |
| [`scripts`](scripts) | Demo drives, a stress drive, and chaos on a real mount |
| [`bench`](bench) | Raw benchmark and chaos tables, by date |
| [`docs`](docs) | Everything else |
