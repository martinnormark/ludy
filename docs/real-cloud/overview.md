# The real cloud, in progress

An end-to-end encrypted cloud is being built beside the simulated one: an encryption format, a protocol for a drive the server cannot read, an engine for it, and a server whose logic is one crate that runs in two places: self-hosted on Rivet actors (`docker compose up`) and as a Cloudflare Worker. Both pass the same chaos suites locally. The app does not use it yet, and nothing has been deployed.

Everything else in these docs runs against the simulated cloud. Its replacement is being built beside it, and the app, the CLI and the mount do not use it yet. The decisions behind it: end-to-end encryption, file and folder names included; a server that can be self-hosted or run on Cloudflare; Cloudflare R2 or any S3 for the blobs; all of it in Rust.

## The pieces

| Piece | What it is |
|---|---|
| [`ludy_proto::crypt`](../../crates/ludy-proto/src/crypt.rs) | The encryption format: shards sealed in 64 KiB frames so a range read fetches only its frames, shard keys and cut points derived from content and a per-drive secret, a key per file and folder wrapped by its parent's, sealed names and manifests. The description is at the top of the file |
| [`ludy_proto::tree`](../../crates/ludy-proto/src/tree.rs) | The protocol for a drive the server cannot read: a tree of nodes with ids, commits of ops applied together or not at all, and a feed of what changed since a sequence number |
| [`ludy_core::sealed_drive`](../../crates/ludy-core/src/sealed_drive.rs) | The engine for it, beside the old one ([`drive`](../../crates/ludy-core/src/drive.rs)) and with the same methods the NFS adapter calls. The tree comes from the feed, each key unwrapped from the root key down; a read goes through the file's sealed manifest and fetches only the frames it needs; an upload cuts and seals what changed, sends the sealed shards the cloud lacks and commits a sealed manifest; creates, moves and deletes are queued ops sent in commits that are safe to send twice. [`sealed_client`](../../crates/ludy-core/src/sealed_client.rs) is its HTTP side |
| [`ludy-server`](../../crates/ludy-server) | The cloud's logic with no platform in it: a drive's tables (the tree, 25 versions per file, which blobs are in use), the rules of a commit, the feed, and a collector that deletes from storage what no version has used for a day. A provider supplies SQLite, a blob store and a timer through its ports. Its own tests run it on SQLite in memory and can make storage fail |
| [`providers/rivet`](../../providers/rivet) | The self-hosted provider, and what `docker compose up` starts: a Rivet actor per drive, an HTTP front that hashes every upload, blobs in a folder, Cloudflare R2 or any S3 |
| [`providers/cloudflare`](../../providers/cloudflare) | The Cloudflare provider: a Worker, one Durable Object per drive with SQLite storage, sealed shards and manifests in R2, checked by R2 against their names |

How the three fit together, and why Rivet could not be the Cloudflare option as well, is in [architecture.md](architecture.md).

## Running it self-hosted

```bash
docker compose up -d --wait
```

That starts the Rivet engine and the server, on port 8080 of this machine only, with blobs in a Docker volume. The settings, and the files that move the blobs to R2 or MinIO, are in [`providers/rivet/README.md`](../../providers/rivet/README.md).

The Cloudflare Worker is set up as [`providers/cloudflare/README.md`](../../providers/cloudflare/README.md) says.

## How it was checked

- **The format against a second implementation.** [`crypt-v1.mjs`](../../crates/ludy-proto/tests/vectors/crypt-v1.mjs) implements it with Node's built-in crypto and writes the test vectors; the Rust code has to reproduce them. Unit tests cover damage (a flipped byte, swapped frames, a missing tail, a frame of another shard, a key, name or manifest moved to another node).
- **The engine against the Worker.** [`tests/sealed_engine.rs`](../../crates/ludy-core/tests/sealed_engine.rs): 13 scenarios and a soak, each ending with the drive read back from outside by a reader that holds the root key and shares none of the engine's code.
  - Listing downloads no content; a 64 KiB read in the middle of a 24 MiB file is one range request for one block (171 to 263 KB sealed, by where the read falls in its shard); identical content under another name uploads about a kilobyte, the sealed manifest; unsent writes, renames and deletes survive the app being killed; a reply lost before a crash does not duplicate a file.
  - Two Macs on one drive: the same name created twice keeps both files, one as `name (conflict ab12cd)`; the last writer of a file wins and the other version stays in its history; a file rewritten here and deleted there is saved again, a file saved into a folder that was deleted there is kept at the top of the drive, and a partial change to a file that is gone is dropped and reported.
  - The soak does random file work on one Mac while the Worker is killed and frozen, requests and replies are lost, the app itself is killed and started again, and another Mac writes alongside; 8 seeds pass (3,100 steps, 92 kills of the Worker, 87 of the app, 82 freezes), every read compared with what was written.
  - In the last three the Mac's cache is small and wiped whenever the app is killed, so that reads come from the cloud through the faults (11 MB in 178 range requests); in the first five nearly every read was answered from the journal.
  - The seed fixes the order of operations, not the drive's keys, so a run does not repeat byte for byte.
  - Five bugs put into the engine on purpose were each caught, one only after a test was strengthened.
- **The Worker under chaos.** [`tests/worker_chaos.rs`](../../crates/ludy-core/tests/worker_chaos.rs) drives it under `wrangler dev` with a client that seals everything for real and repeats a request until it is answered, while the Worker's processes are killed with `kill -9`, frozen and restarted and a proxy loses requests and replies.
  - A model predicts the outcome of every commit, refusals included.
  - The oracle at the end: the drive's own recount of everything it stores twice, the tree read back and decrypted from the root key equal to the model's, every file's content as written, and storage holding exactly the blobs it should.
  - Twelve scenarios and a seeded soak; 16 seeds of the soak pass, 500 steps each for all but the first.
  - The last four ran on the final code, where a `kill -9` lands on a burst of creates, new versions and deletes of files and whole folders (2,000 steps, 2,208 commits, 57 kills, 918 lost messages); the numbers are in [`bench/2026-11-22-worker-chaos.md`](../../bench/2026-11-22-worker-chaos.md).
  - Four bugs put into the drive object on purpose, one at a time, were each caught.

- **The drive by itself.** [`ludy-server/tests/drive.rs`](../../crates/ludy-server/tests/drive.rs) runs the drive on SQLite in memory, with no server: the rules of a commit, the feed, the collector, a panic inside a transaction. It can make storage fail, which no real provider can be asked to do: the database fails at every statement of a commit in turn, and all of the commit or none of it remains.
- **Both providers under the same suites.** The two suites above were written against the Worker. `LUDY_PROVIDER=rivet` runs them against the self-hosted server: `ludy-rivet` beside a Rivet engine in a container, both killed at each `kill -9`. All 13 and 14 tests pass on each provider (2026-11-22, after the server's logic moved into `ludy-server`). One extra engine soak on Rivet failed its check of the server's log while every file was right; what that was is in [known problems](known-issues.md#the-rivet-provider). The tables are in [`bench/2026-11-22-providers-chaos.md`](../../bench/2026-11-22-providers-chaos.md).
- **The Compose files, by hand.** `docker compose up` was built and started, a drive was created, written and read, both containers were killed with SIGKILL and started again, and the drive was as it had been. The same with MinIO as the blob store, plus a run of the collector, to try the S3 path. The R2 file was not started: that needs an account. Later the stack was timed through a restart, a `down` and `up`, `kill -9` of one and of both containers, and a paused server; the numbers are in the same bench file and what they mean for running it is in [the Rivet provider's README](../../providers/rivet/README.md#stopping-and-starting).

On Cloudflare (the default):

```bash
cargo test -p ludy-core --test worker_chaos -- --ignored --test-threads=1
```

```bash
cargo test -p ludy-core --test sealed_engine -- --ignored --test-threads=1
```

On Rivet, with Docker running:

```bash
cargo build --release -p ludy-rivet
```

```bash
LUDY_PROVIDER=rivet cargo test -p ludy-core --test worker_chaos -- --ignored --test-threads=1
```

```bash
LUDY_PROVIDER=rivet cargo test -p ludy-core --test sealed_engine -- --ignored --test-threads=1
```

## What this found, all fixed

| Found | Now |
|---|---|
| A panic inside a commit left the drive answering nothing for 30 seconds: the commit ran in the storage API's asynchronous transaction, whose promise never settled, and every other request to the drive waited behind it until the runtime gave up | Commits run in the synchronous transaction (`transactionSync`, called directly since the `worker` crate does not bind it). A panic undoes the commit, and other requests to the drive were answered within 27 ms. The request that panicked got no answer within the client's 3 seconds; how long the server holds on to it was not measured. The call reaches the storage object through the crate's internals, so an upgrade of `worker` can break it |
| The protocol had nowhere to put a file's modification time (found when the engine needed it) | Each version carries it sealed under the file's key, next to the node, so a folder lists with dates without fetching a manifest and the cloud does not see them |
| The engine's message that a change could not be saved was erased by the next successful upload, so nobody would have seen it | Such notices are kept apart from transient errors until they are read |
| Six readers of one place in a file counted as one reader moving forward and set off read-ahead | Only a read that gets further extends the streak |
| A collector cut short (through a fault switch, then `kill -9`) leaves its shards marked, and a commit wanting one back is refused until the collector finishes. Each refusal set the collector's alarm 50 ms ahead again, so a client retrying faster than that postponed it for ever | The alarm is never moved later than it is already set |
| On Rivet, a panic inside a commit left the drive answering nothing for 36 seconds: the transaction stayed open, and the actor that was restarted after the panic waited for it. Found by the panic scenario the first time the suite ran against Rivet | The adapter rolls the transaction back before it lets the panic go on. The request that panicked gets a 502 at once, and the others were answered within 41 ms |

## Read next

- [Known problems and what is not verified](known-issues.md): what is expected to break, what is fragile, what nobody has checked, what is not built, and the decisions still open.
- [Architecture](architecture.md): what runs where, what a provider must promise, and how each of the two keeps it.
- [Worker chaos numbers](../../bench/2026-11-22-worker-chaos.md): the raw tables.
