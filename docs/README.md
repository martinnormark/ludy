# Ludy documentation

Start with the [README](../README.md) for what Ludy is and how to run it. These pages go deeper, one topic each. Every page opens with a few sentences that say what it concludes, so you can stop there or read on.

## Start here

| Page | What it answers |
|---|---|
| [Getting started](getting-started.md) | How do I run it? Every command, the environment variables, what building needs |
| [Status](status.md) | What works, what does not, and where each list of limits is kept |

## Hosting

How to run the server. One page per way of doing it, each with the same sections.

| Page | What it answers |
|---|---|
| [Overview](hosting/README.md) | Which ways are there, which to choose, how to point the app at a server, and what having no sign-in means |
| [Docker Compose](hosting/compose.md) | The default: one command on one machine |
| [R2, MinIO or another S3](hosting/object-storage.md) | The same, with the sealed blobs in a bucket |
| [The binary beside an engine](hosting/binary.md) | The server from the checkout, for working on it |
| [Cloudflare](hosting/cloudflare.md) | The Worker, locally under `wrangler dev` and deployed |

## How it works

| Page | What it answers |
|---|---|
| [The engine](how-it-works/engine.md) | How a read, a write and a remote change travel; what is kept on the Mac; where keys are |
| [Encryption and shards](how-it-works/encryption.md) | What is sealed and how, how content is cut, what the server can still see |
| [The server](how-it-works/server.md) | The protocol, the one crate of logic, what a provider must promise and how each of the two keeps it |
| [The mount](how-it-works/mount.md) | Why loopback NFS, what else was considered, and the eject hazard |
| [The app](how-it-works/app.md) | What the window shows, and how fast it scrolls a large folder |
| [The workspace](how-it-works/workspace.md) | Which crate is what, and what another client would reuse |

The two providers also have a README beside their code, with their settings and how they are put together: [Rivet](../providers/rivet/README.md), [Cloudflare](../providers/cloudflare/README.md).

## Evidence

What was checked and measured, and how.

| Page | What it answers |
|---|---|
| [What works, and how it was checked](evidence/verification.md) | Which claims rest on tests, which on real mounts, which on a screenshot |
| [Chaos testing](evidence/chaos-testing.md) | What happens when the server or the app dies mid-work; what that found |
| [Benchmarks](evidence/benchmarks.md) | How fast, against each provider on loopback |
| [Known problems](known-issues.md) | What is expected to break, what is fragile, what nobody has checked, what is not built, and the decisions still open |

The raw tables are in [`bench/`](../bench).

## History

Until 2026-11-23 the app ran on another engine, against a simulated server on localhost. Both were removed. Their pages are kept as they were, each marked at the top, because the benchmark tables in `bench/` from 2026-11-19 refer to them and because what they found shaped the engine there is now.

| Page | What it was |
|---|---|
| [The simulated cloud and shards](history/simulated-cloud.md) | The local server and its storage |
| [What worked](history/verification.md) | Checks of the previous engine, including the Finder and video playback |
| [Benchmarks](history/benchmarks.md) | Three rounds of before-and-after numbers against a simulated link |
| [Chaos testing](history/chaos-testing.md) | The previous engine's chaos suite and what it found |

## Where things go

When adding to these pages:

- A fact lives on one page. Other pages link to it.
- A new page opens with what it concludes, in two to four sentences, before any detail.
- Limits go in [known problems](known-issues.md).
- Numbers keep their date and how they were measured.
- A hosting page says what was checked and when. Steps nobody has run are marked as such.
