# Known problems and what is not verified

What is expected to break, what is fragile, what nobody has checked, what is not built, and the decisions still open, for every part of Ludy: the engine and the app on the Mac, the encryption format, the server's logic and its two providers. What was checked, and how, is in [what works](evidence/verification.md) and [chaos testing](evidence/chaos-testing.md). Nothing here has been deployed anywhere: every result is from one Mac with the server on it, and the only thing reached over a real network was an R2 bucket.

State of the tests on 2026-11-23, one run of each with the blobs in a folder, after the app moved to this engine and after the fixes the bucket led to. Cloudflare means local `wrangler dev` 4.166.0; Rivet means `ludy-rivet` beside a `rivetdev/engine:3.1.1` container, both killed at each `kill -9`.

| Suite | Cloudflare | Rivet |
|---|---|---|
| `cargo test` (no server): 87 tests | pass | |
| The server under chaos: 12 scenarios and a soak | 13 pass, 168 s; soak seed 19199, 9 kills | 13 pass, 259 s; soak seed 97809, 9 kills |
| The engine under chaos: 17 scenarios and a soak | 18 pass, 222 s; soak seed 50895, 11 kills of the server, 7 of the app | 18 pass, 258 s; soak seed 76092, 6 kills of the server, 7 of the app |
| Chaos on a real mount (`scripts/chaos-mount.sh`) | not run | pass, once, under Compose |
| Everything above with the blobs in a real R2 bucket and the bucket taken away as well (`scripts/chaos-r2.sh`, four rounds) | does not apply | 14 legs pass: [chaos testing](evidence/chaos-testing.md#state) |

More soak seeds were run on 2026-11-22, before this move: 16 of the server's and 8 of the engine's on Cloudflare, 8 and 7 on Rivet, one of which failed its log check ([below](#the-rivet-provider)). The tables are in [`bench/2026-11-22-providers-chaos.md`](../bench/2026-11-22-providers-chaos.md).

Nothing has run on Cloudflare's Workers, or on a Rivet engine of more than one instance. The Rivet provider was run against a real Cloudflare R2 bucket by hand and under the chaos suites (2026-11-23).

## Known problems

No test fails and no wrong result has been seen. What is listed about the server comes from reading the code against the platforms' documented limits, or from knowing a shortcut was taken; none of it has been reproduced, because the local emulator enforces none of those limits. What is listed about the engine and the app is how they are built, and where a date is given it was seen.

### Sizes the code allows and Cloudflare does not

Each call to R2 counts against a limit per request: 1,000 on Cloudflare's Free plan for calls to its own services (the documentation does not name R2 in that row, so this reading is itself unconfirmed), 10,000 by default on the Paid plan. The page does not say what the Worker sees when it goes over.

| Where | What happens | Size at which it starts |
|---|---|---|
| Commit on the Free plan ([`drive.rs`](../crates/ludy-server/src/drive.rs) `look_up`) | The drive asks R2 about every shard and manifest in the commit that it has no row for, one call each, and allows 8,000 (`MAX_NEW_BLOBS`). Past the platform's limit the commit should fail with a 5xx, which the engine treats as "try again", so the upload would never finish | A new file of about 1,000 shards: roughly 2 GB at the average shard size, 250 MB if every shard is the minimum 256 KiB |
| `POST /missing` on any plan | It accepts 100,000 names (`MAX_VERSION_SHARDS`) and looks each unknown one up. That is ten times the Paid default | Only another client: the engine asks about 8 MiB of sealed shards at a time, 33 names at most |

The collector deletes at most 500 objects per run, which is inside both limits.

A fix is small: cap `/missing` at the commit's limit, and make that limit a setting the provider supplies. The platform's enforcement cannot be tested locally.

### Wrong on every provider

- **One failing delete stalls the collector.** It deletes up to 500 objects per run and forgets them only after every delete succeeded. If storage keeps refusing one, the run fails, forgets none, and the next run starts on the same ones. Marked blobs are refused to every commit until the collector finishes, so those shards could not be uploaded again. Forgetting each blob as its delete succeeds would fix it, and the `stop_after` fault switch can test that.
- **Abandoned uploads are never deleted.** A shard sent for a commit that never happens stays in storage for ever. The drive has no row for it, so the collector does not know it exists.
- **Tombstones are never pruned.** A deleted node keeps its row so the feed can report the deletion. The drive's database only grows.
- **Anyone who can reach the server can do anything.** There is no sign-in. It must not be deployed anywhere public as it is. `docker-compose.yml` publishes the port on the machine itself only for this reason. What that means for each way of running it is in [hosting](hosting/README.md#before-you-expose-it).
- **A drive cannot be deleted.** The protocol has no request for it. A drive whose key was removed from every Mac stays on the server for ever, unreadable. Emptying a drive first lets the collector take its content back; its database and its tombstones remain.
- **The drive's root key is wrapped by itself** on the server, as a stand-in until keys are wrapped for people.

### The engine and the app

- **Keys are kept in a plain file.** `drives.json` in the Mac's Application Support folder holds every drive's root key, readable by its owner only (mode 0600) and not encrypted. Whatever runs as that user can read it. Lose it, and the drives in it cannot be opened by anyone.
- **The cache is plaintext.** Blocks are stored opened, in `~/Library/Caches/Ludy/blocks`. So are the journals of unsent writes. End-to-end encryption protects what leaves the Mac, not what is on its disk.
- **A byte flipped inside a cached block is not detected.** A block's length is checked; its content has no checksum. (A block that arrives damaged from the server is caught: every frame is authenticated.)
- **An open drive asks the server once a second.** That is 86,400 requests a day per open drive per Mac, whether anything changes or not. The app keeps a drive open from the first time it is browsed or mounted until the app quits. On Cloudflare this is a cost; see [the Cloudflare page](hosting/cloudflare.md#deploy-it).
- **The tree is not kept on disk.** Every open reads the drive's whole feed and unwraps every key: 104 to 134 ms for 5,004 nodes over loopback, and it grows with the drive. A drive that was open yesterday cannot start from what it knew.
- **After a crash, the first read of the feed must succeed before recovered writes show up.** So a drive cannot be opened offline to get at writes that were never sent. They stay in the journal folder until the server answers.
- **A full disk while writing the journal** turns into an I/O error for the application. Nothing has tested what happens next.
- **Copying a file in through the app or `ludy put` writes it to the journal first.** For a moment the Mac holds the file twice, and a file larger than the free disk cannot be uploaded that way. Copying through a mount does the same.
- **A mount whose process was killed stays in the mount table until something touches it.** The mounts are soft with a 45-second dead timeout, but the kernel only starts that clock on a request: on 2026-11-23 an untouched dead mount was still listed three minutes after `kill -9`, and was gone 50 seconds after one `ls`. The app removes such leftovers when it next starts.
- **Notices are lost in a crash.** What the engine has to tell the person (a file kept under another name, a change it had to drop) is held in memory until the app or `ludy mount` shows it. The app shows the latest one only.
- **"On this Mac" is blank for a file this session has not read.** The app learns how much of a file is cached from its manifest, which it fetches on the first read.
- **A commit waiting for a silent bucket holds its drive.** A drive handles one request at a time, and a commit that names blobs the drive has not seen asks the store about them from inside the drive. If the bucket stays connected and says nothing, that commit holds the drive for up to 8 seconds before it fails, and the drive's feed and every other request wait behind it; a client that keeps retrying the commit keeps the drive busy for most of an outage. Seen: the feed answered after 7.7 s (2026-11-23). Other drives are not affected. Doing the lookups outside the drive would fix it.
- **An upload to the bucket slower than 250 KB a second fails for ever.** The server gives an upload 8 seconds plus one for every 250 KB, so that a silent bucket is noticed. A server whose link to its bucket is slower than that will time out on a large shard every time, the engine will send it again, and the file will never finish. The numbers are constants in [`blobs.rs`](../providers/rivet/src/blobs.rs), not settings.
- **Small files are slow when the store is far away.** With the blobs in R2, 500 files of 4 KiB took 74.5 s to upload, 7 a second, against about 200 a second with the blobs on the server's own disk (2026-11-23, one run). Each file is five requests, and the server asks the store about or writes to it in four of them. Batching would be the fix and is [not built](#not-built).
- **`ludy bench` leaves a drive behind on every run.** It empties the drive at the end; the empty drive stays, because nothing deletes a drive.

### The Rivet provider

- **After `kill -9` of the server, its drives answer nothing until the engine gives up on the dead node.** `docker-compose.yml` and the chaos rig both shorten that wait to two seconds (`pegboard.node_lost_threshold` and `node_ping_timeout`), a setting found in the engine's binary, not its documentation. A server stopped with SIGTERM does not wait at all. How long each case took, with that setting and with the engine's default, is in [the Compose page](hosting/compose.md#stopping-and-starting).
- **A server that stalls for longer than those two seconds loses its drives for a few seconds.** This is the price of the two-second wait. Under Compose, with nothing in flight: after a pause of 4, 10 and 30 seconds a drive answered again 5.4, 1.6 and 2.1 seconds after the server woke, and the server's log held no fault. With the engine's default a 10-second pause cost nothing. Each was run once.
- **A server stopped for longer than the engine waits loses the answer to a commit in flight.** In one engine soak the server was frozen for more than the rig's two seconds; when it woke, the commit it was in the middle of came back from Rivet as `remote_indeterminate_result` (it may or may not have been applied). The drive answers 5xx, the client sends the same commit again, and the commit's id settles it either way; every file in that run held what was written. The run failed only because the line was logged as an error. It is now logged as `fault`, which the suites do not count, so they no longer tell how often this happens. Seen once in seven engine soaks.
- **One engine instance on its own disk is not what Rivet calls production.** Its documentation asks for three or more instances and a bucket, and says of the file-system store that it does not belong in production. What was seen here: writes answered before `kill -9` of the engine container were all there afterwards (80 across four kills in a first trial, and every commit of the soaks). What a lost disk costs was not tried.
- **A folder of blobs is not flushed to disk by us.** With `LUDY_BLOBS` a folder, an upload is answered once the `object_store` crate has written and renamed the file. Whether it also syncs was not checked. A power cut could then lose a shard whose commit, kept by the engine, survives. `kill -9` cannot show this, since the operating system still has the bytes.
- **The published image has only run on arm64.** `ghcr.io/martinnormark/ludy-rivet:rivet-3.1.1` is built for amd64 and arm64 by [a workflow](../.github/workflows/server-image.yml). The arm64 build of 2026-11-22 was pulled and started under Compose on this Mac, with a drive created and read. No amd64 build was ever started, and no later build was pulled: what ran with the store's limits in it was built from the checkout. The chaos suites run the binary from `target/release`, not the image. The image carries a signed record of its build, which nothing here checks.
- **Rivet 3 is three weeks old** (3.0.0 on 2026-11-01), and its Rust SDK carries no stability label. The crate and the engine image are pinned to 3.1.1 and have to move together.
- **The server holds the engine's admin token.** It is the only token the documented single-machine setup has. A scoped one was not looked into.

### Fragile

- **On Cloudflare, `transaction_sync` reaches into the `worker` crate's internals.** The crate does not bind the runtime's `transactionSync`, so [`drive.rs`](../providers/cloudflare/src/drive.rs) takes the storage object out of `State` with `_inner()` and calls the method by name. An upgrade of `worker` can break it, and then commits fail outright.
- **On Cloudflare, a bad upload is recognised by the wording of R2's error.** [`lib.rs`](../providers/cloudflare/src/lib.rs) `blob_put` treats an error containing "checksum" as the sender's mistake (400) and anything else as storage failing (5xx). If R2 words it differently in production, a corrupt upload becomes a 5xx and the engine retries it for ever. The Rivet provider hashes the body itself and does not depend on this.
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
- **Cost and latency.** Every number is from loopback with no added latency. Nothing measures what a deployed Worker costs per commit or how long a cold Durable Object takes.

### Rivet

- **A bucket that fails in ways a network cannot.** The chaos suites run against R2 with the bucket refusing, silent and cut off ([chaos testing](evidence/chaos-testing.md#the-store-taken-away)). The proxy that does it sees only encrypted bytes, so it cannot fail one request and pass another, and it cannot make the store answer wrongly: an error status from R2 itself, throttling, one object that will not delete, wrong or revoked credentials. Also not tried: a bucket in an R2 jurisdiction, a file larger than 95 MB, MinIO or any other S3 under the suites.
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

### The engine and the mount

- **The Finder, Spotlight, Quick Look and video playback.** Every mount of this engine that was checked was hidden (`nobrowse`) and driven from the shell: `ls`, `dd`, `cp`, `mv`, `cmp`. One exception, by hand on 2026-11-23: a file copied in the Finder into a drive mounted from the app showed up in the app's browser. A Finder-visible mount, a file opened in an application, a video played from the mount and a Finder copy were checked on the previous engine only ([history](history/verification.md)); apart from that one copy, not on this one.
- **A patch whose base lost a shard to the collector.** The engine is meant to drop the change and say so. No test produces it.
- **Sizes near the limits.** The largest real file uploaded is the 95 MB demo video; the largest in a test is 24 MiB. Synthetic files of up to 180 GB were listed and read, which exercises sizes and offsets and no storage.
- **A slow or distant server.** The client has only talked to a server over loopback. The tests add faults, not latency or a bandwidth limit. The one measurement with a real network in it has the network between the server and its R2 bucket, not between the Mac and the server.
- **A server behind HTTPS.** The client was built with TLS and completes a handshake with a public site; it has never talked to a Ludy server over `https://`.
- **Two Macs.** "Another Mac" in every check is a second keystore, cache and journal on the same machine.

### The app

- **Most of its controls.** Seen in screenshots on 2026-11-23: the drives screen with a drive mounted and not, a folder as grid and as list, the drive menu, the file menu, the drive-key dialog, names cut in the grid and the list, and the details pane opened by a posted click and by a posted double-click. `--mount` at launch mounted a drive and quitting ejected it. Not exercised in the window: creating, adding, renaming and removing a drive, New folder, Import, Rename, Delete, Versions and Restore, Copy drive key, Show in Finder, Open in Terminal, Clear. The engine calls behind them are covered by tests; the clicks are not.

### The encryption format

- **Nobody else has reviewed it.** The format is checked against a second implementation ([`crypt-v1.mjs`](../crates/ludy-proto/tests/vectors/crypt-v1.mjs)), written by the same author. That catches coding mistakes. It does not catch a mistake in the design, which both would share.
- **What the server can still see** has not been written down as a threat model: sizes, the shape of the tree, when things change, which files in a drive share shards.
- **Sealing speed** is about 116 MB/s on this Mac with hardware SHA-256 and NEON ChaCha20 off. Turning them on is untried.

## Not built

- **Sign-in and permissions.** No accounts, no read-only access, no way to take access back. Sharing a drive today means handing over its key. Sharing a folder or a file is designed around wrapping a node's key for the recipient and not written. It needs a part of the server that is not a per-drive object; see [the server](how-it-works/server.md#not-built).
- **Deleting a drive, and forking one.** Neither has a request in the protocol.
- **Keeping a file offline** (pinning).
- **Batching.** A new small file is five requests (create, which shards are missing, the shard, the manifest, the commit), and a cold read needs the manifest before the bytes.
- **Push.** Clients poll the feed; a WebSocket that wakes them is not built.
- **Pruning.** Abandoned uploads and tombstones are never removed; see [above](#wrong-on-every-provider).
- **A server with no engine to run beside it, and Rivet on more than one machine.** See [the server](how-it-works/server.md#not-built).
- **In the app:** a details pane for a folder, selecting more than one file, moving the selection with the keyboard, thumbnails, drag and drop, search, a menu-bar launcher, a dark theme, showing more than the latest notice.
- **An app bundle.** The app runs from `cargo run` and is unsigned.
- **Other clients.** Only the Mac engine exists.

## Decisions still open

| Question | Today |
|---|---|
| Two Macs create the same folder name at once: merge, or keep both? | Both are kept, one as `name (conflict ab12cd)`. Files inside go with their folder |
| Fork a drive without copying it | Not designed. Shards are kept per drive, so the drive object can count references in one transaction. A fork then needs shards shared between drives |
| Where a drive's root key lives on the Mac | A plain file, `drives.json`, mode 0600. A stand-in: the Keychain, or keys wrapped for a signed-in person, would replace it |
| How a client is allowed to talk to a server before there are accounts | It is not: anyone who can reach the server can. A shared secret the client sends would be a small step and is not built |
| Is a plain binary with no engine worth having? | See [architecture.md](how-it-works/server.md#open-questions) |
