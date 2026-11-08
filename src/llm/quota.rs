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

    /// 429 的恢复时长（ADR-L24）：两维「最早可能恢复」的时刻取 **max**
    /// （`weight = min` 的对偶——两维都恢复才算恢复），返回距当前时刻的剩余时长。
    /// 不加锁、不淘汰、无 await：只读 guard 创建时（`lock()`）已淘汰过的 deque。
    pub fn retry_after(&self, rpm_limit: Option<u64>, tpm_limit: Option<u64>) -> Option<Duration> {
        let now = Instant::now();
        rpm_recovery(&self.inner.requests, rpm_limit, now).max(tpm_recovery(
            &self.inner.tokens,
            tpm_limit,
            now,
        ))
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

/// RPM 耗尽的恢复时刻：`requests[len - limit] + WINDOW`——淘汰掉这一条后窗口内只剩
/// `limit - 1 < limit` 条（`len == limit` 时即队首）。
///
/// 门缩在助手内部（limit 已设且 `requests.len() >= limit`）：该维未必是约束维度，
/// 不门住 `len - limit` 会下标越界。`now` 由调用者传入，测试因此注入受控时刻。
fn rpm_recovery(
    requests: &VecDeque<Instant>,
    limit: Option<u64>,
    now: Instant,
) -> Option<Duration> {
    let limit = limit?;
    if (requests.len() as u64) < limit {
        return None;
    }
    let evicting = requests.get(requests.len() - limit as usize)?;
    Some((*evicting + WINDOW).saturating_duration_since(now))
}

/// TPM 耗尽的恢复时刻：从前累加被淘汰量，首个令剩余量 `< limit` 的条目 `+ WINDOW`。
/// 配置校验保证 `limit > 0`，故遍历必终止（全淘汰后剩余 0 < limit）。`total` 只算一次，
/// 遍历中只对累计淘汰量作比较，不重读 `tpm_used()`。
fn tpm_recovery(
    tokens: &VecDeque<(Instant, u64)>,
    limit: Option<u64>,
    now: Instant,
) -> Option<Duration> {
    let limit = limit?;
    let total: u64 = tokens.iter().map(|(_, n)| n).sum();
    if total < limit {
        return None;
    }
    let mut evicted = 0u64;
    for (t, n) in tokens {
        evicted += *n;
        if total.saturating_sub(evicted) < limit {
            return Some((*t + WINDOW).saturating_duration_since(now));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 由固定基准 `base` 回溯 `secs` 秒：helper 层断言全部由注入时刻推出，无墙上时钟漂移。
    fn secs_ago(base: Instant, secs: u64) -> Instant {
        base.checked_sub(Duration::from_secs(secs))
            .expect("基准距单调时钟起点足够远，回溯必成功")
    }

    /// 直接向私有 `QuotaInner` 注入回溯条目（回溯 < 60s，`lock()` 的淘汰不会动它们）。
    fn inject(tracker: &QuotaTracker, request_ages: &[u64], tokens: &[(u64, u64)]) {
        let mut inner = tracker.inner.lock().unwrap();
        for &age in request_ages {
            inner.requests.push_back(secs_ago(Instant::now(), age));
        }
        for &(age, count) in tokens {
            inner
                .tokens
                .push_back((secs_ago(Instant::now(), age), count));
        }
    }

    /// 剩余时长应落在注入时刻推出的 `(secs - 1, secs]` 秒内——闭上界容忍注入与调用读到
    /// 同一个时钟值（微秒级漂移只可能让结果略小于 `secs`，绝不可能越过一个整秒）。
    fn assert_remaining(actual: Duration, secs: u64) {
        let lo = Duration::from_secs(secs.saturating_sub(1));
        let hi = Duration::from_secs(secs);
        assert!(
            actual > lo && actual <= hi,
            "剩余 {actual:?} 不在注入时刻推出的 ({lo:?}, {hi:?}] 内"
        );
    }

    #[test]
    fn quota_retry_after_none_when_unlimited_or_not_exhausted() {
        let now = Instant::now();
        let requests: VecDeque<Instant> = [50u64, 40]
            .into_iter()
            .map(|age| secs_ago(now, age))
            .collect();
        let tokens: VecDeque<(Instant, u64)> = [(50u64, 100u64), (40, 40)]
            .into_iter()
            .map(|(age, n)| (secs_ago(now, age), n))
            .collect();

        // 未设限额：永不耗尽，即便队列里有记录。
        assert_eq!(rpm_recovery(&requests, None, now), None);
        assert_eq!(tpm_recovery(&tokens, None, now), None);
        // 设了限额但 used < limit：该维不是约束，同样 None。
        assert_eq!(rpm_recovery(&requests, Some(3), now), None);
        assert_eq!(tpm_recovery(&tokens, Some(200), now), None);

        // guard 层走真实 `lock()` 之后的 deque：两维不限 / 一维有限但未耗尽 → None。
        let tracker = QuotaTracker::new();
        inject(&tracker, &[40u64], &[(40u64, 100u64)]);
        let guard = tracker.lock();
        assert_eq!(guard.retry_after(None, None), None);
        assert_eq!(guard.retry_after(Some(2), None), None);
        assert_eq!(guard.retry_after(None, Some(150)), None);
    }

    #[test]
    fn quota_retry_after_rpm_tracks_evicting_entry() {
        let now = Instant::now();
        // requests = [-50s, -40s, -30s]（最新在后）。
        let requests: VecDeque<Instant> = [50u64, 40, 30]
            .into_iter()
            .map(|age| secs_ago(now, age))
            .collect();

        // len == limit：淘汰队首即恢复 → requests[0] + WINDOW（剩余 10s）。
        assert_eq!(
            rpm_recovery(&requests, Some(3), now),
            Some(Duration::from_secs(10))
        );
        // len > limit：恢复由 requests[len - limit] 决定（index 1 → 20s，index 2 → 30s）。
        assert_eq!(
            rpm_recovery(&requests, Some(2), now),
            Some(Duration::from_secs(20))
        );
        assert_eq!(
            rpm_recovery(&requests, Some(1), now),
            Some(Duration::from_secs(30))
        );

        // guard 层：仅 RPM 耗尽（TPM 不限额）→ 该维剩余窗口。
        let only_rpm = QuotaTracker::new();
        inject(&only_rpm, &[50u64, 40, 30], &[]);
        assert_remaining(
            only_rpm
                .lock()
                .retry_after(Some(2), None)
                .expect("rpm 耗尽应返回 Some"),
            20,
        );

        // 两维同时耗尽取 max：RPM ≈ 10s、TPM ≈ 30s → 供应商级 ≈ 30s。
        let tpm_wins = QuotaTracker::new();
        inject(&tpm_wins, &[50u64], &[(40u64, 100u64), (30, 40), (20, 30)]);
        assert_remaining(
            tpm_wins
                .lock()
                .retry_after(Some(1), Some(60))
                .expect("两维耗尽应返回 Some"),
            30,
        );

        // 反向：RPM ≈ 50s、TPM ≈ 30s → 供应商级 ≈ 50s（钉住是 max 而非 min）。
        let rpm_wins = QuotaTracker::new();
        inject(&rpm_wins, &[10u64], &[(40u64, 100u64), (30, 40), (20, 30)]);
        assert_remaining(
            rpm_wins
                .lock()
                .retry_after(Some(1), Some(60))
                .expect("两维耗尽应返回 Some"),
            50,
        );
    }

    #[test]
    fn quota_retry_after_tpm_accumulates_from_front() {
        let now = Instant::now();
        // tokens = [(-50s, 100), (-40s, 40), (-30s, 30)]，total = 170：
        // 仅淘汰队首(100) 后剩 70 ≥ 60，再淘汰第二条（累计 140）才跌到 30 < 60。
        let tokens: VecDeque<(Instant, u64)> = [(50u64, 100u64), (40, 40), (30, 30)]
            .into_iter()
            .map(|(age, n)| (secs_ago(now, age), n))
            .collect();

        // 落点是中间条目（index 1）而非队首：队首要给 10s，中间条目才是 20s。
        assert_eq!(
            tpm_recovery(&tokens, Some(60), now),
            Some(Duration::from_secs(20))
        );
        // 对照：limit = 100 时淘汰队首即足够（落点回队首 10s），钉住包含式累计比较。
        assert_eq!(
            tpm_recovery(&tokens, Some(100), now),
            Some(Duration::from_secs(10))
        );

        // 严格 `<` 的边界：tokens = [(-50s, 100), (-40s, 40)]，total = 140——仅淘汰队首（累计 100）
        // 后剩余恰 140 - 100 = 40 == limit，严格比较不在此触发，落点继续走到第二条（+20s）；
        // 若退化成 `<=` 会提前报恢复（+10s），本断言即失败。
        let tokens_exact: VecDeque<(Instant, u64)> = [(50u64, 100u64), (40, 40)]
            .into_iter()
            .map(|(age, n)| (secs_ago(now, age), n))
            .collect();
        assert_eq!(
            tpm_recovery(&tokens_exact, Some(40), now),
            Some(Duration::from_secs(20))
        );

        // guard 层：仅 TPM 耗尽（RPM 不限额）→ 落点同样是第二条。
        let tracker = QuotaTracker::new();
        inject(&tracker, &[], &[(50u64, 100u64), (40, 40), (30, 30)]);
        assert_remaining(
            tracker
                .lock()
                .retry_after(None, Some(60))
                .expect("tpm 耗尽应返回 Some"),
            20,
        );
    }
}
