# 数据模型（LLM 子网关）

> **SDD 第 3 步**：核心类型与不变式。依赖第 2 步的模块划分——类型绝不超出架构边界。

数据模型分三层：**配置值对象**（`Protocol` / `Provider` / `ProviderPatch`）、**运行期状态**（`ProviderTable` / `QuotaTracker` / `TailBuffer`）、**观测快照**（`QuotaSnapshot`）。

## 核心枚举：`Protocol`（provider.rs）

```rust
enum Protocol { Openai, Anthropic, Systemone }
```

- 序列化 / URL 协议段：`"openai"` / `"anthropic"` / `"systemone"`（全小写）。
- URL 段匹配大小写不敏感（`/llm/OpenAI/...` 归一化为小写后匹配）。
- **不变式 P0**：`Systemone` 参与 ProviderTable / 选择 / 转发的全部通用路径；仅 `extract_tokens` 未实现其 usage 解析（桩，ADR-L8）。

## 核心值对象：`Provider`（provider.rs）

一个 LLM 供应商。同时是 TOML 文件、管理 API、内存表的共享表示（与 `Route` 同构的共享表示原则）。

| 字段 | 类型 | 约束 |
|---|---|---|
| `id` | `String` | 非空；表内唯一；`PUT` 不可改；POST 省略时自动生成 UUID v4 |
| `name` | `String` | 展示名，默认空串 |
| `protocol` | `Protocol` | 必填 |
| `base_url` | `String` | `http://` 或 `https://` 且含 host；不以 `/` 结尾（校验时自动 trim） |
| `api_key` | `String` | 非空（网关按协议注入到上游请求，客户端的凭证一律被替换） |
| `rpm_limit` | `Option<u64>` | `Some(n)` 时 `n > 0`；`None` = 不限 RPM |
| `tpm_limit` | `Option<u64>` | 同上 |
| `enabled` | `bool` | 默认 `true`；`false` 时**该供应商**被选择排除（协议池全禁 → 503） |

### 不变式

| # | 不变式 | 由谁保证 |
|---|---|---|
| P1 | `base_url` scheme ∈ {http, https} 且 host 非空 | `Provider::validate`（管理 API 与启动加载都调） |
| P2 | 同一 `ProviderTable` 内 `id` 唯一 | `ProviderTable::add`/`replace`（写锁内检查） |
| P3 | `api_key` 非空 | `Provider::validate` |
| P4 | 限额 `Some(n)` → `n > 0` | `Provider::validate` |
| P5 | 限额是**供应商级**的：`rpm_limit` / `tpm_limit` 描述该供应商名下所有模型共享的上限，计数不按 model 分桶 | 结构本身（`Provider` 无 model 字段） |

> 与 `Route` 同宽容策略：`load_providers` 从磁盘读到重复 id 时只告警并全部保留。

## 运行期状态

### `ProviderTable`（provider.rs）

```rust
ProviderTable { providers: Arc<RwLock<Vec<Provider>>> }
```

线性表（与 `RouteTable` 同构）。`list_enabled_by_protocol(protocol)` 是转发热路径的入口。

### `QuotaTracker`（quota.rs）

```rust
QuotaTracker {
    inner: Mutex<QuotaInner>,     // 永远按 ProviderTable 表序加锁，无跨表序加锁
    window: Duration,             // 常量 60s
}
QuotaInner {
    requests: VecDeque<Instant>,        // 每次派遣一条
    tokens:    VecDeque<(Instant, u64)>, // 每次 usage 记账一条（可能为 0）
}
```

- **RPM 在派遣时计数**（`select_and_record` 的同一临界区内），早于上游响应。
- **TPM 在响应记账时累加**：`record_tokens(n)` 追加一条；`n == 0` 也追加（「无 usage」与「未完成」同样占用请求历史，但 token 为 0）。
- 每次读（剩余量 / 快照）先**淘汰**窗口外条目（`while front < now - window`）。
- 在途（已派遣、未完成）请求的 token 尚未入账 → 窗口内 TPM 略低于真实占用（风险 R2）。
- tracker 按 provider id 存在 `AppState` 的独立 map；供应商删除后 tracker 保留（历史计数不丢），配额视图只 join 当前表。

### `TailBuffer`（usage.rs）

```rust
TailBuffer { buf: Vec<u8>, cap: usize }   // cap = 64 KiB
```

- `push(chunk)`：追加后若超过 cap，**从前截断**（保留尾部）。
- 提取时只看尾部：非流式 JSON 的 `usage` 在包尾；SSE 的 usage 在最后几个 `data:` 块。

## 观测快照（`QuotaSnapshot`，序列化形态）

`GET /__admin/llm/quota` 的每条记录：

| 字段 | 类型 | 说明 |
|---|---|---|
| `id` / `protocol` / `enabled` | `String`/`String`/`bool` | 冗余自 `Provider` |
| `rpm` / `tpm` | `{ limit: Option<u64>, used: u64, remaining: Option<u64> }` | `limit=null` 表示不限；`remaining=null` 同义 |
| `window_secs` | `u64` | 常量 60 |

`GET /__admin/llm/metrics` 每条 = `Provider` + `RouteStats::snapshot()`（与 `/__admin/metrics` 的 stats 形态完全一致）。

## 持久化形态

```toml
# top-gateway providers - managed file, rewrites drop comments
[[providers]]
id = "openai-official"
name = "OpenAI"
protocol = "openai"
base_url = "https://api.openai.com"
api_key = "sk-..."
rpm_limit = 500
tpm_limit = 150000
enabled = true
```

`ProvidersFile { providers: Vec<Provider> }` → `toml::to_string_pretty`；原子替换语义与 routes 文件相同（同目录临时文件 + rename，重写丢注释）。`api_key` 以明文落盘——与 `Route.headers` 注入密钥的处理一致（ADR-L11）。

## 数据流上的形态

```
providers.toml ──load_providers──→ Vec<Provider> ──set_all──→ ProviderTable
                                        ▲                        │
                                        │                        ├─ list_enabled_by_protocol → balancer
                                        │                        ├─ add/replace/remove ← /__admin/llm
                                        │                        └──── save_providers（原子写回）──┘
                                        │
选择/派遣 ──同一临界区──→ QuotaTracker.requests（RPM 计数）
响应流 tap ──TailBuffer→ extract_tokens ──→ QuotaTracker.tokens（TPM 记账）
QuotaTracker / RouteStats ──快照──→ /__admin/llm/quota · /__admin/llm/metrics
```
