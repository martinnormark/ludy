//! A drive on Cloudflare: one Durable Object per drive, with SQLite storage.
//!
//! The drive itself (tables, commits, feed, collector) is `ludy_server`,
//! the same on every provider. This file is what Cloudflare supplies to it:
//! the object's SQLite, its synchronous transaction, R2, and the alarm.

use std::cell::RefCell;
use std::future::Future;
use std::rc::Rc;

use futures_util::FutureExt;
use ludy_proto::tree::Refusal;
use ludy_server::{Abort, Config, Fault, Platform, Rows, Sql, Value};
use worker::wasm_bindgen::prelude::Closure;
use worker::wasm_bindgen::{JsCast, JsValue};
use worker::*;

use crate::BUCKET;

fn fault(e: impl std::fmt::Display) -> Fault {
    Fault(e.to_string())
}

/// The object's SQLite. Its calls are synchronous: a statement's future is
/// finished the first time it is asked.
#[derive(Clone)]
pub struct DoSql(SqlStorage);

impl Sql for DoSql {
    async fn exec(&self, sql: &str, params: Vec<Value>) -> std::result::Result<Rows, Fault> {
        let bound: Vec<SqlStorageValue> = params
            .into_iter()
            .map(|v| match v {
                Value::Null => SqlStorageValue::Null,
                Value::Int(n) => SqlStorageValue::Integer(n),
                Value::Text(s) => SqlStorageValue::String(s),
            })
            .collect();
        let cursor = self.0.exec(sql, bound).map_err(fault)?;
        let mut rows = Vec::new();
        for row in cursor.raw() {
            let mut values = Vec::new();
            for value in row.map_err(fault)? {
                values.push(match value {
                    SqlStorageValue::Null => Value::Null,
                    SqlStorageValue::Integer(n) => Value::Int(n),
                    SqlStorageValue::String(s) => Value::Text(s),
                    SqlStorageValue::Boolean(b) => Value::Int(b as i64),
                    other => return Err(Fault(format!("a column the drive never writes: {other:?}"))),
                });
            }
            rows.push(values);
        }
        Ok(Rows { rows, changed: cursor.rows_written() as u64 })
    }
}

/// What Cloudflare supplies to a drive.
pub struct Cloudflare {
    state: State,
    /// The runtime's storage object, for [`transaction_sync`].
    storage: JsValue,
    env: Env,
    sql: DoSql,
}

impl Platform for Cloudflare {
    type Sql = DoSql;
    type Tx = DoSql;

    fn sql(&self) -> &DoSql {
        &self.sql
    }

    async fn transaction<T, F, Fut>(&self, _name: &'static str, body: F) -> std::result::Result<std::result::Result<T, Refusal>, Fault>
    where
        T: 'static,
        F: FnOnce(DoSql) -> Fut + 'static,
        Fut: Future<Output = std::result::Result<T, Abort>> + 'static,
    {
        let tx = self.sql.clone();
        // Nothing the body does waits, so it is finished the first time it
        // is asked. If it ever waited, the transaction would end under it:
        // fail loudly instead.
        transaction_sync(&self.storage, move || body(tx).now_or_never().unwrap_or_else(|| Err(Abort::Fault(Fault::new("a transaction waited for something")))))
    }

    fn now_ms(&self) -> u64 {
        Date::now().as_millis()
    }

    fn config(&self) -> Config {
        let var = |name: &str| self.env.var(name).ok().map(|v| v.to_string());
        Config::from_vars(var("GC_GRACE_MS").as_deref(), var("LUDY_DEV").as_deref())
    }

    fn log(&self, line: &str) {
        console_log!("{line}");
    }

    async fn blob_exists(&self, key: &str) -> std::result::Result<bool, Fault> {
        Ok(self.env.bucket(BUCKET).map_err(fault)?.head(key).await.map_err(fault)?.is_some())
    }

    async fn blob_delete(&self, key: &str) -> std::result::Result<(), Fault> {
        self.env.bucket(BUCKET).map_err(fault)?.delete(key).await.map_err(fault)
    }

    async fn timer(&self) -> std::result::Result<Option<u64>, Fault> {
        Ok(self.state.storage().get_alarm().await.map_err(fault)?.map(|at| at.max(0) as u64))
    }

    async fn set_timer(&self, at_ms: u64) -> std::result::Result<(), Fault> {
        // The alarm takes how long from now.
        let from_now = at_ms.saturating_sub(self.now_ms()) as i64;
        self.state.storage().set_alarm(from_now).await.map_err(fault)
    }

    async fn flush(&self) -> std::result::Result<(), Fault> {
        self.state.storage().sync().await.map_err(fault)
    }

    async fn database_bytes(&self) -> std::result::Result<u64, Fault> {
        Ok(self.sql.0.database_size() as u64)
    }
}

#[durable_object]
pub struct Drive {
    drive: ludy_server::Drive<Cloudflare>,
}

/// A fault is the platform failing: the runtime answers 5xx and the sender tries again.
fn failed(f: Fault) -> Error {
    Error::RustError(f.0)
}

impl DurableObject for Drive {
    fn new(state: State, env: Env) -> Self {
        // `State` does not hand out the storage object itself; take it apart and put it back.
        let raw = state._inner();
        let storage: JsValue = raw.storage().expect("a Durable Object has storage").into();
        let state = State::from(raw);
        let sql = DoSql(state.storage().sql());
        let drive = ludy_server::Drive::new(Cloudflare { state, storage, env, sql });
        drive.open().now_or_never().expect("a Durable Object's SQL does not wait").expect("create the drive's tables");
        Drive { drive }
    }

    async fn fetch(&self, mut req: Request) -> Result<Response> {
        let method = req.method().as_ref().to_string();
        let body = if matches!(req.method(), Method::Put | Method::Post) { req.bytes().await? } else { Vec::new() };
        let request = ludy_server::Request { method, path: req.path(), query: req.url()?.query().unwrap_or_default().to_string(), body };
        crate::respond(self.drive.handle(request).await.map_err(failed)?)
    }

    async fn alarm(&self) -> Result<Response> {
        self.drive.alarm().await.map_err(failed)?;
        Response::ok("collected")
    }
}

/// Run `body` inside the runtime's synchronous storage transaction
/// (`transactionSync`, which the `worker` crate does not bind): what it
/// writes is kept if it returns `Ok` and undone if it stops.
///
/// Synchronous matters. The asynchronous `transaction` waits for a promise,
/// and if the body panics that promise never settles: the drive then answers
/// nothing for 30 seconds, until the runtime gives up on the request. Here a
/// panic is an exception thrown straight through the transaction, which
/// undoes it and fails the one request.
fn transaction_sync<T: 'static>(
    storage_object: &JsValue,
    body: impl FnOnce() -> std::result::Result<T, Abort> + 'static,
) -> std::result::Result<std::result::Result<T, Refusal>, Fault> {
    let slot: Rc<RefCell<Option<std::result::Result<T, Abort>>>> = Rc::new(RefCell::new(None));
    let (out, mut body) = (slot.clone(), Some(body));
    // Returning `Err` makes the glue throw, which is what rolls the transaction back.
    let closure = Closure::wrap(Box::new(move || -> std::result::Result<JsValue, JsValue> {
        let Some(body) = body.take() else {
            return Err(JsValue::from_str("the transaction body ran twice"));
        };
        let result = body();
        let stopped = result.is_err();
        *out.borrow_mut() = Some(result);
        if stopped {
            Err(JsValue::from_str("stopped"))
        } else {
            Ok(JsValue::UNDEFINED)
        }
    }) as Box<dyn FnMut() -> std::result::Result<JsValue, JsValue>>);
    let run: js_sys::Function = js_sys::Reflect::get(storage_object, &JsValue::from_str("transactionSync"))
        .map_err(|e| fault(Error::from(e)))?
        .dyn_into()
        .map_err(|_| Fault::new("this storage has no transactionSync"))?;
    let called = run.call1(storage_object, closure.as_ref());
    let result = slot.borrow_mut().take();
    match (result, called) {
        (Some(Ok(value)), Ok(_)) => Ok(Ok(value)),
        (Some(Err(Abort::Refused(refusal))), _) => Ok(Err(refusal)),
        (Some(Err(Abort::Fault(f))), _) => Err(f),
        // The body was fine and storage could not keep what it wrote.
        (Some(Ok(_)), Err(e)) => Err(fault(Error::from(e))),
        (None, Err(e)) => Err(fault(Error::from(e))),
        (None, Ok(_)) => Err(Fault::new("the transaction did not run")),
    }
}
