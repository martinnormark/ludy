# The simulated cloud and shards

The cloud in this repository is a local S3-like server. A drive is a bucket of objects, and every write makes a version that never changes. Content is cut into shards by content-defined chunking and each shard is stored once under its SHA-256, so an edit uploads only the shards it touched and identical content is never stored twice.

## The cloud

A drive is a bucket. Objects support list, HEAD, GET with `Range`, PUT and DELETE. Every PUT creates an immutable version whose id is the ETag and which names the version it replaced; history is kept (25 versions per key) and an old version can be restored without writing content. Every commit to a drive goes through that drive's lock and bumps its sequence number; `If-Match` makes one of two racing writers lose with a 412. A fork is a new catalog pointing at the same versions. Synthetic objects are the SplitMix64 stream for a seed, generated from the offset. On disk a drive's catalog is a snapshot plus a log: a commit appends one line with the new state of the keys it touched, and every 4,000 commits the log is folded into a fresh snapshot, so the cost of a commit does not grow with the drive (a test makes 4,100 commits, restarts, and restarts again with stale log lines put back).

## Shards

Content is cut into shards with FastCDC content-defined chunking (256 KiB minimum, about 2 MiB on average, 16 MiB maximum), so the cut points follow the bytes and an edit moves only the shards around it. A shard is named by the SHA-256 of its bytes and stored once, in `<data>/shards/`. A version's content is a manifest in `<data>/manifests/`: the ordered list of segments, each either a shard or a range of a synthetic stream. Identical content gives the same shards and the same manifest, whatever key or drive it is under. Manifests and shards are reference counted and deleted when no version points at them; fork, rename and restore write no content.

## An upload in four steps

An upload from the mount goes in four steps:

1. Plan the new content against the manifest of the version underneath. Segments no write touched are kept by reference. A synthetic range is split exactly around what was written. A shard that was written into is read again (journal over cloud) and cut afresh, together with its written neighbours.
2. `POST /_shards` with the hashes of the newly cut shards; the cloud answers which ones it lacks.
3. `PUT /_shards/<sha256>` for those, four at a time. The cloud checks each body against its name.
4. `PUT /<drive>/<key>?manifest` commits the manifest. No content moves in this request. If a shard has gone missing in the meantime the cloud says which (409) and the client sends it.

A small patch skips steps 2 and 3: when the upload keeps part of the old version and its new shards add up to at most 8 MiB, they travel in the commit request itself, after the manifest, and the cloud checks each against its name before committing. The drive also remembers the manifests of versions it uploaded or patched, so planning the next patch needs no request. A second edit to a file is then one round trip.

A plain `PUT` with a body still works, for `curl`: the server cuts it into shards itself. `ludy put` uses the four steps and prints how much was sent and how much the cloud already had. Reads are unchanged: a `GET` with `Range` on the object, which the server maps onto the shards that cover it.
