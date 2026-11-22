# Benchmarks

Three rounds of before-and-after numbers from `ludy bench`, then the same reads through a real mount. In short: reads are at the simulated link's limit, uploads of real data run at the link's rate, and the two things that were slow or are now slower are both about many small files: uploading them was slow and is fixed, and creating them got slower (70 ms to 377 ms for 1,000 files) because the journal's state is now saved on every write. The last section says what the numbers mean and what they leave out.

`ludy bench` runs fixed scenarios through the engine (the same calls the NFS adapter makes) against a cloud started in the same process, with the simulated link at 400 Mbit/s and 30 ms. No kernel mount is involved, so these are the engine's numbers. Release build, Apple silicon. The full tables are in [`bench/`](../../bench).

## First round: hardening

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

## Second round: scale

The second round, on scale (full tables in [`bench/`](../../bench), `scale-baseline` and `scale-after`):

| Scenario | Before | After | What changed |
|---|---|---|---|
| Change 4 KiB in a 64 MiB file | 231 ms, 7.7x faster than re-uploading | **121 ms, 14.8x** | one request instead of four (Space states about 11x) |
| The same at 100 ms latency | 579 ms, 3.9x | **242 ms, 9.1x** | |
| Open a drive of 50,000 files | 149 ms | 139 ms | still one full listing |
| Pick up one remote change in it | 105 ms | **32 ms** | ask for what changed instead of listing 50,000 files |
| Upload 500 small files into it | 8.0 s (62 files/s) | **3.1 s (161 files/s)** | the cloud appends a line per commit instead of rewriting the catalog |
| Delete 1,000 files from it | 49.7 s (20 files/s) | **2.4 s (422 files/s)** | sixteen deletes at a time; the queue on disk is appended to, not rewritten |

Everything in the first table is unchanged by this round except the patch row.

## Third round: cold reads and cold listings

The third round, on cold reads and cold listings (`cold-baseline` and `cold-after` in [`bench/`](../../bench); both binaries were run three times, alternating, and the range is given where the runs differ):

| Scenario | Before | After | What changed |
|---|---|---|---|
| Sequential read, cold | 40 to 41 MB/s | **47 to 48 MB/s** | read-ahead is topped up one request at a time, where it went out in batches that finished together and left the link idle for a round trip; with no latency at all the same read runs at 49 MB/s |
| The same at 100 ms latency | 30 MB/s | **39 to 41 MB/s** | two runs each; on a link with no bandwidth cap, 176 to 178 MB/s became 188 to 190 |
| First 2 MiB of a file, cold (new row) | 220 to 232 ms | **146 to 178 ms** | both changes: a block is handed over when it arrives, not when its 2 MiB request ends, and read-ahead no longer sends single blocks |
| Random 64 KiB read, cold (p50 / p99) | 42 / 53 ms | 42 / 52 ms | unchanged: one round trip (34 ms), about 7 ms for the 256 KiB block around the read to arrive, 1 to 2 ms in the engine |
| Open a drive of 5,000 files | 41 ms | 39 ms | |
| Open a drive of 50,000 files | 136 to 144 ms | **78 to 106 ms** | folding the listing into the tree took 60 ms and takes 19; the after figure is the range of nine runs, most of them between 90 and 105 |
| List a folder of 5,000 files in pages of 100 (new row) | 19 ms | **0.33 ms** | a page costs the page, not the whole folder again. The "before" is the old adapter's steps (list the whole folder, find the place, take a page) written into the benchmark, since the commit before has no call for a page |

The other rows are within noise of the round before.

## Through a real mount

Through a real, hidden mount (the release binaries before and after, a drive of 5,000 synthetic files, link at 400 Mbit/s and 30 ms, three rounds each): reading 256 MiB with `dd` went from 39.8 to 40.4 MB/s to 46.6 to 48.9 MB/s, and the first 2 MiB of a file from 114 to 170 ms to 94 to 115 ms. A cold 64 KiB read was 50 to 53 ms either way (and 73 ms once, in the round that shared the machine with a test run). `ls -la` of the 5,000 files took 344 to 749 ms before and 358 to 607 ms after: no difference that can be told from the noise, so at this size the time is the kernel's and the NFS library's, not the engine's 19 ms.

## What the numbers say

Reads are at the link's limit (a cold random read costs one round trip plus 8 ms, most of it the block around the read arriving; a sequential read runs within a few percent of what the link gives without latency); uploads of real data run at the link's rate; the two things that were slow or are now slower are both about many small files. A patch still re-sends the whole shard it touched (about 2 MiB for a 4 KiB edit), which is most of its 121 ms. Of the roughly 95 ms it takes to open 50,000 files, about 30 is the round trip, 16 the cloud writing the listing, 15 receiving and parsing it and 19 building the tree; the simulated link does not charge the listing's 6.6 MB against its bandwidth, which a real 400 Mbit/s link would (about 130 ms more). And through a real mount the kernel's NFS client adds its own costs, which this benchmark does not see.
