# Getting started

Ludy runs as a server and an app. The server starts with one Docker command; a script makes demo drives on it; the app lists and mounts them. Below the three steps are every other command, the environment variables, and what building needs.

## Run it

All from the repository root (the folder with `Cargo.toml`). You need Docker running, a Rust toolchain and the Xcode Command Line Tools.

Start the server. It answers on `http://127.0.0.1:8080` when the command returns; the first start pulls its images.

```bash
docker compose up -d --wait
```

Make demo drives. This creates three drives, uploads the 95 MB demo video and a few text files for real, and adds files of 12 to 180 GB whose content is generated on read, so they take no storage ([synthetic files](how-it-works/encryption.md#synthetic-files)). The drives' keys go into this Mac's keystore.

```bash
scripts/seed-demo.sh ../e58b62c20765eacd37457c1a53d2e97ace458f85.mp4
```

Start the app.

```bash
cargo run -p ludy-app
```

Click **Mount** on a drive. It mounts at `~/Ludy/<drive>` and appears under Locations in the Finder sidebar as the server `ludy.localhost`, with the drive inside it and an eject button. **Open** browses the drive inside the app, mounted or not. Quitting the app, or Ctrl-C in its terminal, ejects mounted drives first.

Other ways to run the server, without Docker or on Cloudflare, are in [hosting](hosting/README.md).

## Drives and their keys

A drive is end-to-end encrypted, and the server keeps no list of drives. What opens a drive is its key, which the Mac that created it keeps in `~/Library/Application Support/Ludy/drives.json`. More in [the engine](how-it-works/engine.md#drives-and-keys).

- Lose that file and the drives in it cannot be opened by anyone. Back it up, or keep each drive's key somewhere safe.
- To open a drive on another Mac, print its key on the first (`ludy key Footage`, or **Copy drive key** in the app) and add it on the second (`ludy add <key>`, or the arrow button beside **Drives** in the app's sidebar). Both Macs must reach the same server.
- A key gives everything: there is no read-only access and no way to take a key back.

## The command line

`ludy` is the engine without a window. Run it with `cargo run -p ludy-cli --` followed by:

| Command | What it does |
|---|---|
| `server` | Say what answers at `LUDY_SERVER` |
| `drives` | List the drives this Mac holds a key for on that server |
| `mkdrive <drive>` | Create a drive and keep its key here |
| `key <drive>` | Print a drive's key |
| `add <key> [name]` | Add a drive from its key |
| `forget <drive>` | Remove a drive's key from this Mac. The drive stays on the server |
| `ls <drive> [folder]` | List a folder |
| `put <drive> <file-or-folder> [path]` | Upload a file, or a folder and everything in it, and wait until the server has it |
| `get <drive> <path> <dest>` | Download a file, or a folder and everything in it |
| `synth <drive> <path> <size> [seed]` | Add a file of generated content, for example `50GB` |
| `mount <drive> [--read-only] [--nobrowse] [--root DIR]` | Mount until Ctrl-C, then eject |
| `bench [--quick] [--label NAME] [--out FILE] [--dir DIR]` | Time fixed scenarios in a drive of its own |

A drive can be open in one process at a time on a Mac. While the app has a drive open or mounted, `ludy ls` and the rest say so and stop.

## Other commands

| Command | What it does |
|---|---|
| `cargo run -p ludy-app -- --mount Footage` | Mount a drive at launch |
| `cargo run -p ludy-app -- --open Footage/raw` | Start in the file browser (`--list` for the list view) |
| `scripts/seed-stress.sh` then `cargo run -p ludy-app -- --open Stress/clips --perf` | Make a drive with a folder of 5,000 small files, scroll it by itself for a few seconds, print frame times, quit |
| `cargo run -p ludy-app -- --click 327,345` | Post a click to the app's own window at that point (also `--rclick`, `--dclick`, repeatable; `--demo menu\|entry-menu\|key\|new-drive` opens that surface; `--size 820x560` sets the window size) |
| `cargo run --release -p ludy-cli -- bench` | Time fixed engine scenarios against the server and print a table (`--quick` for small sizes) |
| `cargo test` | The tests that need no server |
| `cargo test -p ludy-core --test sealed_engine -- --ignored --test-threads=1` | The engine against a server the test starts; see [chaos testing](evidence/chaos-testing.md#running-them) for what it needs |
| `scripts/chaos-mount.sh <work-dir>` | Flood, copy and kill the server and then the mounting process, on a hidden mount (needs Docker and `cargo build --release -p ludy-cli`) |
| `scripts/chaos-r2.sh <work-dir> [rounds]` | The chaos suites in a loop with the server's blobs in the R2 bucket `.env` names, taking the bucket away as well; see [chaos testing](evidence/chaos-testing.md#the-store-taken-away) |

Set `LUDY=target/release/ludy` to make the two seed scripts use a built binary instead of `cargo run`.

## Environment variables

| Variable | Default | What it is |
|---|---|---|
| `LUDY_SERVER` | `http://127.0.0.1:8080` | The server's address. `https://` works |
| `LUDY_HOME` | `~/Library/Application Support/Ludy` | Where the keystore (`drives.json`), the journals of unsent writes and the record of stuck ejects are kept |
| `LUDY_CACHE_DIR` | `~/Library/Caches/Ludy/blocks` | The block cache. It holds decrypted content |
| `LUDY_CACHE_MB` | `2000` | The cache's size limit, in megabytes |
| `LUDY_MOUNT_ROOT` | `~/Ludy` | Drives mount at `<root>/<drive>` |
| `LUDY_READ_ONLY` | not set | Set it to mount read-only |
| `LUDY_NOBROWSE` | not set | Set it to keep mounts out of the Finder's sidebar |

## Building

Building the app and the CLI needs a Rust toolchain and the Xcode Command Line Tools. gpui is built with its `runtime_shaders` feature, so the Metal shader compiler that ships with full Xcode is not required. The server's two providers build separately and need more; each [hosting](hosting/README.md) page says what.
