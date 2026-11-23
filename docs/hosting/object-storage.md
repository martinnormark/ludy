# Docker Compose with R2, MinIO or another S3

The Compose server can keep its sealed shards and manifests in an S3 bucket instead of a Docker volume. A second Compose file on top of the first one does it. Only the blobs move: every drive's database stays with the Rivet engine on the machine. Both the MinIO path and the R2 path were run, R2 against a real bucket (2026-11-23).

Read [Docker Compose on one machine](compose.md) first. Everything there still applies; this page is what changes.

## What moves and what does not

| | Where it is |
|---|---|
| Sealed shards and manifests, under `d/<drive id>/s/<sha256>` and `d/<drive id>/m/<sha256>` | The bucket |
| Each drive's database: the tree, the versions, which shards are in use | The engine's volumes, `ludy_rivet-log` and `ludy_rivet-objects`, on the machine |

So the machine is still needed, and its volumes still need looking after: a bucket full of shards without the drives' databases cannot be put back together. What reaches the bucket is sealed; the bucket's owner can read none of it.

Choose the store before you create drives. Nothing copies the blobs of an existing server from the volume to a bucket, or from one bucket to another.

## Cloudflare R2

[`docker-compose.r2.yml`](../../docker-compose.r2.yml) was run against a real bucket on 2026-11-23; what was seen is [below](#what-was-seen-with-r2). The steps on Cloudflare's side follow its documentation as read that day; the bucket and the token used for the check were made by the account's owner, not by following this page.

### What you need

- A Cloudflare account with R2 enabled. Cloudflare's documentation says R2 has to be purchased on the account before an API token can be made; its free tier is 10 GB-month of storage, 1 million Class A and 10 million Class B operations a month.
- The account's id, from the Cloudflare dashboard.

### Steps

1. Create a bucket. In the dashboard, or with Wrangler:

   ```bash
   npx wrangler r2 bucket create ludy-blobs
   ```

2. Make an API token for the S3 API. In the dashboard: **R2 object storage**, **Overview**, then **Manage** next to **API Tokens** under Account Details. Create a token with the permission **Object Read & Write** and scope it to the bucket. The page then shows an **Access Key ID** and a **Secret Access Key**; the secret is shown once.

3. Copy [`.env.example`](../../.env.example) to `.env` beside the Compose files and fill in four lines. `.env` is in `.gitignore`.

   ```text
   R2_ACCOUNT_ID=<the account id>
   R2_BUCKET=ludy-blobs
   R2_ACCESS_KEY_ID=<from step 2>
   R2_SECRET_ACCESS_KEY=<from step 2>
   ```

4. Start it with both files:

   ```bash
   docker compose -f docker-compose.yml -f docker-compose.r2.yml up -d --wait
   ```

   Compose stops with a message naming the variable if one of the four is missing.

The R2 file points the server at `https://<account id>.r2.cloudflarestorage.com` with region `auto`. A bucket created in a jurisdiction (EU, FedRAMP) has an endpoint of its own, `https://<account id>.eu.r2.cloudflarestorage.com` for the EU; the R2 file does not cover that, so set `AWS_ENDPOINT` yourself as under [another S3](#another-s3).

### Check it

```bash
curl http://127.0.0.1:8080/
```

That answers `ludy-rivet`, which says the server and the engine are up and nothing about the bucket: as the code reads, the server sets up its S3 client at start without contacting the store (not tried with a bucket it cannot reach). So upload something and look in the bucket.

```bash
cargo run -p ludy-cli -- mkdrive Check
```

```bash
cargo run -p ludy-cli -- put Check README.md
```

The bucket should now hold two objects under `d/<drive id>/`: one in `s/` and one in `m/`. `ludy drives` prints the id. If the upload fails, `docker compose logs ludy` has the store's error.

### What was seen with R2

A Compose project of its own on this Mac, the published arm64 image, a bucket with nothing else in it, and `GC_GRACE_MS=5000` so the collector could be watched. The bucket was listed through R2's S3 API with the same token.

- **Starting.** `up -d --wait` took 13 seconds. The server reported `LUDY_BLOBS=s3://<bucket>` and region `auto`.
- **Uploading.** A 4 KiB file made two objects under `d/<drive id>/`, one in `s/` and one in `m/`, 4,257 bytes together. With a 24 MiB file, a folder and the 95 MB demo video added, the prefix held 52 shards and 5 manifests, 123,304,274 bytes. The video went up in 11.0 s. The server's own blob volume stayed empty.
- **Reading back.** A second keystore with an empty cache downloaded all five files, 117.6 MiB, in 9.9 s, and each compared equal to its source.
- **A mount.** The drive was mounted from a third keystore. A 64 KiB read in the middle of the 24 MiB file compared equal to the source and returned after 224 ms; with the kernel's read-ahead it came as three range requests and 2.1 MiB. A text file and a 5 MB copy were written through the mount.
- **A kill.** Both containers were killed with SIGKILL, a file was written to the mount while they were down, and `up -d --wait` brought them back. The second keystore then listed all seven entries, the 5 MB file compared equal, and the file written during the outage had arrived.
- **The collector.** Everything was deleted through the mount, 64 objects by then. At the first look, about eleven seconds after the deletes, the prefix was empty.

One quick run of `ludy bench` (`--quick`) against it, to see what a store over a real network does to the numbers. The Mac and the server were still on loopback; only the server's calls to R2 crossed the internet, over this Mac's connection, whose speed was not measured.

| Scenario | Result | Notes |
|---|---|---|
| Copy a file in: writes return | 856 MB/s | 8 MiB of incompressible data into the journal |
| Copy a file in: upload | 1.04 s (8 MB/s) | cut, seal, send the sealed shards, commit a sealed manifest |
| Copy the same content in again: upload | 420 ms | cut and sealed again; every sealed shard is already in the cloud |
| Change 4 KiB inside that file: upload | 733 ms (1.4x faster than uploading it) | only the shard the write touched is cut, sealed and sent |
| Read 64 KiB out of that file, cold | 359 ms (2x faster than reading it whole) | 1 range request(s), after the manifest; reading all 8 MiB cold took 854 ms |
| First 2 MiB of a file, cold | 797 ms | the manifest, then 256 KiB reads from the start of the file |
| Sequential read, cold | 20 MB/s | 32 MiB; 256 KiB reads, as the kernel issues them |
| Sequential read, warm | 3002 MB/s | 32 MiB; the same bytes again, from the disk cache |
| Random 64 KiB read, cold | 113 ms / 230 ms | 40 reads; p50 / p99; a block each, nothing cached |
| Random 64 KiB read, warm | 0.11 ms / 0.22 ms | 40 reads; p50 / p99; from the disk cache |
| Create many small files | 242 ms | 500 files of 4 KiB, until the writes returned |
| Upload those files | 74.51 s (7 files/s) | until all 500 are in the cloud |
| Open a drive with many files | 18 ms | 504 nodes: read the feed, unwrap every key, open every name |
| List that folder | 0.03 ms | from the tree in memory |
| List that folder in pages | 0.03 ms | 500 files, 100 at a time, as the NFS client asks for them |
| Pick up one remote change | 4.16 ms | one request to the feed of a drive of 505 nodes |
| Delete files | 89 ms (1125 files/s) | 100 deletes, sent in order, 200 to a commit |

Against the same benchmark with the blobs on the server's own disk ([benchmarks](../evidence/benchmarks.md)): a cold 64 KiB read is 113 ms at the median where it was under 10, a cold sequential read 20 MB/s where it was over 120, and small files upload at 7 a second where they did at about 200. The last is the one to know about before putting many small files in a drive; see [known problems](../known-issues.md#the-engine-and-the-app).

Not tried with R2: the chaos suites, wrong or revoked credentials, a bucket in a jurisdiction, anything larger than the 95 MB video, and what the calls cost.

## MinIO

MinIO is an S3 that runs beside the server, in a third container. It is how the S3 code path is tried without an account anywhere, and it is a way to keep the blobs in a store you can browse.

```bash
docker compose -f docker-compose.yml -f docker-compose.minio.yml up -d --wait
```

Nothing has to be set. [`docker-compose.minio.yml`](../../docker-compose.minio.yml) starts MinIO with a bucket called `ludy` in a volume of its own, `ludy_minio-data`, and points the server at it. `MINIO_USER` and `MINIO_PASSWORD` in `.env` change its credentials from the defaults; MinIO's port is not published, so only the server reaches it.

The image is Chainguard's build of MinIO (`cgr.dev/chainguard/minio:latest`); MinIO no longer publishes images of its own on Docker Hub, and Chainguard offers only the `latest` tag for free, so the version is not pinned.

Checked on 2026-11-23: started from nothing in 12 seconds, a drive created, a 24 MiB file and a small folder uploaded, the file downloaded from a second keystore and compared. MinIO's volume held 15 shards and 3 manifests for the drive, 27 MB, and the server's own blob volume stayed empty. On 2026-11-22 the collector's deletes and a `kill -9` of the containers were tried on this path as well. The chaos suites have not run with MinIO.

## Another S3

Any S3 that accepts a path-style bucket should work the same way; only R2 and MinIO are named here because only they were run. Use the R2 file as a pattern: a Compose file of your own that sets the server's store.

```yaml
services:
  ludy:
    environment:
      LUDY_BLOBS: s3://<bucket>
      AWS_ENDPOINT: https://<the store's endpoint>
      AWS_ACCESS_KEY_ID: <key id>
      AWS_SECRET_ACCESS_KEY: <secret>
      AWS_REGION: <region>
```

With `AWS_ENDPOINT` set, the bucket goes in the path, which R2 and MinIO want. The server needs to put, get with a byte range, check for, delete and list objects. It hashes every upload itself and refuses one that does not match its name, so it asks nothing of the store there.

## What was checked

| | |
|---|---|
| MinIO under Compose | Yes, as above |
| R2 under Compose | Yes, by hand against a real bucket, as [above](#what-was-seen-with-r2) |
| Any other S3 | No |
| The chaos suites with a bucket | No. They run with a folder |
