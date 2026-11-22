Ludy Worker (`app/worker`) under chaos: baseline, 2026-11-22. Local `wrangler dev` 4.166.0 (workerd, simulated R2 and Durable Object storage), release build of the Worker, Apple silicon. No simulated latency: these are loopback numbers, useful for comparing one run of this suite with the next, not for predicting a deployed Worker.

`cargo test -p ludy-core --test worker_chaos -- --ignored --test-threads=1 --nocapture`

Soak, five seeds of 500 steps (`WORKER_CHAOS_SEED=11..15 WORKER_CHAOS_STEPS=500`), all passing. Six replies and four requests in a hundred are lost by the proxy throughout.

| | Seed 11 | Seed 12 | Seed 13 | Seed 14 | Seed 15 | Notes |
|---|---|---|---|---|---|---|
| Wall time | 68 s | 56 s | 68 s | 94 s | 117 s | mostly restarts: about 2.85 s each |
| Commits applied | 463 | 434 | 491 | 522 | 554 | equal to the commits the drive remembers, every run |
| Commits the client saw answered from memory | 37 | 22 | 35 | 42 | 63 | the first answer was lost or the Worker died first |
| Commits refused, all predicted by the model | 89 | 109 | 93 | 97 | 99 | name taken, stale base, no such node, cycle, malformed |
| Requests sent / retried | 2830 / 618 | 2131 / 197 | 3130 / 806 | 2960 / 848 | 3733 / 1668 | |
| Replies lost / requests lost by the proxy | 92 / 84 | 66 / 55 | 102 / 98 | 97 / 107 | 109 / 119 | |
| `kill -9` / freezes (0.5 to 4.5 s) / collector runs | 10 / 9 / 11 | 8 / 8 / 9 | 12 / 8 / 10 | 19 / 11 / 9 | 22 / 11 / 13 | two kills in three land inside a burst of 6 to 15 commits |
| 5xx seen by the client / errors in the Worker's log | 0 / 0 | 0 / 0 | 0 / 0 | 0 / 0 | 0 / 0 | |
| Commit, until answered: p50 / p99 / max | 5 / 3151 / 4519 ms | 5 / 2919 / 3452 ms | 5 / 3052 / 3260 ms | 5 / 3154 / 4473 ms | 5 / 3638 / 4404 ms | the tail is retries through freezes and restarts; the client's timeout is 3 s |
| Which shards are missing: p50 / p99 | 3 / 31 ms | 3 / 71 ms | 3 / 81 ms | 3 / 76 ms | 3 / 84 ms | |
| Put a shard: p50 / p99 | 5 / 38 ms | 4 / 43 ms | 4 / 41 ms | 5 / 62 ms | 5 / 38 ms | up to 1.4 MB |
| Get a shard: p50 / p99 | 2 / 6 ms | 2 / 5 ms | 2 / 6 ms | 2 / 5 ms | 2 / 6 ms | |
| One page of the feed: p50 / p99 | 2 / 30 ms | 2 / 31 ms | 2 / 31 ms | 2 / 30 ms | 2 / 31 ms | pages of 5 to 9 nodes |
| At the end: live nodes / tombstones / versions | 174 / 89 / 157 | 131 / 115 / 112 | 237 / 47 / 194 | 191 / 133 / 108 | 171 / 162 / 124 | |
| At the end: the drive's database | 442 KB | 385 KB | 463 KB | 471 KB | 467 KB | |

In all: 2,500 steps, 2,464 commits applied once each, 199 answered from memory, 71 `kill -9`, 47 freezes, 929 lost messages, 14,784 requests. The Worker's log counted 2 fewer applied commits than the drive holds in seed 14: log lines written just before a `kill -9` are lost, which is why exact checks read the drive's tables and not the log.

After that table was taken the bursts that a `kill -9` lands on were widened from creates only to creates, new versions of up to four files, and deletes of up to two files and one folder with everything under it. Seeds 21 to 24 on that code, all passing:

| | Seed 21 | Seed 22 | Seed 23 | Seed 24 |
|---|---|---|---|---|
| Wall time | 83 s | 70 s | 100 s | 64 s |
| Commits applied | 541 | 498 | 659 | 510 |
| Commits the client saw answered from memory | 43 | 57 | 69 | 45 |
| Requests sent / retried | 3417 / 1265 | 3188 / 974 | 4085 / 1576 | 3043 / 1025 |
| Replies lost / requests lost by the proxy | 115 / 128 | 120 / 101 | 125 / 125 | 99 / 105 |
| `kill -9` / freezes / collector runs | 14 / 9 / 10 | 13 / 7 / 14 | 19 / 12 / 12 | 11 / 7 / 4 |
| 5xx seen by the client / errors in the Worker's log | 0 / 0 | 0 / 0 | 0 / 0 | 0 / 0 |
| Commit, until answered: p50 / p99 / max | 5 / 3301 / 3845 ms | 5 / 3157 / 3683 ms | 6 / 3192 / 4076 ms | 5 / 2987 / 4292 ms |

In the burst scenario itself (six kills, 21 to 23 commits in each burst) four kills came with 2 to 8 commits answered and two came after every commit had been answered: a burst finishes in about ten milliseconds on loopback.

Scenarios:

| Scenario | Result | Notes |
|---|---|---|
| 1,000 files created in one commit | 161 to 178 ms | three commits; the most ops one commit may carry |
| Reading 3,002 nodes through the feed | 41 ms | pages of 1,000 |
| Deleting a folder of 3,000 files in one commit | 183 ms | 3,001 tombstones, one sequence number each |
| The drive's database with 3,002 live nodes | 1.6 MB | |
| A panic inside a commit: other requests to the drive | answered within 24 to 27 ms | 30 s before the fix; the request that panicked itself gets no answer |
| A collector cut short with 190 of 240 objects left: a commit wanting one back | refused 3 times, then through | refused for ever before the fix |
| Worker start (`wrangler dev`, build already done) | 2.83 to 3.11 s | 76 starts |
| Worker build | 783 KB, 267 KB gzipped | |

The sealed engine (`ludy_core::sealed_drive`) against the same Worker, added the same day. Loopback, no simulated latency.

`cargo test -p ludy-core --test sealed_engine -- --ignored --test-threads=1 --nocapture`

| Scenario | Result | Notes |
|---|---|---|
| Read 64 KiB in the middle of a 24 MiB file, cold | 170,845 and 263,008 bytes down in two runs, 1 range request | the manifest and one block of up to four sealed frames (the shard's last block is shorter); a second read downloads nothing |
| Read 16 MiB front to back in 256 KiB reads | 20 to 23 range requests for 64 blocks | a request cannot span two shards, so fewer blocks per request than the old engine's 8 |
| The same 24 MiB under another name | 662 and 1,007 bytes up | a sealed manifest; every shard was already there |
| Change 4 KiB in the middle of a 24 MiB file, from a Mac with nothing cached | 6.2 MB and 2.4 MB, up and down | the one shard the write touched, fetched to be cut again and sent sealed |
| 200 small files, a 12 MiB copy and 40 more written while the Worker was down, with the app killed too | all 241 files in the cloud, none wrong | the Worker was killed with 2.1 to 3.7 MB of the copy sent; 42 replies and 39 requests of about 1,145 lost on the way |

These differ from run to run because cut points are keyed per drive and each test makes a new drive: where shards end, and so how large the touched one is, is different every time.

Soak (`SEALED_SEED=1..5`, 300 steps for seed 1 and 400 for the rest), all passing. Five replies and three requests in a hundred are lost throughout.

| | Seed 1 | Seed 2 | Seed 3 | Seed 4 | Seed 5 |
|---|---|---|---|---|---|
| Wall time | 130 s | 160 s | 147 s | 192 s | 199 s |
| Files and folders at the end | 64 | 68 | 74 | 92 | 102 |
| `kill -9` of the Worker / kills of the app / freezes | 8 / 9 / 7 | 11 / 10 / 11 | 11 / 9 / 11 | 10 / 14 / 9 | 14 / 13 / 12 |
| Proxy: connections / replies lost / requests lost | 593 / 14 / 14 | 713 / 30 / 23 | 734 / 23 / 23 | 819 / 27 / 23 | 851 / 33 / 27 |
| Reads compared with what was written / reads that failed while the cloud was away | 28 / 1 | 30 / 0 | 33 / 3 | 32 / 2 | 25 / 2 |

In those five nearly every read was answered from the journal. For seeds 6 to 8 the Mac's cache was cut to 1 MiB and wiped at every kill of the app, with a step that saves everything and looks at the cloud, so that reads go through the lossy proxy:

| | Seed 6 | Seed 7 | Seed 8 |
|---|---|---|---|
| Wall time | 163 s | 169 s | 172 s |
| Files and folders at the end | 107 | 99 | 105 |
| `kill -9` of the Worker / kills of the app / freezes | 11 / 11 / 7 | 16 / 9 / 15 | 11 / 12 / 10 |
| Proxy: connections / replies lost / requests lost | 1005 / 37 / 32 | 1272 / 53 / 42 | 924 / 44 / 34 |
| Reads compared with what was written / reads that failed while the cloud was away | 32 / 3 | 37 / 4 | 33 / 2 |
| Read back from the cloud: bytes / range requests | 5,774,054 / 68 | 3,322,527 / 73 | 2,152,365 / 37 |

Most of the wall time is restarts: about 3 s for the Worker and about as long again for the app to open the drive through a proxy that loses its requests.
