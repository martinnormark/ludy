# Ludy

Cloud drives that mount in the Finder and stream on demand. A drive holding hundreds of gigabytes is listed in full, opens in any app, and takes almost no local disk: bytes are fetched from the cloud only when an application reads them.

The "cloud" here is simulated: a local S3-like server with configurable latency and bandwidth. The Mac app is written in Rust with [gpui](https://gpui.rs/).

**Status (2026-11-19):** the server, the engine and the app are built, covered by automated tests, and checked against real mounts: listing, range-only reads, video playback, writes, remote changes, eject and the crash case all work (see [What works](#what-works-and-how-it-was-checked)). Not checked: clicking the app's buttons, and the Quick Look panel itself. Read [Eject and the "network volume" prompt](#eject-and-the-network-volume-prompt) for a hazard to know about. The engine has a benchmark and a chaos suite (cloud killed mid-upload, stalls, app crashes with unsent writes); see [Benchmarks](#benchmarks) and [Chaos testing](#chaos-testing).

## Run it

Three terminals, all from this folder.

Start the simulated cloud. Data goes to `./data`; the request log prints to the terminal.

```bash
cargo run -p ludy-cloud -- --data ./data --latency-ms 30 --mbps 400
```

Fill it with demo drives. This uploads the 95 MB demo video for real and adds synthetic files of 12 to 180 GB that take no disk.

```bash
scripts/seed-demo.sh ../e58b62c20765eacd37457c1a53d2e97ace458f85.mp4
```

Start the app.

```bash
cargo run -p ludy-app
```

Click **Mount** on a drive. It mounts at `~/Ludy/<drive>` and appears under Locations in the Finder sidebar as the server `ludy.localhost`, with the drive inside it and an eject button. **Open** browses the drive inside the app, mounted or not. Quitting the app, or Ctrl-C in its terminal, ejects mounted drives first.

Useful variations:

| Command | What it does |
|---|---|
| `cargo run -p ludy-app -- --mount Footage` | Mount a drive at launch |
| `cargo run -p ludy-app -- --open Footage/raw` | Start in the file browser (`--list` for the list view) |
| `scripts/seed-stress.sh` then `cargo run -p ludy-app -- --open Stress --perf` | Make a drive of 5,000 synthetic files, scroll it by itself for a few seconds, print frame times, quit |
| `cargo run -p ludy-app -- --click 327,345` | Post a click to the app's own window at that point (also `--rclick`, `--dclick`, repeatable; `--demo menu\|entry-menu\|share\|new-drive` opens that surface; `--size 820x560` sets the window size) |
| `cargo run -p ludy-cli -- mount Footage` | Mount from the terminal, no window; Ctrl-C ejects |
| `cargo run -p ludy-cli -- log --follow` | Tail the cloud's range log |
| `cargo run --release -p ludy-cli -- bench` | Time fixed engine scenarios against a cloud of its own and print a table (`--quick` for small sizes) |
| `cargo test -p ludy-core --test chaos` | The chaos tests; add `soak -- --ignored --nocapture` for the randomized run (`CHAOS_SEED`, `CHAOS_STEPS`) |
| `scripts/chaos-mount.sh <work-dir>` | Flood, copy and `kill -9` the cloud and then the mounting process, on a hidden mount (needs the release builds) |
| `curl -X PUT 'http://127.0.0.1:9000/_config?mbps=50&latency_ms=120'` | Slow the link down while running |

Environment variables: `LUDY_SERVER` (default `http://127.0.0.1:9000`), `LUDY_CACHE_MB` (default 2000), `LUDY_CACHE_DIR` (default `~/Library/Caches/Ludy/blocks`), `LUDY_MOUNT_ROOT` (default `~/Ludy`), `LUDY_READ_ONLY`, `LUDY_NOBROWSE`.

Building needs only the Xcode Command Line Tools. gpui is built with its `runtime_shaders` feature, so the Metal shader compiler that ships with full Xcode is not required.

## The mount mechanism: loopback NFSv3

Each mounted drive is an NFSv3 server inside the app's own process (the [`nfsserve`](https://crates.io/crates/nfsserve) crate), bound to a random port on 127.0.0.1 and attached with `/sbin/mount_nfs`. This needs no driver, no kernel extension and no root. The mount source is `ludy.localhost:/<drive>`: macOS resolves any `*.localhost` name to the loopback address, and the Finder sidebar shows that name instead of `127.0.0.1`.

Why this one:

| Option | Verdict |
|---|---|
| **Loopback NFSv3** | Chosen. macOS's NFS client turns an application's `read` into NFS READs for just those bytes, so range streaming works. A non-root user can mount onto a folder they own. |
| macFUSE / FUSE-T | Not installed on this machine, and installing either needs an admin password. |
| FSKit (what Space's Mac app uses) | Needs a signed app extension with an Apple entitlement, which the user then enables in System Settings. Not possible for an unsigned local build. |
| Loopback SMB (what Space's CLI uses) | Would need an SMB2/3 server written from scratch. |
| WebDAV (`mount_webdav`) | macOS's WebDAV client downloads a file to a local cache when it is opened, which defeats the purpose for a 50 GB file. Not tested here; ruled out on that basis. |

A throwaway spike confirmed this before anything else was built, and the measurements against the real code are under [What works](#what-works-and-how-it-was-checked): a listing causes no object request, and a 64 KiB read in the middle of a 50 GB file causes one 256 KiB range request.

Mount options are in [`mount.rs`](crates/ludy-core/src/mount.rs): 256 KiB reads to match the cache block size, kernel read-ahead of 8 blocks, a one-second attribute cache so remote changes show up quickly, and a soft mount with a dead timeout so a crashed app cannot hang the Finder for long.

### Eject and the "network volume" prompt

During the spike a plain `umount` blocked forever inside the kernel's `unmount` call and could not be killed. While it was stuck, `diskarbitrationd`, `lsd`, Finder and Raycast each had a thread blocked on the half-unmounted mount point.

The cause, as far as it can be established without kernel stacks: macOS asks for consent the first time an app touches a network volume (*"Raycast" would like to access files on a network volume*). Raycast's file indexer started crawling the new volume under the home folder, and the consent daemon's log shows it began prompting for Raycast seven seconds before the `umount`. Raycast's `lstat` and `getxattr` calls sat suspended on that prompt, which was still on screen an hour later, and the unmount waited behind them. Apple's NFS client has several waits without a timeout on the unmount path; which one was hit is not known. Answering the prompt released it: the consent log shows the answer at 07:02:26 the next morning, and the `umount` and the blocked threads were gone without a reboot. Nine mount and eject cycles since then, hidden and Finder-visible, were clean, each eject taking one to two seconds.

What Ludy does about it:

- Eject always force-unmounts, from a **child process** under a watchdog. The app never calls `unmount` itself, so it cannot become unkillable.
- Before unmounting it uploads pending writes and waits for the volume to go quiet.
- The NFS server keeps serving until the unmount has finished.
- If the watchdog expires, the drive is shown as "eject did not finish" with a hint to answer any pending permission prompt, and the eject completes by itself once the kernel lets it through. A force-unmount probably cannot beat a call held by a prompt; the watchdog is the real protection.
- A stuck eject is recorded on disk with the `umount` process id. While that process is alive, Ludy refuses to mount at the same path, even after a restart.
- The mount table is read with `getmntinfo(MNT_NOWAIT)`. The app's own process never stats, creates or removes a mount-point path; `mkdir` and `rmdir` run as child processes.
- If the app is killed outright, the mounts are soft with a 45-second dead timeout. Tested twice with `kill -9`: the kernel dropped the mount by itself 55 to 57 seconds later, and an `ls` of the dead mount in the meantime failed with "Operation timed out" instead of hanging.

If you ever see a stuck eject: look for a macOS permission prompt and answer it. Do not `ls -la` the mount root in the meantime.

## The app

A plain file explorer in a light theme: drives in the sidebar, a toolbar with the path and the actions, the folder as a grid or a list, and a status bar with the live numbers (streamed, uploads, cache). The drives screen has one card per drive with Mount, Open, Show in Finder and eject. Right-click a drive or a file for the rest (Open in Terminal, Copy path, Rename, Share, Fork, Keep offline, Versions, Delete).

Folders are drawn with a virtual list, so only the rows on screen are built each frame. Measured with `--perf` on a 60 Hz display, in the unoptimized dev build, scrolling a folder of 5,000 files at 2,400 points a second for six seconds:

| View | Frames | Frame time p50 / p99 / max | Slow frames | Building elements |
|---|---|---|---|---|
| Grid | 361 (60.0 fps) | 16.67 / 18.62 / 18.68 ms | 0 | 0.79 ms a frame |
| List | 361 (60.0 fps) | 16.67 / 18.69 / 18.78 ms | 0 | 0.68 ms a frame |

That is the display's limit. "Building elements" is only the app's own share of a frame (constructing the window and the visible rows); gpui's layout and painting were not timed separately, and a 120 Hz display has not been tried. Colours are one `Theme` constant in [`theme.rs`](crates/ludy-app/src/theme.rs); there is no dark variant yet, and no thumbnails.

## How it works

```
 Finder / any app
        │  read(offset, len)
        ▼
 macOS NFS client ──NFSv3 over loopback──▶ ludy-core (in the app's process)
                                              │
                         ┌────────────────────┼─────────────────────┐
                         ▼                    ▼                     ▼
                   block cache          write journal        tree of nodes
                (256 KiB blocks,      (sparse local file     (from the cloud
                 LRU, byte cap)        per written file)        listing)
                         │                    │                     ▲
                         └───── HTTP: GET Range / PUT / long-poll ──┘
                                              │
                                              ▼
                                         ludy-cloud
```

**Reads.** A read maps to the 256 KiB blocks that cover it. Blocks already in the cache are read from disk. Missing neighbours are fetched with one range request of at most 2 MiB, several requests run in parallel, and concurrent readers of the same block share one request. A sequential reader gets read-ahead that grows to 8 MiB; a random read fetches only what it asked for.

**Cache.** Blocks are keyed by `(version id, block index)`. A version never changes, so a cached block is never stale. The cache has a byte cap and evicts the least recently used blocks. The Finder lists every file at its full cloud size, so a drive can show far more than the Mac's disk holds; that is the point. `stat` and `du` on the mount report the full size as well. What the Mac really holds is the cache, shown in the app.

**Writes.** A write goes to a sparse journal file and returns. An existing cloud file is not downloaded first: the journal records which byte ranges were written and reads merge them with the cloud version underneath. When a file has been quiet for 1.5 seconds it is uploaded as a new version, as a patch: only the shards the edit touched are sent (see Shards below). Eight files upload at a time, and a file small enough to be one shard goes up in a single request. What has not been sent yet survives a quit, a crash or a power cut: next to each journal file the engine saves which byte ranges it holds and which cloud version they were written over, and it saves the queue of renames and deletes, so the next start picks everything up and sends it. Renames, deletes and new folders are queued and replayed on the cloud in order. `.DS_Store` and `._*` files stay on the Mac.

**Remote changes.** Each mounted drive long-polls the cloud for its next commit. The first time it fetches the whole listing; after that it asks only for what changed since the sequence number it last saw (`GET /<drive>?feed&since=N&epoch=E`) and applies that: deleted keys, then folders left with neither content nor a marker object, then new and changed objects. So noticing a change costs one round trip whatever the size of the drive. The cloud answers "reset" when it cannot say what changed (it restarted, the drive was deleted and created again, the number is too old), and the drive falls back to the full listing. Node ids of paths that still exist are kept. A listing is never applied while local changes are still uploading.

**The cloud.** A drive is a bucket. Objects support list, HEAD, GET with `Range`, PUT and DELETE. Every PUT creates an immutable version whose id is the ETag and which names the version it replaced; history is kept (25 versions per key) and an old version can be restored without writing content. Every commit to a drive goes through that drive's lock and bumps its sequence number; `If-Match` makes one of two racing writers lose with a 412. A fork is a new catalog pointing at the same versions. Synthetic objects are the SplitMix64 stream for a seed, generated from the offset. On disk a drive's catalog is a snapshot plus a log: a commit appends one line with the new state of the keys it touched, and every 4,000 commits the log is folded into a fresh snapshot, so the cost of a commit does not grow with the drive (a test makes 4,100 commits, restarts, and restarts again with stale log lines put back).

**Shards.** Content is cut into shards with FastCDC content-defined chunking (256 KiB minimum, about 2 MiB on average, 16 MiB maximum), so the cut points follow the bytes and an edit moves only the shards around it. A shard is named by the SHA-256 of its bytes and stored once, in `<data>/shards/`. A version's content is a manifest in `<data>/manifests/`: the ordered list of segments, each either a shard or a range of a synthetic stream. Identical content gives the same shards and the same manifest, whatever key or drive it is under. Manifests and shards are reference counted and deleted when no version points at them; fork, rename and restore write no content.

An upload from the mount goes in four steps:

1. Plan the new content against the manifest of the version underneath. Segments no write touched are kept by reference. A synthetic range is split exactly around what was written. A shard that was written into is read again (journal over cloud) and cut afresh, together with its written neighbours.
2. `POST /_shards` with the hashes of the newly cut shards; the cloud answers which ones it lacks.
3. `PUT /_shards/<sha256>` for those, four at a time. The cloud checks each body against its name.
4. `PUT /<drive>/<key>?manifest` commits the manifest. No content moves in this request. If a shard has gone missing in the meantime the cloud says which (409) and the client sends it.

A small patch skips steps 2 and 3: when the upload keeps part of the old version and its new shards add up to at most 8 MiB, they travel in the commit request itself, after the manifest, and the cloud checks each against its name before committing. The drive also remembers the manifests of versions it uploaded or patched, so planning the next patch needs no request. A second edit to a file is then one round trip.

A plain `PUT` with a body still works, for `curl`: the server cuts it into shards itself. `ludy put` uses the four steps and prints how much was sent and how much the cloud already had. Reads are unchanged: a `GET` with `Range` on the object, which the server maps onto the shards that cover it.

## Workspace

| Crate | What it is |
|---|---|
| [`ludy-proto`](crates/ludy-proto) | What a client and the cloud agree on: the JSON types both sides already shared (listings, the change feed, versions, the request log), the shard cutting rules and manifests, synthetic content. No network or async code. The commit reply, the server stats and the request bodies are still written out separately on each side |
| [`ludy-cloud`](crates/ludy-cloud) | The simulated cloud: library plus the `ludy-cloud` server binary |
| [`ludy-core`](crates/ludy-core) | The engine: cloud client, block cache, drive filesystem, patch planner; with the `mount` feature (on by default) also the NFS adapter, the mount manager and the desktop `Engine` |
| [`ludy-app`](crates/ludy-app) | The gpui app |
| [`ludy-cli`](crates/ludy-cli) | `ludy`, the engine without a window |

**Other clients.** The engine does not depend on the server: `ludy-core` uses `ludy-proto` for the wire types, and `ludy-cloud` only in its tests. Built with `--no-default-features`, `ludy-core` is the part with no NFS and no macOS-specific code, though it does use Unix file calls (`CloudClient`, `BlockCache`, `DriveFs`, the journal, the patch planner), and its tests pass in that build (23 unit, 19 engine, 23 chaos; the one left out tests the NFS adapter). That is the piece a second client would link: it would put its own front end (on iOS, a File Provider extension) where [`nfs.rs`](crates/ludy-core/src/nfs.rs) is now. It has only been built for macOS. A client that does not want the engine at all can use the HTTP API directly: folder listings with `prefix` and `delimiter`, `Range` reads, a plain `PUT`. The cloud has no sign-in, no HTTPS and no CORS headers yet, and the engine's HTTP client is built without TLS.

## What works, and how it was checked

Checked by `cargo test` (65 integration tests and 34 unit tests in the protocol, cloud and engine crates, no kernel mount involved; the cloud server's request log is the witness; 23 of the integration tests are the chaos tests described under [Chaos testing](#chaos-testing)):

- A listing shows real sizes, including a 50 GB synthetic file, with no object bytes downloaded.
- A 64 KiB read from the middle of the 50 GB file returns the right bytes (compared with the generator) and causes one range request of at most two blocks.
- A second read, and a read after "restarting" with a fresh cache object over the same folder, cause no request.
- Sequential reads trigger read-ahead in coalesced requests; a large read is fetched in parallel; six readers of one block share one request.
- The cache stays under its cap and evicts; pinned files survive eviction and read without the network.
- Writes return without waiting for the cloud, are readable immediately, and upload in the background; partial overwrites, appends and truncates merge correctly.
- Folder creation, rename (including the write-temp-then-rename save pattern) and delete are replayed in order.
- Changes made directly on the server (add, modify, delete) appear in the drive's tree in under two seconds.
- The NFS adapter serves lookups, paginated listings, reads and writes, and rejects file handles from an earlier mount.
- Server: HTTP range semantics, versions and restore, `If-Match` races, rename, fork without copying, long-poll, throttling, the range log, and persistence across restarts.
- Shards on the server: a 24 MiB object is stored as shards named by their SHA-256 and reads back across shard boundaries; the same content under three keys in two drives is stored once; a 4 KiB edit adds at most three shards; a shard whose body does not hash to its name, a manifest naming an absent shard, and one whose lengths do not add up are refused; deletes and the history cap release shards nothing else uses, and keep the ones a fork or another key still needs; a restart keeps what is in use and removes uploads that never committed; a data folder written before shards existed (whole-object blobs) still serves and its keys move to shards when next written.
- Shards from the engine: a new 10 MiB file goes up as shards, each byte once, and the same bytes written under another name send nothing; a 4 KiB write into a 24 MiB file sends at most three shards and under 8 MiB, and downloads under 8 MiB to re-cut the touched shard; 605,000 bytes written into a 50 GB synthetic file upload exactly 605,000 bytes and download nothing; truncating and growing a file keeps its untouched leading shards and never brings old bytes back.

Checked against real mounts on macOS 27.0.1, with the CLI and with the app (`--mount`), using the demo data from `scripts/seed-demo.sh`:

- **Listing.** `ls -la` shows every file at its real size, the 50 GB one included, and causes no object request.
- **Mid-file read.** A 64 KiB `dd` at the 25 GiB mark of the 50 GB file returns bytes equal to an independent SplitMix64 written in Python, and the server log shows one request: `bytes=26843545600-26843807743` (256 KiB). The cache grew by 256 KiB.
- **Video.** Playing `demo.mp4` (95 MB, 104 s) with AVPlayer, the framework Quick Look uses, from a hidden mount with an empty cache: first frame after 0.30 s, 7.6 s played in 8 s of wall time, 12 range requests of 256 KiB to 2 MiB covering the first 14.9 MB (16% of the file), cache at 14 MB. A Quick Look thumbnail (`qlmanage -t`) was produced from the mount.
- **Nothing reads behind your back.** A Finder-visible mount left untouched for 30 seconds caused no range request. During the earlier tests one 30-byte read of `shot-list.txt` appeared that the test did not make; its origin is unknown.
- **Finder.** The mount has no `nobrowse` flag, the sidebar lists `ludy.localhost` under Locations with an eject button, and the computer view lists the drive as a remote volume.
- **Writes.** `cp` of a 3 MB file returned in 0.08 s; the PUT followed in the background and the server's copy has the same SHA-256. `mkdir`, `mv` and `rm` reached the server in order. `._*` files stayed local.
- **Remote changes.** A file put on the server with `curl`, and one deleted there, showed in `ls` within three seconds. Repeated after the change feed went in: the server's log shows the mount asking `?feed` (1 changed; then 1 deleted) and no listing.
- **Shards.** On a hidden mount, with the server log as witness. `cp` of a 20 MiB file: 8 shard uploads, 20,971,520 bytes, then a commit with an empty body. `cp` of the same file under another name: the check answered "0 of 8 missing", no shard was sent, and the server's disk did not grow. Five bytes written at 10 MiB + 123 with `dd conv=notrunc`: one shard of 5,289,289 bytes sent, and the server's copy equals the expected patched file while the second copy is unchanged. 8 KiB written at 25 GiB + 4096 of the 50 GB synthetic file: one shard of 8,192 bytes sent (the kernel first read the 256 KiB block around the write, one range request); the file is still 50 GB; reading it back through the mount and from the server gives the generated bytes, then the 8 KiB, then generated bytes again; the server's data folder stayed at 116 MB. After a server restart the shard totals were unchanged (52 shards, 121,369,066 bytes) and `ludy put` of a 7 MB file sent 5 shards the first time and nothing under a second key.
- **Eject.** Ctrl-C in the CLI and SIGTERM to the app both ejected in one to two seconds, with a Finder window open on the volume, and a write made just before the eject was uploaded first. No mount was left behind in nine cycles.
- **The app.** Started with `--mount Footage`, it mounted the drive, showed the card as mounted with its path, and the status panel counted the range request and the cache blocks.
- **Crash.** See the eject section: the kernel drops the mount of a killed process after 55 to 57 seconds.

Checked by hand, without a mount:

- The server, with `curl`: upload of the 95 MB video, range reads compared against the source file, and mid-file bytes of a synthetic 50 GB object compared against the Python generator.
- The app, from screenshots: the drives screen with a drive mounted and not, the folder grid and list, the drive and file context menus, the share, fork, new-folder and versions dialogs, and an 820 by 560 window.
- Clicks, posted to the app's own window with `--click` and checked in a screenshot after each: Mount on a card (the drive mounted), the eject icon (it ejected), Open, a folder tile, a breadcrumb, the grid/list toggle, a sidebar drive, the new-folder button, the card's "..." menu and Fork in it, and Versions in a file's menu.

**Not verified:**

- **The rest of the app's controls.** Right-click itself (the injected event does not carry the right button, so the file menu was opened with `--demo`), typing in a dialog, Import, Show in Finder, Open in Terminal, Rename, Delete, Share's Send invite, Restore, Keep offline, Clear, and scrolling by hand.
- **The Quick Look panel** (space bar in the Finder). `qlmanage -p` crashes on any movie on this macOS version, the local file included, so playback was checked with AVPlayer instead.
- **Dragging a file in with the Finder**, and how quickly a Finder window redraws after a server-side change. Both were checked from the shell only.
- **Another app raising the "network volume" prompt during an eject.** The watchdog path for that is written but has only been reasoned about.

## Benchmarks

`ludy bench` runs fixed scenarios through the engine (the same calls the NFS adapter makes) against a cloud started in the same process, with the simulated link at 400 Mbit/s and 30 ms. No kernel mount is involved, so these are the engine's numbers. Release build, Apple silicon. The full tables are in [`bench/`](bench).

| Scenario | Before hardening | After | Notes |
|---|---|---|---|
| Link: one 64 KiB range request | 35 ms | 34 ms | straight to the server: the floor for a cold read |
| Link: one large range request | 34 MB/s | 35 MB/s | the simulated link delivers about 70% of its nominal 50 MB/s |
| Sequential read, cold | 41 MB/s | 41 MB/s | 256 MiB in 256 KiB reads; faster than one stream because fetches run in parallel |
| Sequential read, warm | 3,562 MB/s | 3,554 MB/s | from the disk cache |
| Random 64 KiB read, cold (p50 / p99) | 42 / 52 ms | 42 / 52 ms | one range request each; 7 ms over the link's floor |
| Random 64 KiB read, warm (p50 / p99) | 0.11 / 0.31 ms | 0.11 / 0.25 ms | |
| Open a drive of 5,000 files | 39 ms | 41 ms | fetch the listing, build the tree |
| Create 1,000 files of 4 KiB (writes return) | 70 ms | 377 ms | the cost of saving the journal's state on every write |
| Upload those 1,000 files | 105 s (10 files/s) | **6.3 s (159 files/s)** | eight at a time, one request each, instead of one at a time with three |
| Copy 64 MiB in (writes return) | 4,145 MB/s | 993 MB/s | same cost; still twenty times the link |
| Upload that copy | 1.77 s (38 MB/s) | 1.77 s (38 MB/s) | cut, hash, send, commit: the link's rate |
| Upload the same content again | 361 ms | 367 ms | every shard already in the cloud |
| Change 4 KiB in that file | 233 ms, 7.6x faster than re-uploading | 235 ms, 7.5x | Space states about 11x |
| Read 64 KiB of that file, cold | 33 ms, 58x faster than fetching it whole | 34 ms, 56x | Space states about 34x |

The second round, on scale (full tables in [`bench/`](bench), `scale-baseline` and `scale-after`):

| Scenario | Before | After | What changed |
|---|---|---|---|
| Change 4 KiB in a 64 MiB file | 231 ms, 7.7x faster than re-uploading | **121 ms, 14.8x** | one request instead of four (Space states about 11x) |
| The same at 100 ms latency | 579 ms, 3.9x | **242 ms, 9.1x** | |
| Open a drive of 50,000 files | 149 ms | 139 ms | still one full listing |
| Pick up one remote change in it | 105 ms | **32 ms** | ask for what changed instead of listing 50,000 files |
| Upload 500 small files into it | 8.0 s (62 files/s) | **3.1 s (161 files/s)** | the cloud appends a line per commit instead of rewriting the catalog |
| Delete 1,000 files from it | 49.7 s (20 files/s) | **2.4 s (422 files/s)** | sixteen deletes at a time; the queue on disk is appended to, not rewritten |

Everything in the first table is unchanged by this round except the patch row.

What the numbers say: reads are at the link's limit (a cold random read costs one round trip plus 7 ms); uploads of real data run at the link's rate; the two things that were slow or are now slower are both about many small files. A patch still re-sends the whole shard it touched (about 2 MiB for a 4 KiB edit), which is most of its 121 ms. And through a real mount the kernel's NFS client adds its own costs, which this benchmark does not see.

## Chaos testing

[`tests/chaos.rs`](crates/ludy-core/tests/chaos.rs) runs the engine against a cloud that misbehaves on purpose. The cloud used in tests can be killed (every request in flight is dropped) and restarted on the same data and port, which is what a deploy looks like to a client; it can also stall (accept and never answer), refuse everything with 503, hold commits back, and delete shards no manifest lists yet. Each test ends with the same check: once the cloud is back and the drive has settled, the cloud holds exactly the files the application wrote, byte for byte, nothing else, and deleting everything leaves no shard or manifest behind. Faults fire on observed events (the fortieth upload, a commit waiting), not on sleeps, and every step has a timeout, so a hang fails the test.

The scenarios: a flood of 360 files and a 12 MiB copy with the cloud killed and restarted in the middle and more files written during the outage; the same flood with the cloud refusing requests while a folder is renamed and a file deleted; shards swept between upload and commit (the commit is refused with the list, the client sends them again); a restart between the shards and the commit; a stalled cloud; the background uploader and a flush running together through renames and deletes; paths longer than the cloud accepts; a file deleted in the cloud under a pending local patch; a listing that is already out of date; a damaged cache block; writes, renames and deletes on files that are mid-upload; awkward names (percent signs, quotes, emoji, 255 bytes); two writers on one file; the app killed, or quit, with unsent writes while the cloud is down.

A randomized soak (`soak`, ignored by default) drives 1,200 random operations per run (create, patch, resize, rename files and folders, delete, read back and compare) while it randomly kills and restarts the cloud, stalls it, makes it refuse requests, and kills and restarts the app. The seed is printed. After the fixes below, 40 consecutive seeds pass. Since the scale round the soak also has a second writer changing the cloud's side of the drive, and a separate test (`applying_only_what_changed_matches_a_full_listing`) makes 400 random remote changes and checks after each that the drive, asking only for changes, shows exactly what a full listing would; 80 more soak seeds and 30 seeds of that test pass, the last 55 with every chaos test also comparing the drive's whole tree with the cloud's listing at the end.

On a real mount, [`scripts/chaos-mount.sh`](scripts/chaos-mount.sh) was run once with the release binaries on a hidden mount: 2,000 small files and a 200 MiB copy written through the mount, `kill -9` of the cloud four seconds in, restart five seconds later: all 2,001 files in the cloud, none wrong. Then the cloud was killed, 500 more files written, and the mounting process killed with `kill -9` as well; after both were started again the 500 files arrived, none wrong. The kernel took about two minutes to drop the dead mount this time (nothing was touching it; with a reader on it, it took 55 seconds before).

What this found, all fixed:

| Found | Now |
|---|---|
| Writes that had not reached the cloud were deleted when the app quit or was killed while the cloud was down | The journal's state and the queue of renames and deletes are saved on disk and picked up at the next start |
| A cloud file renamed locally could not be read, and a patch to it could not upload, until the rename had reached the cloud | Each remote version carries the key the cloud currently has it under |
| The background uploader and a flush (eject, quit) could run at once and replay a rename under an upload in flight | One sync pass at a time |
| A stalled cloud hung reads and uploads for ever | A read gives up after 8 seconds without a byte (below the NFS client's own 10 second timeout, so the mount stays alive); every request has a time limit |
| A path longer than the cloud's 1,024-byte key limit queued a change the cloud would never accept, blocking everything behind it | Refused when it is created (`ENAMETOOLONG`); a change the cloud rejects is dropped with an error |
| A patch whose base version was deleted in the cloud failed for ever and held up other uploads | If the drive still has the rest of the file (it uploaded it, and its cache holds the blocks), the edit is saved and the file exists again: the last writer wins and nothing is lost. If it does not, the cloud's delete wins and the drive reports it. Either way the rest continues |
| The same, for a file the drive had uploaded itself, after manifests began to be remembered: the plan came from memory, the cloud's "gone" was never seen, and the upload failed for ever (found in review of the scale round) | A failed upload forgets the remembered manifest; the retry asks the cloud |
| A folder that existed only through its content (no marker object) stayed in the drive after its last file was deleted or moved out locally, though the cloud no longer had it | Emptying a folder locally gives it a marker object, so it stays in both places until it is removed itself |
| A listing fetched before an upload finished could be applied after it, making the new file vanish or revert | A listing is only applied if the drive has changed nothing in the cloud since it was requested |
| A cache block cut short on disk was served as data | Its length is checked; it is discarded and fetched again |
| A new file renamed over a file the cloud has, then deleted before its upload, left the cloud's file in place (found by the soak) | The replaced file is deleted in the cloud explicitly |
| Uploads ran one file at a time with three round trips each | Eight at a time; one request for a file that fits one shard |
| Simulated cloud: a request cancelled by a disconnecting client could leave content referenced for ever (found by the soak) | Changes to the store run to their end whatever happens to the request |
| Simulated cloud: its rate limiter held a fast link to about half its rate | Short waits are batched |

Known and not fixed:

- A byte flipped inside a cached block is not detected; blocks have no checksum.
- A full disk while writing the journal turns into an I/O error for the application, and nothing has tested what happens next.
- After a crash, the first listing must succeed before recovered writes show up, so a drive cannot be mounted offline to get at them.
- The dead-mount wait after a crash is the kernel's and can be two minutes.
- Opening a drive is still one full listing held in memory (139 ms and one JSON document for 50,000 files); a drive of millions of files would need a listing in pages and a tree that is not all in memory. Uploads run eight at a time with one commit each; there is no batched commit.
- The cloud remembers the last 200,000 changed keys per drive for the change feed, in memory; a client further behind than that, or any client after a server restart, does one full listing.
- The chaos script on a real mount was run once; the Finder was not involved.

## Not built

- **Shards on the read path and in the cache.** Reads are range requests on the object, and the disk cache is keyed by version and block, not by shard. So after a patch the Mac's cache does not carry over to the new version except for the file that was just uploaded, and two files with the same content are cached twice.
- **Insert.** A write at an offset is a patch; inserting bytes in the middle of a file through the mount is a rewrite of everything after it, as in any filesystem. Content-defined cutting resynchronises (a unit test covers it), so most of the shards after the insert are found in the cloud and not sent again, but they are read and hashed.
- **Cleanup of abandoned uploads while the server runs.** Shards sent for an upload that never commits stay until the server restarts.
- **Not exercised:** the client's second attempt after the cloud reports a missing shard at commit (the server's 409 is tested; the retry has never fired).
- **A second user.** Sharing records an email and a role on the drive and shows who has access. There are no accounts, and nothing enforces View versus Edit.
- **Thumbnails** in the in-app browser, drag and drop, search, and a menu-bar launcher.
- **An app bundle.** The app runs from `cargo run` and is unsigned.
