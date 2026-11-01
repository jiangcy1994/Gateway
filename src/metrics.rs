use std::sync::atomic::{AtomicI32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use serde::Serialize;

/// Per-route counters. Everything is lock-free: the proxy hot path only touches
/// atomics, and snapshots read them once for the admin API.
#[derive(Debug, Default)]
pub struct RouteStats {
    total_requests: AtomicU64,
    in_flight: AtomicI32,
    errors: AtomicU64,
    total_latency_ms: AtomicU64,
}

#[derive(Debug, Serialize)]
pub struct StatsSnapshot {
    pub total_requests: u64,
    pub in_flight: i64,
    pub errors: u64,
    pub avg_latency_ms: f64,
}

/// Created by [`RouteStats::guard`]; decrements `in_flight` and records the request
/// latency when dropped, so the bookkeeping cannot be skipped on early returns.
pub struct InFlightGuard {
    stats: Arc<RouteStats>,
    started: Instant,
}

impl RouteStats {
    /// Counts this request as in flight for as long as the returned guard lives.
    pub fn guard(self: &Arc<Self>) -> InFlightGuard {
        self.in_flight.fetch_add(1, Ordering::Relaxed);
        InFlightGuard {
            stats: self.clone(),
            started: Instant::now(),
        }
    }

    pub fn record_error(&self) {
        self.errors.fetch_add(1, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> StatsSnapshot {
        let total_requests = self.total_requests.load(Ordering::Relaxed);
        let total_latency_ms = self.total_latency_ms.load(Ordering::Relaxed);
        StatsSnapshot {
            total_requests,
            in_flight: self.in_flight.load(Ordering::Relaxed) as i64,
            errors: self.errors.load(Ordering::Relaxed),
            avg_latency_ms: if total_requests == 0 {
                0.0
            } else {
                total_latency_ms as f64 / total_requests as f64
            },
        }
    }
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.stats.total_requests.fetch_add(1, Ordering::Relaxed);
        self.stats.in_flight.fetch_sub(1, Ordering::Relaxed);
        self.stats
            .total_latency_ms
            .fetch_add(self.started.elapsed().as_millis() as u64, Ordering::Relaxed);
    }
}
