# The real cloud: known problems and what is not verified

Written 2026-11-22. This covers the encrypted cloud being built beside the simulated one: the format and protocol in `ludy-proto`, the Worker in `worker/`, and the engine in `ludy-core` (`sealed_drive`, `sealed_client`). [The overview](overview.md) says what was built and how it was checked. This file is the other half: what is expected to break, what is fragile, what nobody has checked, and what is not built.

State of the tests on this date, all against local `wrangler dev` 4.166.0:

| Suite | Result |
|---|---|
| `cargo test --workspace` | passes |
| Worker chaos, 12 scenarios | pass |
| Worker chaos soak | 16 seeds pass |
| Engine scenarios, 13 | pass (rerun in full after the last test change, 70 s) |
| Engine soak | 8 seeds pass |

Nothing has run on Cloudflare itself.

## Known problems

No test fails and no wrong result has been seen. Everything in this section comes from reading the code against the platforms' documented limits, or from knowing a shortcut was taken. None of it has been reproduced, because the local emulator enforces none of these limits.

### Sizes the code allows and the platform does not

Each call to R2 counts against a limit per request: 1,000 on Cloudflare's Free plan for calls to its own services (the documentation does not name R2 in that row, so this reading is itself unconfirmed), 10,000 by default on the Paid plan. The page does not say what the Worker sees when it goes over.

| Where | What happens | Size at which it starts |
|---|---|---|
| Commit on the Free plan ([`drive.rs`](../../worker/src/drive.rs) `look_up`) | The drive asks R2 about every shard and manifest in the commit that it has no row for, one call each, and allows 8,000 (`MAX_NEW_BLOBS`). Past the platform's limit the commit should fail with a 5xx, which the engine treats as "try again", so the upload would never finish | A new file of about 1,000 shards: roughly 2 GB at the average shard size, 250 MB if every shard is the minimum 256 KiB |
| `POST /missing` on any plan | It accepts 100,000 names (`MAX_VERSION_SHARDS`) and looks each unknown one up. That is ten times the Paid default | Only another client: the engine asks about 8 MiB of sealed shards at a time, 33 names at most |

The collector deletes at most 500 objects per run, which is inside both limits.

A fix is small: cap `/missing` at the commit's limit, and make that limit a setting the provider supplies. The platform's enforcement cannot be tested locally.

### Wrong on any plan

- **One failing delete stalls the collector.** It deletes up to 500 objects per run and forgets them only after every delete succeeded. If R2 keeps refusing one, the run fails, forgets none, and the next run starts on the same ones. Marked blobs are refused to every commit until the collector finishes, so those shards could not be uploaded again. Forgetting each blob as its delete succeeds would fix it, and the `stop_after` fault switch can test that.
- **Abandoned uploads are never deleted.** A shard sent for a commit that never happens stays in R2 for ever. The drive has no row for it, so the collector does not know it exists.
- **Tombstones are never pruned.** A deleted node keeps its row so the feed can report the deletion. The drive's database only grows.
- **Notices are lost in a crash.** What the engine has to tell the person (a file kept under another name, a change it had to drop) is held in memory until something asks. Nothing asks yet.
- **Anyone who can reach the Worker can do anything.** There is no sign-in. It must not be deployed anywhere public as it is.
- **The drive's root key is wrapped by itself**, as a stand-in until keys are wrapped for people. `DriveKeys` exists only in memory in the tests; there is no keystore.

### Fragile

- **`transaction_sync` reaches into the `worker` crate's internals.** The crate does not bind the runtime's `transactionSync`, so [`drive.rs`](../../worker/src/drive.rs) takes the storage object out of `State` with `_inner()` and calls the method by name. An upgrade of `worker` can break it, and then commits fail outright.
- **A bad upload is recognised by the wording of R2's error.** [`lib.rs`](../../worker/src/lib.rs) `blob_put` treats an error containing "checksum" as the sender's mistake (400) and anything else as storage failing (5xx). If R2 words it differently in production, a corrupt upload becomes a 5xx and the engine retries it for ever.
- **`ludy_proto::now_ms()` panics on WebAssembly** (`SystemTime` is not implemented there). The Worker uses the runtime's clock and must not call it. Nothing stops a future change from doing so.
- **One engine test depends on luck.** `sequential_and_concurrent_reads_share_requests` asserts a number of range requests that depends on where shards end, which depends on the drive's random keys. It failed once at `blocks / 3` (23 requests against 21) and now asserts `blocks / 2`. A run could still land above it.
- **A soak seed does not replay.** The seed fixes the order of operations, not the drive's keys, so a failing seed may pass when run again.

## Not verified

### The platform

- **Everything that was seen only on local workerd:** that R2 refuses a body that does not hash to its name; that a refused or panicking commit is rolled back; that a write acknowledged before `kill -9` is there afterwards; that `storage.sync()` makes the collector's marks durable before it deletes; that an alarm fires after a restart.
- **Limits, none of which the emulator enforces:**

| Limit (documented) | What comes near it | Checked |
|---|---|---|
| 8 MB per row or string in a Durable Object's SQLite | A version's shard list is one string of up to 6.4 MB (100,000 shards at 64 characters) | No. The largest version in any test has a handful of shards |
| 30 s of CPU per request by default | A commit of 100,000 shards runs about 200,000 SQL statements in one transaction | No |
| 128 MB of memory per isolate | The Worker reads each uploaded shard whole, up to 16 MiB. The engine sends up to 4 shards of a file at once and uploads up to 8 files at once | No |
| 10 GB of storage per Durable Object (Paid), 1 GB (Free) | One drive's whole tree and history | No. The largest in a test is 1.6 MB for 3,002 nodes |
| Six R2 calls waiting at once | Lookups and deletes go out 32 at a time; the rest should queue | No |

- **Large files.** The limits are design numbers nothing has tested: 100,000 shards in a version, and at most 8,000 shards in one commit that the drive has not seen, about 16 GB of new content. Manifests are one sealed blob, about 6 MB for a 180 GB file.
- **Storage failing.** No test can make R2 or the drive's database fail. How the Worker behaves then is reasoned about only.
- **Cost and latency.** Every number is from loopback with no simulated latency. Nothing measures what a deployed Worker costs per commit or how long a cold Durable Object takes.

### The Worker

- **The collector racing a commit.** A marked shard is refused inside the commit's transaction; a bug put in on purpose confirmed that check matters. The scenario that runs the collector behind every commit never actually produced the race. The in-memory pins that spare a commit a refusal are covered by no test.
- **The request that panics.** Other requests to the drive were answered within 27 ms. How long the server holds on to the one that panicked was not measured.

### The engine

- **A real mount.** `SealedDrive` has the methods the NFS adapter calls, but nothing has ever mounted one. Finder, Spotlight and real applications have not touched it.
- **A patch whose base lost a shard to the collector.** The engine is meant to drop the change and say so. No test produces it.
- **Sizes near the limits.** The largest file in a test is 24 MiB.
- **Speed.** `ludy bench` is not ported, so there is nothing to set beside the old engine's tables. Known without measuring: a new small file is five requests where it was one, and a cold read needs the manifest first.
- **Keeping a file offline** is not written, so not tested.

### The encryption format

- **Nobody else has reviewed it.** The format is checked against a second implementation ([`crypt-v1.mjs`](../../crates/ludy-proto/tests/vectors/crypt-v1.mjs)), written by the same author. That catches coding mistakes. It does not catch a mistake in the design, which both would share.
- **What the server can still see** has not been written down as a threat model: sizes, the shape of the tree, when things change, which files in a drive share shards.
- **Sealing speed** is about 116 MB/s on this Mac with hardware SHA-256 and NEON ChaCha20 off. Turning them on is untried.

## Not built

- **Sign-in and permissions.** Sharing a drive, folder or file is designed around it (a node's key wrapped for the recipient) and not written.
- **Wiring.** The NFS adapter, the app and the CLI still hold the old engine, so nothing mounts a sealed drive yet. That needs a place to keep each drive's root key, which waits for sign-in. `ludy bench` is not ported, and the tests add no latency, so there are no numbers to set beside [the old engine's](../evidence/benchmarks.md).
- **Batching.** A new small file is five requests where it was one (create, which shards are missing, the shard, the manifest, the commit), and a cold read needs the manifest before the bytes. Batching these is not done.
- **Keeping a file offline** (pinning) in the new engine.
- **Showing notices.** Nothing shows the person what the engine has to report.
- **Push.** Clients poll the feed; a WebSocket that wakes them is not built.
- **Other places to run the server.** See [architecture.md](architecture.md).

## Decisions still open

| Question | Today |
|---|---|
| Two Macs create the same folder name at once: merge, or keep both? | Both are kept, one as `name (conflict ab12cd)`. Files inside go with their folder. The old cloud merged them |
| Fork a drive without copying it | Not designed. Shards are kept per drive, so the drive object can count references in one transaction. A fork then needs shards shared between drives |
| Where a drive's root key lives on the Mac | Nowhere. Waits for sign-in |
| Which servers to support besides Cloudflare | See [architecture.md](architecture.md) |
