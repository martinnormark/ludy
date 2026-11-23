# Cloudflare: the Worker

The Cloudflare provider is a Worker, a Durable Object per drive and an R2 bucket. It runs on this Mac under `wrangler dev`, which is how it is tested and a fine way to try Ludy without Docker. It can be deployed to Cloudflare with one command, but nobody has done that yet, and a deployed Worker is open to anyone who finds its address.

## What you need

For both:

- Node and npm.
- A Rust toolchain with the WebAssembly target, and `worker-build`. Once:

  ```bash
  rustup target add wasm32-unknown-unknown
  ```

  ```bash
  cargo install worker-build
  ```

- Wrangler, which is a dependency of the package. In `providers/cloudflare`, once:

  ```bash
  npm install
  ```

It was run with Wrangler 4.166.0 and `worker-build` 0.9.0. Use the Wrangler this installs (`npx wrangler`, or the `npm run` scripts), not an older global one.

To deploy, also:

- A Cloudflare account. The Workers Free plan is enough to start: Durable Objects with SQLite storage are available on it.
- R2 enabled on the account.

## Run it locally

In `providers/cloudflare`:

```bash
npm run dev
```

The first start compiles the Worker to WebAssembly. It then answers on `http://127.0.0.1:8787`, on the loopback addresses only, under workerd, Cloudflare's runtime, with R2 and the Durable Objects' storage emulated in `.wrangler` in that folder. Delete `.wrangler` to delete every local drive.

Check it:

```bash
curl http://127.0.0.1:8787/
```

That answers `ludy-worker`. Point Ludy at it, from the top of the repository:

```bash
LUDY_SERVER=http://127.0.0.1:8787 cargo run -p ludy-cli -- mkdrive Footage
```

```bash
LUDY_SERVER=http://127.0.0.1:8787 cargo run -p ludy-app
```

The emulator enforces none of Cloudflare's limits (requests to R2 per invocation, CPU time, row size, storage per object), so a drive that works here can still be too large for the real thing; the list is in [known problems](../known-issues.md#sizes-the-code-allows-and-cloudflare-does-not).

## Deploy it

Not verified: the Worker has never been deployed. The steps follow Wrangler's documentation as read on 2026-11-23, and `wrangler deploy --dry-run` builds and lists the two bindings below. Read the three warnings before you run them.

**It is public.** Wrangler gives a new Worker a `workers.dev` address, `https://ludy-worker.<your subdomain>.workers.dev`, and Cloudflare documents that address as public. There is no sign-in. Anyone who has the address can create drives and upload to them on your account, and anyone who also has a drive's id can read its sealed bytes and delete from it. Ludy's client cannot send a token or a header, so putting Cloudflare Access in front would lock the app out too.

**An open drive is a request a second.** A drive that is mounted or open in the app asks the feed once a second, which is 86,400 requests a day for one drive on one Mac. The Free plan allows an account's Durable Objects 100,000 requests a day in all, and past a limit Cloudflare fails further requests until the next day (UTC). The paid plan includes a million a month and bills beyond it; one drive open around the clock is about 2.6 million.

**Its limits are untested.** See [known problems](../known-issues.md#cloudflare). In particular a new file of more than about 1,000 shards (roughly 2 GB) is expected to fail to commit on the Free plan.

In `providers/cloudflare`:

1. Sign Wrangler in. It opens a browser:

   ```bash
   npx wrangler login
   ```

2. Create the bucket [`wrangler.toml`](../../providers/cloudflare/wrangler.toml) names:

   ```bash
   npx wrangler r2 bucket create ludy-shards
   ```

3. Deploy:

   ```bash
   npm run deploy
   ```

   That compiles the Worker, uploads it (about 800 KiB), creates the Durable Object class `Drive` with SQLite storage from the migration in `wrangler.toml`, and prints the address.

4. Check it, with the address from step 3:

   ```bash
   curl https://ludy-worker.<your subdomain>.workers.dev/
   ```

   It should answer `ludy-worker`.

5. Point Ludy at it:

   ```bash
   LUDY_SERVER=https://ludy-worker.<your subdomain>.workers.dev cargo run -p ludy-cli -- mkdrive Footage
   ```

Watch what it does:

```bash
npx wrangler tail
```

Take it down again:

```bash
npx wrangler delete
```

Whether that also removes the bucket's content was not tried; look in the dashboard afterwards.

## Settings

They are in [`wrangler.toml`](../../providers/cloudflare/wrangler.toml).

| Name | What it is |
|---|---|
| `SHARDS` | The R2 bucket binding. The bucket is named `ludy-shards`; change `bucket_name` to use another |
| `DRIVE` | The Durable Object binding: class `Drive`, with SQLite storage |
| `GC_GRACE_MS` | A variable, not set by default. How long a shard no version uses is kept; a day if it is not set |
| `LUDY_DEV` | A variable, not set by default. `1` turns on the `_dev` routes, which let a caller break things on purpose. Only the chaos suites set it. Never set it on a deployed Worker |

## Where the data is

| | Locally | Deployed |
|---|---|---|
| Sealed shards and manifests | `.wrangler` | The R2 bucket, under `d/<drive id>/` |
| Each drive's database | `.wrangler` | That drive's Durable Object |

## What was checked

- **Under `wrangler dev`:** both chaos suites pass (see [chaos testing](../evidence/chaos-testing.md)). On 2026-11-23 a drive was created, filled and mounted from it: listing, reads, a copy in through the mount, and a read back from a second keystore.
- **`wrangler deploy --dry-run`** on 2026-11-23: builds, 799 KiB to upload, bindings `DRIVE` and `SHARDS`.
- **Deployed:** nothing. Every behaviour the Worker depends on Cloudflare for was only seen in the emulator; the list is in [known problems](../known-issues.md#cloudflare).
