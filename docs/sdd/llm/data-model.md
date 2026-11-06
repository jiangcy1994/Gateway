# 数据模型（LLM 子网关）

> **SDD 第 3 步**：核心类型与不变式。依赖第 2 步的模块划分——类型绝不超出架构边界。

数据模型分三层：**配置值对象**（`Protocol` / `Model` / `Provider` / `ProviderPatch` / `App` / `AppPatch`）、**运行期状态**（`ProviderTable` / `AppTable` / `QuotaTracker` / `UsageLedger` / `TailBuffer`）、**观测快照**（`QuotaSnapshot` / `UsageEntry`）。

## 核心枚举：`Protocol`（provider.rs）

```rust
enum Protocol { Openai, Anthropic, Systemone }
```

- 序列化 / URL 协议段：`"openai"` / `"anthropic"` / `"systemone"`（全小写）。
- URL 段匹配大小写不敏感（`/llm/OpenAI/...` 归一化为小写后匹配）。
- **不变式 P0**：`Systemone` 参与 ProviderTable / 选择 / 转发的全部通用路径；仅 `extract_tokens` 未实现其 usage 解析（桩，ADR-L8）；它也**不参与 v1 端点推断**（ADR-L12），只能经显式协议段访问。

## 核心函数：`infer_protocol`（provider.rs）

`/llm/v1/{端点}` 的端点首段 → `Protocol` 的闭表推断（ADR-L12）。输入先转 ASCII 小写；`messages`/`complete` → anthropic，`chat`/`completions`/`responses`/`embeddings`/`moderations`/`audio`/`images`/`fine_tuning`/`batches`/`uploads`/`threads`/`assistants` → openai，`models`/`files` → `Err(Ambiguous)`，其余 → `Err(Unknown)`。闭表封闭：新端点必须显式登记，不允许前缀/模糊匹配。

## 核心值对象：`Model`（provider.rs）

一个供应商对外提供的一个**网关侧模型名**。`model` 字段在请求 body 里出现时即按此表解析。

| 字段 | 类型 | 约束 |
|---|---|---|
| `name` | `String` | 网关模型名（app 组合里写它）；非空；**provider 内唯一** |
| `upstream` | `Option<String>` | 上游真实模型名；缺省 = `name`；给值时非空 |
| `priority` | `i32` | 越大越优先；默认 0；由 balancer 分层使用（ADR-L16） |

- serde 默认值：`upstream` / `priority` 均可省略（`#[serde(default)]`）。
- **不变式 P7**：`Model.name` 非空且在**同一 provider 的 models 表内唯一**（`Provider::validate`）。跨供应商可以同名（两个供应商都声明 `chat-pro` 是常态，正是分层的意义）。
- **不变式 P8**：`Model.upstream` 为 `Some` 时非空（`Provider::validate`）。

## 核心值对象：`Provider`（provider.rs）

一个 LLM 供应商。同时是 TOML 文件、管理 API、内存表的共享表示（与 `Route` 同构的共享表示原则）。

| 字段 | 类型 | 约束 |
|---|---|---|
| `id` | `String` | 非空；表内唯一；`PUT` 不可改；POST 省略时自动生成 UUID v4 |
| `name` | `String` | 展示名，默认空串 |
| `protocols` | `Vec<Protocol>` | 至少一个、不可重复；反序列化同时接受列表与旧形态单值（`protocol = "openai"`，ADR-L13）；序列化只产出列表 |
| `base_url` | `String` | `http://` 或 `https://` 且含 host；不以 `/` 结尾（校验时自动 trim） |
| `api_key` | `String` | 非空（网关按协议注入到上游请求，客户端的凭证一律被替换） |
| `rpm_limit` | `Option<u64>` | `Some(n)` 时 `n > 0`；`None` = 不限 RPM |
| `tpm_limit` | `Option<u64>` | 同上 |
| `models` | `Vec<Model>` | 默认空表；**空表 = 协议级兜底**：参与任意 model 请求的选择、不改写 `model` 字段，effective priority = `i32::MIN`（ADR-L14） |
| `timeout_secs` | `Option<u64>` | `Some(n)` 时 `n > 0`；缺省 = 回落 `GATEWAY_DEFAULT_TIMEOUT`（ADR-L17） |
| `proxy_url` | `Option<String>` | 缺省 = 共享直连 client（`state.client`）；给值时 scheme ∈ {http,https}、host 非空且可被 `reqwest::Proxy::all` 解析（ADR-L19） |
| `enabled` | `bool` | 默认 `true`；`false` 时**该供应商**被选择排除（协议池全禁 → 503） |

### 不变式

| # | 不变式 | 由谁保证 |
|---|---|---|
| P1 | `base_url` scheme ∈ {http, https} 且 host 非空 | `Provider::validate`（管理 API 与启动加载都调） |
| P2 | 同一 `ProviderTable` 内 `id` 唯一 | `ProviderTable::add`/`replace`（写锁内检查） |
| P3 | `api_key` 非空 | `Provider::validate` |
| P4 | 限额 `Some(n)` → `n > 0` | `Provider::validate` |
| P5 | 限额是**供应商级**的：`rpm_limit` / `tpm_limit` 描述该供应商名下所有模型共享的上限，计数不按 model 分桶 | 结构本身（`Provider` 无 model 字段） |
| P6 | `protocols` 非空且无重复；一个供应商说多个协议时同时进入多个协议池，限额**按供应商记一份**（跨协议共用） | `Provider::validate` + tracker 按 provider id 存放（P5 的自然推论） |
| P7 | `Model.name` 非空且在同一 provider 的 `models` 表内唯一 | `Provider::validate` |
| P8 | `Model.upstream` 为 `Some` 时非空 | `Provider::validate` |
| P9 | 空模型表 = 协议级兜底（effective priority = `i32::MIN`，不改写 body）；模型表与限额正交——限额仍按供应商记一份（P5 不因模型表改变） | 结构本身 + balancer 的分层规则 |
| P10 | `timeout_secs` 为 `Some(n)` 时 `n > 0` | `Provider::validate` |
| P14 | `proxy_url` 为 `Some(u)` 时：scheme ∈ {http, https}、host 非空，且 `u` 可被 `reqwest::Proxy::all(u)` 解析；socks5 等本构建未启用 `socks` feature 的形态判非法 | `Provider::validate`（配置时 400，而非请求时 500） |

## 核心值对象：`App`（app.rs）

一个**调用方应用**：以入站凭证（`Bearer` / `x-api-key`）识别，限定其可用的网关模型集合。

| 字段 | 类型 | 约束 |
|---|---|---|
| `id` | `String` | 非空；表内唯一；`PUT` 不可改；POST 省略时自动生成 UUID v4 |
| `name` | `String` | 展示名，默认空串 |
| `key` | `String` | 非空；表内唯一；POST 省略 → 自动生成 `sk-gw-<uuid v4 simple>` |
| `models` | `Vec<String>` | 允许的**网关模型名**；空表 = 不限制（默认策略） |
| `enabled` | `bool` | 默认 `true`；`false` → 命中该 app 的请求 403 |

不变式：

| # | 不变式 | 由谁保证 |
|---|---|---|
| P11 | `App.key` 非空且在同一 `AppTable` 内唯一 | `AppTable::add`/`replace`（写锁内检查） |
| P12 | `App.id` 在表内唯一 | `AppTable::add`/`replace` |
| P13 | `models` 引用的是**网关模型名**（provider 声明的 `Model.name`）；表内不做外键校验——组合允许引用尚无供应商声明的模型（供应商下线不应让 app 配置失效） | 结构本身（约定） |

> key 比较必须走**常量时间**（复用 `admin.rs` 的 `constant_time_eq`），避免按响应时序泄露 key 前缀。

> `App.models` 为空 = 不限制：无凭证（app = None）的请求与此形态语义一致，即默认策略 = 全部已映射模型（ADR-L15）。

> 与 `Route` 同宽容策略：`load_providers` 从磁盘读到重复 id 时只告警并全部保留。
> 字段兼容（ADR-L13）：磁盘上的旧文件若写 `protocol = "openai"` 单值仍能读入（serde alias），下一次原子写回即迁移为 `protocols = [...]`；两个键都没有则解析失败（保持缺字段的大错行为）。

## 运行期状态

### `ProviderTable`（provider.rs）

```rust
ProviderTable { providers: Arc<RwLock<Vec<Provider>>> }
```

线性表（与 `RouteTable` 同构）。`list_enabled_by_protocol(protocol)` 仍是无 model 请求的入口；带 model 的请求走 `pool_for(protocol, model)`（见 `interfaces.md`）。

### `ClientCache`（client.rs，ADR-L19）

```rust
ClientCache { clients: Arc<RwLock<HashMap<String, reqwest::Client>>> }
```

按 `proxy_url` 复用 reqwest client（每个代理 url 一个自带连接池的 client）；`None` 永远映射到 `AppState.client`（共享直连 client），不进缓存表。缓存只增不删；代理 url 变更后旧 client 自然淘汰。

### `AppTable`（app.rs）

```rust
AppTable { apps: Arc<RwLock<Vec<App>>> }
```

线性表（与 `ProviderTable` 同构）。`find_by_key(key)` 是转发热路径的入口（常量时间比较）；CRUD 方法镜像 `ProviderTable`。

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

### `Usage`（usage.rs，ADR-L20）

跨协议对齐的 token 分桶明细（`input` / `output` / `cache_read` / `cache_write`，均 `u64`，`Default` = 全 0）。`total = input + output + cache_read + cache_write`（由 `total()` 计算，不存字段）。归一化规则（提取时应用，见 `interfaces.md`）：

| 协议 | input | output | cache_read | cache_write |
|---|---|---|---|---|
| openai | `prompt_tokens - cached_tokens`（saturating） | `completion_tokens` | `prompt_tokens_details.cached_tokens` | 恒 0（上游无此字段） |
| anthropic | `input_tokens`（本就不含缓存） | `output_tokens` | `cache_read_input_tokens` | `cache_creation_input_tokens` |
| systemone | 恒 0（桩，ADR-L8/L20） | 同左 | 同左 | 同左 |

- **不变式 P15**：`Usage` 的四桶**互不重叠**且覆盖 `extract_tokens` 所计总数的主要来源——openai 的 `reasoning_tokens` 属 `output` 子集，不单列（R16）；anthropic 的 `cache_read`/`cache_write` 属 input 侧的配套计费，不计入 `input`。
- **不变式 P16**：`Usage` 只由 `extract_usage` 从上游响应尾包产出；无 usage / 解析失败 / 流被中断 → 全 0（best-effort，与 R1 同源）。

### `UsageLedger`（ledger.rs，ADR-L20）

```rust
UsageLedger {
    inner: RwLock<BTreeMap<(NaiveDate, String), Usage>>,   // key: (日期 UTC, app id；无凭证 = "")
    dirty: AtomicBool,
}
```

- 主键是 **(UTC 日期, app id)**：`record` 在响应记账时以 `Utc::now().date_naive()` 取当天日期——**日期按记账时刻而非请求开始时刻**，跨日界还在飞的响应记到新的一天。
- 无凭证流量（默认策略，app = None）以空串 app 入账；**在视图边界转回 `None`**（TOML 行内 `app = ""`，JSON 响应里 `app: null`）——空串只是存储哨兵，不是可配置的 app id。
- 聚合维度为 app 而非供应商：账本回答「哪个应用哪天用了多少 token」，与配额（供应商维度）正交；同一响应不会被按供应商再记一份。
- `BTreeMap` 天然按 `(date, app)` 排序——admin 视图与落盘顺序即 map 迭代序，无需另排。
- dirty 位：`record` 置位，`flush_if_dirty` 落盘后清除；**flush 任务只在 dirty 时写盘**，空闲窗口不做无谓 IO。
- 并发：`record` 只持有 ledger 写锁（在 `UsageTap::finish` 中、quota 记账之后，**不与任何 quota 锁嵌套**）；`flush_if_dirty` 取读锁快照后在锁外序列化——flush 与 record 可以交错，但 dirty 仍在，**不丢条目，只可能延迟一个 flush 窗口**。

## 观测快照（`QuotaSnapshot`，序列化形态）

`GET /__admin/llm/quota` 的每条记录：

| 字段 | 类型 | 说明 |
|---|---|---|
| `id` / `protocols` / `enabled` | `String`/`Vec<String>`/`bool` | 冗余自 `Provider`；多协议供应商只此一条配额记录 |
| `rpm` / `tpm` | `{ limit: Option<u64>, used: u64, remaining: Option<u64> }` | `limit=null` 表示不限；`remaining=null` 同义 |
| `window_secs` | `u64` | 常量 60 |

`GET /__admin/llm/metrics` 每条 = `Provider` + `RouteStats::snapshot()`（与 `/__admin/metrics` 的 stats 形态完全一致）。

## 观测快照（`UsageEntry`，序列化形态，ADR-L20）

`GET /__admin/llm/usage` 的每条记录，同时是 `usage.toml` 的行结构：

| 字段 | 类型 | 说明 |
|---|---|---|
| `date` | `String`（`YYYY-MM-DD`） | UTC 日期；字符串形态规避 toml 对 chrono feature 的要求 |
| `app` | `Option<String>` | 应用 id；`None` = 无凭证流量（存储哨兵为空串，见 P15/P16） |
| `input` / `output` / `cache_read` / `cache_write` | `u64` | 分桶明细 |
| `total` | `u64` | 各桶之和；**序列化时填入、反序列化时忽略**（以桶为准，兼容手工编辑） |

## 持久化形态（providers.toml）

```toml
# top-gateway providers - managed file, rewrites drop comments
[[providers]]
id = "shared-vendor"
name = "Shared Vendor"
protocols = ["openai", "anthropic"]
base_url = "https://api.shared-vendor.example"
api_key = "sk-..."
rpm_limit = 500
tpm_limit = 150000
timeout_secs = 30
proxy_url = "http://127.0.0.1:7890"   # 缺省 = 直连（共享 client，ADR-L19）
enabled = true

  [[providers.models]]
  name = "chat-pro"
  upstream = "chat-pro-2026-04"
  priority = 10

  [[providers.models]]
  name = "chat-max"      # upstream 缺省 = name；priority 缺省 = 0
```

`ProvidersFile { providers: Vec<Provider> }` → `toml::to_string_pretty`；原子替换语义与 routes 文件相同（同目录临时文件 + rename，重写丢注释）。`api_key` 以明文落盘——与 `Route.headers` 注入密钥的处理一致（ADR-L11）。

## 持久化形态（apps.toml，ADR-L18）

```toml
# top-gateway apps - managed file, rewrites drop comments
[[apps]]
id = "cli-code"
name = "Claude Code CLI"
key = "sk-gw-cli-code-xxxx"
models = ["chat-pro", "chat-max"]
enabled = true

[[apps]]
id = "cherry-studio"
models = []            # 空表 = 不限制（默认策略）
```

`AppsFile { apps: Vec<App> }`；缺文件非致命、原子写、重写丢注释——与 providers 文件完全同构。

## 持久化形态（usage.toml，ADR-L20）

```toml
# top-gateway usage - managed file, rewrites drop comments
[[usage]]
date = "2026-11-05"
app = "cli-code"       # 无凭证流量此行为空串
input = 12340
output = 890
cache_read = 2100
cache_write = 0
total = 15330

[[usage]]
date = "2026-11-06"
app = ""
input = 500
output = 120
cache_read = 0
cache_write = 0
total = 620
```

`UsageFile { usage: Vec<UsageEntry> }`；缺文件非致命、原子写、重写丢注释；**全量写**（不做增量追加）。加载时 `(date, app)` 重复行合并累加；`total` 反序列化时忽略。**这是观测数据而非配置**：没有任何管理 API 能写它，唯一的写入者是 flush 任务与 shutdown flush（手工编辑可读，但会在下次落盘时被覆盖）。

## 数据流上的形态

```
providers.toml ──load_providers──→ Vec<Provider> ──set_all──→ ProviderTable
                                        ▲                        │
                                        │                        ├─ list_enabled_by_protocol → balancer（无 model 请求）
                                        │                        ├─ pool_for(protocol, model) → balancer（带 model 请求）
                                        │                        ├─ add/replace/remove ← /__admin/llm/providers
                                        │                        └──── save_providers（原子写回）──┘
                                        │
apps.toml ──load_apps──→ Vec<App> ──set_all──→ AppTable
                                  ▲              │
                                  │              ├─ find_by_key → forward（入站凭证）
                                  │              ├─ add/replace/remove ← /__admin/llm/apps
                                  │              └──── save_apps（原子写回）──┘
                                  │
请求 body（≤16MiB）→ 取 model → app 组合校验 → balancer → 改写 model → 上游
Provider.proxy_url ──ClientCache.get──→ forward（选 client：None = 共享直连，Some = 按代理复用）
选择/派遣 ──同一临界区──→ QuotaTracker.requests（RPM 计数）
响应流 tap ──TailBuffer→ extract_tokens ──→ QuotaTracker.tokens（TPM 记账）
                └──────→ extract_usage ──→ UsageLedger.record(app_id, 当日 UTC, Usage)
UsageLedger ──snapshot + UsageFilter──→ /__admin/llm/usage（只读，不写盘）
UsageLedger ──flush_if_dirty（周期任务 / shutdown）──→ usage.toml（全量原子写回）
usage.toml ──load_usage──→ Vec<UsageEntry> ──set_all──→ UsageLedger（启动恢复）
QuotaTracker / RouteStats ──快照──→ /__admin/llm/quota · /__admin/llm/metrics
```
