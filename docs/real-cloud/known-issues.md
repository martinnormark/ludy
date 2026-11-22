# The real cloud: known problems and what is not verified

Written 2026-11-22. This covers the encrypted cloud being built beside the simulated one: the format and protocol in `ludy-proto`, the server's logic in `ludy-server`, its two providers (Rivet actors in `providers/rivet`, the Cloudflare Worker in `providers/cloudflare`), and the engine in `ludy-core` (`sealed_drive`, `sealed_client`). [The overview](overview.md) says what was built and how it was checked. This file is the other half: what is expected to break, what is fragile, what nobody has checked, and what is not built.

State of the tests on this date, after the server's logic moved into `ludy-server`. Cloudflare means local `wrangler dev` 4.166.0; Rivet means `ludy-rivet` beside a `rivetdev/engine:3.1.1` container, both killed at each `kill -9`.

| Suite | Cloudflare | Rivet |
|---|---|---|
| `cargo test` | passes | |
| `ludy-server`'s own, 14 tests on SQLite in memory | pass | |
| Worker chaos, 12 scenarios | pass | pass |
| Worker chaos soak | 3 runs pass | 8 runs pass: 3 of them 500 steps, one with only the server killed, one with only the engine |
| Engine scenarios, 13 | pass | pass |
| Engine soak | 2 runs pass | 6 runs pass, one each with only the server or only the engine killed; 1 failed its log check, [below](#the-rivet-provider) |

Before the move, 16 seeds of the Worker soak and 8 of the engine soak passed on Cloudflare; those were not all run again. The raw tables of the runs after the move are in [`bench/2026-11-22-providers-chaos.md`](../../bench/2026-11-22-providers-chaos.md).

Nothing has run on Cloudflare itself, on Cloudflare R2, or on a Rivet engine of more than one instance.

## Known problems

No test fails and no wrong result has been seen. Everything in this section comes from reading the code against the platforms' documented limits, or from knowing a shortcut was taken. None of it has been reproduced, because the local emulator enforces none of these limits.

### Sizes the code allows and Cloudflare does not

Each call to R2 counts against a limit per request: 1,000 on Cloudflare's Free plan for calls to its own services (the documentation does not name R2 in that row, so this reading is itself unconfirmed), 10,000 by default on the Paid plan. The page does not say what the Worker sees when it goes over.

| Where | What happens | Size at which it starts |
|---|---|---|
| Commit on the Free plan ([`drive.rs`](../../crates/ludy-server/src/drive.rs) `look_up`) | The drive asks R2 about every shard and manifest in the commit that it has no row for, one call each, and allows 8,000 (`MAX_NEW_BLOBS`). Past the platform's limit the commit should fail with a 5xx, which the engine treats as "try again", so the upload would never finish | A new file of about 1,000 shards: roughly 2 GB at the average shard size, 250 MB if every shard is the minimum 256 KiB |
| `POST /missing` on any plan | It accepts 100,000 names (`MAX_VERSION_SHARDS`) and looks each unknown one up. That is ten times the Paid default | Only another client: the engine asks about 8 MiB of sealed shards at a time, 33 names at most |

The collector deletes at most 500 objects per run, which is inside both limits.

A fix is small: cap `/missing` at the commit's limit, and make that limit a setting the provider supplies. The platform's enforcement cannot be tested locally.

### Wrong on every provider

- **One failing delete stalls the collector.** It deletes up to 500 objects per run and forgets them only after every delete succeeded. If storage keeps refusing one, the run fails, forgets none, and the next run starts on the same ones. Marked blobs are refused to every commit until the collector finishes, so those shards could not be uploaded again. Forgetting each blob as its delete succeeds would fix it, and the `stop_after` fault switch can test that.
- **Abandoned uploads are never deleted.** A shard sent for a commit that never happens stays in storage for ever. The drive has no row for it, so the collector does not know it exists.
- **Tombstones are never pruned.** A deleted node keeps its row so the feed can report the deletion. The drive's database only grows.
- **Notices are lost in a crash.** What the engine has to tell the person (a file kept under another name, a change it had to drop) is held in memory until something asks. Nothing asks yet.
- **Anyone who can reach the server can do anything.** There is no sign-in. It must not be deployed anywhere public as it is. `docker-compose.yml` publishes the port on the machine itself only for this reason.
- **The drive's root key is wrapped by itself**, as a stand-in until keys are wrapped for people. `DriveKeys` exists only in memory in the tests; there is no keystore.

### The Rivet provider

- **After `kill -9` of the server, its drives answer nothing until the engine gives up on the dead node.** `docker-compose.yml` and the chaos rig both shorten that wait to two seconds (`pegboard.node_lost_threshold` and `node_ping_timeout`), a setting found in the engine's binary, not its documentation. A server stopped with SIGTERM does not wait at all. How long each case took, with that setting and with the engine's default, is in [the provider's README](../../providers/rivet/README.md#stopping-and-starting).
- **A server that stalls for longer than those two seconds loses its drives for a few seconds.** This is the price of the two-second wait. Under Compose, with nothing in flight: after a pause of 4, 10 and 30 seconds a drive answered again 5.4, 1.6 and 2.1 seconds after the server woke, and the server's log held no fault. With the engine's default a 10-second pause cost nothing. Each was run once.
- **A server stopped for longer than the engine waits loses the answer to a commit in flight.** In one engine soak the server was frozen for more than the rig's two seconds; when it woke, the commit it was in the middle of came back from Rivet as `remote_indeterminate_result` (it may or may not have been applied). The drive answers 5xx, the client sends the same commit again, and the commit's id settles it either way; every file in that run held what was written. The run failed only because the line was logged as an error. It is now logged as `fault`, which the suites do not count, so they no longer tell how often this happens. Seen once in seven engine soaks.
- **One engine instance on its own disk is not what Rivet calls production.** Its documentation asks for three or more instances and a bucket, and says of the file-system store that it does not belong in production. What was seen here: writes answered before `kill -9` of the engine container were all there afterwards (80 across four kills in a first trial, and every commit of the soaks). What a lost disk costs was not tried.
- **A folder of blobs is not flushed to disk by us.** With `LUDY_BLOBS` a folder, an upload is answered once the `object_store` crate has written and renamed the file. Whether it also syncs was not checked. A power cut could then lose a shard whose commit, kept by the engine, survives. `kill -9` cannot show this, since the operating system still has the bytes.
- **Rivet 3 is three weeks old** (3.0.0 on 2026-11-01), and its Rust SDK carries no stability label. The crate and the engine image are pinned to 3.1.1 and have to move together.
- **The server holds the engine's admin token.** It is the only token the documented single-machine setup has. A scoped one was not looked into.

### Fragile

- **On Cloudflare, `transaction_sync` reaches into the `worker` crate's internals.** The crate does not bind the runtime's `transactionSync`, so [`drive.rs`](../../providers/cloudflare/src/drive.rs) takes the storage object out of `State` with `_inner()` and calls the method by name. An upgrade of `worker` can break it, and then commits fail outright.
- **On Cloudflare, a bad upload is recognised by the wording of R2's error.** [`lib.rs`](../../providers/cloudflare/src/lib.rs) `blob_put` treats an error containing "checksum" as the sender's mistake (400) and anything else as storage failing (5xx). If R2 words it differently in production, a corrupt upload becomes a 5xx and the engine retries it for ever. The Rivet provider hashes the body itself and does not depend on this.
- **`ludy_proto::now_ms()` panics on WebAssembly** (`SystemTime` is not implemented there). `ludy-server` takes the time from its platform and must not call it. Nothing stops a future change from doing so.
- **On Rivet, a transaction that is left open blocks the drive.** A panic inside a commit once kept a drive unanswered for 36 s, because the restarted actor waited for the transaction. The adapter now rolls back before it lets the panic go on (41 ms in the scenario that covers it). Any other way of abandoning a transaction, such as a request cancelled in the middle, would do the same; the runtime does not cancel a request it has started, as far as its source shows.
- **One engine test depends on luck.** `sequential_and_concurrent_reads_share_requests` asserts a number of range requests that depends on where shards end, which depends on the drive's random keys. It failed once at `blocks / 3` (23 requests against 21) and now asserts `blocks / 2`. A run could still land above it.
- **A soak seed does not replay.** The seed fixes the order of operations, not the drive's keys, so a failing seed may pass when run again.

## Not verified

### Cloudflare

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
- **Storage failing, on a real provider.** `ludy-server`'s own tests fail the database at every statement of a commit in turn, and take the blob store away; all of the commit or none of it remains. No test can make R2, a Durable Object's database or the Rivet engine fail the same way, so each adapter's handling of a failure is reasoned about only.
- **Cost and latency.** Every number is from loopback with no simulated latency. Nothing measures what a deployed Worker costs per commit or how long a cold Durable Object takes.

### Rivet

- **Cloudflare R2 as the blob store.** [`docker-compose.r2.yml`](../../docker-compose.r2.yml) has never been started against an R2 account. The S3 path it shares was tried against MinIO under Compose ([`docker-compose.minio.yml`](../../docker-compose.minio.yml)): upload, read, range read, the lookups of a commit, listing, and the collector's deletes. The chaos suites ran with a folder only.
- **Requests near the size limits through the engine's gateway.** A commit of 100,000 shard names is about 6.7 MB. One request that size was tried: a `/missing` question about 100,000 shards, 6.8 MB up and 6.7 MB back, answered in 7.3 s with a folder and 11.8 s with MinIO. A commit that size was not.
- **A frozen server.** The soaks stop the server for up to 4.5 s, longer than the two seconds after which the rig's engine gives its actors away, and thaw it again; every run passed. Whether a thawed server can still write to a drive the engine has started elsewhere was not looked at directly, and with one node there is no elsewhere.
- **Only the engine dying, or only the server.** The suites kill both together. One soak of each suite was run each way (`LUDY_RIVET_KILL=server`, `engine`) and passed; the twelve scenarios were not.
- **The collector's timer going off in the middle of a commit.** On Rivet the two can run side by side, which they cannot on Cloudflare. What keeps them apart is that the collector marks inside a transaction and a transaction has the actor's SQL to itself; that is read from `rivetkit-core`'s source. One scenario runs the collector from its timer behind every commit and passes on both providers, but nothing shows the two ever overlapped in it.
- **What is open source in Rivet.** The crates say Apache-2.0 and name a repository called `rivet-ee`. Under what licence the `rivetdev/engine` image is published, and whether running several engine instances needs a paid edition, was not checked.
- **An actor asleep.** Rivet puts an actor to sleep after 30 s without requests and wakes it for the next one. No test waits that long, so the cost of waking was not measured.
- **Cost and speed under load.** One drive, one client, loopback.

### The drive

- **The collector racing a commit.** A marked shard is refused inside the commit's transaction; a bug put in on purpose confirmed that check matters. The scenario that runs the collector behind every commit never actually produced the race. The in-memory pins that spare a commit a refusal are covered by no test.
- **The request that panics.** Other requests to the drive were answered within about 20 ms on Cloudflare and 41 ms on Rivet. On Rivet the one that panicked is answered with a 502 at once; on Cloudflare it got no answer within the client's 3 seconds, and how long the server holds on to it was not measured.

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
- **A server with no engine to run beside it, and Rivet on more than one machine.** See [architecture.md](architecture.md#not-built).

## Decisions still open

| Question | Today |
|---|---|
| Two Macs create the same folder name at once: merge, or keep both? | Both are kept, one as `name (conflict ab12cd)`. Files inside go with their folder. The old cloud merged them |
| Fork a drive without copying it | Not designed. Shards are kept per drive, so the drive object can count references in one transaction. A fork then needs shards shared between drives |
| Where a drive's root key lives on the Mac | Nowhere. Waits for sign-in |
| Is a plain binary with no engine worth having? | See [architecture.md](architecture.md#open-questions) |
