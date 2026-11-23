# The server from the checkout, beside a Rivet engine

For working on the server's code: the Rivet engine runs in a container and the server, `ludy-rivet`, runs from the checkout with `cargo run`. It is the same server Compose starts, without its image. Both chaos suites start it this way.

## What you need

- Docker, for the engine.
- A Rust toolchain. The first build of `ludy-rivet` takes about three and a half minutes.
- Port 6420 free for the engine and 8080 for the server.

## Start it

The engine first. Its port is published on this machine only, with the token `dev`:

```bash
docker run -d --name rivet -p 127.0.0.1:6420:6420 -e RIVET__AUTH__ADMIN_TOKEN=dev -e RIVET__STORAGE__LOG_DIRECTORY=/var/lib/rivet/log -e RIVET__STORAGE__AUTO_PROVISION_PARTITIONS=1 -e RIVET__STORAGE__BALANCING__ENABLED=false -e RIVET__OBJECT_STORAGE__PRIMARY__FILE_SYSTEM__PATH=/var/lib/rivet/objects -e RIVET__PEGBOARD__NODE_LOST_THRESHOLD=2000 -e RIVET__PEGBOARD__NODE_PING_TIMEOUT=2000 -v /var/lib/rivet/log -v /var/lib/rivet/objects rivetdev/engine:3.1.1
```

Wait until it says it is ready:

```bash
curl -fsS http://127.0.0.1:6420/ready
```

Then the server, from the top of the repository:

```bash
LUDY_LISTEN=127.0.0.1:8080 RIVET_ENDPOINT=http://default:dev@127.0.0.1:6420 cargo run --release -p ludy-rivet
```

`LUDY_LISTEN` matters. Left out, the server listens on every interface (`0.0.0.0:8080`, which is what a container wants), and with no sign-in that offers every drive to the network the Mac is on.

The package is left out of a plain `cargo build` and `cargo test` because it is a large build; name it with `-p ludy-rivet`. The engine image and the `rivetkit` crate are pinned to one release, 3.1.1, and have to move together.

## Check it is up

```bash
curl http://127.0.0.1:8080/
```

That answers `ludy-rivet` once the server has joined the engine. `ludy-rivet health` does the fuller check Compose uses: it exits 0 when a drive answers.

## Use it

It listens where Compose does, so the app and the CLI find it with nothing set. If you chose another port in `LUDY_LISTEN`, set `LUDY_SERVER` to match.

## Where the data is

- Sealed shards and manifests: the folder `./ludy-blobs`, relative to where the server was started, unless `LUDY_BLOBS` says otherwise. `LUDY_BLOBS=s3://<bucket>` with the `AWS_` variables puts them in a bucket, as in [object storage](object-storage.md#another-s3).
- The drives' databases: the two anonymous volumes of the `rivet` container. `docker rm -v rivet` deletes them, and the drives with them.

The server's settings are in [the provider's README](../../providers/rivet/README.md#settings).

## Stop it

Ctrl-C stops the server. The engine:

```bash
docker stop rivet
```

`docker start rivet` brings it back with its drives.

## What was checked

Both chaos suites launch exactly this: `target/release/ludy-rivet` beside a `rivetdev/engine:3.1.1` container of their own, blobs in a folder, with the same two-second node-lost setting as above. They kill both with `kill -9`, freeze the server, and lose requests in a proxy; see [chaos testing](../evidence/chaos-testing.md). The commands on this page were run as written on 2026-11-23: the engine became ready, the server answered `ludy-rivet` on the loopback address only and `ludy-rivet health` exited 0, the demo script filled three drives through it, and the engine answered ready again after `docker stop` and `docker start`. That the drives were still there after that restart was not looked at.
