# Ludy

Cloud drives that mount in the Finder and stream on demand. A drive holding hundreds of gigabytes is listed in full, opens in any app, and takes almost no local disk: bytes are fetched from the cloud only when an application reads them.

The "cloud" here is simulated: a local S3-like server with configurable latency and bandwidth. The Mac app is written in Rust with [gpui](https://gpui.rs/).

**Status (2026-11-19):** the server, the engine and the app are built, covered by automated tests, and checked against real mounts: listing, range-only reads, video playback, writes, remote changes, eject and the crash case all work (see [What works](#what-works-and-how-it-was-checked)). Not checked: clicking the app's buttons, and the Quick Look panel itself. One thing does not work as intended: the Finder shows a file's full size as its size "on disk". Read [Eject and the "network volume" prompt](#eject-and-the-network-volume-prompt) for a hazard to know about.

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
| `cargo run -p ludy-app -- --open Footage/raw` | Start in the file browser |
| `cargo run -p ludy-cli -- mount Footage` | Mount from the terminal, no window; Ctrl-C ejects |
| `cargo run -p ludy-cli -- log --follow` | Tail the cloud's range log |
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

**Cache.** Blocks are keyed by `(version id, block index)`. A version never changes, so a cached block is never stale. The cache has a byte cap and evicts the least recently used blocks. The NFS server reports each file's locally held bytes as its space used, but `stat` and `du` on the mount report the full size regardless, so the Finder's "on disk" figure shows the cloud size. Sending a fixed value instead changed nothing, so the figure does not follow what the server sends; whether the macOS client or the NFS library drops it was not established. What the Mac really holds is shown in the app, under "On this Mac".

**Writes.** A write goes to a sparse journal file and returns. An existing cloud file is not downloaded first: the journal records which byte ranges were written and reads merge them with the cloud version underneath. When a file has been quiet for 1.5 seconds it is uploaded as a new version. Renames, deletes and new folders are queued and replayed on the cloud in order. `.DS_Store` and `._*` files stay on the Mac.

**Remote changes.** Each mounted drive long-polls the cloud for its next commit and folds the new listing into the tree, keeping node ids stable. A listing is never applied while local changes are still uploading.

**The cloud.** A drive is a bucket. Objects support list, HEAD, GET with `Range`, PUT and DELETE. Every PUT creates an immutable version whose id is the ETag; history is kept (25 versions per key) and an old version can be restored without writing content. Every commit to a drive goes through that drive's lock and bumps its sequence number; `If-Match` makes one of two racing writers lose with a 412. A fork is a new catalog pointing at the same versions. Synthetic objects are the SplitMix64 stream for a seed, generated from the offset.

## Workspace

| Crate | What it is |
|---|---|
| [`ludy-cloud`](crates/ludy-cloud) | The simulated cloud: library plus the `ludy-cloud` server binary |
| [`ludy-core`](crates/ludy-core) | The engine: cloud client, block cache, drive filesystem, NFS adapter, mount manager |
| [`ludy-app`](crates/ludy-app) | The gpui app |
| [`ludy-cli`](crates/ludy-cli) | `ludy`, the engine without a window |

## What works, and how it was checked

Checked by `cargo test` (25 integration tests and 15 unit tests, no kernel mount involved; the cloud server's request log is the witness):

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

Checked against real mounts on macOS 27.0.1, with the CLI and with the app (`--mount`), using the demo data from `scripts/seed-demo.sh`:

- **Listing.** `ls -la` shows every file at its real size, the 50 GB one included, and causes no object request.
- **Mid-file read.** A 64 KiB `dd` at the 25 GiB mark of the 50 GB file returns bytes equal to an independent SplitMix64 written in Python, and the server log shows one request: `bytes=26843545600-26843807743` (256 KiB). The cache grew by 256 KiB.
- **Video.** Playing `demo.mp4` (95 MB, 104 s) with AVPlayer, the framework Quick Look uses, from a hidden mount with an empty cache: first frame after 0.30 s, 7.6 s played in 8 s of wall time, 12 range requests of 256 KiB to 2 MiB covering the first 14.9 MB (16% of the file), cache at 14 MB. A Quick Look thumbnail (`qlmanage -t`) was produced from the mount.
- **Nothing reads behind your back.** A Finder-visible mount left untouched for 30 seconds caused no range request. During the earlier tests one 30-byte read of `shot-list.txt` appeared that the test did not make; its origin is unknown.
- **Finder.** The mount has no `nobrowse` flag, the sidebar lists `ludy.localhost` under Locations with an eject button, and the computer view lists the drive as a remote volume.
- **Writes.** `cp` of a 3 MB file returned in 0.08 s; the PUT followed in the background and the server's copy has the same SHA-256. `mkdir`, `mv` and `rm` reached the server in order. `._*` files stayed local.
- **Remote changes.** A file put on the server with `curl`, and one deleted there, showed in `ls` within three seconds.
- **Eject.** Ctrl-C in the CLI and SIGTERM to the app both ejected in one to two seconds, with a Finder window open on the volume, and a write made just before the eject was uploaded first. No mount was left behind in nine cycles.
- **The app.** Started with `--mount Footage`, it mounted the drive, showed the card as mounted with its path, and the status panel counted the range request and the cache blocks.
- **Crash.** See the eject section: the kernel drops the mount of a killed process after 55 to 57 seconds.

Checked by hand, without a mount:

- The server, with `curl`: upload of the 95 MB video, range reads compared against the source file, and mid-file bytes of a synthetic 50 GB object compared against the Python generator.
- The app: it launches, lists drives with live totals from the server, and the file browser shows a drive's folders and files (screenshots of both screens).

**Not verified:**

- **Clicks in the app.** Every button is wired and compiles, and Mount and quit-ejects were exercised through `--mount` and SIGTERM, which call the same engine functions. No button, menu or dialog has been clicked: synthetic clicks need an Accessibility permission this build was not given.
- **The Quick Look panel** (space bar in the Finder). `qlmanage -p` crashes on any movie on this macOS version, the local file included, so playback was checked with AVPlayer instead.
- **Dragging a file in with the Finder**, and how quickly a Finder window redraws after a server-side change. Both were checked from the shell only.
- **Another app raising the "network volume" prompt during an eject.** The watchdog path for that is written but has only been reasoned about.

**Does not work as intended:**

- The Finder's "on disk" size is the full file size, not the cached bytes (see Cache above).

## Not built

- **Content-defined shards.** Objects are stored whole, one blob per version. There is no FastCDC chunking, no SHA-256 shard naming and no deduplication, so an edit re-uploads the whole file. Versions, restore and fork exist, but on whole-object versions.
- **Patch uploads.** A partial overwrite is merged locally and uploaded as a complete new object.
- **A second user.** Sharing records an email and a role on the drive and shows who has access. There are no accounts, and nothing enforces View versus Edit.
- **Thumbnails** in the in-app browser, drag and drop, search, and a menu-bar launcher.
- **Journal recovery.** Writes that have not uploaded when the app is killed are lost; a clean quit or eject uploads them first.
- **An app bundle.** The app runs from `cargo run` and is unsigned.
