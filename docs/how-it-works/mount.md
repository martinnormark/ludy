# The mount: loopback NFSv3

Each mounted drive is an NFSv3 server inside the app's own process, attached with `/sbin/mount_nfs`. It needs no driver, no kernel extension and no root. The second half of this page is about one hazard: an eject can hang behind a macOS permission prompt, and what Ludy does about that.

## The mechanism

Each mounted drive is an NFSv3 server inside the app's own process (the [`nfsserve`](https://crates.io/crates/nfsserve) crate), bound to a random port on 127.0.0.1 and attached with `/sbin/mount_nfs`. This needs no driver, no kernel extension and no root. The mount source is `ludy.localhost:/<drive>`: macOS resolves any `*.localhost` name to the loopback address, and the Finder sidebar shows that name instead of `127.0.0.1`.

## Why this one

| Option | Verdict |
|---|---|
| **Loopback NFSv3** | Chosen. macOS's NFS client turns an application's `read` into NFS READs for just those bytes, so range streaming works. A non-root user can mount onto a folder they own. |
| macFUSE / FUSE-T | Not installed on this machine, and installing either needs an admin password. |
| FSKit (what Space's Mac app uses) | Needs a signed app extension with an Apple entitlement, which the user then enables in System Settings. Not possible for an unsigned local build. |
| Loopback SMB (what Space's CLI uses) | Would need an SMB2/3 server written from scratch. |
| WebDAV (`mount_webdav`) | macOS's WebDAV client downloads a file to a local cache when it is opened, which defeats the purpose for a 50 GB file. Not tested here; ruled out on that basis. |

A throwaway spike confirmed this before anything else was built, and the measurements against the real code are under [what works](../evidence/verification.md#against-real-mounts): a listing fetches no content, and a 64 KiB read in the middle of a 24 MiB file causes one range request.

Mount options are in [`mount.rs`](../../crates/ludy-core/src/mount.rs): 256 KiB reads to match the cache block size, kernel read-ahead of 8 blocks, a one-second attribute cache so remote changes show up quickly, and a soft mount with a dead timeout so a crashed app cannot hang the Finder for long.

## Eject and the "network volume" prompt

During the spike a plain `umount` blocked forever inside the kernel's `unmount` call and could not be killed. While it was stuck, `diskarbitrationd`, `lsd`, Finder and Raycast each had a thread blocked on the half-unmounted mount point.

The cause, as far as it can be established without kernel stacks: macOS asks for consent the first time an app touches a network volume (*"Raycast" would like to access files on a network volume*). Raycast's file indexer started crawling the new volume under the home folder, and the consent daemon's log shows it began prompting for Raycast seven seconds before the `umount`. Raycast's `lstat` and `getxattr` calls sat suspended on that prompt, which was still on screen an hour later, and the unmount waited behind them. Apple's NFS client has several waits without a timeout on the unmount path; which one was hit is not known. Answering the prompt released it: the consent log shows the answer at 07:02:26 the next morning, and the `umount` and the blocked threads were gone without a reboot. Nine mount and eject cycles since then, hidden and Finder-visible, were clean, each eject taking one to two seconds.

What Ludy does about it:

- Eject always force-unmounts, from a **child process** under a watchdog. The app never calls `unmount` itself, so it cannot become unkillable.
- Before unmounting it uploads pending writes and waits for the volume to go quiet.
- The NFS server keeps serving until the unmount has finished.
- If the watchdog expires, the drive is shown as "eject did not finish" with a hint to answer any pending permission prompt, and the eject completes by itself once the kernel lets it through. A force-unmount probably cannot beat a call held by a prompt; the watchdog is the real protection.
- A stuck eject is recorded on disk with the `umount` process id. While that process is alive, Ludy refuses to mount at the same path, even after a restart.
- The mount table is read with `getmntinfo(MNT_NOWAIT)`. The app's own process never stats, creates or removes a mount-point path; `mkdir` and `rmdir` run as child processes.
- If the app is killed outright, the mounts are soft with a 45-second dead timeout. The kernel starts that clock when something next asks the dead mount for anything: after `kill -9`, an `ls` of the mount failed with "Operation timed out" instead of hanging, and the kernel dropped the mount 50 to 57 seconds after it (seen three times). A dead mount that nothing touches stays in the mount table; one was still there three minutes later (2026-11-23). The app unmounts such leftovers under its mount root when it next starts.

If you ever see a stuck eject: look for a macOS permission prompt and answer it. Do not `ls -la` the mount root in the meantime.
