# The app

A plain file explorer written with gpui, for the drives this Mac holds a key for. It browses a drive whether it is mounted or not. Scrolling a folder of 5,000 files holds 60 frames a second on a 60 Hz display in the unoptimized build. There is no dark theme and there are no thumbnails.

## What it shows

A plain file explorer in a light theme: drives in the sidebar with the server's name and address under them, a toolbar with the path and the actions, the folder as a grid or a list, and a status bar with the live numbers (streamed, uploads, cache). The drives screen has one card per drive with Mount, Open, Show in Finder and eject.

The drives are the ones this Mac holds a key for on the server it is talking to. The plus beside **Drives** creates one; the arrow beside it adds a drive from a key copied on another Mac. A card shows a drive's size once the drive has been opened, since the size is in the tree only this Mac can read.

Right-click a drive, or use the dots on its card, for Open in Terminal, Copy path, Rename, Copy drive key and Remove drive. Rename changes the name on this Mac. Remove drive forgets the key on this Mac and leaves the drive on the server. Right-click a file for Rename, Versions (the last 25, each can be restored) and Delete. Import, in the toolbar, copies files and folders from the Mac into the open folder.

The browser works on a drive whether it is mounted or not. Opening files in other applications, Show in Finder and Open in Terminal need the drive mounted.

Not there: sharing with people, forking a drive, keeping a file offline. The first two have nothing behind them on the server yet; see [not built](../known-issues.md#not-built).

## Scrolling a large folder

Folders are drawn with a virtual list, so only the rows on screen are built each frame. Measured with `--perf` on a 60 Hz display, in the unoptimized dev build, scrolling a folder of 5,000 files at 2,400 points a second for six seconds (2026-11-23, one run of each, the folder made by `scripts/seed-stress.sh`):

| View | Frames | Frame time p50 / p99 / max | Slow frames | Building elements |
|---|---|---|---|---|
| Grid | 360 (59.8 fps) | 16.67 / 18.66 / 33.62 ms | 1 | 0.79 ms a frame |
| List | 361 (60.0 fps) | 16.67 / 18.53 / 18.71 ms | 0 | 0.72 ms a frame |

That is the display's limit. "Building elements" is only the app's own share of a frame (constructing the window and the visible rows); gpui's layout and painting were not timed separately, and a 120 Hz display has not been tried. Colours are one `Theme` constant in [`theme.rs`](../../crates/ludy-app/src/theme.rs); there is no dark variant yet, and no thumbnails.
