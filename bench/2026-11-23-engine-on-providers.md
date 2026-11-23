# The engine against both providers, and chaos on a real mount

2026-11-23, on one Mac (Apple silicon, macOS 27.0), loopback. The first numbers for the end-to-end encrypted engine: `ludy bench` did not run against it before this date. Release build of `ludy`.

The two columns are not a comparison of the providers as they would run for real. "Compose" is `docker compose up` with the published arm64 image and blobs in a Docker volume, so every request crosses Docker Desktop's network into its Linux VM. "wrangler dev" is the Worker under workerd as a process on the Mac, with R2 and the Durable Objects emulated on local disk. Neither has any latency or bandwidth limit added.

## `ludy bench`, three runs each

Runs alternated between the two servers after the first. Each cell is the three runs in order.

| Scenario | Compose (ludy-rivet, volume) | wrangler dev (ludy-worker) | Notes |
|---|---|---|---|
| Copy a file in: writes return | 732 MB/s · 1055 MB/s · 1018 MB/s | 838 MB/s · 964 MB/s · 1060 MB/s | 64 MiB of incompressible data into the journal |
| Copy a file in: upload | 890 ms (75 MB/s) · 986 ms (68 MB/s) · 941 ms (71 MB/s) | 1.42 s (47 MB/s) · 1.43 s (47 MB/s) · 1.95 s (34 MB/s) | cut, seal, send the sealed shards, commit a sealed manifest |
| Copy the same content in again: upload | 680 ms · 735 ms · 693 ms | 728 ms · 689 ms · 707 ms | cut and sealed again; every sealed shard is already in the cloud |
| Change 4 KiB inside that file: upload | 140 ms · 68 ms · 110 ms | 49 ms · 187 ms · 83 ms | only the shard the write touched is cut, sealed and sent |
| Read 64 KiB out of that file, cold | 7.16 ms · 48 ms · 5.28 ms | 6.76 ms · 5.18 ms · 6.22 ms | one range request, after the manifest |
| First 2 MiB of a file, cold | 155 ms · 144 ms · 173 ms | 28 ms · 23 ms · 19 ms | the manifest, then 256 KiB reads from the start of the file |
| Sequential read, cold | 145 MB/s · 133 MB/s · 124 MB/s | 351 MB/s · 342 MB/s · 333 MB/s | 256 MiB; 256 KiB reads, as the kernel issues them |
| Sequential read, warm | 3181 MB/s · 3077 MB/s · 3087 MB/s | 3189 MB/s · 3144 MB/s · 2989 MB/s | 256 MiB; the same bytes again, from the disk cache |
| Random 64 KiB read, cold | 8.48 ms / 58 ms · 9.23 ms / 57 ms · 7.27 ms / 57 ms | 3.64 ms / 13 ms · 3.61 ms / 12 ms · 3.76 ms / 12 ms | 200 reads; p50 / p99; a block each, nothing cached |
| Random 64 KiB read, warm | 0.08 ms / 0.16 ms · 0.07 ms / 0.15 ms · 0.09 ms / 0.18 ms | 0.04 ms / 0.09 ms · 0.05 ms / 0.09 ms · 0.05 ms / 0.10 ms | 200 reads; p50 / p99; from the disk cache |
| Create many small files | 2.12 s · 2.16 s · 2.16 s | 2.48 s · 2.09 s · 2.04 s | 5000 files of 4 KiB, until the writes returned |
| Upload those files | 25.46 s (196 files/s) · 28.30 s (177 files/s) · 27.16 s (184 files/s) | 48.00 s (104 files/s) · 48.76 s (103 files/s) · 51.64 s (97 files/s) | until all 5000 are in the cloud |
| Open a drive with many files | 112 ms · 134 ms · 127 ms | 110 ms · 104 ms · 106 ms | 5004 nodes: read the feed, unwrap every key, open every name |
| List that folder | 0.41 ms · 0.62 ms · 0.38 ms | 0.37 ms · 0.41 ms · 0.53 ms | from the tree in memory |
| List that folder in pages | 0.27 ms · 0.28 ms · 0.27 ms | 0.25 ms · 0.26 ms · 0.30 ms | 5000 files, 100 at a time, as the NFS client asks for them |
| Pick up one remote change | 1.69 ms · 1.71 ms · 1.76 ms | 2.66 ms · 2.46 ms · 2.63 ms | one request to the feed of a drive of 5005 nodes |
| Delete files | 947 ms (1055 files/s) · 707 ms (1415 files/s) · 638 ms (1567 files/s) | 376 ms (2661 files/s) · 484 ms (2067 files/s) · 396 ms (2525 files/s) | 1000 deletes, sent in order, 200 to a commit |

What the two columns show, without having looked into why:

- **Reads through Compose are slower and less even.** A cold random read is 7 to 9 ms at the median and 57 ms at p99 through Compose, against 3.7 and 12 ms; the first 2 MiB of a file take 144 to 173 ms against 19 to 28 ms; a cold sequential read runs at 124 to 145 MB/s against 333 to 351. Whether that is Docker's network on a Mac, the server's front or the volume was not measured.
- **Uploads go the other way.** A 64 MiB file goes up at 68 to 75 MB/s through Compose and 34 to 47 under `wrangler dev`, and 5,000 small files at 177 to 196 a second against 97 to 104.
- **A second copy of the same content still takes about 0.7 s for 64 MiB.** Nothing is sent, but every shard is cut and sealed again to learn its name.
- **Single-request rows vary between runs** by more than the difference between the servers: the 4 KiB patch took 49 to 187 ms, and one cold 64 KiB read took 48 ms where the others took 5 to 7.

`ludy bench` makes a drive of its own, empties it at the end and leaves it behind. Each number above is one drive on a server that had run the earlier rounds.

## The app scrolling 5,000 files

`cargo run -p ludy-app -- --open Stress/clips --perf`, dev build, 60 Hz display, the folder made by `scripts/seed-stress.sh` on the Compose server. One run of each.

| View | Frames in 6 s | Frame time p50 / p95 / p99 / max | Frames over 1.5x the median | Building elements |
|---|---|---|---|---|
| Grid | 360 (59.8 fps) | 16.67 / 17.88 / 18.66 / 33.62 ms | 1 | 0.79 ms a frame |
| List | 361 (60.0 fps) | 16.67 / 18.22 / 18.53 / 18.71 ms | 0 | 0.72 ms a frame |

`scripts/seed-stress.sh` itself: 5,000 files, 5.2 MiB, uploaded in 28.6 s.

## Chaos on a real mount

`scripts/chaos-mount.sh <dir> 2000 200 9310`: a Compose project of its own (published image, blobs in a volume), a drive mounted hidden with the release `ludy`, and a second keystore with its own cache that downloads and compares.

| Time | Step |
|---|---|
| 08:37:04 | Server started, drive made and mounted |
| 08:37:16 | Flood of 2,000 small files and a 200 MiB copy through the mount |
| 08:37:21 | Both containers killed with SIGKILL |
| 08:37:26 | `docker compose up -d --wait` on the same volumes |
| 08:37:46 | The writers had finished |
| by 08:39:35 | 2,000 small files and the 200 MiB file downloaded by the second keystore, all as written |
| 08:39:35 | Server killed again; 500 more files written through the mount; `kill -9` of the mounting process |
| 08:39:38 | One `ls` of the dead mount, left to time out |
| 08:40:32 | The kernel had dropped the mount (54 s); server and mount started again |
| by 08:41:40 | The 500 files downloaded by the second keystore, all as written; the remounted drive lists 500 |
| 08:41:42 | Ejected. Passed |

Run once to the end. An earlier run the same morning passed its first half the same way and was stopped in its second by a mistake in the script (it killed a shell instead of the mounting process, so the mount never died). That run is also where it showed that a dead mount nothing touches stays in the mount table: it was still there three minutes after the kill, and went 50 s after an `ls`.

The eight seconds between "server comes back" and "writers finished" plus the wait for the uploads are not a measure of anything: the check polls every few seconds and downloads everything each time.

## Compose with the blobs in Cloudflare R2, quick sizes

`ludy bench --quick`, one run, against a Compose project whose server keeps its blobs in a real R2 bucket (region `auto`). The Mac reached the server over loopback; the server's calls to R2 crossed the internet over this Mac's connection, whose speed was not measured. Not comparable row for row with the tables above, which are full sizes.

| Scenario | Result | Notes |
|---|---|---|
| Copy a file in: writes return | 856 MB/s | 8 MiB of incompressible data into the journal |
| Copy a file in: upload | 1.04 s (8 MB/s) | cut, seal, send the sealed shards, commit a sealed manifest |
| Copy the same content in again: upload | 420 ms | cut and sealed again; every sealed shard is already in the cloud |
| Change 4 KiB inside that file: upload | 733 ms (1.4x faster than uploading it) | only the shard the write touched is cut, sealed and sent |
| Read 64 KiB out of that file, cold | 359 ms (2x faster than reading it whole) | 1 range request(s), after the manifest; reading all 8 MiB cold took 854 ms |
| First 2 MiB of a file, cold | 797 ms | the manifest, then 256 KiB reads from the start of the file |
| Sequential read, cold | 20 MB/s | 32 MiB; 256 KiB reads, as the kernel issues them |
| Sequential read, warm | 3002 MB/s | 32 MiB; the same bytes again, from the disk cache |
| Random 64 KiB read, cold | 113 ms / 230 ms | 40 reads; p50 / p99; a block each, nothing cached |
| Random 64 KiB read, warm | 0.11 ms / 0.22 ms | 40 reads; p50 / p99; from the disk cache |
| Create many small files | 242 ms | 500 files of 4 KiB, until the writes returned |
| Upload those files | 74.51 s (7 files/s) | until all 500 are in the cloud |
| Open a drive with many files | 18 ms | 504 nodes: read the feed, unwrap every key, open every name |
| List that folder | 0.03 ms | from the tree in memory |
| List that folder in pages | 0.03 ms | 500 files, 100 at a time, as the NFS client asks for them |
| Pick up one remote change | 4.16 ms | one request to the feed of a drive of 505 nodes |
| Delete files | 89 ms (1125 files/s) | 100 deletes, sent in order, 200 to a commit |

The same morning, by hand against that bucket: the 95 MB demo video uploaded in 11.0 s; five files, 117.6 MiB, downloaded cold in 9.9 s; 64 objects deleted by the collector within about eleven seconds of the deletes, with `GC_GRACE_MS=5000`.
