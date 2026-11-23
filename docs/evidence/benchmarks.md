# Benchmarks

The first numbers for the engine on a real server, from 2026-11-23: three runs of `ludy bench` against each provider, on one Mac over loopback, and one quick run with the store in a real R2 bucket. The loopback runs say what the engine and the server cost with no network between them, and nothing about a real link. In short: a cold 64 KiB read out of a large file is one request and usually 5 to 7 ms; an upload of new content runs at 34 to 75 MB/s, limited by cutting and sealing; a 4 KiB change in a 64 MiB file uploads in 49 to 187 ms; and small files are the slow case, at 100 to 200 a second.

## How it is measured

`ludy bench` runs fixed scenarios through the engine (the same calls the NFS adapter makes) against the server in `LUDY_SERVER`. No kernel mount is involved. It makes a drive of its own, uses three sets of cache and journal as three Macs, and empties the drive at the end. Release build, Apple silicon.

```bash
cargo run --release -p ludy-cli -- bench
```

The two servers were:

- **Compose:** `docker compose up` with the published image and blobs in a Docker volume. Every request crosses Docker Desktop's network into its Linux VM.
- **`wrangler dev`:** the Worker under workerd, as a process on the Mac, with R2 and Durable Objects emulated on local disk.

So the columns compare two local setups, not Rivet with Cloudflare. No latency or bandwidth limit was added to either.

## Results

Lowest and highest of the three runs. All nine are in [`bench/2026-11-23-engine-on-providers.md`](../../bench/2026-11-23-engine-on-providers.md).

| Scenario | Compose | `wrangler dev` | Notes |
|---|---|---|---|
| Copy a 64 MiB file in: writes return | 732 to 1,055 MB/s | 838 to 1,060 MB/s | Into the journal on the Mac; no server involved |
| Copy a 64 MiB file in: upload | 0.89 to 0.99 s (68 to 75 MB/s) | 1.42 to 1.95 s (34 to 47 MB/s) | Cut, seal, send the sealed shards, commit a sealed manifest |
| Copy the same content in again: upload | 680 to 735 ms | 689 to 728 ms | Nothing is sent, but every shard is cut and sealed again to learn its name |
| Change 4 KiB inside that file: upload | 68 to 140 ms | 49 to 187 ms | Only the shard the write touched is cut, sealed and sent |
| Read 64 KiB out of that file, cold | 5.3, 7.2 and 48 ms | 5.2 to 6.8 ms | One range request, after the manifest |
| First 2 MiB of a file, cold | 144 to 173 ms | 19 to 28 ms | The manifest, then 256 KiB reads from the start |
| Sequential read of 256 MiB, cold | 124 to 145 MB/s | 333 to 351 MB/s | 256 KiB reads, as the kernel issues them |
| The same again, warm | 3,077 to 3,181 MB/s | 2,989 to 3,189 MB/s | From the disk cache |
| Random 64 KiB read, cold, p50 / p99 | 7.3 to 9.2 ms / 57 to 58 ms | 3.6 to 3.8 ms / 12 to 13 ms | 200 reads, a block each |
| Random 64 KiB read, warm, p50 / p99 | 0.07 to 0.09 ms / 0.15 to 0.18 ms | 0.04 to 0.05 ms / 0.09 to 0.10 ms | From the disk cache |
| Create 5,000 files of 4 KiB | 2.1 to 2.2 s | 2.0 to 2.5 s | Until the writes returned |
| Upload those files | 25 to 28 s (177 to 196 files/s) | 48 to 52 s (97 to 104 files/s) | Five requests a file, eight files at a time |
| Open a drive of 5,004 nodes | 112 to 134 ms | 104 to 110 ms | Read the feed, unwrap every key, open every name |
| List a folder of 5,000 | 0.4 to 0.6 ms | 0.4 to 0.5 ms | From the tree in memory |
| List it in pages of 100 | 0.27 to 0.28 ms | 0.25 to 0.30 ms | As the NFS client asks |
| Pick up one remote change | 1.7 to 1.8 ms | 2.5 to 2.7 ms | One request to the feed |
| Delete 1,000 files | 0.64 to 0.95 s (1,055 to 1,567 files/s) | 0.38 to 0.48 s (2,067 to 2,661 files/s) | 200 deletes to a commit |

## What the numbers say

- **Reads fetch what is asked for.** A 64 KiB read in the middle of a 64 MiB file is one range request for one block, and the rest of the file is not fetched.
- **Sealing is the ceiling for uploads.** Sealing alone measured about 116 MB/s on this Mac (2026-11-22). With cutting, hashing and the requests, new content goes up at 34 to 75 MB/s on loopback. A link slower than that would be the limit instead.
- **Identical content is not free.** A shard's name is the hash of its sealed bytes, so content the server already has is still cut and sealed before the engine can know: 0.7 s for 64 MiB.
- **Small files are the slow case.** Each is five requests. Batching them is [not built](../known-issues.md#not-built).
- **Compose reads were slower and less even than `wrangler dev`, and its uploads faster.** Why was not looked into; the candidates are in the bench file.
- **Rows that are one request vary more between runs than between servers.** Do not read a difference of a few milliseconds off this table.

## With a store over a real network

One quick run with the Compose server's blobs in a real R2 bucket, so that the server's calls to its store cross the internet while the Mac still reaches the server over loopback: a cold 64 KiB read took 113 ms at the median, a cold sequential read ran at 20 MB/s, an 8 MiB file went up at 8 MB/s, and 500 small files uploaded at 7 a second. The table and what it was run on are in [object storage](../hosting/object-storage.md#what-was-seen-with-r2).

## Not measured

- A real network between the Mac and the server. Round trips are free there in every number so far; on a real link the five requests of a small file and the manifest before a cold read will count for more.
- A deployed Worker, or Compose on a Linux machine without Docker Desktop in the way.
- Large files. The largest here is 256 MiB.
- The same reads through a kernel mount. They were timed for the previous engine only.

The engine Ludy started on, and the simulated link it was measured against, have their numbers in [history](../history/benchmarks.md). They are not comparable with these: that server did no encryption and the link was shaped to 400 Mbit/s and 30 ms.
