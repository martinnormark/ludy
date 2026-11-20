Ludy engine benchmark: after the cold-path work (release build), simulated link 400 Mbit/s, 30 ms

| Scenario | Result | Notes |
|---|---|---|
| Link: one 64 KiB range request | 34 ms | straight to the server; the floor for a cold read |
| Link: one large range request | 34 MB/s | the link allows 50 MB/s |
| Sequential read, cold | 48 MB/s | 256 MiB; 256 KiB reads, as the kernel issues them |
| Sequential read, warm | 3274 MB/s | 256 MiB; the same bytes again, from the disk cache |
| Random 64 KiB read, cold | 42 ms / 52 ms | 200 reads; p50 / p99; one range request each |
| Random 64 KiB read, warm | 0.09 ms / 0.25 ms | 200 reads; p50 / p99; from the disk cache |
| First 2 MiB of a file, cold | 163 ms | 256 KiB reads from the start of the file |
| Open a drive with many files | 39 ms | 5000 files: fetch the listing and build the tree |
| List that folder again | 0.34 ms | from the tree in memory |
| List that folder in pages | 0.34 ms | 5000 files, 100 at a time, as the NFS client asks for them |
| Create many small files | 387 ms | 1000 files of 4 KiB, until the writes returned |
| Upload those files | 6.03 s (166 files/s) | until all 1000 are in the cloud |
| Copy a file in: writes return | 966 MB/s | 64 MiB of incompressible data into the journal |
| Copy a file in: upload | 1.84 s (36 MB/s) | cut, hash, send shards, commit; the link allows 50 MB/s |
| Copy the same content in again: upload | 442 ms | every shard is already in the cloud |
| Change 4 KiB inside that file: upload | 122 ms (15.1x faster than re-uploading) | Space states about 11x for 4 KiB in 64 MiB |
| Read 64 KiB out of that file, cold | 34 ms (56x faster than fetching it whole) | Space states about 34x for 64 KiB of 64 MiB |
| Change 4 KiB in a file, at 100 ms latency | 236 ms (9.4x faster than the 2.23 s re-upload) | 64 MiB file; round trips dominate here |
| Open a large drive | 101 ms | 50000 files in 500 folders |
| Pick up one remote change in it | 34 ms | after the long-poll woke: fetch what changed and apply it |
| Upload small files into it | 3.01 s (166 files/s) | 500 files of 4 KiB |
| Delete files from it | 2.82 s (355 files/s) | 1000 deletes, replayed in order |
