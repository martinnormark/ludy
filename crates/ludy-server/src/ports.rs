//! What a provider supplies. Every promise here is one the chaos suites
//! check; `docs/how-it-works/server.md` lists them in words.
//!
//! The methods are asynchronous because some providers wait for their
//! database (Rivet) and some do not (a Durable Object's SQLite, `rusqlite`).
//! Nothing here asks for `Send`: the Worker's types cannot move between
//! threads. The code is generic, so it is as movable as what it is given,
//! and it holds nothing unmovable of its own across a wait.

use std::future::Future;

use ludy_proto::tree::Refusal;

/// A value bound to a statement or read from a row. The drive's tables hold
/// nothing else.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Null,
    Int(i64),
    Text(String),
}

impl Value {
    pub fn text(s: &str) -> Value {
        Value::Text(s.to_string())
    }

    pub fn int(n: u64) -> Value {
        Value::Int(n as i64)
    }
}

/// The platform failed (storage, the network to it, a timer). The request is
/// answered with a 5xx and the sender tries again. Never the sender's mistake.
#[derive(Clone, Debug)]
pub struct Fault(pub String);

impl Fault {
    pub fn new(what: impl Into<String>) -> Fault {
        Fault(what.into())
    }
}

impl std::fmt::Display for Fault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// What a statement returned: its rows, by position, and how many rows it changed.
#[derive(Clone, Debug, Default)]
pub struct Rows {
    pub rows: Vec<Vec<Value>>,
    pub changed: u64,
}

fn shape(what: &str, at: usize) -> Fault {
    Fault(format!("a row is not shaped as expected: column {at} is not {what}"))
}

/// Read a row by position.
pub struct Row<'a>(pub &'a [Value]);

impl Row<'_> {
    pub fn text(&self, at: usize) -> Result<String, Fault> {
        match self.0.get(at) {
            Some(Value::Text(s)) => Ok(s.clone()),
            _ => Err(shape("text", at)),
        }
    }

    pub fn opt_text(&self, at: usize) -> Result<Option<String>, Fault> {
        match self.0.get(at) {
            Some(Value::Text(s)) => Ok(Some(s.clone())),
            Some(Value::Null) => Ok(None),
            _ => Err(shape("text or null", at)),
        }
    }

    pub fn int(&self, at: usize) -> Result<u64, Fault> {
        match self.0.get(at) {
            Some(Value::Int(n)) if *n >= 0 => Ok(*n as u64),
            _ => Err(shape("a number", at)),
        }
    }

    pub fn opt_int(&self, at: usize) -> Result<Option<u64>, Fault> {
        match self.0.get(at) {
            Some(Value::Int(n)) if *n >= 0 => Ok(Some(*n as u64)),
            Some(Value::Null) => Ok(None),
            _ => Err(shape("a number or null", at)),
        }
    }
}

impl Rows {
    pub fn iter(&self) -> impl Iterator<Item = Row<'_>> {
        self.rows.iter().map(|r| Row(r))
    }

    pub fn first(&self) -> Option<Row<'_>> {
        self.rows.first().map(|r| Row(r))
    }

    /// The single number of a `SELECT COUNT(*)`.
    pub fn count(&self) -> Result<u64, Fault> {
        self.first().ok_or_else(|| Fault::new("a count returned no row"))?.int(0)
    }
}

/// Run one statement with its values.
pub trait Sql {
    fn exec(&self, sql: &str, params: Vec<Value>) -> impl Future<Output = Result<Rows, Fault>>;
}

/// Why a transaction's body stopped. Either way nothing it wrote is kept.
#[derive(Debug)]
pub enum Abort {
    /// The request is refused: the sender gets this answer.
    Refused(Refusal),
    /// The platform failed: the sender gets a 5xx and tries again.
    Fault(Fault),
}

impl From<Refusal> for Abort {
    fn from(r: Refusal) -> Abort {
        Abort::Refused(r)
    }
}

impl From<Fault> for Abort {
    fn from(f: Fault) -> Abort {
        Abort::Fault(f)
    }
}

/// The two settings a drive reads.
#[derive(Clone, Copy, Debug)]
pub struct Config {
    /// How long a blob no version uses is kept. `GC_GRACE_MS`.
    pub gc_grace_ms: i64,
    /// `LUDY_DEV=1`: the `_dev` routes exist. Only the chaos harness sets it.
    pub dev: bool,
}

/// How long a shard no version uses is kept, unless `GC_GRACE_MS` says otherwise.
pub const DEFAULT_GRACE_MS: i64 = 24 * 60 * 60 * 1000;

impl Config {
    /// From the two settings as text, the way every provider has them.
    pub fn from_vars(gc_grace_ms: Option<&str>, ludy_dev: Option<&str>) -> Config {
        Config { gc_grace_ms: gc_grace_ms.and_then(|v| v.trim().parse().ok()).unwrap_or(DEFAULT_GRACE_MS), dev: ludy_dev.map(str::trim) == Some("1") }
    }
}

/// Everything one drive needs from where it runs.
///
/// A provider promises, beyond the signatures:
///
/// * one live instance per drive, reached by every request for that drive;
/// * what [`Platform::transaction`] wrote is kept if the body returns `Ok`
///   and gone if it returns an [`Abort`] or panics, and no other statement
///   of the drive runs in the middle;
/// * a write that was answered survives the process being killed;
/// * the timer survives a restart.
pub trait Platform {
    /// Statements outside a transaction.
    type Sql: Sql;
    /// Statements inside one. On some providers the same type.
    type Tx: Sql + 'static;

    fn sql(&self) -> &Self::Sql;

    /// Run `body` as one transaction. `name` is for the provider's own tracing.
    fn transaction<T, F, Fut>(&self, name: &'static str, body: F) -> impl Future<Output = Result<Result<T, Refusal>, Fault>>
    where
        T: 'static,
        F: FnOnce(Self::Tx) -> Fut + 'static,
        Fut: Future<Output = Result<T, Abort>> + 'static;

    /// Milliseconds since 1970.
    fn now_ms(&self) -> u64;

    fn config(&self) -> Config;

    /// Write one line to the log.
    fn log(&self, line: &str);

    /// Is this object in the blob store? `key` is `d/<drive>/s/<sha256>` or `.../m/<sha256>`.
    fn blob_exists(&self, key: &str) -> impl Future<Output = Result<bool, Fault>>;

    /// Delete it. Deleting what is not there is not an error.
    fn blob_delete(&self, key: &str) -> impl Future<Output = Result<(), Fault>>;

    /// When the drive's timer is set for (milliseconds since 1970), if it is set.
    /// While the timer is firing it counts as not set.
    fn timer(&self) -> impl Future<Output = Result<Option<u64>, Fault>>;

    /// Set the drive's one timer, replacing whatever it was set for.
    /// [`crate::Drive::alarm`] is to be called at that time.
    fn set_timer(&self, at_ms: u64) -> impl Future<Output = Result<(), Fault>>;

    /// Everything written so far is on disk when this returns.
    fn flush(&self) -> impl Future<Output = Result<(), Fault>>;

    /// The size of the drive's database, for the statistics.
    fn database_bytes(&self) -> impl Future<Output = Result<u64, Fault>>;
}
