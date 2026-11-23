//! What every request to the server shares: the error for an answer that
//! was a failure, the time limits, and a range body that arrives in pieces.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use bytes::Bytes;

use crate::stats::Stats;

/// The server answered, and the answer was an error status.
#[derive(Debug)]
pub struct HttpError {
    pub status: u16,
    pub what: String,
    pub body: String,
}

impl std::fmt::Display for HttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {} {}", self.what, self.status, self.body)
    }
}

impl std::error::Error for HttpError {}

/// The HTTP status behind an error, if the server answered at all.
pub fn http_status(e: &anyhow::Error) -> Option<u16> {
    e.downcast_ref::<HttpError>().map(|h| h.status)
}

/// A request the server will refuse however often it is repeated.
pub fn is_rejection(e: &anyhow::Error) -> bool {
    matches!(http_status(e), Some(400 | 413 | 414))
}

/// How long a transfer may go without a single byte arriving before it is
/// given up. Shorter than the NFS client's own 10 s timeout, so a stalled
/// server turns into an I/O error for the reader and the mount stays alive.
pub(crate) const STALL: Duration = Duration::from_secs(8);
/// Total time for a small request (a page of the feed, a commit).
pub(crate) const SMALL_REQUEST: Duration = Duration::from_secs(30);

pub(crate) fn stalled(what: &str) -> anyhow::Error {
    anyhow!("{what}: the server sent nothing for {} s", STALL.as_secs())
}

/// Total time allowed for sending `n` bytes: generous even for a slow link.
pub(crate) fn transfer_timeout(n: u64) -> Duration {
    SMALL_REQUEST + Duration::from_secs(n / 50_000)
}

/// The body of a range request, arriving.
pub struct RangeBody {
    resp: reqwest::Response,
    stats: Arc<Stats>,
    what: String,
    start: u64,
    len: u64,
    got: u64,
}

impl RangeBody {
    pub(crate) fn new(resp: reqwest::Response, stats: Arc<Stats>, what: String, start: u64, len: u64) -> RangeBody {
        RangeBody { resp, stats, what, start, len, got: 0 }
    }

    /// The next piece of the body, or `None` once exactly the bytes asked
    /// for have arrived. A body that is longer or shorter is an error, and
    /// so is one that stops arriving.
    pub async fn chunk(&mut self) -> Result<Option<Bytes>> {
        let chunk = tokio::time::timeout(STALL, self.resp.chunk()).await.map_err(|_| stalled(&self.what))??;
        if let Some(chunk) = &chunk {
            Stats::add(&self.stats.bytes_down, chunk.len() as u64);
            self.got += chunk.len() as u64;
        }
        if self.got > self.len || (chunk.is_none() && self.got != self.len) {
            bail!("{}: wanted {} bytes at {}, got {}{}", self.what, self.len, self.start, self.got, if chunk.is_some() { " and more" } else { "" });
        }
        Ok(chunk)
    }
}
