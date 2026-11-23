# The engine

An application's read reaches the engine through the macOS NFS client. The engine keeps three things on the Mac: a cache of 256 KiB blocks, a journal of written bytes, and the drive's tree, which it builds from the server's feed and decrypts with the drive's key. Bytes are fetched only when something reads them, writes return at once and upload in the background, and changes made elsewhere arrive by asking the feed once a second.

Everything that needs a key happens here. The server stores sealed bytes and a tree it cannot read; what it does with them is in [the server](server.md), and how they are sealed is in [encryption](encryption.md).

```
 Finder / any app
        │  read(offset, len)
        ▼
 macOS NFS client ──NFSv3 over loopback──▶ ludy-core (in the app's process)
                                              │
                         ┌────────────────────┼─────────────────────┐
                         ▼                    ▼                     ▼
                   block cache          write journal        tree of nodes
                (256 KiB blocks,      (sparse local file    (from the feed,
                 LRU, byte cap)        per written file)     opened with the
                         │                    │               drive's key)
                         │                    │                     ▲
                         └── HTTP: sealed shards and manifests, ────┘
                                   commits, the feed
                                              │
                                              ▼
                                  the server (a provider)
```

The code is [`sealed_drive.rs`](../../crates/ludy-core/src/sealed_drive.rs) (the drive as a filesystem) and [`sealed_client.rs`](../../crates/ludy-core/src/sealed_client.rs) (its HTTP side). [`engine.rs`](../../crates/ludy-core/src/engine.rs) is what the app and the CLI hold: the list of drives, which are open, which are mounted.

## Drives and keys

The server lists no drives and holds no key, so the list of drives is on the Mac: [`keystore.rs`](../../crates/ludy-core/src/keystore.rs) keeps a file, `drives.json`, with each drive's name, the server it lives on, its id and the key of its root folder. Only the drives of the server the engine is talking to are listed.

- The file is readable by its owner only (mode 0600) and is not encrypted. Whoever can read it can read and change every drive in it. It is a stand-in until there is sign-in.
- A drive whose entry is lost cannot be opened by anyone. `ludy key <drive>` prints the entry as one line, `ludy1-<drive id>-<root key>`, and `ludy add <key>` on another Mac turns it back into an entry after checking with the server that the key is that drive's.
- A drive's name is this Mac's. Renaming a drive changes the entry; the name sealed on the server when the drive was created stays as it was.
- Removing a drive forgets its entry. Nothing asks the server to delete a drive, because the protocol has no such request. The entry is kept while the drive still has writes to send, or while the server cannot be reached to find out.

A drive is opened once per Mac at a time. The engine holds a lock file beside the drive's journal for as long as the drive is open, so the app and `ludy mount` cannot both work on one journal; the second one is told which is which and stops.

## The tree

Opening a drive reads its whole feed, in pages of 1,000 nodes. Each node carries its key wrapped by its folder's key and its name sealed under its own; the engine unwraps from the root key down and ends with an ordinary tree of names, sizes and modification times. The tree is kept in memory only, so every open reads the feed from the start.

## Reads

A file's bytes are found through its manifest: a sealed list of the shards that make up the version, each with its key. The engine fetches the manifest the first time a version is read and remembers the last 256 it opened.

A read maps to the 256 KiB blocks of the shards that cover it. Blocks already in the cache are read from disk. Missing neighbours in one shard are fetched with one range request of at most 2 MiB, up to eight requests run at once, and concurrent readers of the same block share one request. A request never spans two shards. A block is opened (decrypted and checked, four 64 KiB frames at a time), written to the cache and handed to its readers as soon as it has arrived; nobody waits for the rest of the request. A sequential reader gets read-ahead of up to 8 MiB; a random read fetches only the block it asked for.

A file of generated content (see [synthetic files](encryption.md#synthetic-files)) is produced here from its seed and fetches nothing.

## Cache

Blocks are keyed by sealed shard and block index and hold opened bytes, so the cache is plaintext on the Mac's disk. A shard never changes, so a cached block is never stale, and two files that share a shard share its blocks. The cache has a byte cap (`LUDY_CACHE_MB`) and evicts the least recently used blocks. The Finder lists every file at its full size, so a drive can show far more than the Mac's disk holds; `stat` and `du` on the mount report the full size as well. What the Mac really holds is the cache, shown in the app.

## Writes

A write goes to a sparse journal file and returns. An existing file is not downloaded first: the journal records which byte ranges were written, and reads merge them with the version underneath. When a file has been quiet for 1.5 seconds it is uploaded as a new version:

1. Plan the new content against the manifest of the version underneath. Shards no write touched are kept by reference. A shard that was written into is read again (journal over cloud) and cut afresh with its written neighbours.
2. Seal the new shards. A shard's name is the SHA-256 of its sealed bytes, so it has to be sealed before the engine knows whether the server already has it.
3. Ask the server which of them it lacks (`POST /drives/:drive/missing`, about 8 MiB of sealed shards at a time) and send those, four at a time.
4. Send the sealed manifest and commit the version.

Eight files upload at a time. Creating, renaming, moving and deleting are ops on nodes; they are queued and sent in order, up to 200 to a commit. Every commit carries an id, so sending it again after a lost reply changes nothing.

What has not been sent survives a quit, a crash or a power cut: next to each journal file the engine saves which byte ranges it holds and which version they were written over, and it saves the queue of ops, so the next open picks everything up and sends it. `.DS_Store` and `._*` files stay on the Mac: the mount lists them, because the Finder made them and expects them back, but they are never sent, and the app's browser, `ludy ls`, `ludy get` and a drive's file count leave them out.

## Changes made elsewhere

An open drive asks the feed once a second for nodes changed after the sequence number it last saw, and applies what comes back. Noticing a change costs one request whatever the size of the drive. The server answers "reset" when it cannot say what changed, and the drive reads everything again. Nothing from the feed is applied while local changes are still waiting to be sent.

When two Macs disagree:

- The same name created twice in one folder keeps both, one as `name (conflict ab12cd)`.
- Two writers of one file: the last to commit is the file, and the other's version stays in its history.
- A file rewritten here and deleted there is saved again; a partial change to a file that is gone is dropped.
- A file saved into a folder that was deleted elsewhere is kept at the top of the drive.

Whenever the engine does something nobody asked for (keeps a file under another name, drops a change), it says so: the app shows it in its notice bar and `ludy mount` prints it. The notices are held in memory, so one that is not read before a crash is lost.

## History

The server keeps the last 25 versions of a file. The engine lists them (`GET /drives/:drive/nodes/:id/versions`) and can make an older one the newest again by committing a version that names the old manifest and shards; no content moves.
