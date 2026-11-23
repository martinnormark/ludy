# What works, and how it was checked

Four kinds of check, in four sections: automated tests, real mounts on macOS, the command line and scripts by hand, and the app from screenshots. Everything here was done on 2026-11-23 on one Mac over loopback unless a line says otherwise, with the engine the app uses now and a real server. What was not checked is in [known problems](../known-issues.md#not-verified); the largest gaps are the Finder and applications on a mount, most of the app's controls, and anything deployed.

## By automated tests

`cargo test`, which needs no server: 83 tests pass, in the protocol, the engine, the server's logic and the app.

- **The format.** The Rust code reproduces test vectors written by a second implementation in JavaScript ([`crypt-v1.mjs`](../../crates/ludy-proto/tests/vectors/crypt-v1.mjs)). Unit tests cover damage: a flipped byte, swapped frames, a missing tail, a frame of another shard, a key, name or manifest moved to another node.
- **The server's logic**, on SQLite in memory: the rules of a commit, the feed, the collector, and a commit that fails at every statement in turn leaving all of it or none.
- **The keystore.** Drives are kept per server, survive a reopen, cannot share a name or be listed twice; the file is mode 0600; a file that does not parse is not replaced.
- **The mount's bookkeeping**, the cache, the patch planner and the range arithmetic.

Against a server the test starts, on each provider (ignored in a plain `cargo test`; see [chaos testing](chaos-testing.md)): the server under chaos, 13 tests, and the engine under chaos, 18 tests, all pass on both. They cover reads that fetch only what is asked for, uploads that send only what changed, crashes with unsent writes, two Macs on one drive, restoring a version, synthetic files, copying folders in and out, and drive keys.

## Against real mounts

Hidden mounts (`nobrowse`) under a scratch folder, driven from the shell.

**From the Compose server**, a drive seeded with the CLI:

- **Listing.** `ls -la` showed every file at its size, a 50 GiB synthetic one included (53,687,091,200 bytes).
- **Reading.** A 3 MB file read through the mount compared equal to its source. 64 KiB read at 26 GB into the synthetic file came back with no range request; that the bytes are the right ones is checked by the engine's tests, not here.
- **A cold read in the middle.** With an empty cache, 64 KiB read with `dd` from the middle of a 24 MiB file compared equal to the source and caused one range request; 513 KiB came down in all, the manifest included.
- **Writing.** A new text file, a new folder, a 5 MB copy into it, a rename with a move, and five bytes written in place at 1 MB with `dd conv=notrunc`. A second keystore with its own cache then downloaded the files: the patched 5 MB file equalled the expected bytes and the text file its content.
- **Eject.** Ctrl-C ejected in under two seconds and left no mount behind.
- **One drive, one process.** While the drive was mounted, `ludy ls` on it from the same keystore was refused with a message naming the cause; after the eject it worked.

**From the Worker under `wrangler dev`:** a drive created, a 24 MiB file, a folder and a synthetic file put in it, then mounted. Listing showed all three; a 64 KiB read from the middle of the 24 MiB file and a whole 3 MB file compared equal to their sources, both from the cache the upload had filled; a text file and a 5 MB copy written through the mount were read back, equal, from a second keystore with an empty cache.

**From the app:** started with `--mount Footage`, it mounted the drive, `ls` on the mount point listed its three entries, the card showed the drive as mounted with its path and size, and stopping the app with SIGTERM ejected it and left no mount behind.

### Chaos on a real mount

[`scripts/chaos-mount.sh`](../../scripts/chaos-mount.sh), once to the end: 2,000 small files and a 200 MiB copy written through a mount while both of the server's containers were killed and restarted, then 500 more files written with the server down and the mounting process killed with `kill -9`. Every file reached the server as written, checked by download from a second keystore. More in [chaos testing](chaos-testing.md#a-real-mount).

## The command line and the scripts

- **Every `ludy` command** was run against the Compose server: `server`, `drives`, `mkdrive`, `key`, `add`, `ls`, `put` of a file and of a folder, `get` of a file and of a folder, `synth`, `mount`, `bench`. `forget` was run against the Worker under `wrangler dev`.
- **A drive's key on a second keystore.** `ludy key` on one, `ludy add` on another with its own cache, and the second read what the first wrote. This stands in for a second Mac throughout.
- **`scripts/seed-demo.sh`** made three drives, uploaded the 95 MB video in 5.3 s and added five synthetic files of 12 to 180 GB. Run a second time it changed nothing and sent 2.3 KiB for the video: every shard was already there.
- **`scripts/seed-stress.sh`** uploaded 5,000 small files in 28.6 s.
- **TLS.** `LUDY_SERVER=https://example.com ludy server` completed a handshake and reported that what answered is not a Ludy server. No Ludy server has been reached over `https://`.

## The app, from screenshots

Taken of the app's own window against the Compose server with the demo drives:

- The drives screen: three cards, one mounted with its path and "79.6 GB · 4 files", two showing "End-to-end encrypted" because they had not been opened; the sidebar's "Server connected" and `ludy-rivet · 127.0.0.1:8090`.
- A drive's folder as a grid and as a list, opened without mounting: names, sizes and dates read from the encrypted tree.
- The drive menu (Open, Show in Finder, Open in Terminal, Copy path, Rename, Copy drive key, Mount, Remove drive) and the file menu (Open, Show in Finder, Open in Terminal, Copy path, Rename, Versions, Delete), opened with `--demo`.
- The drive-key dialog.
- Names in a folder of long and awkward ones (no spaces, many dots, accents, a run of forty W's) and in a folder of 1,500 names of 30 to 70 characters: two lines in the grid and one in the list, the middle left out and the extension readable, at the default window size and at 820 by 560.
- The details pane, opened by a click posted with `--click` on a tile and on a list row: the name in full, kind, size, date, place, and the tile or row shown as selected.
- A double-click posted with `--dclick` on a file in the last column of the grid, at a point where the pane is blank once open and at a point where its Show in Finder button is: the pane opened, the tiles moved, and both times the file the first click selected was the one opened (the drive was not mounted, so what showed was the notice asking to mount it to open files, not the one about the Finder).
- That opening the pane moves nothing beside it: screenshots of a folder before and after a file was selected, at window widths of 820, 908, 1000, 1180 and 1400 points, compared pixel by pixel over the header and the first row left of the pane. No pixel differed at 1000, 1180 and 1400 with a file of the second row selected. With a file of the first row selected and the selection's background colour ignored, none differed at those three, and at 820 and 908 the only ones were where the uncovered shot shows the start of the next tile's name and the other shows the margin before the pane.
- The list with the pane open in a window 908 points wide: two columns left, Name and Size, and names still readable.
- A file selected in the second row of a window 480 points high: after the grid went from six tiles in a row to four, the selected tile was on screen.

Scrolling a folder of 5,000 files was measured with `--perf`; the numbers are in [the app](../how-it-works/app.md#scrolling-a-large-folder).

Apart from those posted clicks, no control was clicked in the window. What each does goes through engine calls that the tests cover, and the list of what was not exercised is in [known problems](../known-issues.md#the-app).

## The ways of hosting

Each [hosting](../hosting/README.md) page ends with what was checked for it. In one line each: Compose with a volume, Compose with MinIO, Compose with a real R2 bucket, the binary beside an engine and the Worker under `wrangler dev` were all started and used today; the deployed Worker has never been run.

## What was checked before and not again

These were seen on the engine Ludy started on, against the simulated server, and are recorded in [history](../history/verification.md). The mount adapter and the app's window code are largely the same, but none of this has been repeated on the current engine:

- A Finder-visible mount: the sidebar entry, the eject button, a Finder window open during an eject.
- Video playback from a mount (first frame after 0.30 s with AVPlayer) and a Quick Look thumbnail.
- That an untouched mount causes no reads.
