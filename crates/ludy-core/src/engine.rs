//! The facade the app and the CLI talk to: drives, mounts, cache and live
//! status, on its own tokio runtime.

use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, bail, Result};

use crate::cache::{BlockCache, CacheSnapshot};
use crate::fs::Pending;
use crate::keystore::{self, Keystore};
use crate::mount::{self, AttachOptions, Detach};
use crate::sealed_client::SealedClient;
use crate::sealed_drive::{self, SealedDrive};
use crate::stats::{Stats, StatsSnapshot};

/// Where `docker compose up` at the top of the repository answers.
pub const DEFAULT_SERVER: &str = "http://127.0.0.1:8080";

#[derive(Clone, Debug)]
pub struct EngineConfig {
    pub server_url: String,
    pub cache_dir: PathBuf,
    pub cache_cap: u64,
    pub journal_dir: PathBuf,
    /// The file that holds what opens each drive; see [`crate::keystore`].
    pub keystore: PathBuf,
    /// Drives mount at `<mount_root>/<drive>`.
    pub mount_root: PathBuf,
    pub read_only: bool,
    pub browse: bool,
    /// Where ejects that never finished are remembered.
    pub stuck_file: PathBuf,
}

impl EngineConfig {
    /// Defaults, overridable through `LUDY_SERVER`, `LUDY_CACHE_MB`,
    /// `LUDY_CACHE_DIR`, `LUDY_MOUNT_ROOT`, `LUDY_HOME`, `LUDY_READ_ONLY`
    /// and `LUDY_NOBROWSE`.
    pub fn from_env() -> EngineConfig {
        let home = PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/tmp".into()));
        let state = std::env::var("LUDY_HOME").map(PathBuf::from).unwrap_or_else(|_| home.join("Library/Application Support/Ludy"));
        let cache_mb = std::env::var("LUDY_CACHE_MB").ok().and_then(|s| s.parse::<u64>().ok()).unwrap_or(2000);
        EngineConfig {
            server_url: std::env::var("LUDY_SERVER").unwrap_or_else(|_| DEFAULT_SERVER.into()).trim_end_matches('/').to_string(),
            cache_dir: std::env::var("LUDY_CACHE_DIR").map(PathBuf::from).unwrap_or_else(|_| home.join("Library/Caches/Ludy/blocks")),
            cache_cap: cache_mb * 1_000_000,
            journal_dir: state.join("journal"),
            keystore: state.join("drives.json"),
            stuck_file: state.join("stuck-ejects.json"),
            mount_root: std::env::var("LUDY_MOUNT_ROOT").map(PathBuf::from).unwrap_or_else(|_| home.join("Ludy")),
            read_only: std::env::var("LUDY_READ_ONLY").is_ok(),
            browse: std::env::var("LUDY_NOBROWSE").is_err(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum MountState {
    Mounted,
    /// The unmount did not finish; the volume is still being served.
    Stuck,
}

pub struct Mounted {
    pub drive: String,
    pub mountpoint: PathBuf,
    pub port: u16,
    pub fs: Arc<SealedDrive>,
    state: Mutex<MountState>,
    tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

/// A drive whose tree is in memory: being browsed, mounted, or both. Its
/// uploader and its poller run for as long as it is open.
struct Open {
    fs: Arc<SealedDrive>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
    /// Held for as long as the drive is open; see [`lock_journal`].
    _lock: std::fs::File,
}

/// Two processes must not work on one drive's journal: each would send, and
/// then remove, what the other wrote. So whoever opens a drive holds a lock
/// on a file beside its journal, which the system lets go of when the
/// process ends, however it ends.
fn lock_journal(journal: &std::path::Path, drive: &str) -> Result<std::fs::File> {
    use std::os::fd::AsRawFd;
    if let Some(dir) = journal.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let file = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(journal.with_extension("lock"))?;
    // SAFETY: `flock` on a descriptor this function owns.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        bail!("{drive} is open in another Ludy process on this Mac (the app, or `ludy mount`); use that one, or quit it first");
    }
    Ok(file)
}

/// A drive this Mac holds the key of, on the server it is talking to.
#[derive(Clone, Debug, PartialEq)]
pub struct DriveInfo {
    pub name: String,
    /// The drive's id on the server, 32 hex characters.
    pub id: String,
    /// Files and their bytes, known once the drive has been opened.
    pub totals: Option<(u64, u64)>,
    /// Changes whenever the drive's tree may have: for views that list it.
    pub seq: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct MountView {
    pub mountpoint: PathBuf,
    pub state: MountState,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DriveView {
    pub info: DriveInfo,
    pub mount: Option<MountView>,
    /// What an open drive still has to send.
    pub pending: Option<Pending>,
    /// "Mounting…" / "Ejecting…" while an operation is in flight.
    pub busy: Option<String>,
}

/// Everything the UI shows, refreshed twice a second.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct View {
    pub online: bool,
    pub server_url: String,
    /// What the server calls itself (`ludy-rivet`, `ludy-worker`), once it has answered.
    pub server_name: Option<String>,
    pub drives: Vec<DriveView>,
    pub stats: StatsSnapshot,
    pub cache: CacheSnapshot,
    pub notice: Option<String>,
}

struct Inner {
    cfg: EngineConfig,
    client: SealedClient,
    keystore: Keystore,
    cache: Arc<BlockCache>,
    stats: Arc<Stats>,
    open: Mutex<HashMap<String, Arc<Open>>>,
    /// One drive is opened at a time, so a drive is never opened twice.
    opening: tokio::sync::Mutex<()>,
    mounts: Mutex<HashMap<String, Arc<Mounted>>>,
    busy: Mutex<HashMap<String, String>>,
    server: Mutex<(bool, Option<String>)>,
    notice: Mutex<Option<String>>,
    view: Mutex<Arc<View>>,
}

pub struct Engine {
    rt: tokio::runtime::Runtime,
    inner: Arc<Inner>,
}

impl Engine {
    pub fn new(cfg: EngineConfig) -> Result<Engine> {
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(4).thread_name("ludy-engine").enable_all().build()?;
        let stats = Arc::new(Stats::default());
        let cache = Arc::new(BlockCache::open(&cfg.cache_dir, cfg.cache_cap)?);
        let client = SealedClient::new(&cfg.server_url, stats.clone());
        let keystore = Keystore::open(&cfg.keystore, &cfg.server_url)?;
        let inner = Arc::new(Inner {
            client,
            keystore,
            cache,
            stats,
            open: Mutex::new(HashMap::new()),
            opening: tokio::sync::Mutex::new(()),
            mounts: Mutex::new(HashMap::new()),
            busy: Mutex::new(HashMap::new()),
            server: Mutex::new((false, None)),
            notice: Mutex::new(None),
            view: Mutex::new(Arc::new(View { server_url: cfg.server_url.clone(), ..Default::default() })),
            cfg,
        });
        // Mounts left behind by a run that crashed have no server any more.
        let stale = mount::ludy_mounts(&inner.cfg.mount_root);
        if !stale.is_empty() {
            let names: Vec<String> = stale.iter().map(|m| m.on.display().to_string()).collect();
            inner.set_notice(format!("Cleaning up {} stale mount(s): {}", stale.len(), names.join(", ")));
            rt.spawn(async move {
                for m in stale {
                    let _ = mount::detach(&m.on, Duration::from_secs(20)).await;
                }
            });
        }
        rt.spawn(inner.clone().refresher());
        Ok(Engine { rt, inner })
    }

    pub fn config(&self) -> &EngineConfig {
        &self.inner.cfg
    }

    pub fn cache(&self) -> Arc<BlockCache> {
        self.inner.cache.clone()
    }

    /// The engine's counters as they are right now; [`Engine::view`] lags by
    /// up to half a second.
    pub fn stats(&self) -> Arc<Stats> {
        self.inner.stats.clone()
    }

    pub fn view(&self) -> Arc<View> {
        self.inner.view.lock().unwrap().clone()
    }

    /// Run a future on the engine's runtime. The handle can be awaited from
    /// any executor (gpui's included).
    pub fn spawn<F>(&self, fut: F) -> tokio::task::JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        self.rt.spawn(fut)
    }

    /// Run blocking work (copying files into a drive's journal) off the
    /// engine's async threads.
    pub fn spawn_blocking<F, R>(&self, work: F) -> tokio::task::JoinHandle<R>
    where
        F: FnOnce() -> R + Send + 'static,
        R: Send + 'static,
    {
        self.rt.spawn_blocking(work)
    }

    pub fn block_on<F: Future>(&self, fut: F) -> F::Output {
        self.rt.block_on(fut)
    }

    pub fn mounted(&self, drive: &str) -> Option<Arc<Mounted>> {
        self.inner.mounts.lock().unwrap().get(drive).cloned()
    }

    pub fn mountpoint(&self, drive: &str) -> PathBuf {
        self.inner.cfg.mount_root.join(drive)
    }

    pub fn set_notice(&self, text: impl Into<String>) {
        self.inner.set_notice(text.into());
    }

    pub fn clear_notice(&self) {
        *self.inner.notice.lock().unwrap() = None;
    }

    /// The drives this Mac holds a key for on the configured server.
    pub fn drives(&self) -> Vec<DriveInfo> {
        self.inner.drive_infos()
    }

    /// What the server calls itself, or why it does not answer.
    pub async fn hello(&self) -> Result<String> {
        let inner = self.inner.clone();
        self.rt.spawn(async move { inner.client.hello().await }).await?
    }

    /// Create an empty drive on the server and keep its key here.
    pub async fn create_drive(&self, name: &str) -> Result<()> {
        let (inner, name) = (self.inner.clone(), name.to_string());
        self.rt
            .spawn(async move {
                if !keystore::valid_drive_name(&name) {
                    bail!("{name:?} cannot be a drive's name: no slash or colon, no leading dot, at most 64 bytes");
                }
                if inner.keystore.get(&name).is_some() {
                    bail!("there is already a drive called {name}");
                }
                let keys = sealed_drive::create_drive(&inner.client, &name).await?;
                inner.keystore.add(&name, &keys)?;
                inner.rebuild_view();
                Ok(())
            })
            .await?
    }

    /// A drive's key as one line of text; see [`Keystore::export`].
    pub fn export_drive(&self, name: &str) -> Result<String> {
        self.inner.keystore.export(name)
    }

    /// Add a drive made on another Mac from its exported key. Without a
    /// name it gets the one it was created with. Returns the name.
    pub async fn import_drive(&self, key: &str, name: Option<&str>) -> Result<String> {
        let (inner, key, name) = (self.inner.clone(), key.to_string(), name.map(str::to_string));
        self.rt
            .spawn(async move {
                let (keys, created_as) = keystore::import(&inner.client, &key).await?;
                let name = name.unwrap_or(created_as);
                inner.keystore.add(&name, &keys)?;
                inner.rebuild_view();
                Ok(name)
            })
            .await?
    }

    /// Give a drive another name on this Mac.
    pub async fn rename_drive(&self, from: &str, to: &str) -> Result<()> {
        let (inner, from, to) = (self.inner.clone(), from.to_string(), to.to_string());
        self.rt
            .spawn(async move {
                if inner.mounts.lock().unwrap().contains_key(&from) {
                    bail!("eject {from} before renaming it");
                }
                inner.close(&from, Duration::from_secs(60)).await?;
                inner.keystore.rename(&from, &to)?;
                inner.rebuild_view();
                Ok(())
            })
            .await?
    }

    /// Forget a drive's key on this Mac. The drive stays on the server,
    /// where nobody without the key can read it; there is no request that
    /// deletes a drive. Refused while the drive has writes that have not
    /// reached the server, or while the server cannot be asked.
    pub async fn forget_drive(&self, name: &str) -> Result<()> {
        let (inner, name) = (self.inner.clone(), name.to_string());
        self.rt
            .spawn(async move {
                if inner.mounts.lock().unwrap().contains_key(&name) {
                    bail!("eject {name} before removing it");
                }
                // Without its key, writes still in the journal could never
                // be sent: open the drive, so that what an earlier run left
                // is found too, and keep the key unless everything went.
                if inner.keystore.get(&name).is_none() {
                    bail!("no drive called {name}");
                }
                match inner.open(&name).await {
                    Ok(fs) => {
                        if !fs.flush(Duration::from_secs(60)).await {
                            let p = fs.pending();
                            bail!("{name} still has {} file(s) and {} change(s) to send; its key is kept until they are sent", p.files, p.ops);
                        }
                    }
                    Err(e) => bail!("{name} cannot be opened to check for unsent writes, so its key is kept: {e:#}"),
                }
                inner.close(&name, Duration::from_secs(5)).await?;
                inner.keystore.remove(&name)?;
                inner.rebuild_view();
                Ok(())
            })
            .await?
    }

    /// Open a drive without mounting it: its tree is read and kept current,
    /// and what is written to it uploads in the background.
    pub async fn open(&self, drive: &str) -> Result<Arc<SealedDrive>> {
        let (inner, drive) = (self.inner.clone(), drive.to_string());
        self.rt.spawn(async move { inner.open(&drive).await }).await?
    }

    /// A drive that is already open, without waiting for anything.
    pub fn opened(&self, drive: &str) -> Option<Arc<SealedDrive>> {
        self.inner.open.lock().unwrap().get(drive).map(|o| o.fs.clone())
    }

    /// Mount a drive in the background; progress shows up in [`Engine::view`].
    pub fn mount(&self, drive: &str) {
        let (inner, drive) = (self.inner.clone(), drive.to_string());
        self.rt.spawn(async move {
            inner.set_busy(&drive, Some("Mounting…"));
            if let Err(e) = inner.mount(&drive).await {
                inner.set_notice(format!("Could not mount {drive}: {e:#}"));
            }
            inner.set_busy(&drive, None);
            inner.rebuild_view();
        });
    }

    /// Eject a drive in the background.
    pub fn eject(&self, drive: &str) {
        let (inner, drive) = (self.inner.clone(), drive.to_string());
        self.rt.spawn(async move {
            inner.set_busy(&drive, Some("Ejecting…"));
            if let Err(e) = inner.eject(&drive).await {
                inner.set_notice(format!("Could not eject {drive}: {e:#}"));
            }
            inner.set_busy(&drive, None);
            inner.rebuild_view();
        });
    }

    pub async fn mount_now(&self, drive: &str) -> Result<Arc<Mounted>> {
        let (inner, drive) = (self.inner.clone(), drive.to_string());
        self.rt.spawn(async move { inner.mount(&drive).await }).await?
    }

    pub async fn eject_now(&self, drive: &str) -> Result<()> {
        let (inner, drive) = (self.inner.clone(), drive.to_string());
        self.rt.spawn(async move { inner.eject(&drive).await }).await?
    }

    /// Eject everything, briefly (called when the app quits or is signalled),
    /// then send what open drives still hold. Writes that cannot be uploaded
    /// within a few seconds stay in the journal for the next start.
    pub fn shutdown(&self) {
        let drives: Vec<String> = self.inner.mounts.lock().unwrap().keys().cloned().collect();
        let inner = self.inner.clone();
        self.rt.block_on(async move {
            for d in drives {
                let _ = inner.eject_with(&d, true).await;
            }
            let open: Vec<String> = inner.open.lock().unwrap().keys().cloned().collect();
            for d in open {
                if !inner.mounts.lock().unwrap().contains_key(&d) {
                    let _ = inner.close(&d, Duration::from_secs(15)).await;
                }
            }
        });
    }

    /// Block until SIGINT or SIGTERM arrives.
    pub fn wait_for_signal(&self) {
        self.rt.block_on(async {
            use tokio::signal::unix::{signal, SignalKind};
            let (Ok(mut int), Ok(mut term)) = (signal(SignalKind::interrupt()), signal(SignalKind::terminate())) else { return };
            tokio::select! {
                _ = int.recv() => {}
                _ = term.recv() => {}
            }
        });
    }

    /// Ask the refresher to look at the server and the drives right away.
    pub fn refresh_drives(&self) {
        let inner = self.inner.clone();
        self.rt.spawn(async move {
            inner.ping().await;
            inner.rebuild_view();
        });
    }
}

impl Mounted {
    pub fn state(&self) -> MountState {
        self.state.lock().unwrap().clone()
    }
}

impl Inner {
    fn set_notice(&self, text: String) {
        *self.notice.lock().unwrap() = Some(text);
    }

    fn set_busy(&self, drive: &str, what: Option<&str>) {
        let mut busy = self.busy.lock().unwrap();
        match what {
            Some(w) => busy.insert(drive.to_string(), w.to_string()),
            None => busy.remove(drive),
        };
        drop(busy);
        self.rebuild_view();
    }

    /// Open a drive, or hand back the one that is open.
    async fn open(self: &Arc<Self>, drive: &str) -> Result<Arc<SealedDrive>> {
        let _one_at_a_time = self.opening.lock().await;
        if let Some(open) = self.open.lock().unwrap().get(drive) {
            return Ok(open.fs.clone());
        }
        let saved = self.keystore.get(drive).ok_or_else(|| anyhow!("this Mac holds no key for a drive called {drive} on {}", self.cfg.server_url))?;
        // The journal is kept by the drive's id: a name is only this Mac's.
        let journal = self.cfg.journal_dir.join(&saved.keys.drive);
        let lock = lock_journal(&journal, drive)?;
        let fs = SealedDrive::open(drive, saved.keys, self.client.clone(), self.cache.clone(), self.stats.clone(), journal)?;
        if let Err(e) = fs.refresh().await {
            fs.stop();
            return Err(anyhow!("cannot list drive: {e:#}"));
        }
        let tasks = vec![tokio::spawn(fs.clone().run_uploader()), tokio::spawn(fs.clone().run_poller())];
        self.open.lock().unwrap().insert(drive.to_string(), Arc::new(Open { fs: fs.clone(), tasks, _lock: lock }));
        Ok(fs)
    }

    /// Send what an open drive still holds, for at most `patience`, and let
    /// go of it. What could not be sent stays in its journal.
    async fn close(&self, drive: &str, patience: Duration) -> Result<()> {
        let _one_at_a_time = self.opening.lock().await;
        let Some(open) = self.open.lock().unwrap().get(drive).cloned() else { return Ok(()) };
        let sent = open.fs.flush(patience).await;
        for t in &open.tasks {
            t.abort();
        }
        open.fs.stop();
        self.open.lock().unwrap().remove(drive);
        if !sent {
            let p = open.fs.pending();
            self.set_notice(format!("{drive}: {} file(s) not uploaded yet; they are kept on this Mac and sent when the drive is next opened", p.files));
        }
        Ok(())
    }

    async fn mount(self: &Arc<Self>, drive: &str) -> Result<Arc<Mounted>> {
        if self.mounts.lock().unwrap().contains_key(drive) {
            bail!("already mounted");
        }
        let mountpoint = self.cfg.mount_root.join(drive);
        if let Some(stuck) = mount::load_stuck(&self.cfg.stuck_file).into_iter().find(|s| s.mountpoint == mountpoint) {
            bail!(
                "an earlier eject of {} is still stuck in the kernel (umount pid {}). Answer any macOS permission prompt for that volume, or restart the Mac",
                mountpoint.display(),
                stuck.umount_pid
            );
        }
        let fs = self.open(drive).await?;
        let (port, nfs_task) = mount::serve(fs.clone(), self.cfg.read_only).await?;
        let opts = AttachOptions { read_only: self.cfg.read_only, browse: self.cfg.browse };
        if let Err(e) = mount::attach(port, drive, &mountpoint, &opts).await {
            nfs_task.abort();
            return Err(e);
        }
        let mounted = Arc::new(Mounted { drive: drive.to_string(), mountpoint, port, fs, state: Mutex::new(MountState::Mounted), tasks: Mutex::new(vec![nfs_task]) });
        self.mounts.lock().unwrap().insert(drive.to_string(), mounted.clone());
        Ok(mounted)
    }

    async fn eject(self: &Arc<Self>, drive: &str) -> Result<()> {
        self.eject_with(drive, false).await
    }

    /// `quitting`: the app is going away, so be brief and unmount even if
    /// some writes could not be uploaded in time.
    async fn eject_with(self: &Arc<Self>, drive: &str, quitting: bool) -> Result<()> {
        let mounted = self.mounts.lock().unwrap().get(drive).cloned().ok_or_else(|| anyhow!("not mounted"))?;
        if mounted.state() == MountState::Stuck {
            bail!("an eject is already waiting on another app; answer any macOS permission prompt for this volume");
        }
        // Publish what is still in the journal before the volume goes away.
        let patience = Duration::from_secs(if quitting { 15 } else { 120 });
        if !mounted.fs.flush(patience).await && !quitting {
            let p = mounted.fs.pending();
            bail!("{} file(s) are still uploading{}", p.files, p.last_error.map(|e| format!(" ({e})")).unwrap_or_default());
        }
        if !quitting {
            mount::wait_quiet(&mounted.fs, Duration::from_millis(1500), Duration::from_secs(8)).await;
        }
        match mount::detach(&mounted.mountpoint, Duration::from_secs(if quitting { 8 } else { 25 })).await? {
            Detach::Done => {}
            Detach::Stuck(mut child) => {
                // Keep serving the volume, and finish the job whenever the
                // kernel lets the unmount through.
                *mounted.state.lock().unwrap() = MountState::Stuck;
                mount::record_stuck(&self.cfg.stuck_file, &mounted.mountpoint, child.id().unwrap_or(0));
                let (this, mounted, drive) = (self.clone(), mounted.clone(), drive.to_string());
                tokio::spawn(async move {
                    let _ = child.wait().await;
                    if !mount::is_mounted(&mounted.mountpoint) {
                        this.finish_eject(&drive, &mounted).await;
                        this.set_notice(format!("{drive} has been ejected."));
                        this.rebuild_view();
                    }
                });
                bail!(
                    "the unmount is waiting on another app. If macOS is showing a \"would like to access files on a network volume\" prompt, answer it and the eject will complete"
                );
            }
        }
        self.finish_eject(drive, &mounted).await;
        Ok(())
    }

    async fn finish_eject(&self, drive: &str, mounted: &Mounted) {
        let tasks: Vec<_> = mounted.tasks.lock().unwrap().drain(..).collect();
        for t in tasks {
            t.abort();
        }
        // The drive stays open: the app may be browsing it.
        self.mounts.lock().unwrap().remove(drive);
        // Leave no empty folder behind (checked and removed by a child process).
        mount::remove_mountpoint(&mounted.mountpoint).await;
    }

    /// Ask the server whether it is there.
    async fn ping(&self) {
        let answer = self.client.hello().await;
        let mut server = self.server.lock().unwrap();
        match answer {
            Ok(name) => *server = (true, Some(name)),
            Err(_) => server.0 = false,
        }
    }

    fn drive_infos(&self) -> Vec<DriveInfo> {
        let open = self.open.lock().unwrap();
        self.keystore
            .list()
            .into_iter()
            .map(|d| {
                let fs = open.get(&d.name).map(|o| &o.fs);
                DriveInfo { totals: fs.map(|fs| fs.totals()), seq: fs.map(|fs| fs.seq().wrapping_add(fs.sync_token())).unwrap_or(0), name: d.name, id: d.keys.drive }
            })
            .collect()
    }

    /// What open drives have to tell the person: a file kept under another
    /// name, a change that had to be dropped.
    fn collect_notices(&self) {
        let open: Vec<(String, Arc<SealedDrive>)> = self.open.lock().unwrap().iter().map(|(name, o)| (name.clone(), o.fs.clone())).collect();
        for (name, fs) in open {
            let notices = fs.take_notices();
            if let Some(last) = notices.last() {
                let more = if notices.len() > 1 { format!(" (and {} more)", notices.len() - 1) } else { String::new() };
                self.set_notice(format!("{name}: {last}{more}"));
            }
        }
    }

    fn rebuild_view(&self) {
        let infos = self.drive_infos();
        let (online, server_name) = self.server.lock().unwrap().clone();
        let open = self.open.lock().unwrap();
        let mounts = self.mounts.lock().unwrap();
        let busy = self.busy.lock().unwrap();
        let drives = infos
            .into_iter()
            .map(|info| {
                let mount = mounts.get(&info.name).map(|m| MountView { mountpoint: m.mountpoint.clone(), state: m.state() });
                let pending = open.get(&info.name).map(|o| o.fs.pending());
                let busy = busy.get(&info.name).cloned();
                DriveView { info, mount, pending, busy }
            })
            .collect();
        let view = View {
            online,
            server_url: self.cfg.server_url.clone(),
            server_name,
            drives,
            stats: self.stats.sample(),
            cache: self.cache.snapshot(),
            notice: self.notice.lock().unwrap().clone(),
        };
        *self.view.lock().unwrap() = Arc::new(view);
    }

    /// Keep the view fresh: counters twice a second, the server every two
    /// seconds.
    async fn refresher(self: Arc<Self>) {
        let mut tick = 0u64;
        loop {
            if tick % 4 == 0 {
                self.ping().await;
            }
            self.collect_notices();
            self.rebuild_view();
            tick += 1;
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }
}
