# Status

Where each part of Ludy stands, and where its lists of limits are kept. Each list lives on one page only; this page points at them. The one list that lives here is [Not built](#not-built).

## At a glance

| Part | State | Detail |
|---|---|---|
| Simulated cloud, engine, mount | Built, covered by automated tests, and checked against real mounts | [What works](evidence/verification.md) |
| The app | Built. Some controls were clicked and checked in screenshots; the rest were not | [Not verified](evidence/verification.md#not-verified) |
| The engine under faults | A chaos suite and a randomized soak pass; seven problems are known and not fixed | [Chaos testing](evidence/chaos-testing.md#known-and-not-fixed) |
| The encrypted cloud | Built beside the simulated one and passing its chaos suites on the local emulator. The app does not use it, and nothing has run on Cloudflare | [The real cloud](real-cloud/overview.md), [known problems](real-cloud/known-issues.md) |
| Self-hosting the cloud | A plan only | [Architecture](real-cloud/architecture.md) |

## Simulated cloud, engine and app (2026-11-19)

The server, the engine and the app are built, covered by automated tests, and checked against real mounts: listing, range-only reads, video playback, writes, remote changes, eject and the crash case all work (see [What works](evidence/verification.md)). Not checked: most of the app's controls beyond the handful that were clicked (see [Not verified](evidence/verification.md#not-verified)), and the Quick Look panel itself. Read [Eject and the "network volume" prompt](how-it-works/mount.md#eject-and-the-network-volume-prompt) for a hazard to know about. The engine has a benchmark and a chaos suite (cloud killed mid-upload, stalls, app crashes with unsent writes); see [Benchmarks](evidence/benchmarks.md) and [Chaos testing](evidence/chaos-testing.md).

## The encrypted cloud (2026-11-22)

The format, the protocol, the Worker and a second engine exist and are tested against each other under `wrangler dev`. Nothing mounts a sealed drive yet, there is no sign-in, and nothing has been deployed. What was built and how it was checked is in [the real cloud](real-cloud/overview.md); what is expected to break, what is fragile and what nobody has checked is in [known problems](real-cloud/known-issues.md).

## Where the limits are listed

| List | About | Page |
|---|---|---|
| Not verified | The app's controls, Quick Look, the Finder | [verification](evidence/verification.md#not-verified) |
| Known and not fixed | The engine and the simulated cloud | [chaos testing](evidence/chaos-testing.md#known-and-not-fixed) |
| Known problems, not verified, not built, open decisions | The encrypted cloud | [known problems](real-cloud/known-issues.md) |
| Not built | The engine, the simulated cloud and the app | below |

## Not built

This list is about the engine, the simulated cloud and the app. What the encrypted cloud lacks is in [known problems](real-cloud/known-issues.md#not-built).

- **Shards on the read path and in the cache.** Reads are range requests on the object, and the disk cache is keyed by version and block, not by shard. So after a patch the Mac's cache does not carry over to the new version except for the file that was just uploaded, and two files with the same content are cached twice.
- **Insert.** A write at an offset is a patch; inserting bytes in the middle of a file through the mount is a rewrite of everything after it, as in any filesystem. Content-defined cutting resynchronises (a unit test covers it), so most of the shards after the insert are found in the cloud and not sent again, but they are read and hashed.
- **Cleanup of abandoned uploads while the server runs.** Shards sent for an upload that never commits stay until the server restarts.
- **Not exercised:** the client's second attempt after the cloud reports a missing shard at commit (the server's 409 is tested; the retry has never fired).
- **A second user.** Sharing records an email and a role on the drive and shows who has access. There are no accounts, and nothing enforces View versus Edit.
- **Thumbnails** in the in-app browser, drag and drop, search, and a menu-bar launcher.
- **An app bundle.** The app runs from `cargo run` and is unsigned.
