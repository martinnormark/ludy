//! The facade the app and the CLI talk to: drives, mounts, cache and live
//! status, on its own tokio runtime.

use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, bail, Result};

use crate::cache::{BlockCache, CacheSnapshot};
use crate::client::{CloudClient, DriveInfo, ServerStats};
use crate::drive::{DriveFs, Pending};
use crate::mount::{self, AttachOptions, Detach};
use crate::stats::{Stats, StatsSnapshot};

#[derive(Clone, Debug)]
pub struct EngineConfig {
    pub server_url: String,
    pub cache_dir: PathBuf,
    pub cache_cap: u64,
    pub journal_dir: PathBuf,
    /// Drives mount at `<mount_root>/<drive>`.
    pub mount_root: PathBuf,
    pub read_only: bool,
    pub browse: bool,
    /// Where ejects that never finished are remembered.
    pub stuck_file: PathBuf,
}

impl EngineConfig {
    /// Defaults, overridable through `LUDY_SERVER`, `LUDY_CACHE_MB`,
    /// `LUDY_MOUNT_ROOT`, `LUDY_HOME`, `LUDY_READ_ONLY` and `LUDY_NOBROWSE`.
    pub fn from_env() -> EngineConfig {
        let home = PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/tmp".into()));
        let state = std::env::var("LUDY_HOME").map(PathBuf::from).unwrap_or_else(|_| home.join("Library/Application Support/Ludy"));
        let cache_mb = std::env::var("LUDY_CACHE_MB").ok().and_then(|s| s.parse::<u64>().ok()).unwrap_or(2000);
        EngineConfig {
            server_url: std::env::var("LUDY_SERVER").unwrap_or_else(|_| "http://127.0.0.1:9000".into()),
            cache_dir: std::env::var("LUDY_CACHE_DIR").map(PathBuf::from).unwrap_or_else(|_| home.join("Library/Caches/Ludy/blocks")),
            cache_cap: cache_mb * 1_000_000,
            journal_dir: state.join("journal"),
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
    pub fs: Arc<DriveFs>,
    state: Mutex<MountState>,
    tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct MountView {
    pub mountpoint: PathBuf,
    pub state: MountState,
    pub pending: Pending,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DriveView {
    pub info: DriveInfo,
    pub mount: Option<MountView>,
    /// "Mounting…" / "Ejecting…" while an operation is in flight.
    pub busy: Option<String>,
}

/// Everything the UI shows, refreshed twice a second.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct View {
    pub online: bool,
    pub server_url: String,
    pub drives: Vec<DriveView>,
    pub stats: StatsSnapshot,
    pub cache: CacheSnapshot,
    pub server: Option<ServerStats>,
    pub notice: Option<String>,
}

struct Inner {
    cfg: EngineConfig,
    client: CloudClient,
    cache: Arc<BlockCache>,
    stats: Arc<Stats>,
    mounts: Mutex<HashMap<String, Arc<Mounted>>>,
    busy: Mutex<HashMap<String, String>>,
    drives: Mutex<(bool, Vec<DriveInfo>)>,
    server: Mutex<Option<ServerStats>>,
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
        let client = CloudClient::new(&cfg.server_url, stats.clone());
        let inner = Arc::new(Inner {
            client,
            cache,
            stats,
            mounts: Mutex::new(HashMap::new()),
            busy: Mutex::new(HashMap::new()),
            drives: Mutex::new((false, Vec::new())),
            server: Mutex::new(None),
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

    pub fn client(&self) -> CloudClient {
        self.inner.client.clone()
    }

    pub fn cache(&self) -> Arc<BlockCache> {
        self.inner.cache.clone()
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

    /// Eject everything, briefly (called when the app quits or is signalled).
    /// Writes that cannot be uploaded within a few seconds are abandoned.
    pub fn shutdown(&self) {
        let drives: Vec<String> = self.inner.mounts.lock().unwrap().keys().cloned().collect();
        let inner = self.inner.clone();
        self.rt.block_on(async move {
            for d in drives {
                let _ = inner.eject_with(&d, true).await;
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

    /// Ask the refresher to re-read the drive list right away.
    pub fn refresh_drives(&self) {
        let inner = self.inner.clone();
        self.rt.spawn(async move {
            inner.fetch_drives().await;
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
        let fs = DriveFs::new(drive, self.client.clone(), self.cache.clone(), self.stats.clone(), self.cfg.journal_dir.join(drive))?;
        fs.refresh().await.map_err(|e| anyhow!("cannot list drive: {e:#}"))?;
        let (port, nfs_task) = mount::serve(fs.clone(), self.cfg.read_only).await?;
        let opts = AttachOptions { read_only: self.cfg.read_only, browse: self.cfg.browse };
        if let Err(e) = mount::attach(port, drive, &mountpoint, &opts).await {
            nfs_task.abort();
            fs.stop();
            return Err(e);
        }
        let tasks = vec![nfs_task, tokio::spawn(fs.clone().run_uploader()), tokio::spawn(fs.clone().run_poller())];
        let mounted = Arc::new(Mounted { drive: drive.to_string(), mountpoint, port, fs, state: Mutex::new(MountState::Mounted), tasks: Mutex::new(tasks) });
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
        mounted.fs.stop();
        self.mounts.lock().unwrap().remove(drive);
        // Leave no empty folder behind (checked and removed by a child process).
        mount::remove_mountpoint(&mounted.mountpoint).await;
    }

    async fn fetch_drives(&self) {
        match self.client.drives().await {
            Ok(list) => *self.drives.lock().unwrap() = (true, list),
            Err(_) => self.drives.lock().unwrap().0 = false,
        }
        if let Ok(s) = self.client.server_stats().await {
            *self.server.lock().unwrap() = Some(s);
        }
    }

    fn rebuild_view(&self) {
        let (online, infos) = self.drives.lock().unwrap().clone();
        let mounts = self.mounts.lock().unwrap();
        let busy = self.busy.lock().unwrap();
        let drives = infos
            .into_iter()
            .map(|info| {
                let mount = mounts.get(&info.name).map(|m| MountView { mountpoint: m.mountpoint.clone(), state: m.state(), pending: m.fs.pending() });
                let busy = busy.get(&info.name).cloned();
                DriveView { info, mount, busy }
            })
            .collect();
        let view = View {
            online,
            server_url: self.cfg.server_url.clone(),
            drives,
            stats: self.stats.sample(),
            cache: self.cache.snapshot(),
            server: self.server.lock().unwrap().clone(),
            notice: self.notice.lock().unwrap().clone(),
        };
        *self.view.lock().unwrap() = Arc::new(view);
    }

    /// Keep the view fresh: counters twice a second, the drive list every
    /// two seconds.
    async fn refresher(self: Arc<Self>) {
        let mut tick = 0u64;
        loop {
            if tick % 4 == 0 {
                self.fetch_drives().await;
            }
            self.rebuild_view();
            tick += 1;
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }
}
