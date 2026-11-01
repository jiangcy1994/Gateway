use rand::distributions::{Distribution, WeightedIndex};
use rand::thread_rng;

use crate::error::ApiError;
use crate::llm::provider::{Protocol, Provider};
use crate::llm::quota::QuotaGuard;
use crate::state::AppState;

/// 负载均衡的单一入口：按协议选供应商并**在同一临界区内**记一次 RPM 派遣，
/// 因此并发请求不会重复选走只剩 1 RPM 的供应商。
///
/// - 无启用供应商 → `NoProviderForProtocol`（503）
/// - 全部供应商 RPM 或 TPM 余量为 0 → `QuotaExhausted`（429）
/// - 其余按剩余配额加权随机（`weight = min(rpm_ratio, tpm_ratio)`，未设限额
///   的维度比例按 1.0）
pub fn select_and_record(state: &AppState, protocol: Protocol) -> Result<Provider, ApiError> {
    let candidates = state.providers.list_enabled_by_protocol(protocol);
    if candidates.is_empty() {
        return Err(ApiError::NoProviderForProtocol(protocol.to_string()));
    }

    // 按 ProviderTable 表序一次性持有所有候选 tracker 的锁，期间不做任何 await。
    let trackers: Vec<std::sync::Arc<crate::llm::quota::QuotaTracker>> =
        candidates.iter().map(|p| state.quota_for(&p.id)).collect();
    let mut guards: Vec<QuotaGuard<'_>> = trackers.iter().map(|t| t.lock()).collect();

    let weights: Vec<f64> = candidates
        .iter()
        .zip(guards.iter())
        .map(|(p, g)| weight(p, g))
        .collect();

    if weights.iter().all(|&w| w <= 0.0) {
        return Err(ApiError::QuotaExhausted(protocol.to_string()));
    }

    let dist =
        WeightedIndex::new(&weights).expect("weighted index needs at least one positive weight");
    let idx = dist.sample(&mut thread_rng());

    // 选 + 记同处一个临界区
    guards[idx].record_request();
    Ok(candidates[idx].clone())
}

/// `min(rpm_ratio, tpm_ratio)`；未设限额的维度比例按 `1.0`；余量为 0 即 0。
fn ratio(limit: Option<u64>, used: u64) -> f64 {
    match limit {
        None => 1.0,
        Some(l) => l.saturating_sub(used) as f64 / l as f64,
    }
}

fn weight(provider: &Provider, guard: &QuotaGuard<'_>) -> f64 {
    ratio(provider.rpm_limit, guard.rpm_used()).min(ratio(provider.tpm_limit, guard.tpm_used()))
}
