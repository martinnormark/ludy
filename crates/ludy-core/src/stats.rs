//! Counters shared by every mounted drive, plus a sampler that turns them
//! into transfer rates for the UI.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Instant;

#[derive(Default)]
pub struct Stats {
    /// Bytes downloaded from the cloud (range bodies).
    pub bytes_down: AtomicU64,
    /// Bytes uploaded to the cloud.
    pub bytes_up: AtomicU64,
    /// Bytes handed to applications through the mount.
    pub bytes_read: AtomicU64,
    /// Bytes applications wrote through the mount.
    pub bytes_written: AtomicU64,
    /// Bytes of reads answered from the disk cache.
    pub cache_hit_bytes: AtomicU64,
    pub range_requests: AtomicU64,
    rate: Mutex<RateState>,
}

struct RateState {
    at: Instant,
    down: u64,
    up: u64,
    down_rate: f64,
    up_rate: f64,
}

impl Default for RateState {
    fn default() -> Self {
        RateState { at: Instant::now(), down: 0, up: 0, down_rate: 0.0, up_rate: 0.0 }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct StatsSnapshot {
    pub bytes_down: u64,
    pub bytes_up: u64,
    pub bytes_read: u64,
    pub bytes_written: u64,
    pub cache_hit_bytes: u64,
    pub range_requests: u64,
    /// Bytes per second, smoothed.
    pub down_rate: f64,
    pub up_rate: f64,
}

impl Stats {
    pub fn add(counter: &AtomicU64, n: u64) {
        counter.fetch_add(n, Ordering::Relaxed);
    }

    /// Call at a steady interval (about twice a second) to refresh the rates.
    pub fn sample(&self) -> StatsSnapshot {
        let down = self.bytes_down.load(Ordering::Relaxed);
        let up = self.bytes_up.load(Ordering::Relaxed);
        let mut r = self.rate.lock().unwrap();
        let dt = r.at.elapsed().as_secs_f64();
        if dt >= 0.2 {
            let inst_down = (down - r.down) as f64 / dt;
            let inst_up = (up - r.up) as f64 / dt;
            // Rise quickly, fall off over a second or so.
            r.down_rate = if inst_down > r.down_rate { inst_down } else { r.down_rate * 0.5 + inst_down * 0.5 };
            r.up_rate = if inst_up > r.up_rate { inst_up } else { r.up_rate * 0.5 + inst_up * 0.5 };
            if r.down_rate < 1.0 {
                r.down_rate = 0.0;
            }
            if r.up_rate < 1.0 {
                r.up_rate = 0.0;
            }
            r.at = Instant::now();
            r.down = down;
            r.up = up;
        }
        StatsSnapshot {
            bytes_down: down,
            bytes_up: up,
            bytes_read: self.bytes_read.load(Ordering::Relaxed),
            bytes_written: self.bytes_written.load(Ordering::Relaxed),
            cache_hit_bytes: self.cache_hit_bytes.load(Ordering::Relaxed),
            range_requests: self.range_requests.load(Ordering::Relaxed),
            down_rate: r.down_rate,
            up_rate: r.up_rate,
        }
    }
}
