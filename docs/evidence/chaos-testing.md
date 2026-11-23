# Chaos testing

The server and the engine are run while things die: the server's processes killed with `kill -9`, frozen and restarted, requests and replies lost in a proxy, the app killed with writes unsent. Every test ends by reading the drive back from outside, with the root key and none of the engine's code, and comparing it with what was written. Two suites do this against each provider, a third tests the server's logic with storage made to fail, and a script does it to a real mount. A loop runs all of it with the server's blobs in a real Cloudflare R2 bucket, taking the bucket away as well. All pass on both providers (2026-11-23). What they found on the way, nine problems, is fixed and listed below.

What is still expected to break, and what none of this covers, is in [known problems](../known-issues.md).

## The suites

| Suite | What runs | Where |
|---|---|---|
| The server under chaos | A client that seals everything for real and repeats a request until it is answered, against a real provider. A model predicts the outcome of every commit, refusals included | [`worker_chaos.rs`](../../crates/ludy-core/tests/worker_chaos.rs): 12 scenarios and a soak, and the five store scenarios below |
| The engine under chaos | The engine itself, as one Mac or several, against a real provider under the same faults | [`sealed_engine.rs`](../../crates/ludy-core/tests/sealed_engine.rs): 17 scenarios and a soak |
| The drive by itself | The server's logic on SQLite in memory, with no provider: the rules of a commit, the feed, the collector, a panic inside a transaction. It can make storage fail, which no real provider can be asked to do | [`ludy-server/tests/drive.rs`](../../crates/ludy-server/tests/drive.rs): 10 tests |
| A real mount | A drive mounted through the kernel, flooded, with the server and then the mounting process killed | [`scripts/chaos-mount.sh`](../../scripts/chaos-mount.sh) |
| The store taken away | The Rivet server with its blobs in a real bucket, reached through a proxy that can refuse, stall and cut its connections | Five scenarios in [`worker_chaos.rs`](../../crates/ludy-core/tests/worker_chaos.rs), store faults in both soaks, and a loop: [`scripts/chaos-r2.sh`](../../scripts/chaos-r2.sh) |

The first two go through one rig, [`worker_rig`](../../crates/ludy-core/tests/worker_rig/mod.rs). It starts the provider `LUDY_PROVIDER` names as real processes, puts a proxy in front that can lose a request or its reply, and can freeze, thaw, `kill -9` and restart the server. For `cloudflare` (the default) the server is the Worker under `wrangler dev`. For `rivet` it is `target/release/ludy-rivet` beside a Rivet engine in a container of the rig's own; a `kill -9` takes both, as a machine losing power would. They are the definition of a correct provider: see [the server](../how-it-works/server.md#conformance).

### The server under chaos

- A model predicts the outcome of every commit.
- The oracle at the end: the drive's own recount of everything it stores twice, the tree read back and decrypted from the root key equal to the model's, every file's content as written, and storage holding exactly the blobs it should.
- The scenarios: a lost reply does not apply a commit twice; a refused commit leaves nothing behind; `kill -9` in a burst of commits; of two writers on one base exactly one wins; a shard lost before the commit is asked for again; the collector never takes what a commit names; a panic inside a commit leaves no half commit; the fault switches are off unless turned on; a collector cut short never hands out what it marked; a wide tree in few commits; history is capped and the dropped versions let go; oversized and malformed requests are refused.
- The soak does random commits while the server is killed and frozen and messages are lost.
- Four bugs put into the drive on purpose, one at a time, were each caught (2026-11-22).

### The engine under chaos

Each scenario ends with the drive read back from outside by a reader that holds the root key and shares none of the engine's code.

- **One Mac.** Listing downloads no content. A 64 KiB read in the middle of a 24 MiB file is one range request for one block (171 to 263 KB sealed, by where the read falls in its shard). Identical content under another name uploads about a kilobyte, the sealed manifest. A small change in a large file sends only the shards it touches. Folders, renames and deletes reach the server in order. The Finder's own files stay on the Mac, and names compare after normalisation.
- **Dying.** Unsent writes, renames and deletes survive the app being killed. A reply lost before a crash does not duplicate a file. The server and the app die in the middle of a flood.
- **Two Macs on one drive.** The same name created twice keeps both files, one as `name (conflict ab12cd)`. The last writer of a file wins and the other version stays in its history. A file rewritten here and deleted there is saved again; a file saved into a folder that was deleted there is kept at the top of the drive; a partial change to a file that is gone is dropped and reported. A rename that lost to another Mac gives way.
- **Added on 2026-11-23, with the move of the app to this engine.** An earlier version is restored without sending content. A synthetic file reads like any other, stores no shard, and takes a real edit. Folders copy in and out and are removed with what is in them. A drive's key opens the drive from another keystore, and a wrong key, another drive's key and a key for a drive the server lacks do not.
- **The soak** does random file work on one Mac while the server is killed and frozen, requests and replies are lost, the app itself is killed and started again, and another Mac writes alongside, every read compared with what was written. In some runs the Mac's cache is small and wiped whenever the app is killed, so that reads come from the server through the faults.
- A soak's seed fixes the order of operations, not the drive's keys or the timing, so a failing seed may pass when run again.
- Five bugs put into the engine on purpose were each caught, one only after a test was strengthened (2026-11-22).

### The drive by itself

[`tests/drive.rs`](../../crates/ludy-server/tests/drive.rs) fails the database at every statement of a commit in turn and checks that all of the commit or none of it remains, takes the blob store away, and proves a request can move between threads when the platform's types can. Ten tests in about 50 ms; they run in a plain `cargo test`.

### A real mount

[`scripts/chaos-mount.sh`](../../scripts/chaos-mount.sh) starts a Compose project of its own with the published server image, makes a drive and mounts it hidden with the release `ludy`. A second keystore with its own cache does the checking: it downloads and compares bytes.

On 2026-11-23 it ran to the end once:

1. 2,000 small files and a 200 MiB copy written through the mount; both of the server's containers killed with SIGKILL five seconds in, and started again five seconds later. Result: all 2,001 files on the server, none wrong.
2. The server killed again, 500 more files written through the mount, then `kill -9` of the mounting process. The kernel dropped the dead mount 54 seconds after it was next touched. Server and mount started again: the 500 files that had never been sent arrived, none wrong, and the remounted drive listed them.

The timeline is in [`bench/2026-11-23-engine-on-providers.md`](../../bench/2026-11-23-engine-on-providers.md#chaos-on-a-real-mount). The Finder was not involved, and it has not run against the Cloudflare provider.

### The store taken away

With a folder for its blobs the server's store cannot fail, so none of the above ever saw it fail. A bucket is on a network. When `LUDY_RIG_S3_BUCKET` and its companions are set, the rig starts `ludy-rivet` with its blobs in that bucket and tells its S3 client to go through a proxy of the rig's own. The client tunnels TLS through it, so the proxy sees bytes and can do what a network does: turn connections away, hold them open and carry nothing, or cut them. The faults reach connections that are already open, because the S3 client keeps its connections. Nothing sets those variables but the loop script, so an ordinary test run never touches a bucket.

What is held to be right when the store fails:

- A request that needs the store gets a 503 within the limit the server gives that call to the store, never a wrong answer and never silence. The scenarios use small shards, whose limit is under 10 seconds, and allow 20.
- The 503 says that the store did not answer and nothing about where or what the store is.
- What needs no store keeps working, and a drive is never held for longer than one bounded wait.
- A commit is applied whole or not at all.
- Once the store is back, the next attempt succeeds and storage holds exactly what the commits name.

Five scenarios check that directly:

- **The store is gone** (connections refused). An upload, a read and a commit that has to look blobs up each got a 503 within two seconds (0.6 to 1.9 s over the runs), with the one sentence `the blob store did not answer; try again` and no address or bucket name in it; the commit was not applied and was not told its blobs were missing. A new folder, the feed and the drive's own check went on working. With the store back, the same commit was refused for what it was and a new file was written and read.
- **The store says nothing** (connections open, no bytes). An upload and a read of a stored shard each got a 503 after 8.0 seconds. A commit waiting for a lookup held its drive: the drive's feed was answered after 7.7 seconds, behind it, and another drive's head after 12 ms.
- **The store says nothing for 35 seconds**, longer than the server lets any call wait and long enough for a client's repeated attempts to pile up in the drive, while a client that keeps asking is writing a file. The file was in 1.3 seconds after the store spoke again, the drive's feed answered at once afterwards, another drive never noticed, and the server had given up on the store eight times and logged each.
- **Connections cut in mid-flight**, every 250 ms under 10 MB of uploads and their commits, then under the collector's deletes. Nothing was lost and nothing was stored twice; the S3 client's retries carried it.
- **The collector asked to run with the store gone**, which came back seven seconds later: it ran to its end, storage held exactly what it should, and what it removed could be written again.

Both soaks also give the store weather when it is a bucket: about one step in fourteen it goes away, goes silent or drops its connections. The dice give mostly a second or two, sometimes 4 to 8 seconds (longer than the rig's client waits), and three times in a hundred more than 30. Each soak reports what it drew: in the four soaks that recorded it, 12 to 26 spells each, the longest 7.9 seconds, none over 30, which is why the 35-second scenario exists. That is on top of the kills, the freezes and the lost messages, and the oracle at the end is unchanged.

[`scripts/chaos-r2.sh`](../../scripts/chaos-r2.sh) is the loop. Each round runs the five scenarios, the server's soak and the engine's soak against the bucket in `.env`; what `kill -9` takes rotates with the round (server and engine, server alone, engine alone). The first round also runs `chaos-mount.sh` on a Compose project with `docker-compose.r2.yml`, with the server's image built from the checkout, and every scenario of both suites. After each leg it deletes the drives that leg made from the bucket, by id, and nothing else. It stops at the first failure and keeps the output, the seeds and the server's logs.

What these do not do: more than one client at a time on a drive (two Macs at most, in the engine's suite), load, a store that fails one request and not another (the proxy cannot tell requests apart inside TLS), or a store that answers wrongly.

## State

One run of each on 2026-11-23, with the server's blobs in a folder, after the app moved to this engine and after the fixes the bucket led to. Each soak was 300 steps with a seed from the clock. The server's suite counts 18: its 12 scenarios, the soak, and the five store scenarios, which skip themselves without a bucket.

| Suite | Cloudflare (`wrangler dev` 4.166.0) | Rivet (`ludy-rivet` beside `rivetdev/engine:3.1.1`) |
|---|---|---|
| The server under chaos | 13 pass in 168 s. Soak, seed 19199: 9 kills, 7 freezes, 8 collector runs | 13 pass in 259 s. Soak, seed 97809: 9 kills, 4 freezes, 4 collector runs |
| The engine under chaos | 18 pass in 222 s. Soak, seed 50895: 11 kills of the server, 7 of the app, 8 freezes | 18 pass in 258 s. Soak, seed 76092: 6 kills of the server, 7 of the app, 10 freezes |
| The drive by itself | 10 pass, in `cargo test` | |
| A real mount | Not run | Passed, once, under Compose |

With the blobs in a real R2 bucket, the same day, after the three fixes it led to (Rivet only; the Worker's R2 is a binding, not a network client the rig can get between):

| What ran | Result |
|---|---|
| `scripts/chaos-r2.sh`, four rounds, 14 legs | All passed |
| Two more rounds after the last changes to the server, 6 legs (seeds 32529, 33318) | All passed |
| The store scenarios: four of them six times, the 35-second one once (it was added last) | Passed each time |
| The server's soak with store weather, four seeds (27572, 29368, 30122, 30745) | Passed: 36 kills, 27 freezes, 78 spells of store weather |
| The engine's soak with store weather, the same four seeds | Passed: 28 kills of the server, 54 of the app, 43 freezes, 60 spells of store weather |
| Every scenario of both suites on the bucket | 16 and 17 pass |
| A real mount on Compose with `docker-compose.r2.yml`, the image built from the checkout | Passed, once |

The kills rotated: server and engine together in rounds one and four, the server alone in round two, the engine alone in round three. The tables are in [`bench/2026-11-23-r2-chaos.md`](../../bench/2026-11-23-r2-chaos.md). After the fixes both suites were run again on a folder and on Cloudflare; those are the runs in the table above.

The Rivet server suite is the run after the last change of the day; the other three ran after the store limits went in and before the drive's path was made to answer 503, a change in the Rivet provider's error reply only. These are the second runs of the day; the first, that morning and before the server changed, passed the same way (seeds 51220, 52453, 88292, 38116). One soak per suite and provider is still thin. More seeds were run on 2026-11-22, before the app used this engine: 16 of the server's soak and 8 of the engine's on Cloudflare, 8 and 7 on Rivet, one of which failed its check of the server's log while every file was right ([known problems](../known-issues.md#the-rivet-provider)). Those tables are in the `bench/` files named at the end of this page.

## Running them

The server and engine suites are ignored in a plain `cargo test`, because each needs a provider's tools. They must run one test at a time.

On Cloudflare (the default). Needs Node, and `npm install` run once in `providers/cloudflare`; the rig starts `wrangler dev` from there itself.

```bash
cargo test -p ludy-core --test worker_chaos -- --ignored --test-threads=1
```

```bash
cargo test -p ludy-core --test sealed_engine -- --ignored --test-threads=1
```

On Rivet. Needs Docker running and the server built first.

```bash
cargo build --release -p ludy-rivet
```

```bash
LUDY_PROVIDER=rivet cargo test -p ludy-core --test worker_chaos -- --ignored --test-threads=1
```

```bash
LUDY_PROVIDER=rivet cargo test -p ludy-core --test sealed_engine -- --ignored --test-threads=1
```

With the blobs in a bucket. Needs Docker and the four `R2_` settings in `.env` ([object storage](../hosting/object-storage.md#cloudflare-r2)); use a bucket made for it. Three rounds take about three quarters of an hour. How many requests that makes to the bucket was not counted; four rounds left 1,884 objects to remove:

```bash
scripts/chaos-r2.sh /tmp/ludy-chaos-r2 3
```

It prints a line per leg and writes them to `rounds.md` in that folder. To leave it running, start it with `nohup` and read `status` there.

`LUDY_RIVET_KILL=server` or `engine` makes a `kill -9` take only that one; by default it takes both. A soak takes its seed from the clock and runs 300 steps; `WORKER_CHAOS_SEED` and `WORKER_CHAOS_STEPS` set them for the server's soak, `SEALED_SEED` and `SEALED_STEPS` for the engine's. If a run is itself killed, remove the engine containers it left with `docker rm -fv $(docker ps -aq --filter label=ludy-rig)`.

The mount script needs Docker and a release build of the CLI, and takes about five minutes:

```bash
cargo build --release -p ludy-cli
```

```bash
scripts/chaos-mount.sh /tmp/ludy-chaos
```

It mounts under the folder you give it and removes its server's containers and volumes at the end.

## What this found, all fixed

| Found | Now |
|---|---|
| A panic inside a commit left the drive answering nothing for 30 seconds: the commit ran in the storage API's asynchronous transaction, whose promise never settled, and every other request to the drive waited behind it until the runtime gave up | On Cloudflare, commits run in the synchronous transaction (`transactionSync`, called directly since the `worker` crate does not bind it). A panic undoes the commit, and other requests to the drive were answered within 27 ms. The request that panicked got no answer within the client's 3 seconds; how long the server holds on to it was not measured. The call reaches the storage object through the crate's internals, so an upgrade of `worker` can break it |
| The protocol had nowhere to put a file's modification time (found when the engine needed it) | Each version carries it sealed under the file's key, next to the node, so a folder lists with dates without fetching a manifest and the server does not see them |
| The engine's message that a change could not be saved was erased by the next successful upload, so nobody would have seen it | Such notices are kept apart from transient errors until they are read. Since 2026-11-23 the app and `ludy mount` read and show them |
| Six readers of one place in a file counted as one reader moving forward and set off read-ahead | Only a read that gets further extends the streak |
| A collector cut short (through a fault switch, then `kill -9`) leaves its shards marked, and a commit wanting one back is refused until the collector finishes. Each refusal set the collector's alarm 50 ms ahead again, so a client retrying faster than that postponed it for ever | The alarm is never moved later than it is already set |
| On Rivet, a panic inside a commit left the drive answering nothing for 36 seconds: the transaction stayed open, and the actor that was restarted after the panic waited for it. Found by the panic scenario the first time the suite ran against Rivet | The adapter rolls the transaction back before it lets the panic go on. The request that panicked gets a 502 at once, and the others were answered within 41 ms |

| A store that stayed connected and said nothing kept a request waiting for as long as the S3 client would retry: an upload was answered after 107 seconds, and a commit waiting for a lookup would have held its drive as long. Found by the first run of the store scenarios | Every call to the store has a limit: 8 seconds for a lookup, a delete and the start of a read, 8 seconds plus one per 250 KB for an upload. The upload was answered with a 503 after 8 seconds, and the drive's feed after 7.7, behind a commit that was waiting |
| A store failure was logged as an `error`, like something that should not happen, and its text went to the sender of the request: the S3 client's own words, with the bucket's address and name in them | A failed call to the store is its own kind of log line, `store`, with those words; the sender gets a 503 and `the blob store did not answer; try again`, from the front and from a drive alike, and a scenario checks that the reply names neither the bucket nor its address |
| The chaos harness's way of cutting a collector short deleted its objects one at a time. Against a bucket, 50 deletes took longer than the rig's client waits, so the scenario never got an answer. Not a fault in what a deployed server runs, but it kept one scenario from ever passing on a bucket | It deletes in the same batches as the collector proper |

The runs of 2026-11-23 on a folder, the first with the app, the CLI and a real mount on this engine, found no new fault in the engine or the server. The three rows above came from the bucket, the same day. They did find two things about what surrounds them: a dead mount that nothing touches stays in the mount table ([the mount](../how-it-works/mount.md#eject-and-the-network-volume-prompt)), and two processes on one Mac could open the same drive's journal, which the engine now refuses with a lock ([the engine](../how-it-works/engine.md#drives-and-keys)).

Earlier runs, with their soak tables: [`bench/2026-11-22-worker-chaos.md`](../../bench/2026-11-22-worker-chaos.md) and [`bench/2026-11-22-providers-chaos.md`](../../bench/2026-11-22-providers-chaos.md). The bucket's: [`bench/2026-11-23-r2-chaos.md`](../../bench/2026-11-23-r2-chaos.md).
