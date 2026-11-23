# Status

Where each part of Ludy stands on 2026-11-23, and where its lists of limits are kept. Ludy runs on a real server: the app, the CLI and the mount use the end-to-end encrypted engine against either provider. Everything was checked from one Mac, with the server on that Mac; the one thing outside it so far is a Cloudflare R2 bucket as the store. Nothing is deployed, and there is no sign-in.

## At a glance

| Part | State | Detail |
|---|---|---|
| The engine | Built. Covered by a suite of scenarios and a soak against both providers, under kills, freezes and lost messages | [Chaos testing](evidence/chaos-testing.md) |
| The mount | Built. Drives were mounted from both providers and read and written from the shell; a mount survived the server and the mounting process being killed. The Finder and applications have not been tried on this engine | [What works](evidence/verification.md#against-real-mounts) |
| The app | Built. Lists, browses and mounts encrypted drives. A handful of screens were looked at; most controls were not clicked | [Not verified](known-issues.md#the-app) |
| The server's logic | Built. Tested by itself on SQLite in memory, with storage made to fail | [The server](how-it-works/server.md) |
| The Rivet provider (Docker Compose) | Built. Passes both chaos suites. One machine only | [Hosting](hosting/compose.md) |
| The Cloudflare provider | Built. Passes both chaos suites under `wrangler dev`. Never deployed | [Hosting](hosting/cloudflare.md) |
| R2 as the store under Compose | Works against a real bucket, by hand and under the chaos suites in a loop, with the bucket itself made to refuse, go silent and drop connections | [Hosting](hosting/object-storage.md#cloudflare-r2), [chaos testing](evidence/chaos-testing.md#the-store-taken-away) |
| The encryption format | Version 1. Checked against a second implementation by the same author; reviewed by nobody else | [Encryption](how-it-works/encryption.md) |
| Sign-in, sharing | Not built. A drive is opened by a key kept in a file on the Mac | [Not built](known-issues.md#not-built) |

## What changed on 2026-11-23

Until then the app ran on an earlier engine against a simulated server on localhost, and the encrypted engine and the real server were being built beside them. On this date:

- The app, the CLI, the NFS adapter and `ludy bench` moved to the encrypted engine.
- The simulated server (`ludy-cloud`), the earlier engine and their tests were removed. Their pages are under [history](README.md#history).
- A keystore was added, since the server lists no drives and holds no key.
- Features that the simulated server faked or that the real one has nothing for were taken out of the app: sharing by email, forking a drive, deleting a drive on the server, keeping a file offline, the link's simulated speed.

## Where the limits are listed

One page: [known problems](known-issues.md). Its sections:

| Section | About |
|---|---|
| [Known problems](known-issues.md#known-problems) | What is expected to break or is wrong today, on the server, in the engine and the app, on each provider |
| [Not verified](known-issues.md#not-verified) | What nobody has checked |
| [Not built](known-issues.md#not-built) | What is missing |
| [Decisions still open](known-issues.md#decisions-still-open) | Questions waiting for an answer |

Each [hosting](hosting/README.md) page ends with what was checked for that way of running the server.
