# Chaos testing

> **History.** This page is about chaos tests of the engine Ludy started on. That engine and its simulated server were removed on 2026-11-23, when the app moved to the end-to-end encrypted engine and a real server; the last commit that has them is `9f45636`. Nothing here was measured on what Ludy is now, and the page is kept as it was written, so its present tense is the past. Links to code that no longer exists were turned into plain text. For the current system see [what works](../evidence/verification.md), [chaos testing](../evidence/chaos-testing.md) and [benchmarks](../evidence/benchmarks.md).

The engine is run against a cloud that misbehaves on purpose: killed mid-upload, stalled, refusing requests, with the app itself killed while writes are unsent. Every test ends by checking that the cloud holds exactly what the application wrote. This found fifteen problems, all fixed, and leaves seven known ones, listed at the end. The encrypted cloud has chaos suites of its own, described in the real cloud.

## How the tests work

`tests/chaos.rs` runs the engine against a cloud that misbehaves on purpose. The cloud used in tests can be killed (every request in flight is dropped) and restarted on the same data and port, which is what a deploy looks like to a client; it can also stall (accept and never answer), refuse everything with 503, hold commits back, and delete shards no manifest lists yet. Each test ends with the same check: once the cloud is back and the drive has settled, the cloud holds exactly the files the application wrote, byte for byte, nothing else, and deleting everything leaves no shard or manifest behind. Faults fire on observed events (the fortieth upload, a commit waiting), not on sleeps, and every step has a timeout, so a hang fails the test.

## Scenarios

The scenarios:

- A flood of 360 files and a 12 MiB copy with the cloud killed and restarted in the middle and more files written during the outage
- The same flood with the cloud refusing requests while a folder is renamed and a file deleted
- Shards swept between upload and commit (the commit is refused with the list, the client sends them again)
- A restart between the shards and the commit
- A stalled cloud
- The cloud killed half way through a range request (the reader of all of it gets an error, the blocks that had arrived read back without the cloud, and only the rest is fetched again)
- The background uploader and a flush running together through renames and deletes
- Paths longer than the cloud accepts
- A file deleted in the cloud under a pending local patch
- A listing that is already out of date
- A damaged cache block
- Writes, renames and deletes on files that are mid-upload
- Awkward names (percent signs, quotes, emoji, 255 bytes)
- Two writers on one file
- The app killed, or quit, with unsent writes while the cloud is down

## Randomized soak

A randomized soak (`soak`, ignored by default) drives 1,200 random operations per run (create, patch, resize, rename files and folders, delete, read back and compare) while it randomly kills and restarts the cloud, stalls it, makes it refuse requests, and kills and restarts the app. The seed is printed. After the fixes below, 40 consecutive seeds pass. Since the scale round the soak also has a second writer changing the cloud's side of the drive, and a separate test (`applying_only_what_changed_matches_a_full_listing`) makes 400 random remote changes and checks after each that the drive, asking only for changes, shows exactly what a full listing would; 80 more soak seeds and 30 seeds of that test pass, the last 55 with every chaos test also comparing the drive's whole tree with the cloud's listing at the end. Since the cold-path round that test also keeps a second drive that is only ever handed full listings, as after a cloud restart, and checks it against the same listing; 13 seeds of it and 6 more soak seeds pass.

## On a real mount

On a real mount, [`scripts/chaos-mount.sh`](../../scripts/chaos-mount.sh) was run once with the release binaries on a hidden mount: 2,000 small files and a 200 MiB copy written through the mount, `kill -9` of the cloud four seconds in, restart five seconds later: all 2,001 files in the cloud, none wrong. Then the cloud was killed, 500 more files written, and the mounting process killed with `kill -9` as well; after both were started again the 500 files arrived, none wrong. The kernel took about two minutes to drop the dead mount this time (nothing was touching it; with a reader on it, it took 55 seconds before).

## What this found, all fixed

| Found | Now |
|---|---|
| Writes that had not reached the cloud were deleted when the app quit or was killed while the cloud was down | The journal's state and the queue of renames and deletes are saved on disk and picked up at the next start |
| A cloud file renamed locally could not be read, and a patch to it could not upload, until the rename had reached the cloud | Each remote version carries the key the cloud currently has it under |
| The background uploader and a flush (eject, quit) could run at once and replay a rename under an upload in flight | One sync pass at a time |
| A stalled cloud hung reads and uploads for ever | A read gives up after 8 seconds without a byte (below the NFS client's own 10 second timeout, so the mount stays alive); every request has a time limit |
| A path longer than the cloud's 1,024-byte key limit queued a change the cloud would never accept, blocking everything behind it | Refused when it is created (`ENAMETOOLONG`); a change the cloud rejects is dropped with an error |
| A patch whose base version was deleted in the cloud failed for ever and held up other uploads | If the drive still has the rest of the file (it uploaded it, and its cache holds the blocks), the edit is saved and the file exists again: the last writer wins and nothing is lost. If it does not, the cloud's delete wins and the drive reports it. Either way the rest continues |
| The same, for a file the drive had uploaded itself, after manifests began to be remembered: the plan came from memory, the cloud's "gone" was never seen, and the upload failed for ever (found in review of the scale round) | A failed upload forgets the remembered manifest; the retry asks the cloud |
| A folder that existed only through its content (no marker object) stayed in the drive after its last file was deleted or moved out locally, though the cloud no longer had it | Emptying a folder locally gives it a marker object, so it stays in both places until it is removed itself |
| A listing fetched before an upload finished could be applied after it, making the new file vanish or revert | A listing is only applied if the drive has changed nothing in the cloud since it was requested |
| A cache block cut short on disk was served as data | Its length is checked; it is discarded and fetched again |
| A full listing applied over a tree where a name had been a file and was now a folder, or the other way round, dropped the old node and did not put the new one in, and only another full listing would have (found by handing a second drive only full listings in the change-feed test; it failed the same way on the code before) | The objects whose place was taken are put in once the old node has gone |
| A new file renamed over a file the cloud has, then deleted before its upload, left the cloud's file in place (found by the soak) | The replaced file is deleted in the cloud explicitly |
| Uploads ran one file at a time with three round trips each | Eight at a time; one request for a file that fits one shard |
| Simulated cloud: a request cancelled by a disconnecting client could leave content referenced for ever (found by the soak) | Changes to the store run to their end whatever happens to the request |
| Simulated cloud: its rate limiter held a fast link to about half its rate | Short waits are batched |

## Known and not fixed

- A byte flipped inside a cached block is not detected; blocks have no checksum.
- A full disk while writing the journal turns into an I/O error for the application, and nothing has tested what happens next.
- After a crash, the first listing must succeed before recovered writes show up, so a drive cannot be mounted offline to get at them.
- The dead-mount wait after a crash is the kernel's and can be two minutes.
- Opening a drive is still one full listing held in memory (about 95 ms and one 6.6 MB JSON document for 50,000 files), fetched again at every mount: the tree is not kept on disk, so a drive that was open yesterday cannot start from what it knew and ask only for the changes; a drive of millions of files would need a listing in pages and a tree that is not all in memory. Uploads run eight at a time with one commit each; there is no batched commit.
- The cloud remembers the last 200,000 changed keys per drive for the change feed, in memory; a client further behind than that, or any client after a server restart, does one full listing.
- The chaos script on a real mount was run once; the Finder was not involved.
