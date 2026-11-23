# The app

A plain file explorer written with gpui, for the drives this Mac holds a key for. It browses a drive whether it is mounted or not. Scrolling a folder of 5,000 files holds 60 frames a second on a 60 Hz display in the unoptimized build. There is no dark theme and there are no thumbnails.

## What it shows

A plain file explorer in a light theme: drives in the sidebar with the server's name and address under them, a toolbar with the path and the actions, the folder as a grid or a list, and a status bar with the live numbers (streamed, uploads, cache). The drives screen has one card per drive with Mount, Open, Show in Finder and eject.

The drives are the ones this Mac holds a key for on the server it is talking to. The plus beside **Drives** creates one; the arrow beside it adds a drive from a key copied on another Mac. A card shows a drive's size once the drive has been opened, since the size is in the tree only this Mac can read.

Right-click a drive, or use the dots on its card, for Open in Terminal, Copy path, Rename, Copy drive key and Remove drive. Rename changes the name on this Mac. Remove drive forgets the key on this Mac and leaves the drive on the server. Right-click a file for Rename, Versions (the last 25, each can be restored) and Delete. Import, in the toolbar, copies files and folders from the Mac into the open folder.

The browser works on a drive whether it is mounted or not. It shows what the drive holds: the files the Finder leaves on a mounted drive for itself (`.DS_Store`, `._*`) are [not part of the drive](engine.md) and are not listed. Opening files in other applications, Show in Finder and Open in Terminal need the drive mounted.

### Names and the details pane

A name is never cut at its end, where the extension is. A tile shows a name in up to two lines, and when that is not enough the middle of the second line is left out (`a very long holiday / video from…al cut.mov`); a row of the list has one line and leaves the middle out the same way. The cut is measured in the font the name is drawn in ([`name_lines`](../../crates/ludy-app/src/theme.rs)).

Click a file once and a pane opens beside the folder with the name in full, the kind, the exact size, when it was modified, how much of it this Mac holds, where it is in the drive and in the Finder, and buttons for Open, Show in Finder and Versions. Escape or the cross closes it. The buttons are at the bottom of the pane, so they are in the same place whatever the length of the name above them. A second click opens the file; a folder opens on the first click and has no pane. How much this Mac holds reads "Not known until the file is read" for a file this session has not read, for [the reason the list shows a dash](../known-issues.md#the-engine-and-the-app).

Opening the pane moves no tile sideways and changes no tile's size. A tile's width is a whole number of points worked out from the folder's full width, pane or no pane, and the pane is as wide as a whole number of tiles and their gaps: the smallest such width of at least 260 points, which is between 300 and 380 at the window sizes tried ([`grid_fit`](../../crates/ludy-app/src/theme.rs)). The tiles left of the pane stay exactly where they were; the ones it covers go to later rows, and the grid scrolls only if that takes the selected file out of view. While the pane is open the list drops its Modified and On this Mac columns, which the pane shows, so that names keep their room.

Because the tiles the pane covers move to later rows, the second click of a double-click can land on another tile, on the pane, or on one of its buttons. For 0.6 seconds after a file is selected, a second click opens that file whatever it lands on.

Not there: sharing with people, forking a drive, keeping a file offline. The first two have nothing behind them on the server yet; see [not built](../known-issues.md#not-built).

## Scrolling a large folder

Folders are drawn with a virtual list, so only the rows on screen are built each frame. Measured with `--perf` on a 60 Hz display, in the unoptimized dev build, scrolling a folder of 5,000 files at 2,400 points a second for six seconds (2026-11-23, one run of each, the folder made by `scripts/seed-stress.sh`):

| View | Frames | Frame time p50 / p99 / max | Slow frames | Building elements |
|---|---|---|---|---|
| Grid | 360 (59.8 fps) | 16.67 / 18.66 / 33.62 ms | 1 | 0.79 ms a frame |
| List | 361 (60.0 fps) | 16.67 / 18.53 / 18.71 ms | 0 | 0.72 ms a frame |

With names of 30 to 70 characters, which are measured and cut every frame, a folder of 1,500 files held the same rate (2026-11-23, one run of each, unoptimized build): grid 60.0 fps with 1.56 ms a frame building elements, list 60.0 fps with 1.17 ms.

That is the display's limit. "Building elements" is only the app's own share of a frame (constructing the window and the visible rows); gpui's layout and painting were not timed separately, and a 120 Hz display has not been tried. Colours are one `Theme` constant in [`theme.rs`](../../crates/ludy-app/src/theme.rs); there is no dark variant yet, and no thumbnails.
