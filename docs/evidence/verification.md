# What works, and how it was checked

Three kinds of check, in three sections: automated tests with no kernel mount involved, real mounts on macOS, and by hand. The last section lists what was not verified. All of this is about the engine and the simulated cloud; the encrypted cloud's checks are in [the real cloud](../real-cloud/overview.md).

## By automated tests

Checked by `cargo test` (68 integration tests and 34 unit tests in the protocol, cloud and engine crates, no kernel mount involved; the cloud server's request log is the witness; 24 of the integration tests are the chaos tests described under [Chaos testing](chaos-testing.md)):

- A listing shows real sizes, including a 50 GB synthetic file, with no object bytes downloaded.
- A 64 KiB read from the middle of the 50 GB file returns the right bytes (compared with the generator) and causes one range request of at most two blocks.
- A second read, and a read after "restarting" with a fresh cache object over the same folder, cause no request.
- Sequential reads trigger read-ahead in coalesced requests; a large read is fetched in parallel; six readers of one block share one request. A reader that wants the first block of a 2 MiB request gets it while the rest is still arriving.
- The cache stays under its cap and evicts; pinned files survive eviction and read without the network.
- Writes return without waiting for the cloud, are readable immediately, and upload in the background; partial overwrites, appends and truncates merge correctly.
- Folder creation, rename (including the write-temp-then-rename save pattern) and delete are replayed in order.
- Changes made directly on the server (add, modify, delete) appear in the drive's tree in under two seconds.
- The NFS adapter serves lookups, paginated listings, reads and writes, and rejects file handles from an earlier mount. A folder read in pages of any size gives every entry once; a page asked for after an entry that has since gone starts the listing over.
- Server: HTTP range semantics, versions and restore, `If-Match` races, rename, fork without copying, long-poll, throttling, the range log, and persistence across restarts.
- Shards on the server: a 24 MiB object is stored as shards named by their SHA-256 and reads back across shard boundaries; the same content under three keys in two drives is stored once; a 4 KiB edit adds at most three shards; a shard whose body does not hash to its name, a manifest naming an absent shard, and one whose lengths do not add up are refused; deletes and the history cap release shards nothing else uses, and keep the ones a fork or another key still needs; a restart keeps what is in use and removes uploads that never committed; a data folder written before shards existed (whole-object blobs) still serves and its keys move to shards when next written.
- Shards from the engine: a new 10 MiB file goes up as shards, each byte once, and the same bytes written under another name send nothing; a 4 KiB write into a 24 MiB file sends at most three shards and under 8 MiB, and downloads under 8 MiB to re-cut the touched shard; 605,000 bytes written into a 50 GB synthetic file upload exactly 605,000 bytes and download nothing; truncating and growing a file keeps its untouched leading shards and never brings old bytes back.

## Against real mounts

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
- **Crash.** See [the eject section](../how-it-works/mount.md#eject-and-the-network-volume-prompt): the kernel drops the mount of a killed process after 55 to 57 seconds.

## By hand, without a mount

Checked by hand, without a mount:

- The server, with `curl`: upload of the 95 MB video, range reads compared against the source file, and mid-file bytes of a synthetic 50 GB object compared against the Python generator.
- The app, from screenshots: the drives screen with a drive mounted and not, the folder grid and list, the drive and file context menus, the share, fork, new-folder and versions dialogs, and an 820 by 560 window.
- Clicks, posted to the app's own window with `--click` and checked in a screenshot after each: Mount on a card (the drive mounted), the eject icon (it ejected), Open, a folder tile, a breadcrumb, the grid/list toggle, a sidebar drive, the new-folder button, the card's "..." menu and Fork in it, and Versions in a file's menu.

## Not verified

- **The rest of the app's controls.** Right-click itself (the injected event does not carry the right button, so the file menu was opened with `--demo`), typing in a dialog, Import, Show in Finder, Open in Terminal, Rename, Delete, Share's Send invite, Restore, Keep offline, Clear, and scrolling by hand.
- **The Quick Look panel** (space bar in the Finder). `qlmanage -p` crashes on any movie on this macOS version, the local file included, so playback was checked with AVPlayer instead.
- **Dragging a file in with the Finder**, and how quickly a Finder window redraws after a server-side change. Both were checked from the shell only.
- **Another app raising the "network volume" prompt during an eject.** The watchdog path for that is written but has only been reasoned about.
