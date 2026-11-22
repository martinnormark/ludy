//! Ludy's cloud on Rivet actors: the self-hosted provider.
//!
//! One process, two halves:
//!
//! * [`front`]: the HTTP side every client talks to. It stores and serves
//!   sealed shards and manifests itself, straight to the blob store, after
//!   checking each upload against the SHA-256 in its name. Every other
//!   request about a drive it hands to that drive's actor through the Rivet
//!   engine's gateway.
//! * [`actor`]: a Rivet actor per drive, running `ludy_server`: the same
//!   drive logic as every other provider, on the actor's own SQLite.
//!
//! The Rivet engine (the `rivetdev/engine` image) places the actors, keeps
//! their SQLite and their timers, and is the only thing this process needs
//! besides a blob store. Settings come from the environment; see
//! `providers/rivet/README.md`.

mod actor;
mod blobs;
mod front;

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::Context;
use rivetkit::{EngineSpawnMode, Registry, ServeConfig};

/// How to reach the Rivet engine, for the front's calls to the gateway.
pub struct Engine {
    pub endpoint: String,
    pub token: Option<String>,
    pub namespace: String,
    pub pool: String,
    pub http: reqwest::Client,
}

/// What both halves share.
pub struct Env {
    pub config: ludy_server::Config,
    pub blobs: blobs::Blobs,
    pub engine: Engine,
    /// The engine has the pool this process serves.
    pub ready: AtomicBool,
}

static ENV: OnceLock<Arc<Env>> = OnceLock::new();

/// An actor is created by the runtime, with no way to hand it anything.
pub fn env() -> Arc<Env> {
    ENV.get().expect("set at start, before any actor").clone()
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// One line on standard output, flushed at once: the log is what is left of
/// a process that was killed.
pub fn log(line: &str) {
    use std::io::Write;
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
}

fn var(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

/// The engine has to know the pool before a node may join it. Safe to
/// repeat, and repeated until the engine answers: it may still be starting.
async fn ensure_pool(engine: &Engine) {
    let url = format!("{}/pools/{}?namespace={}", engine.endpoint, engine.pool, engine.namespace);
    let mut attempts = 0u32;
    loop {
        let mut put = engine.http.put(&url).timeout(Duration::from_secs(5)).json(&serde_json::json!({}));
        if let Some(token) = &engine.token {
            put = put.bearer_auth(token);
        }
        let problem = match put.send().await {
            Ok(r) if r.status().is_success() => return,
            Ok(r) => format!("{} {}", r.status(), r.text().await.unwrap_or_default().chars().take(200).collect::<String>()),
            Err(e) => e.to_string(),
        };
        attempts += 1;
        if attempts % 20 == 1 {
            log(&ludy_server::log_line("engine", "", &[("event", "waiting".into()), ("endpoint", engine.endpoint.clone().into()), ("problem", problem.into())]));
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

fn listen_address() -> anyhow::Result<SocketAddr> {
    var("LUDY_LISTEN").unwrap_or_else(|| "0.0.0.0:8080".into()).parse().context("LUDY_LISTEN is not an address and a port")
}

/// A drive nobody creates. Asking for it shows whether drives answer.
pub const PROBE_DRIVE: &str = "ffffffffffffffffffffffffffffffff";

/// `ludy-rivet health`: ask the server on this machine for a drive that does
/// not exist. "No such drive" came from an actor, so the front, the engine
/// and this node's actors are all there. It is the image's health check;
/// the image has no `curl` to do it with.
///
/// It keeps asking for four seconds before it says no. A health check is
/// only run every few seconds, and this way a server that is starting is
/// found healthy when it becomes so, not at the next check.
async fn health() -> anyhow::Result<()> {
    let url = format!("http://127.0.0.1:{}/drives/{PROBE_DRIVE}", listen_address()?.port());
    let http = reqwest::Client::new();
    let give_up = std::time::Instant::now() + Duration::from_secs(4);
    loop {
        let left = give_up.saturating_duration_since(std::time::Instant::now());
        let problem = match http.get(&url).timeout(left.max(Duration::from_millis(100))).send().await {
            Ok(answer) if matches!(answer.status().as_u16(), 200 | 404) => return Ok(()),
            Ok(answer) => format!("drives do not answer yet: {} {}", answer.status().as_u16(), answer.text().await.unwrap_or_default().trim()),
            Err(e) => format!("the server did not answer: {e}"),
        };
        if std::time::Instant::now() + Duration::from_millis(250) >= give_up {
            anyhow::bail!(problem);
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// SIGTERM, which `docker stop` sends, or Ctrl-C.
async fn stop_signal() {
    use tokio::signal::unix::{signal, SignalKind};
    let Ok(mut term) = signal(SignalKind::terminate()) else { return std::future::pending().await };
    tokio::select! {
        _ = term.recv() => {}
        _ = tokio::signal::ctrl_c() => {}
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    match std::env::args().nth(1).as_deref() {
        None => {}
        Some("health") => return health().await,
        Some(other) => anyhow::bail!("no such command: {other}. Run it with no arguments to serve, or with `health`"),
    }
    if var("RIVET_ENDPOINT").is_none() {
        anyhow::bail!("RIVET_ENDPOINT is not set. It is where the Rivet engine is, as http://<namespace>:<token>@<host>:6420");
    }
    let mut serve = ServeConfig::from_env().context("reading RIVET_ENDPOINT and its companions")?;
    // The engine is a service of its own here; never start one.
    serve.engine_spawn = EngineSpawnMode::Never;

    let listen = listen_address()?;
    let engine = Engine {
        endpoint: serve.endpoint.trim_end_matches('/').to_string(),
        token: serve.token.clone(),
        namespace: serve.namespace.clone(),
        pool: serve.pool_name.clone(),
        http: reqwest::Client::builder().build()?,
    };
    let env = Arc::new(Env {
        config: ludy_server::Config::from_vars(var("GC_GRACE_MS").as_deref(), var("LUDY_DEV").as_deref()),
        blobs: blobs::Blobs::from_env()?,
        engine,
        ready: AtomicBool::new(false),
    });
    ENV.set(env.clone()).ok();
    log(&ludy_server::log_line(
        "start",
        "",
        &[("listen", listen.to_string().into()), ("engine", env.engine.endpoint.clone().into()), ("blobs", env.blobs.describe().into()), ("dev", env.config.dev.into())],
    ));

    // The front answers from the start; until the engine is there it says so.
    let listener = tokio::net::TcpListener::bind(listen).await.with_context(|| format!("listening on {listen}"))?;
    let front = tokio::spawn(front::serve(listener, env.clone()));

    // The runtime below stops on SIGTERM by itself. Until it runs nothing
    // does, and a process that is first in its container is not stopped by
    // a signal it has no handler for.
    tokio::select! {
        _ = ensure_pool(&env.engine) => {}
        _ = stop_signal() => return Ok(()),
    }
    env.ready.store(true, Ordering::SeqCst);

    let mut registry = Registry::new();
    registry.register_actor::<actor::DriveActor>(actor::NAME);
    tokio::select! {
        served = registry.start_with_config(serve) => served.context("the actor node stopped"),
        fronted = front => fronted?.context("the HTTP front stopped"),
    }
}
