//! A cloud server that can be killed and brought back, for tests and
//! benchmarks. It runs on a runtime of its own, so killing it drops every
//! request in flight the way a dying process would, and a restart reopens
//! the same data folder on the same port: a deploy, as a client sees it.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use crate::{Cloud, Config};

pub struct Harness {
    cfg: Config,
    running: Option<(tokio::runtime::Runtime, Arc<Cloud>)>,
    addr: SocketAddr,
}

impl Harness {
    /// Start serving `cfg`. With port 0 the port is chosen once and kept
    /// across restarts.
    pub async fn start(cfg: Config) -> std::io::Result<Harness> {
        let mut h = Harness { addr: cfg.addr, cfg, running: None };
        h.boot().await?;
        Ok(h)
    }

    async fn boot(&mut self) -> std::io::Result<()> {
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(4).thread_name("ludy-cloud").enable_all().build()?;
        let mut cfg = self.cfg.clone();
        cfg.addr = self.addr;
        // The old listener may take a moment to let go of the port.
        let mut last = None;
        for _ in 0..100 {
            let (tx, rx) = tokio::sync::oneshot::channel();
            let attempt = cfg.clone();
            rt.spawn(async move {
                let _ = tx.send(crate::serve(attempt).await.map(|(addr, cloud, _task)| (addr, cloud)));
            });
            match rx.await {
                Ok(Ok((addr, cloud))) => {
                    self.addr = addr;
                    self.running = Some((rt, cloud));
                    return Ok(());
                }
                Ok(Err(e)) if e.kind() == std::io::ErrorKind::AddrInUse => last = Some(e),
                Ok(Err(e)) => {
                    rt.shutdown_background();
                    return Err(e);
                }
                Err(_) => break,
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        rt.shutdown_background();
        Err(last.unwrap_or_else(|| std::io::Error::other("the server task died while starting")))
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub fn is_up(&self) -> bool {
        self.running.is_some()
    }

    /// The running server's state. Panics while it is down.
    pub fn cloud(&self) -> Arc<Cloud> {
        self.running.as_ref().expect("the cloud is down").1.clone()
    }

    /// Stop at once: connections close, requests in flight are dropped.
    pub fn kill(&mut self) {
        if let Some((rt, _cloud)) = self.running.take() {
            rt.shutdown_background();
        }
    }

    /// Kill (if running) and start again on the same data and port.
    pub async fn restart(&mut self) -> std::io::Result<()> {
        self.kill();
        self.boot().await
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.kill();
    }
}
