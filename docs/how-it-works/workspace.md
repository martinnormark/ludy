# The workspace

Five crates. The engine does not depend on the server, so another client could link the engine, or use the HTTP API directly.

## Crates

| Crate | What it is |
|---|---|
| [`ludy-proto`](../../crates/ludy-proto) | What a client and the cloud agree on: the JSON types both sides already shared (listings, the change feed, versions, the request log), the shard cutting rules and manifests, synthetic content. No network or async code. The commit reply, the server stats and the request bodies are still written out separately on each side |
| [`ludy-cloud`](../../crates/ludy-cloud) | The simulated cloud: library plus the `ludy-cloud` server binary |
| [`ludy-core`](../../crates/ludy-core) | The engine: cloud client, block cache, drive filesystem, patch planner; with the `mount` feature (on by default) also the NFS adapter, the mount manager and the desktop `Engine` |
| [`ludy-app`](../../crates/ludy-app) | The gpui app |
| [`ludy-cli`](../../crates/ludy-cli) | `ludy`, the engine without a window |

These five are the system the app mounts today. The encrypted cloud being built beside it adds a sixth crate, [`ludy-server`](../../crates/ludy-server), and two packages that run it, [`providers/rivet`](../../providers/rivet) and [`providers/cloudflare`](../../providers/cloudflare); see [its architecture](../real-cloud/architecture.md#packages).

## Other clients

The engine does not depend on the server: `ludy-core` uses `ludy-proto` for the wire types, and `ludy-cloud` only in its tests. Built with `--no-default-features`, `ludy-core` is the part with no NFS and no macOS-specific code, though it does use Unix file calls (`CloudClient`, `BlockCache`, `DriveFs`, the journal, the patch planner), and its tests pass in that build (23 unit, 21 engine, 24 chaos; the one left out tests the NFS adapter). That is the piece a second client would link: it would put its own front end (on iOS, a File Provider extension) where [`nfs.rs`](../../crates/ludy-core/src/nfs.rs) is now. It has only been built for macOS. A client that does not want the engine at all can use the HTTP API directly: folder listings with `prefix` and `delimiter`, `Range` reads, a plain `PUT`. The cloud has no sign-in, no HTTPS and no CORS headers yet, and the engine's HTTP client is built without TLS.
