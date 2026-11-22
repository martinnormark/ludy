# The app

A plain file explorer written with gpui. Scrolling a folder of 5,000 files holds 60 frames a second on a 60 Hz display in the unoptimized build. There is no dark theme and there are no thumbnails.

## What it shows

A plain file explorer in a light theme: drives in the sidebar, a toolbar with the path and the actions, the folder as a grid or a list, and a status bar with the live numbers (streamed, uploads, cache). The drives screen has one card per drive with Mount, Open, Show in Finder and eject. Right-click a drive or a file for the rest (Open in Terminal, Copy path, Rename, Share, Fork, Keep offline, Versions, Delete).

## Scrolling a large folder

Folders are drawn with a virtual list, so only the rows on screen are built each frame. Measured with `--perf` on a 60 Hz display, in the unoptimized dev build, scrolling a folder of 5,000 files at 2,400 points a second for six seconds:

| View | Frames | Frame time p50 / p99 / max | Slow frames | Building elements |
|---|---|---|---|---|
| Grid | 361 (60.0 fps) | 16.67 / 18.62 / 18.68 ms | 0 | 0.79 ms a frame |
| List | 361 (60.0 fps) | 16.67 / 18.69 / 18.78 ms | 0 | 0.68 ms a frame |

That is the display's limit. "Building elements" is only the app's own share of a frame (constructing the window and the visible rows); gpui's layout and painting were not timed separately, and a 120 Hz display has not been tried. Colours are one `Theme` constant in [`theme.rs`](../../crates/ludy-app/src/theme.rs); there is no dark variant yet, and no thumbnails.
