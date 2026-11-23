//! Ludy's cloud with no platform in it.
//!
//! * [`drive`]: one drive: the tree of nodes, each file's versions, which
//!   sealed shards and manifests those versions use, the order of commits,
//!   and the collector that deletes what no version uses any more. The
//!   protocol is `ludy_proto::tree`.
//! * [`front`]: what the HTTP side in front of the drives shares: which
//!   request goes where, the checks on a shard's path and size, byte ranges.
//! * [`ports`]: what a provider supplies: SQLite with a transaction, a blob
//!   store, one timer per drive, a clock, two settings and a log.
//!
//! A provider is a thin package around this: `providers/cloudflare` (a
//! Worker, a Durable Object per drive, R2) and `providers/rivet` (a Rivet
//! actor per drive, any S3 or a folder). The server never holds a key.

pub mod drive;
pub mod front;
pub mod ports;

pub use drive::Drive;
pub use ports::{Abort, Config, Fault, Platform, Rows, Sql, Value};

/// A request to a drive, as the HTTP side hands it over.
#[derive(Clone, Debug)]
pub struct Request {
    /// Upper case: `GET`, `PUT`, `POST`.
    pub method: String,
    /// `/drives/<drive>` and what follows, without the query.
    pub path: String,
    /// What follows the `?`, or empty.
    pub query: String,
    pub body: Vec<u8>,
}

#[derive(Clone, Debug)]
pub struct Response {
    pub status: u16,
    pub content_type: &'static str,
    pub body: Vec<u8>,
}

impl Response {
    pub fn json<T: serde::Serialize>(status: u16, value: &T) -> Response {
        Response { status, content_type: "application/json", body: serde_json::to_vec(value).unwrap_or_default() }
    }

    pub fn text(status: u16, body: &str) -> Response {
        Response { status, content_type: "text/plain; charset=utf-8", body: body.as_bytes().to_vec() }
    }

    pub fn refusal(r: &ludy_proto::tree::Refusal) -> Response {
        Response::json(r.status(), r)
    }
}

/// One line of JSON per event, for whoever reads the server's log. The same
/// shape on every provider, so the chaos suites and a dashboard read one format.
pub fn log_line(event: &str, drive: &str, fields: &[(&str, serde_json::Value)]) -> String {
    let mut line = serde_json::Map::new();
    line.insert("t".into(), event.into());
    line.insert("drive".into(), drive.into());
    for (k, v) in fields {
        line.insert((*k).into(), v.clone());
    }
    serde_json::Value::Object(line).to_string()
}
