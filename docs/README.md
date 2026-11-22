# Ludy documentation

Start with the [README](../README.md) for what Ludy is and how to run it. These pages go deeper, one topic each. Every page opens with a few sentences that say what it concludes, so you can stop there or read on.

## Start here

| Page | What it answers |
|---|---|
| [Getting started](getting-started.md) | How do I run it? Every command, the environment variables, what building needs |
| [Status](status.md) | What works, what does not, and where each list of limits is kept |

## How it works

The system the app mounts today: the engine on the Mac and a simulated cloud.

| Page | What it answers |
|---|---|
| [The engine](how-it-works/engine.md) | How a read, a write and a remote change travel; what is kept on the Mac |
| [The mount](how-it-works/mount.md) | Why loopback NFS, what else was considered, and the eject hazard |
| [The simulated cloud and shards](how-it-works/simulated-cloud.md) | How the cloud stores versions, how content is cut into shards, how an upload goes |
| [The app](how-it-works/app.md) | What the window shows, and how fast it scrolls a large folder |
| [The workspace](how-it-works/workspace.md) | Which crate is what, and what another client would reuse |

## Evidence

What was measured and how. These pages are long because they hold the numbers.

| Page | What it answers |
|---|---|
| [What works, and how it was checked](evidence/verification.md) | Which claims rest on tests, which on real mounts, which on a screenshot, and which on nothing |
| [Benchmarks](evidence/benchmarks.md) | How fast, before and after three rounds of work |
| [Chaos testing](evidence/chaos-testing.md) | What happens when the cloud or the app dies mid-work; what that found |

The raw benchmark tables are in [`bench/`](../bench).

## The real cloud

The end-to-end encrypted cloud being built beside the simulated one, self-hosted or on Cloudflare. The app does not use it yet.

| Page | What it answers |
|---|---|
| [Overview](real-cloud/overview.md) | What has been built, how it was checked, what the checks found |
| [Known problems](real-cloud/known-issues.md) | What is expected to break, what is fragile, what nobody has checked, what is not built |
| [Architecture](real-cloud/architecture.md) | How one crate of server logic runs on Rivet actors under Docker Compose and on Cloudflare, what a provider must promise, and why Rivet is not the Cloudflare option too |
| [The Rivet provider](../providers/rivet/README.md) | How do I self-host it? The Compose files, the settings, R2 or MinIO for the blobs |

## Where things go

When adding to these pages:

- A fact lives on one page. Other pages link to it.
- A new page opens with what it concludes, in two to four sentences, before any detail.
- Limits go in the list that already covers that part of the system; [Status](status.md) says which.
- Numbers keep their date and how they were measured.
