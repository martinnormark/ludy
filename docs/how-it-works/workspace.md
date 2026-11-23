# The workspace

Five crates and two provider packages. The engine and the server share one crate, the protocol, and nothing else, so another client could link the engine, or speak the protocol directly.

## Crates

| Crate | What it is |
|---|---|
| [`ludy-proto`](../../crates/ludy-proto) | What a client and the server agree on: the node-tree protocol ([`tree`](../../crates/ludy-proto/src/tree.rs)), the encryption format ([`crypt`](../../crates/ludy-proto/src/crypt.rs)), the cutting parameters, synthetic content. No network or async code; it builds for WebAssembly |
| [`ludy-core`](../../crates/ludy-core) | The engine: the server's client, the block cache, the drive as a filesystem, the keystore, the patch planner; with the `mount` feature (on by default) also the NFS adapter, the mount manager and the desktop `Engine` |
| [`ludy-app`](../../crates/ludy-app) | The gpui app |
| [`ludy-cli`](../../crates/ludy-cli) | `ludy`, the engine without a window |
| [`ludy-server`](../../crates/ludy-server) | The server's logic with no platform in it: a drive's tables, the rules of a commit, the feed, the collector |

## Providers

| Package | What it is |
|---|---|
| [`providers/rivet`](../../providers/rivet) | `ludy-rivet`: the server on Rivet actors, which `docker compose up` starts. In the workspace, left out of a plain `cargo build` because it is a large build |
| [`providers/cloudflare`](../../providers/cloudflare) | The server as a Cloudflare Worker. A workspace of its own, because it only builds for WebAssembly |

How the server's crate and its providers fit together is in [the server](server.md). How to run each is in [hosting](../hosting/README.md).

## Other clients

The engine does not depend on the server: `ludy-core` uses `ludy-proto` for the protocol and the format, and `ludy-server` not at all. Built with `--no-default-features`, `ludy-core` is the part with no NFS and no macOS-specific code, though it does use Unix file calls (`SealedClient`, `BlockCache`, `SealedDrive`, the keystore, the journal, the patch planner). That is the piece a second client would link: it would put its own front end (on iOS, a File Provider extension) where [`nfs.rs`](../../crates/ludy-core/src/nfs.rs) is now. It has only been built for macOS.

A client that does not want the engine has to do what the engine does: the protocol is in [the server](server.md#the-protocol-is-the-boundary), and everything it carries is sealed as [`crypt.rs`](../../crates/ludy-proto/src/crypt.rs) describes. A second implementation of the format exists in JavaScript, for the test vectors only: [`crypt-v1.mjs`](../../crates/ludy-proto/tests/vectors/crypt-v1.mjs). The server sends no CORS headers, so a browser cannot talk to it today.
