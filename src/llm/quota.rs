use std::collections::VecDeque;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::llm::provider::Protocol;

/// 60 秒滚动窗口（常量）。
const WINDOW: Duration = Duration::from_secs(60);

/// 一个供应商的 RPM/TPM 记账器。RPM 在派遣时计数（`balancer` 的同一临界区内），
/// TPM 在响应 usage 记账时累加；每次读前淘汰窗口外条目。
///
/// 并发约定：锁永远按 `ProviderTable` 表序获取，`balancer::select_and_record`
/// 一次性持有所有候选 tracker 的锁后再算权重，期间不做任何 await。
#[derive(Debug, Default)]
pub struct QuotaTracker {
    inner: Mutex<QuotaInner>,
}

#[derive(Debug, Default)]
struct QuotaInner {
    /// 每次派遣一条。
    requests: VecDeque<Instant>,
    /// 每次 usage 记账一条（可能为 0：无 usage 也占一条请求历史）。
    tokens: VecDeque<(Instant, u64)>,
}

/// `select_and_record` 持有的临界区句柄：创建时即淘汰窗口外记录，
/// 期间可读余量、可 `record_request`——选择与记账同处一个临界区。
pub struct QuotaGuard<'a> {
    inner: MutexGuard<'a, QuotaInner>,
}

impl<'a> QuotaGuard<'a> {
    pub fn rpm_used(&self) -> u64 {
        self.inner.requests.len() as u64
    }

    pub fn tpm_used(&self) -> u64 {
        self.inner.tokens.iter().map(|(_, n)| n).sum()
    }

    /// 在已持有的临界区内记一次派遣（不再加锁、不再淘汰）。
    pub fn record_request(&mut self) {
        self.inner.requests.push_back(Instant::now());
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct LimitView {
    /// `null` 表示不限。
    pub limit: Option<u64>,
    pub used: u64,
    /// `null` 同义不限。
    pub remaining: Option<u64>,
}

/// `GET /__admin/llm/quota` 的每条记录。
#[derive(Debug, Clone, Serialize)]
pub struct QuotaSnapshot {
    pub id: String,
    /// 该供应商说的全部协议（多协议供应商只有一条配额记录，ADR-L13）。
    pub protocols: Vec<String>,
    pub enabled: bool,
    pub rpm: LimitView,
    pub tpm: LimitView,
    pub window_secs: u64,
}

impl QuotaTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// 加锁并淘汰窗口外记录，返回临界区句柄。锁中毒 panic（与 `RouteTable` 同策略）。
    pub fn lock(&self) -> QuotaGuard<'_> {
        let mut inner = self.inner.lock().unwrap();
        let now = Instant::now();
        evict(&mut inner.requests, now);
        evict_pairs(&mut inner.tokens, now);
        QuotaGuard { inner }
    }

    pub fn record_request(&self) {
        let mut guard = self.lock();
        guard.record_request();
    }

    /// `tokens == 0` 也记：无 usage 与未完成同样占用请求历史，但 token 为 0。
    pub fn record_tokens(&self, tokens: u64) {
        let mut inner = self.inner.lock().unwrap();
        inner.tokens.push_back((Instant::now(), tokens));
    }

    pub fn remaining_rpm(&self, limit: Option<u64>) -> Option<u64> {
        let guard = self.lock();
        limit.map(|l| l.saturating_sub(guard.rpm_used()))
    }

    pub fn remaining_tpm(&self, limit: Option<u64>) -> Option<u64> {
        let guard = self.lock();
        limit.map(|l| l.saturating_sub(guard.tpm_used()))
    }

    pub fn snapshot(
        &self,
        id: &str,
        protocols: &[Protocol],
        enabled: bool,
        rpm_limit: Option<u64>,
        tpm_limit: Option<u64>,
    ) -> QuotaSnapshot {
        let guard = self.lock();
        QuotaSnapshot {
            id: id.to_string(),
            protocols: protocols.iter().map(|p| p.to_string()).collect(),
            enabled,
            rpm: LimitView {
                limit: rpm_limit,
                used: guard.rpm_used(),
                remaining: rpm_limit.map(|l| l.saturating_sub(guard.rpm_used())),
            },
            tpm: LimitView {
                limit: tpm_limit,
                used: guard.tpm_used(),
                remaining: tpm_limit.map(|l| l.saturating_sub(guard.tpm_used())),
            },
            window_secs: WINDOW.as_secs(),
        }
    }
}

fn evict(deq: &mut VecDeque<Instant>, now: Instant) {
    while let Some(front) = deq.front() {
        if now.duration_since(*front) >= WINDOW {
            deq.pop_front();
        } else {
            break;
        }
    }
}

fn evict_pairs(deq: &mut VecDeque<(Instant, u64)>, now: Instant) {
    while let Some((front, _)) = deq.front() {
        if now.duration_since(*front) >= WINDOW {
            deq.pop_front();
        } else {
            break;
        }
    }
}
