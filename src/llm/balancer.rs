use rand::distributions::{Distribution, WeightedIndex};
use rand::thread_rng;

use crate::error::ApiError;
use crate::llm::provider::{Candidate, Protocol, Provider};
use crate::llm::quota::QuotaGuard;
use crate::state::AppState;

/// 一次选择的结果（ADR-L14）：命中的供应商 + 该 (provider, model) 条目要
/// 改写进上游 body 的模型名。`upstream_model = None` 表示不改写
/// （供应商未声明该模型——空模型表兜底，或请求本身没有 model 字段）。
#[derive(Debug, Clone)]
pub struct Selection {
    pub provider: Provider,
    pub upstream_model: Option<String>,
}

/// 负载均衡的单一入口：按协议（+ 可选 model）选供应商并**在同一临界区内**
/// 记一次 RPM 派遣，因此并发请求不会重复选走只剩 1 RPM 的供应商。
///
/// - `model = None`：协议级池（L4 原语义，无分层），无启用供应商 →
///   `NoProviderForProtocol`（503）
/// - `model = Some`：池 = 声明了该 model（或空模型表兜底）的启用供应商
///   （ADR-L14）；池空时，无任何供应商声明过该 model → `ModelNotFound`
///   （404），有声明但都不启用/不说该协议 → `NoProviderForModel`（503）
/// - 有 model 时按 (provider, model) 条目的 `priority` **降序分层**
///   （ADR-L16）：从高到低取第一个仍存在可选用（weight > 0）成员的层，
///   层内 `min(rpm_ratio, tpm_ratio)` 加权随机；所有层全耗尽 →
///   `QuotaExhausted`（429）
///
/// 锁序：候选按 ProviderTable 表序一次性持有全部 tracker 锁，期间无 await。
pub fn select_and_record(
    state: &AppState,
    protocol: Protocol,
    model: Option<&str>,
) -> Result<Selection, ApiError> {
    let candidates: Vec<Candidate> = match model {
        Some(model) => state.providers.pool_for(protocol, model),
        None => state
            .providers
            .list_enabled_by_protocol(protocol)
            .into_iter()
            .map(|p| Candidate {
                provider: p,
                // 无 model 请求不分层：全员 priority 0 同层（L4 原语义）。
                priority: 0,
                upstream: None,
            })
            .collect(),
    };

    if candidates.is_empty() {
        return match model {
            // 池空时区分「没人认识这个模型」与「认识但都用不了」。
            Some(model) if !state.providers.declares_model(model) => {
                Err(ApiError::ModelNotFound(model.to_string()))
            }
            Some(model) => Err(ApiError::NoProviderForModel(model.to_string())),
            None => Err(ApiError::NoProviderForProtocol(protocol.to_string())),
        };
    }

    // 按 ProviderTable 表序一次性持有所有候选 tracker 的锁，期间不做任何 await。
    let trackers: Vec<std::sync::Arc<crate::llm::quota::QuotaTracker>> = candidates
        .iter()
        .map(|c| state.quota_for(&c.provider.id))
        .collect();
    let mut guards: Vec<QuotaGuard<'_>> = trackers.iter().map(|t| t.lock()).collect();

    let weights: Vec<f64> = candidates
        .iter()
        .zip(guards.iter())
        .map(|(c, g)| weight(&c.provider, g))
        .collect();

    if weights.iter().all(|&w| w <= 0.0) {
        return Err(exhausted(protocol, model));
    }

    // 分层（ADR-L16）：按 priority 降序切成相同 priority 的连续层，从高到低
    // 取第一个仍有可选用（weight > 0）成员的层。
    let mut order: Vec<usize> = (0..candidates.len()).collect();
    order.sort_by(|&a, &b| candidates[b].priority.cmp(&candidates[a].priority));
    let mut tiers: Vec<Vec<usize>> = Vec::new();
    for &i in &order {
        if let Some(last) = tiers.last_mut() {
            if candidates[last[0]].priority == candidates[i].priority {
                last.push(i);
                continue;
            }
        }
        tiers.push(vec![i]);
    }
    let tier = tiers
        .iter()
        .find(|t| t.iter().any(|&j| weights[j] > 0.0))
        .expect("weights are not all zero at this point");

    // 层内加权随机：weight 为 0 的成员永远不会被采样（rand 允许零权重）。
    let tier_weights: Vec<f64> = tier.iter().map(|&j| weights[j]).collect();
    let dist = WeightedIndex::new(&tier_weights)
        .expect("weighted index needs at least one positive weight");
    let picked = tier[dist.sample(&mut thread_rng())];

    // 选 + 记同处一个临界区
    guards[picked].record_request();
    let candidate = &candidates[picked];
    Ok(Selection {
        provider: candidate.provider.clone(),
        upstream_model: candidate.upstream.clone(),
    })
}

/// 429 的消息：协议级请求沿用旧文案；带 model 的请求给出 model 上下文。
fn exhausted(protocol: Protocol, model: Option<&str>) -> ApiError {
    match model {
        Some(model) => ApiError::QuotaExhausted(format!(
            "model '{model}': all providers are at their rpm or tpm limit (protocol {protocol})"
        )),
        None => ApiError::QuotaExhausted(protocol.to_string()),
    }
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
