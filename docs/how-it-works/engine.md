# The engine

An application's read reaches the engine through the macOS NFS client. The engine keeps three things on the Mac: a cache of 256 KiB blocks, a journal of written bytes, and the tree of files from the cloud's listing. Bytes are fetched only when something reads them, writes return at once and upload in the background, and remote changes arrive by long-poll.

This is the engine the app mounts today, against the simulated cloud. The engine for the encrypted cloud is described in [the real cloud](../real-cloud/overview.md).

```
 Finder / any app
        │  read(offset, len)
        ▼
 macOS NFS client ──NFSv3 over loopback──▶ ludy-core (in the app's process)
                                              │
                         ┌────────────────────┼─────────────────────┐
                         ▼                    ▼                     ▼
                   block cache          write journal        tree of nodes
                (256 KiB blocks,      (sparse local file     (from the cloud
                 LRU, byte cap)        per written file)        listing)
                         │                    │                     ▲
                         └───── HTTP: GET Range / PUT / long-poll ──┘
                                              │
                                              ▼
                                         ludy-cloud
```

## Reads

A read maps to the 256 KiB blocks that cover it. Blocks already in the cache are read from disk. Missing neighbours are fetched with one range request of at most 2 MiB, several requests run in parallel, and concurrent readers of the same block share one request. A block is written to the cache and handed to its readers as soon as it has arrived; nobody waits for the rest of the request, and if the request dies half way the blocks that came are kept. A sequential reader gets read-ahead that grows to 8 MiB and is topped up one 2 MiB request at a time, so the requests overlap and the link does not sit idle for a round trip between batches; a random read fetches only what it asked for.

## Cache

Blocks are keyed by `(version id, block index)`. A version never changes, so a cached block is never stale. The cache has a byte cap and evicts the least recently used blocks. The Finder lists every file at its full cloud size, so a drive can show far more than the Mac's disk holds; that is the point. `stat` and `du` on the mount report the full size as well. What the Mac really holds is the cache, shown in the app.

## Writes

A write goes to a sparse journal file and returns. An existing cloud file is not downloaded first: the journal records which byte ranges were written and reads merge them with the cloud version underneath. When a file has been quiet for 1.5 seconds it is uploaded as a new version, as a patch: only the shards the edit touched are sent (see [Shards](simulated-cloud.md#shards)). Eight files upload at a time, and a file small enough to be one shard goes up in a single request. What has not been sent yet survives a quit, a crash or a power cut: next to each journal file the engine saves which byte ranges it holds and which cloud version they were written over, and it saves the queue of renames and deletes, so the next start picks everything up and sends it. Renames, deletes and new folders are queued and replayed on the cloud in order. `.DS_Store` and `._*` files stay on the Mac.

## Remote changes

Each mounted drive long-polls the cloud for its next commit. The first time it fetches the whole listing; after that it asks only for what changed since the sequence number it last saw (`GET /<drive>?feed&since=N&epoch=E`) and applies that: deleted keys, then folders left with neither content nor a marker object, then new and changed objects. So noticing a change costs one round trip whatever the size of the drive. The cloud answers "reset" when it cannot say what changed (it restarted, the drive was deleted and created again, the number is too old), and the drive falls back to the full listing. Node ids of paths that still exist are kept. A listing is never applied while local changes are still uploading.
