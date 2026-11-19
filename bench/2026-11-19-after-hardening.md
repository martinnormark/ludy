Ludy engine benchmark: after hardening (96d0b2d) (release build), simulated link 400 Mbit/s, 30 ms

| Scenario | Result | Notes |
|---|---|---|
| Link: one 64 KiB range request | 34 ms | straight to the server; the floor for a cold read |
| Link: one large range request | 35 MB/s | the link allows 50 MB/s |
| Sequential read, cold | 41 MB/s | 256 MiB; 256 KiB reads, as the kernel issues them |
| Sequential read, warm | 3554 MB/s | 256 MiB; the same bytes again, from the disk cache |
| Random 64 KiB read, cold | 42 ms / 52 ms | 200 reads; p50 / p99; one range request each |
| Random 64 KiB read, warm | 0.11 ms / 0.25 ms | 200 reads; p50 / p99; from the disk cache |
| Open a drive with many files | 41 ms | 5000 files: fetch the listing and build the tree |
| List that folder again | 0.43 ms | from the tree in memory |
| Create many small files | 377 ms | 1000 files of 4 KiB, until the writes returned |
| Upload those files | 6.28 s (159 files/s) | until all 1000 are in the cloud |
| Copy a file in: writes return | 993 MB/s | 64 MiB of incompressible data into the journal |
| Copy a file in: upload | 1.77 s (38 MB/s) | cut, hash, send shards, commit; the link allows 50 MB/s |
| Copy the same content in again: upload | 367 ms | every shard is already in the cloud |
| Change 4 KiB inside that file: upload | 235 ms (7.5x faster than re-uploading) | Space states about 11x for 4 KiB in 64 MiB |
| Read 64 KiB out of that file, cold | 34 ms (56x faster than fetching it whole) | Space states about 34x for 64 KiB of 64 MiB |
