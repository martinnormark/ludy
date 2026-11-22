//! A drive on Rivet: one actor per drive, keyed by the drive's id.
//!
//! The drive itself is `ludy_server`. This file is what Rivet supplies to
//! it: the actor's SQLite and its transactions, a schedule for the
//! collector, and the blob store of the process.
//!
//! Requests reach the actor through `on_fetch`, which the runtime calls one
//! at a time for an actor. The collector arrives as a scheduled action and
//! runs beside them; it chooses and marks inside a transaction. In
//! rivetkit-core 3.1.1 a transaction holds the actor's SQL gate exclusively
//! and a plain statement shares it, so a commit cannot land in the middle.
//! That is read from its source; no test has produced the overlap.

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;

use async_trait::async_trait;
use futures_util::FutureExt;
use ludy_proto::tree::Refusal;
use ludy_server::{Abort, Config, Fault, Platform, Rows, Sql, Value};
use rivetkit::prelude::*;
use rivetkit::{BindParam, ColumnValue, Request, Response, SqliteDb, SqliteTransaction};
use serde::{Deserialize, Serialize};

use crate::Env;

/// The actor's name in the registry, and in the gateway's path.
pub const NAME: &str = "drive";
/// The scheduled action that runs the collector.
const COLLECT: &str = "collect";
/// A response with this header came from a drive. Anything else the gateway
/// answered itself, and the front turns it into a 502.
pub const OURS: &str = "x-ludy-drive";
/// After a collector run that failed, try again this much later.
const RETRY_MS: u64 = 5_000;

fn fault(e: impl std::fmt::Display) -> Fault {
    Fault(format!("{e:#}"))
}

fn bind(params: Vec<Value>) -> Option<Vec<BindParam>> {
    Some(
        params
            .into_iter()
            .map(|v| match v {
                Value::Null => BindParam::Null,
                Value::Int(n) => BindParam::Integer(n),
                Value::Text(s) => BindParam::Text(s),
            })
            .collect(),
    )
}

/// What a statement gave back, as the drive reads it.
fn rows(found: Vec<Vec<ColumnValue>>, changes: i64) -> Result<Rows, Fault> {
    let mut out = Rows { rows: Vec::with_capacity(found.len()), changed: changes.max(0) as u64 };
    for row in found {
        let mut values = Vec::with_capacity(row.len());
        for value in row {
            values.push(match value {
                ColumnValue::Null => Value::Null,
                ColumnValue::Integer(n) => Value::Int(n),
                ColumnValue::Text(s) => Value::Text(s),
                other => return Err(Fault(format!("a column the drive never writes: {other:?}"))),
            });
        }
        out.rows.push(values);
    }
    Ok(out)
}

#[derive(Clone)]
pub struct RivetSql(SqliteDb);

impl Sql for RivetSql {
    async fn exec(&self, sql: &str, params: Vec<Value>) -> Result<Rows, Fault> {
        let result = self.0.execute(sql, bind(params)).await.map_err(fault)?;
        rows(result.rows, result.changes)
    }
}

#[derive(Clone)]
pub struct RivetTx(SqliteTransaction);

impl Sql for RivetTx {
    async fn exec(&self, sql: &str, params: Vec<Value>) -> Result<Rows, Fault> {
        let result = self.0.execute(sql, bind(params)).await.map_err(fault)?;
        rows(result.rows, result.changes)
    }
}

/// What Rivet supplies to a drive.
pub struct Rivet {
    ctx: Ctx<DriveActor>,
    sql: RivetSql,
    env: Arc<Env>,
}

impl Rivet {
    /// Forget every scheduled run of the collector.
    async fn clear_timer(&self) -> Result<(), Fault> {
        let schedule = self.ctx.schedule();
        for event in schedule.list().await.map_err(fault)? {
            if event.action == COLLECT {
                schedule.cancel(&event.id).await.map_err(fault)?;
            }
        }
        Ok(())
    }
}

impl Platform for Rivet {
    type Sql = RivetSql;
    type Tx = RivetTx;

    fn sql(&self) -> &RivetSql {
        &self.sql
    }

    async fn transaction<T, F, Fut>(&self, name: &'static str, body: F) -> Result<Result<T, Refusal>, Fault>
    where
        T: 'static,
        F: FnOnce(RivetTx) -> Fut + 'static,
        Fut: Future<Output = Result<T, Abort>> + 'static,
    {
        // Other transactions and the actor's other statements queue behind this one.
        let tx = self.sql.0.begin_named_transaction(Some(name), None).await.map_err(fault)?;
        // A panic must not leave the transaction open: the actor is
        // restarted after one, and closing its database then waits for the
        // transaction, which kept a drive unanswered for 36 seconds. Roll
        // back first, then let the panic go on and restart the actor.
        let outcome = match std::panic::AssertUnwindSafe(body(RivetTx(tx.clone()))).catch_unwind().await {
            Ok(outcome) => outcome,
            Err(panic) => {
                let _ = tx.rollback().await;
                std::panic::resume_unwind(panic)
            }
        };
        match outcome {
            Ok(value) => match tx.commit().await {
                Ok(()) => Ok(Ok(value)),
                Err(e) => {
                    let _ = tx.rollback().await;
                    Err(fault(e))
                }
            },
            Err(stop) => {
                let _ = tx.rollback().await;
                match stop {
                    Abort::Refused(r) => Ok(Err(r)),
                    Abort::Fault(f) => Err(f),
                }
            }
        }
    }

    fn now_ms(&self) -> u64 {
        crate::now_ms()
    }

    fn config(&self) -> Config {
        self.env.config
    }

    fn log(&self, line: &str) {
        crate::log(line);
    }

    async fn blob_exists(&self, key: &str) -> Result<bool, Fault> {
        self.env.blobs.exists(key).await.map_err(fault)
    }

    async fn blob_delete(&self, key: &str) -> Result<(), Fault> {
        self.env.blobs.delete(key).await.map_err(fault)
    }

    async fn timer(&self) -> Result<Option<u64>, Fault> {
        let events = self.ctx.schedule().list().await.map_err(fault)?;
        Ok(events.iter().filter(|e| e.action == COLLECT).map(|e| e.run_at.max(0) as u64).min())
    }

    async fn set_timer(&self, at_ms: u64) -> Result<(), Fault> {
        self.clear_timer().await?;
        self.ctx.schedule().at(at_ms as i64, COLLECT, &[]).await.map_err(fault)?;
        Ok(())
    }

    /// A statement that was answered is already on the engine's disk.
    async fn flush(&self) -> Result<(), Fault> {
        Ok(())
    }

    async fn database_bytes(&self) -> Result<u64, Fault> {
        self.sql.exec("SELECT page_count * page_size FROM pragma_page_count(), pragma_page_size()", vec![]).await?.count()
    }
}

pub struct DriveActor {
    drive: ludy_server::Drive<Rivet>,
}

/// The collector's timer went off.
#[derive(Serialize, Deserialize)]
pub struct Collect;

impl Action for Collect {
    type Output = ();
    const NAME: &'static str = COLLECT;
}

fn response(status: u16, content_type: &str, body: Vec<u8>) -> Result<Response> {
    let headers = HashMap::from([("content-type".to_string(), content_type.to_string()), (OURS.to_string(), "1".to_string())]);
    Response::from_parts(status, headers, body)
}

#[async_trait]
impl Actor for DriveActor {
    type Input = ();
    type Actions = (Collect,);
    type Events = ();
    type ConnParams = ();
    type Action = action::Raw;

    async fn create(ctx: &Ctx<Self>) -> Result<Self> {
        let drive = ludy_server::Drive::new(Rivet { ctx: ctx.clone(), sql: RivetSql(ctx.sql().clone()), env: crate::env() });
        drive.open().await.map_err(|f| anyhow!("creating the drive's tables: {f}"))?;
        Ok(DriveActor { drive })
    }

    async fn on_start(self: Arc<Self>, _ctx: Ctx<Self>) -> Result<()> {
        // A timer that was lost with the last instance is set again.
        if let Err(f) = self.drive.schedule_collector().await {
            crate::log(&ludy_server::log_line("fault", "", &[("where", "start".into()), ("error", f.0.into())]));
        }
        Ok(())
    }

    async fn on_fetch(self: Arc<Self>, _ctx: Ctx<Self>, req: Request) -> Result<Response> {
        let started = crate::now_ms();
        let mut body = req.body().clone();
        // A larger body arrives in pieces.
        if let Some(mut pieces) = req.take_body_stream() {
            while let Some(piece) = pieces.recv().await.map_err(|e| anyhow!("reading the request's body: {e:?}"))? {
                body.extend_from_slice(&piece);
            }
        }
        let (method, path) = (req.method().as_str().to_string(), req.uri().path().to_string());
        let request = ludy_server::Request { method: method.clone(), path: path.clone(), query: req.uri().query().unwrap_or_default().to_string(), body };
        match self.drive.handle(request).await {
            Ok(r) => response(r.status, r.content_type, r.body),
            // The platform failed: a 5xx, and the sender tries again. This is
            // what a request in flight gets when the engine goes away, or
            // when this process was stopped for so long that the engine gave
            // up on it; a commit caught that way is settled by its retry.
            Err(f) => {
                let ms = crate::now_ms().saturating_sub(started);
                crate::log(&ludy_server::log_line("fault", "", &[("method", method.into()), ("path", path.into()), ("error", f.0.clone().into()), ("ms", ms.into())]));
                response(500, "text/plain; charset=utf-8", f.0.into_bytes())
            }
        }
    }
}

impl Handler<Collect> for DriveActor {
    async fn handle(self: Arc<Self>, _ctx: Ctx<Self>, _action: Collect) -> Result<()> {
        let platform = self.drive.platform();
        // The timer that is going off no longer counts as set.
        let ran = match platform.clear_timer().await {
            Ok(()) => self.drive.alarm().await,
            Err(f) => Err(f),
        };
        if let Err(f) = ran {
            crate::log(&ludy_server::log_line("gc", "", &[("failed", f.0.into())]));
            // Rivet does not run a failed schedule again by itself.
            if let Err(f) = platform.set_timer(crate::now_ms() + RETRY_MS).await {
                crate::log(&ludy_server::log_line("fault", "", &[("where", "collector retry".into()), ("error", f.0.into())]));
            }
        }
        Ok(())
    }
}
