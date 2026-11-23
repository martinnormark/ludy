# Chaos testing

The server and the engine are run while things die: the server's processes killed with `kill -9`, frozen and restarted, requests and replies lost in a proxy, the app killed with writes unsent. Every test ends by reading the drive back from outside, with the root key and none of the engine's code, and comparing it with what was written. Two suites do this against each provider, a third tests the server's logic with storage made to fail, and a script does it to a real mount. All pass on both providers (2026-11-23). What they found on the way, six problems, is fixed and listed below.

What is still expected to break, and what none of this covers, is in [known problems](../known-issues.md).

## The suites

| Suite | What runs | Where |
|---|---|---|
| The server under chaos | A client that seals everything for real and repeats a request until it is answered, against a real provider. A model predicts the outcome of every commit, refusals included | [`worker_chaos.rs`](../../crates/ludy-core/tests/worker_chaos.rs): 12 scenarios and a soak |
| The engine under chaos | The engine itself, as one Mac or several, against a real provider under the same faults | [`sealed_engine.rs`](../../crates/ludy-core/tests/sealed_engine.rs): 17 scenarios and a soak |
| The drive by itself | The server's logic on SQLite in memory, with no provider: the rules of a commit, the feed, the collector, a panic inside a transaction. It can make storage fail, which no real provider can be asked to do | [`ludy-server/tests/drive.rs`](../../crates/ludy-server/tests/drive.rs): 10 tests |
| A real mount | A drive mounted through the kernel, flooded, with the server and then the mounting process killed | [`scripts/chaos-mount.sh`](../../scripts/chaos-mount.sh) |

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

## State

One run of each on 2026-11-23, after the app, the CLI and the mount moved to this engine. Each soak was 300 steps with a seed from the clock.

| Suite | Cloudflare (`wrangler dev` 4.166.0) | Rivet (`ludy-rivet` beside `rivetdev/engine:3.1.1`) |
|---|---|---|
| The server under chaos | 13 pass in 148 s. Soak, seed 51220: 10 kills, 5 freezes, 8 collector runs | 13 pass in 339 s. Soak, seed 52453: 18 kills, 5 freezes, 6 collector runs |
| The engine under chaos | 18 pass in 306 s. Soak, seed 88292: 5 kills of the server, 16 of the app, 14 freezes | 18 pass in 308 s. Soak, seed 38116: 9 kills of the server, 12 of the app, 8 freezes |
| The drive by itself | 10 pass, in `cargo test` | |
| A real mount | Not run | Passed, once, under Compose |

One soak per suite and provider is thin. More seeds were run on 2026-11-22, before the app used this engine: 16 of the server's soak and 8 of the engine's on Cloudflare, 8 and 7 on Rivet, one of which failed its check of the server's log while every file was right ([known problems](../known-issues.md#the-rivet-provider)). Those tables are in the `bench/` files named at the end of this page.

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

The runs of 2026-11-23, the first with the app, the CLI and a real mount on this engine, found no new fault in the engine or the server. They did find two things about what surrounds them: a dead mount that nothing touches stays in the mount table ([the mount](../how-it-works/mount.md#eject-and-the-network-volume-prompt)), and two processes on one Mac could open the same drive's journal, which the engine now refuses with a lock ([the engine](../how-it-works/engine.md#drives-and-keys)).

Earlier runs, with their soak tables: [`bench/2026-11-22-worker-chaos.md`](../../bench/2026-11-22-worker-chaos.md) and [`bench/2026-11-22-providers-chaos.md`](../../bench/2026-11-22-providers-chaos.md).
