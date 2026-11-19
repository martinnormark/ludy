//! Attaching a drive to the Finder: a loopback NFSv3 server in this process,
//! mounted with `/sbin/mount_nfs` onto a folder the user owns.
//!
//! Lessons from the spike that shaped this file:
//! * A plain `umount` of a browsable NFS volume can block forever inside the
//!   kernel's `unmount` call if another process is using the volume, and a
//!   thread stuck there cannot be killed. So eject always force-unmounts,
//!   from a child process, under a watchdog; this process never calls
//!   `unmount` itself.
//! * The NFS server keeps serving until the unmount has finished.
//! * The mount table is read with `getmntinfo(MNT_NOWAIT)`; nothing here
//!   stats a mount point, because stat on a half-dead mount blocks too.

use std::ffi::CStr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use nfsserve::tcp::{NFSTcp, NFSTcpListener};
use tokio::process::Command;

use crate::drive::DriveFs;
use crate::nfs::NfsDrive;

#[derive(Clone, Debug, PartialEq)]
pub struct MountEntry {
    pub from: String,
    pub on: PathBuf,
    pub fstype: String,
}

/// The kernel's mount table, without touching any of the mounted volumes.
pub fn mount_table() -> Vec<MountEntry> {
    let mut out = Vec::new();
    let mut ptr: *mut libc::statfs = std::ptr::null_mut();
    // SAFETY: getmntinfo hands back a pointer to an array of `n` statfs
    // structs that it owns; we only read from it before the next call.
    let n = unsafe { libc::getmntinfo(&mut ptr, libc::MNT_NOWAIT) };
    if n <= 0 || ptr.is_null() {
        return out;
    }
    for i in 0..n as isize {
        // SAFETY: `i` is within the `n` entries returned above, and the
        // name fields are NUL-terminated C strings.
        let (from, on, fstype) = unsafe {
            let s = &*ptr.offset(i);
            (
                CStr::from_ptr(s.f_mntfromname.as_ptr()).to_string_lossy().into_owned(),
                CStr::from_ptr(s.f_mntonname.as_ptr()).to_string_lossy().into_owned(),
                CStr::from_ptr(s.f_fstypename.as_ptr()).to_string_lossy().into_owned(),
            )
        };
        out.push(MountEntry { from, on: PathBuf::from(on), fstype });
    }
    out
}

pub fn is_mounted(mountpoint: &Path) -> bool {
    mount_table().iter().any(|m| m.on == mountpoint)
}

/// Loopback NFS mounts under `root` (leftovers if no server of ours owns them).
pub fn ludy_mounts(root: &Path) -> Vec<MountEntry> {
    mount_table().into_iter().filter(|m| m.fstype == "nfs" && m.from.starts_with(&format!("{MOUNT_HOST}:/")) && m.on.starts_with(root)).collect()
}

/// Start serving `fs` over NFSv3 on a free loopback port.
pub async fn serve(fs: Arc<DriveFs>, read_only: bool) -> Result<(u16, tokio::task::JoinHandle<()>)> {
    let export = fs.name.clone();
    let mut listener = NFSTcpListener::bind("127.0.0.1:0", NfsDrive::new(fs, read_only)).await.context("bind NFS listener")?;
    listener.with_export_name(&export);
    let port = listener.get_listen_port();
    let task = tokio::spawn(async move {
        let _ = listener.handle_forever().await;
    });
    Ok((port, task))
}

/// Run a short helper command, giving up (and abandoning it) after `limit`.
async fn run_child(program: &str, args: &[&std::ffi::OsStr], limit: Duration) -> Result<()> {
    let mut child = Command::new(program).args(args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn()?;
    match tokio::time::timeout(limit, child.wait()).await {
        Ok(Ok(status)) if status.success() => Ok(()),
        Ok(Ok(status)) => bail!("{program} exited with {status}"),
        Ok(Err(e)) => Err(e.into()),
        Err(_) => bail!("{program} did not finish within {limit:?}; the path may be a stuck mount point"),
    }
}

/// Remove an empty mount-point folder, from a child process.
pub async fn remove_mountpoint(mountpoint: &Path) {
    if !is_mounted(mountpoint) {
        let _ = run_child("/bin/rmdir", &[mountpoint.as_os_str()], Duration::from_secs(5)).await;
    }
}

/// Ejects whose `umount` never returned, remembered across app restarts so
/// the same path is not touched while the kernel is still stuck on it.
/// (A half-unmounted volume no longer shows in the mount table.)
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct StuckEject {
    pub mountpoint: PathBuf,
    pub umount_pid: u32,
}

fn pid_alive(pid: u32) -> bool {
    // SAFETY: kill with signal 0 only checks that the process exists.
    pid != 0 && unsafe { libc::kill(pid as libc::pid_t, 0) } == 0
}

/// Read the stuck-eject record, dropping entries whose `umount` has exited.
pub fn load_stuck(state_file: &Path) -> Vec<StuckEject> {
    let all: Vec<StuckEject> = std::fs::read(state_file).ok().and_then(|raw| serde_json::from_slice(&raw).ok()).unwrap_or_default();
    let live: Vec<StuckEject> = all.iter().filter(|s| pid_alive(s.umount_pid)).cloned().collect();
    if live.len() != all.len() {
        save_stuck(state_file, &live);
    }
    live
}

pub fn save_stuck(state_file: &Path, list: &[StuckEject]) {
    if let Some(dir) = state_file.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(state_file, serde_json::to_vec_pretty(list).unwrap_or_default());
}

pub fn record_stuck(state_file: &Path, mountpoint: &Path, umount_pid: u32) {
    let mut list = load_stuck(state_file);
    list.retain(|s| s.mountpoint != mountpoint);
    list.push(StuckEject { mountpoint: mountpoint.to_path_buf(), umount_pid });
    save_stuck(state_file, &list);
}

#[derive(Clone, Debug)]
pub struct AttachOptions {
    pub read_only: bool,
    /// Show the volume in the Finder sidebar and on the desktop.
    pub browse: bool,
}

/// The name the volume's server gets in the Finder sidebar. macOS resolves any
/// `*.localhost` name to the loopback address without configuration.
pub const MOUNT_HOST: &str = "ludy.localhost";

pub fn mount_options(port: u16, opts: &AttachOptions) -> String {
    let mut o = vec![
        // No lock daemon on our side; NFSv3 over TCP on our port.
        "nolocks".to_string(),
        "vers=3".into(),
        "tcp".into(),
        format!("port={port}"),
        format!("mountport={port}"),
        // 256 KiB reads match the cache block size; the kernel reads up to
        // 8 of them ahead for sequential access.
        "rsize=262144".into(),
        "wsize=262144".into(),
        "readahead=8".into(),
        // If this process dies, fail operations instead of hanging the
        // Finder, and let the kernel drop the mount after a minute.
        "soft".into(),
        "intr".into(),
        "timeo=100".into(),
        "retrans=3".into(),
        "deadtimeout=45".into(),
        // Re-check attributes every second so remote changes show up quickly.
        "actimeo=1".into(),
    ];
    if opts.read_only {
        o.push("rdonly".into());
    }
    if !opts.browse {
        o.push("nobrowse".into());
    }
    o.join(",")
}

/// Mount the export served on `port` at `mountpoint`.
pub async fn attach(port: u16, export: &str, mountpoint: &Path, opts: &AttachOptions) -> Result<()> {
    if is_mounted(mountpoint) {
        bail!("{} is already a mount point", mountpoint.display());
    }
    // Created by a child process: if the path is a half-unmounted mount point
    // the call blocks in the kernel, and that must not happen to a thread of ours.
    run_child("/bin/mkdir", &["-p".as_ref(), mountpoint.as_os_str()], Duration::from_secs(5))
        .await
        .with_context(|| format!("create {}", mountpoint.display()))?;
    let mut child = Command::new("/sbin/mount_nfs")
        .arg("-o")
        .arg(mount_options(port, opts))
        .arg(format!("{MOUNT_HOST}:/{export}"))
        .arg(mountpoint)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("run /sbin/mount_nfs")?;
    let mut stderr = child.stderr.take();
    match tokio::time::timeout(Duration::from_secs(20), child.wait()).await {
        Ok(Ok(status)) if status.success() => Ok(()),
        Ok(Ok(status)) => {
            let mut msg = String::new();
            if let Some(mut e) = stderr.take() {
                use tokio::io::AsyncReadExt;
                let _ = e.read_to_string(&mut msg).await;
            }
            bail!("mount_nfs failed ({status}): {}", msg.trim())
        }
        Ok(Err(e)) => Err(anyhow!("mount_nfs: {e}")),
        Err(_) => {
            let _ = child.kill().await;
            bail!("mount_nfs did not finish within 20 s")
        }
    }
}

pub enum Detach {
    Done,
    /// `umount` is still running after the watchdog expired. It is left
    /// alone (it cannot be killed while inside the kernel). The caller must
    /// keep the NFS server up, must not reuse the mount point, and can await
    /// the child to learn when the unmount finally completes.
    ///
    /// The known way to get here: another app touched the volume and macOS
    /// is holding its call on a "would like to access files on a network
    /// volume" prompt. The unmount finishes once the prompt is answered.
    Stuck(tokio::process::Child),
}

/// Force-unmount `mountpoint` from a child process under a watchdog.
pub async fn detach(mountpoint: &Path, watchdog: Duration) -> Result<Detach> {
    if !is_mounted(mountpoint) {
        return Ok(Detach::Done);
    }
    let mut child = Command::new("/sbin/umount")
        .arg("-f")
        .arg(mountpoint)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("run /sbin/umount")?;
    match tokio::time::timeout(watchdog, child.wait()).await {
        Ok(Ok(status)) if status.success() => Ok(Detach::Done),
        Ok(Ok(status)) => {
            let mut msg = String::new();
            if let Some(mut e) = child.stderr.take() {
                use tokio::io::AsyncReadExt;
                let _ = e.read_to_string(&mut msg).await;
            }
            if !is_mounted(mountpoint) {
                return Ok(Detach::Done);
            }
            bail!("umount failed ({status}): {}", msg.trim())
        }
        Ok(Err(e)) => Err(anyhow!("umount: {e}")),
        Err(_) => Ok(Detach::Stuck(child)),
    }
}

/// Wait until the volume has seen no filesystem calls for `quiet`, giving up
/// after `max`. Ejecting while something is crawling the volume is what the
/// spike showed to be dangerous, so give crawlers a moment to finish.
pub async fn wait_quiet(fs: &DriveFs, quiet: Duration, max: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + max;
    loop {
        let last = fs.last_activity_ms.load(std::sync::atomic::Ordering::Relaxed);
        if ludy_cloud::now_ms().saturating_sub(last) >= quiet.as_millis() as u64 {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mount_table_lists_the_root_volume() {
        let table = mount_table();
        assert!(table.iter().any(|m| m.on == Path::new("/")), "no / in {table:?}");
    }

    #[test]
    fn stuck_ejects_are_remembered_only_while_umount_is_alive() {
        let dir = std::env::temp_dir().join(format!("ludy-stuck-test-{}", std::process::id()));
        let file = dir.join("stuck.json");
        // Our own pid is certainly alive; pid 0x7fff_fff0 certainly is not.
        record_stuck(&file, Path::new("/tmp/ludy-a"), std::process::id());
        record_stuck(&file, Path::new("/tmp/ludy-b"), 0x7fff_fff0);
        let live = load_stuck(&file);
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].mountpoint, Path::new("/tmp/ludy-a"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn options_reflect_flags() {
        let o = mount_options(4242, &AttachOptions { read_only: true, browse: false });
        assert!(o.contains("port=4242") && o.contains("mountport=4242"));
        assert!(o.contains("rdonly") && o.contains("nobrowse") && o.contains("soft"));
        let o = mount_options(1, &AttachOptions { read_only: false, browse: true });
        assert!(!o.contains("rdonly") && !o.contains("nobrowse"));
    }
}
