use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use chrono::NaiveDate;
use serde::{Deserialize, Serialize};

use crate::error::ApiError;
use crate::llm::usage::Usage;
use crate::state::AppState;

/// 无凭证流量的 app 标识：账本内部用空串区分，落盘 TOML 也是 `app = ""`，
/// 只在快照边界转成 `None`（ADR-L20）。
pub const ANONYMOUS_APP: &str = "";

/// 按日 × 应用的分桶 token 用量条目：`GET /__admin/llm/usage` 的返回元素与
/// `usage.toml` 的行共用这一份序列化形态。`total` 落盘以便人工对账，但
/// 加载时忽略——快照里的 `total` 永远由四桶重算。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageEntry {
    /// 记账时刻的 UTC 日期，`YYYY-MM-DD`。
    pub date: String,
    /// 无凭证流量为 `None`（TOML 里写作 `app = ""`）。
    #[serde(default)]
    pub app: Option<String>,
    #[serde(default)]
    pub input: u64,
    #[serde(default)]
    pub output: u64,
    #[serde(default)]
    pub cache_read: u64,
    #[serde(default)]
    pub cache_write: u64,
    /// 四桶之和：序列化时写入，反序列化时忽略。
    #[serde(default)]
    pub total: u64,
}

/// `GET /__admin/llm/usage` 的查询参数（AND 语义；`from`/`to` 为闭区间）。
#[derive(Debug, Clone, Default)]
pub struct UsageFilter {
    pub app: Option<String>,
    pub date: Option<NaiveDate>,
    pub from: Option<NaiveDate>,
    pub to: Option<NaiveDate>,
}

/// 按日 × 应用的用量账本（ADR-L20）：主键 `(UTC 日期, app id)`，值为跨协议
/// 归一的四桶 usage。与配额路径完全解耦——`extract_tokens` / `QuotaTracker`
/// 不经过这里，本账本的锁也不与任何 `QuotaTracker` 的锁嵌套（`UsageTap::finish`
/// 先记配额、再记账本）。`Clone` 是廉价的：两个成员都是 `Arc`。
#[derive(Debug, Clone, Default)]
pub struct UsageLedger {
    inner: Arc<RwLock<BTreeMap<(NaiveDate, String), Usage>>>,
    dirty: Arc<AtomicBool>,
}

impl UsageLedger {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(BTreeMap::new())),
            dirty: Arc::new(AtomicBool::new(false)),
        }
    }

    /// 记一笔用量。`app = None` 表示无凭证流量；**四桶全 0 也建条目**
    /// （systemone 桩与 usage 解析失败的对账需要「条目存在、桶为 0」）。
    pub fn record(&self, app: Option<&str>, date: NaiveDate, usage: Usage) {
        let app = app.unwrap_or(ANONYMOUS_APP).to_string();
        let mut map = self.inner.write().unwrap();
        map.entry((date, app))
            .and_modify(|entry| *entry = *entry + usage)
            .or_insert(usage);
        self.dirty.store(true, Ordering::Release);
    }

    /// 按过滤条件取快照。AND 语义；结果按 `(date, app)` 升序（BTreeMap 自然序，
    /// 同日无凭证条目 `""` 排最前）；无凭证条目在视图边界转成 `app: None`。
    pub fn snapshot(&self, filter: &UsageFilter) -> Vec<UsageEntry> {
        let map = self.inner.read().unwrap();
        map.iter()
            .filter(|((date, app), _)| {
                if let Some(want) = &filter.app {
                    if app != want {
                        return false;
                    }
                }
                if let Some(day) = filter.date {
                    if *date != day {
                        return false;
                    }
                }
                if let Some(from) = filter.from {
                    if *date < from {
                        return false;
                    }
                }
                if let Some(to) = filter.to {
                    if *date > to {
                        return false;
                    }
                }
                true
            })
            .map(|((date, app), usage)| UsageEntry {
                date: date.to_string(),
                app: if app.is_empty() {
                    None
                } else {
                    Some(app.clone())
                },
                input: usage.input,
                output: usage.output,
                cache_read: usage.cache_read,
                cache_write: usage.cache_write,
                total: usage.total(),
            })
            .collect()
    }

    /// dirty 才写盘（全量原子写，`config::save_usage`）；否则完全不动文件——
    /// 不创建、不改 mtime（钉住「flush 不是无脑周期写」）。
    pub fn flush_if_dirty(&self, path: &Path) -> Result<(), ApiError> {
        if !self.dirty.load(Ordering::Acquire) {
            return Ok(());
        }
        let entries = self.snapshot(&UsageFilter::default());
        crate::config::save_usage(path, &entries)?;
        self.dirty.store(false, Ordering::Release);
        Ok(())
    }

    /// 启动加载：整表替换。日期解析失败的行跳过（只告警）；同一
    /// `(date, app)` 的重复行合并。加载后 dirty = false——刚从盘上读进来的
    /// 数据不需要写回。
    pub fn set_all(&self, entries: Vec<UsageEntry>) {
        let mut map = self.inner.write().unwrap();
        map.clear();
        for entry in entries {
            let Ok(date) = NaiveDate::parse_from_str(&entry.date, "%Y-%m-%d") else {
                tracing::warn!("usage entry with unparsable date '{}' skipped", entry.date);
                continue;
            };
            let app = entry.app.unwrap_or_default();
            let usage = Usage {
                input: entry.input,
                output: entry.output,
                cache_read: entry.cache_read,
                cache_write: entry.cache_write,
            };
            map.entry((date, app))
                .and_modify(|row| *row = *row + usage)
                .or_insert(usage);
        }
        self.dirty.store(false, Ordering::Release);
    }
}

/// 周期 flush 任务（ADR-L21）：每 `GATEWAY_USAGE_FLUSH_SECS` 秒写一次盘；
/// `0` = 不起周期任务，仅 shutdown flush。写盘失败只告警——账本丢一个窗口
/// 不影响配额与转发（R15）。
pub fn spawn_flush(state: AppState) -> tokio::task::JoinHandle<()> {
    if state.settings.usage_flush_secs == 0 {
        tracing::info!(
            "usage ledger flush task disabled (GATEWAY_USAGE_FLUSH_SECS=0); flush at shutdown only"
        );
        return tokio::spawn(async {});
    }
    tokio::spawn(async move {
        let interval = state.settings.usage_flush_secs;
        loop {
            if let Err(e) = state.llm_usage.flush_if_dirty(&state.settings.usage_file) {
                tracing::error!("usage ledger flush failed: {e}");
            }
            tokio::time::sleep(Duration::from_secs(interval)).await;
        }
    })
}
