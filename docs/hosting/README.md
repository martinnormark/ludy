# Hosting the server

Ludy's server can be run in six ways, on two providers. Each has a page with the same sections: what you need, the steps, how to tell it is up, how to point the app at it, and what was actually checked. Start with Docker Compose on one machine unless you have a reason not to.

Read [before you expose it](#before-you-expose-it) first: the server has no sign-in.

## The paths

| Path | The server runs as | Drives' databases | Sealed shards and manifests | Checked | Page |
|---|---|---|---|---|---|
| Docker Compose, the default | Two containers on one machine | A Docker volume | A Docker volume | Yes: suites, a mount, kills (2026-11-23) | [compose.md](compose.md) |
| Docker Compose with Cloudflare R2 | Two containers on one machine | A Docker volume | An R2 bucket | Yes, against a real bucket (2026-11-23). Not the chaos suites | [object-storage.md](object-storage.md#cloudflare-r2) |
| Docker Compose with MinIO or another S3 | Two or three containers on one machine | A Docker volume | A bucket | MinIO: yes (2026-11-23). Any other S3: no | [object-storage.md](object-storage.md#minio) |
| The binary beside a Rivet engine | A process from the checkout, the engine in a container | The engine's volume | A folder or a bucket | Yes: both chaos suites run it this way | [binary.md](binary.md) |
| Cloudflare Worker, local | `wrangler dev` on this Mac | `.wrangler` in the checkout | `.wrangler` in the checkout | Yes: suites and a mount (2026-11-23) | [cloudflare.md](cloudflare.md#run-it-locally) |
| Cloudflare Worker, deployed | Cloudflare's network | Durable Objects | An R2 bucket | No. Never deployed | [cloudflare.md](cloudflare.md#deploy-it) |

"Checked" means what the page's last section says and no more. Everything was done from one Mac with the server running on it; the R2 bucket is the only part that was somewhere else.

## Which one

- **To try Ludy, or to keep drives on a machine of your own:** [Docker Compose](compose.md). One command, nothing to set.
- **To keep the bulk of the data off that machine's disk:** Compose with [R2 or another S3](object-storage.md). The drives' small databases stay on the machine; only the sealed blobs move.
- **To work on the server's code:** [the binary beside an engine](binary.md), or the [Worker under `wrangler dev`](cloudflare.md#run-it-locally).
- **To run without a machine of your own:** [the Worker, deployed](cloudflare.md#deploy-it). Read that page's warnings first. Nobody has done it yet.

The two providers do not share data. A drive lives on the server it was created on, and nothing moves a drive from one to the other.

## Pointing Ludy at a server

The app and the CLI talk to one server, the one in `LUDY_SERVER`. The default is `http://127.0.0.1:8080`, where Compose answers. `https://` works too; certificates are checked against the Mac's trusted roots.

```bash
LUDY_SERVER=http://127.0.0.1:8787 cargo run -p ludy-cli -- server
```

That prints the address and the name the server gives itself: `ludy-rivet` for both Compose paths and the binary, `ludy-worker` for Cloudflare. If nothing of Ludy's answers there, it says so.

The Mac's keystore lists drives per server address, so drives made on one server do not show while `LUDY_SERVER` names another. The address is part of the entry: if a server moves to a new address, its drives have to be added again from their keys (`ludy key`, then `ludy add`).

## Before you expose it

There is no sign-in, and no path here adds one.

- Whoever can reach the server can create drives and upload to them, at your cost in storage.
- Whoever can reach the server **and knows a drive's id** can read that drive's sealed bytes, commit to it and delete from it. They cannot read content or names without the drive's key, and an id is 128 random bits, but an id is not a secret the design protects: it travels in every URL, in the clear over `http://`.
- Whoever has a drive's key and can reach the server has the whole drive, read and write.

So:

- The Compose file publishes its port on the machine itself only (`127.0.0.1`). Keep it that way unless something in front authenticates.
- To use a Compose server from another Mac, reach it over a private network or tunnel you already trust (an SSH tunnel, a VPN), or put a reverse proxy in front that terminates TLS and authenticates. Neither was tried here. Ludy's client cannot send extra headers or credentials, so a proxy that expects a token from the client will not work with the app.
- A deployed Worker is public at its `workers.dev` address. See [the Cloudflare page](cloudflare.md#deploy-it).

Sign-in is listed under [not built](../known-issues.md#not-built).
