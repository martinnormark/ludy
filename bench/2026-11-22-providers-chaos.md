# Chaos suites on both providers, after the move to ludy-server

2026-11-22, on this Mac, loopback. One run of each suite on each provider; the soak tables are below as the suites printed them. Cloudflare is the Worker under `wrangler dev` 4.166.0. Rivet is `ludy-rivet` (release build) with blobs in a folder, beside a `rivetdev/engine:3.1.1` container whose `pegboard.node_lost_threshold` and `node_ping_timeout` are 2,000 ms; each `kill -9` takes the server and the engine container together.

| Suite | Cloudflare | Rivet |
|---|---|---|
| Worker chaos: 12 scenarios and the soak | 13 passed, 135 s | 13 passed, 285 s |
| Engine: 13 scenarios and the soak | 14 passed, 209 s | 14 passed, 223 s |

The seeds differ between the two columns (each run took the clock's), so the tables are not a like-for-like timing comparison. Two things they do show:

- **Starting again after a kill** takes about 2.9 s on Cloudflare every time. On Rivet it took 2.4 s five times and 8.8 s three times. Why some are longer was not looked into; 8.6 s is what a first trial measured for a killed server with this threshold, and 46.6 s with the engine's default.
- **Requests while the server is away** end differently. Under `wrangler dev` nearly all fail at once with a refused connection (1,473 of those, 2 timeouts). On Rivet 177 ran into the client's 3 s timeout, because the engine's gateway holds a request while no node has the actor.

## Runs after these

The tables below are from the first complete run on each provider. Three small changes followed (on Rivet a platform failure is logged as `fault`, and the front refuses `rvt-` parameters; on both, a replayed commit also sets the collector's timer), and everything was run again:

| Run | Result |
|---|---|
| Worker chaos, all 13, Rivet | pass, 272 s |
| Engine, all 14, Rivet | pass, 223 s |
| Worker chaos, all 13, Cloudflare | pass, 145 s |
| Engine, all 14, Cloudflare | pass, 188 s |
| Worker soak on Rivet, seeds 11, 22, 33, 500 steps each (before the three changes) | pass; 14, 15 and 15 kills |
| Worker soak on Rivet, only the server killed (seed 41) | pass, 3 kills |
| Worker soak on Rivet, only the engine killed (seed 43) | pass, 10 kills |
| Engine soak on Rivet, seed 6 (before the three changes) | pass |
| Engine soak on Rivet, seed 5 (before the three changes) | **failed its log check.** Every file held what was written; the server's log had one commit that came back from Rivet with its result unknown after a freeze. See [known problems](../docs/known-issues.md#the-rivet-provider) |
| Engine soak on Rivet, seed 5 again | pass, 11 kills. A seed fixes the order of steps, not the drive's keys or the timing, so this is not the same run |
| Engine soak on Rivet, only the server killed (seed 42) | pass, 10 kills |
| Engine soak on Rivet, only the engine killed (seed 44) | pass, 7 kills |

## Worker chaos soak, Rivet

| Worker soak, seed 467, 300 steps | |
| Wall time | 161 s |
| Commits applied | 341 |
| Commits answered from memory (client saw a replay) | 29 |
| Commits refused, as the model predicted | conflict 8, exists 26, no_node 17 |
| Requests sent / retried | 2086 / 823 |
| Timeouts / connection errors / 5xx seen by the client | 177 / 646 / 0 |
| Proxy: connections / replies lost / requests lost | 2086 / 70 / 86 |
| kill -9 / freezes / collector runs | 8 / 13 / 5 |
| Worker start, ms (first, then after each kill) | [2464, 2448, 8754, 8764, 8759, 2447, 2470, 2451, 2467] |
| At the end: live nodes / files / tombstones | 95 / 71 / 79 |
| At the end: versions / blobs in use / database bytes | 88 / 157 / 380928 |
| Worker log: commits ok / replay / refused / errors | 337 / 37 / 52 / 0 |
| commit: n, p50 / p99 / max ms until answered | 392, 9 / 9265 / 10838 |
| missing: n, p50 / p99 / max ms until answered | 148, 4 / 76 / 82 |
| put shard: n, p50 / p99 / max ms until answered | 113, 1 / 71 / 72 |
| put manifest: n, p50 / p99 / max ms until answered | 148, 1 / 28 / 29 |
| get shard: n, p50 / p99 / max ms until answered | 136, 0 / 3 / 3 |
| feed: n, p50 / p99 / max ms until answered | 183, 3 / 33 / 34 |
| check: n, p50 / p99 / max ms until answered | 18, 8 / 5089 / 5089 |

## Worker chaos soak, Cloudflare

| Worker soak, seed 66411, 300 steps | |
| Wall time | 52 s |
| Commits applied | 381 |
| Commits answered from memory (client saw a replay) | 30 |
| Commits refused, as the model predicted | conflict 8, exists 12, malformed 1, no_node 19 |
| Requests sent / retried | 2982 / 1475 |
| Timeouts / connection errors / 5xx seen by the client | 2 / 1473 / 0 |
| Proxy: connections / replies lost / requests lost | 2982 / 76 / 111 |
| kill -9 / freezes / collector runs | 9 / 4 / 8 |
| Worker start, ms (first, then after each kill) | [2852, 2878, 2864, 2864, 2841, 2858, 2839, 2890, 2855, 2872] |
| At the end: live nodes / files / tombstones | 130 / 99 / 82 |
| At the end: versions / blobs in use / database bytes | 90 / 160 / 380928 |
| Worker log: commits ok / replay / refused / errors | 381 / 31 / 43 / 0 |
| commit: n, p50 / p99 / max ms until answered | 421, 6 / 3176 / 4469 |
| missing: n, p50 / p99 / max ms until answered | 165, 3 / 71 / 73 |
| put shard: n, p50 / p99 / max ms until answered | 128, 5 / 74 / 77 |
| put manifest: n, p50 / p99 / max ms until answered | 165, 4 / 32 / 32 |
| get shard: n, p50 / p99 / max ms until answered | 180, 2 / 5 / 5 |
| feed: n, p50 / p99 / max ms until answered | 270, 2 / 31 / 78 |
| check: n, p50 / p99 / max ms until answered | 19, 6 / 36 / 36 |

## Engine soak, Rivet

| Sealed engine soak, seed 6153, 300 steps | |
| Wall time | 154 s |
| Files and folders at the end | 80 |
| kill -9 of the Worker / kills of the app / freezes | 7 / 6 / 7 |
| Proxy: connections / replies lost / requests lost | 722 / 39 / 27 |
| Reads checked against what was written / reads that failed while the cloud was away | 33 / 0 |
| Read back from the cloud by this Mac: bytes / range requests | 1036703 / 41 |

## Engine soak, Cloudflare

| Sealed engine soak, seed 67918, 300 steps | |
| Wall time | 134 s |
| Files and folders at the end | 71 |
| kill -9 of the Worker / kills of the app / freezes | 9 / 9 / 6 |
| Proxy: connections / replies lost / requests lost | 833 / 34 / 29 |
| Reads checked against what was written / reads that failed while the cloud was away | 15 / 2 |
| Read back from the cloud by this Mac: bytes / range requests | 1854047 / 45 |

## Compose: stopping and starting

Later on 2026-11-22, on this Mac, each step once. The stack from `docker-compose.yml` with the image already built, one drive created, and "answers" meaning `GET` of that drive's head returns 200. The first column of numbers is with the engine's own wait for a silent server; the second is with 2,000 ms, which the Compose file now sets. Both were run with a health check asked every second while starting (`start_interval`, which needs Docker Engine 25). The third is the files as committed: 2,000 ms, and a check every three seconds that keeps asking for four.

| Step | Engine default | 2,000 ms | As committed |
|---|---|---|---|
| `up -d --wait` from nothing (no volumes) | not timed | 7.3 s | 12.0 s, 11.5 s |
| `restart ludy`: the command, then until the drive answers | 0.3 s, 0.1 s | 0.4 s, 0.1 s | 0.4 s, 0.1 s |
| `down`, then `up -d --wait`, then until the drive answers | 1.6 s, 6.4 s, 0.1 s | 1.6 s, 9.8 s, 0.1 s | 1.6 s, 12.4 s, 0.1 s |
| `kill -s KILL ludy`, `start ludy`, until the drive answers | 45.0 s | 6.7 s | 6.7 s |
| `kill -s KILL` of both, `up -d --wait`, then until the drive answers | 51.9 s, 2.1 s | 19.3 s, 1.9 s | 20.9 s, 1.9 s |
| `pause ludy` for 4 s, until the drive answers after `unpause` | not run | 5.4 s | not run |
| `pause ludy` for 10 s | 0.0 s | 1.6 s | not run |
| `pause ludy` for 30 s | not run | 2.1 s | not run |

No `fault`, `upstream` or `error` line was in the server's log after any column. Nothing was being written during a pause.

With `docker-compose.minio.yml` on top, `up -d --wait` from nothing took 16.6 s (with a check every three seconds that did not keep asking), and a shard was stored, read whole and by range, and a body that did not match its name was refused with 400.
