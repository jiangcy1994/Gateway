# 接口契约（LLM 子网关）

> **SDD 第 4 步**：签名与契约。依赖第 3 步的数据类型——签名里出现的类型必须先定义过。
>
> 本文件是 LLM 子网关**实现的唯一权威**。签名、前后置条件变化必须先改这里，再改代码；测试（`tests/llm_gateway.rs`）与映射（`docs/tdd/mapping.md`）以本文件为准。top-gateway 通用行为（鉴权、错误体格式、持久化原子性）以 `../interfaces.md` 为准，此处不重复，只写差异与新增。

---

## provider.rs

### `Protocol`（枚举）

变体 `Openai` / `Anthropic` / `Systemone`；serde 小写字符串。

- `from_url_segment(&str) -> Option<Protocol>`：输入先转 ASCII 小写再匹配 `{openai, anthropic, systemone}`；其余 `None`。

### `InferError`（枚举，ADR-L12）

`/llm/v1/{端点}` 推断失败的原因：`Ambiguous`（端点两协议族都有，无法无歧义判定）/ `Unknown`（端点不在推断闭表内）。

### `infer_protocol(endpoint: &str) -> Result<Protocol, InferError>`

`/llm/v1/{端点}` 的端点段 → 协议推断**闭表**。输入为 `v1/` 之后的**首段**（`messages/count_tokens` 归 `messages`），先转 ASCII 小写再匹配；推断结果只用于选池与凭证注入，不改变转发路径。

| 端点首段 | 推断结果 |
|---|---|
| `messages`、`complete` | `Anthropic` |
| `chat`、`completions`、`responses`、`embeddings`、`moderations`、`audio`、`images`、`fine_tuning`、`batches`、`uploads`、`threads`、`assistants` | `Openai` |
| `models`、`files` | `Err(Ambiguous)` |
| 其余 | `Err(Unknown)` |

systemone 不参与推断（只能经显式协议段 `/llm/systemone/...` 访问）。

### `Provider`（结构体）

字段与不变式见 `data-model.md`（P1–P6）。实现 `Serialize/Deserialize`（TOML 与 JSON 共用同一结构）。`protocols` 字段反序列化同时接受列表与**旧形态单值**（`#[serde(alias = "protocol")]`，见 ADR-L13）；序列化只产出列表形态。

### `Provider::validate(&self) -> Result<(), String>`

- 前置：字段已填充。
- 后置：全部规则通过返回 `Ok(())`；任一失败 `Err(原因)`（管理 API 映射 400）。
- 规则（顺序即错误优先级）：`id` 非空 → `protocols` 非空 → `protocols` 无重复 → `base_url` scheme ∈ {http,https} → `base_url` host 非空 → `api_key` 非空 → `rpm_limit`/`tpm_limit` 为 `Some` 时 `> 0`。

### `Provider::apply_patch(&self, patch: ProviderPatch) -> Provider`

- 后置：`Some` 字段覆盖，`None` 保留；**`id` 永远来自 `self`**；不校验（调用方 `replace` 前调 `validate`）。

### `ProviderTable::{set_all, list, get, add, replace, remove, list_enabled_by_protocol}`

| 方法 | 签名 | 后置 | 错误 |
|---|---|---|---|
| `set_all` | `(&self, Vec<Provider>)` | 整表替换（启动加载用） | 不抛 |
| `list` | `(&self) -> Vec<Provider>` | 全量克隆，表内顺序 | 不抛 |
| `get` | `(&self, id: &str) -> Option<Provider>` | 按 id 查 | 不抛 |
| `add` | `(&self, Provider) -> Result<(), ApiError>` | 追加表尾 | `DuplicateId`（400） |
| `replace` | `(&self, id: &str, Provider) -> Result<(), ApiError>` | 原地替换 | `ProviderNotFound`（404）/ `DuplicateId`（排除自身位置后比较） |
| `remove` | `(&self, id: &str) -> Option<Provider>` | 移除并返回 | 不抛 |
| `list_enabled_by_protocol` | `(&self, protocol) -> Vec<Provider>` | `enabled && protocols` **包含**该协议，表内顺序；多协议供应商同时出现在多个协议池里 | 不抛 |

---

## quota.rs

### `QuotaTracker`（结构体）

构造：`QuotaTracker::new()`，窗口恒为 60s。

| 方法 | 签名 | 调用方 | 后置 / 错误 |
|---|---|---|---|
| `record_request` | `(&self)` | balancer（**选择临界区内**） | 淘汰窗口外记录后追加 `Instant::now()`；锁中毒 panic（与 `RouteTable` 同策略） |
| `record_tokens` | `(&self, tokens: u64)` | forward 的 usage tap | 同上；`tokens==0` 也记 |
| `remaining_rpm` | `(&self, limit: Option<u64>) -> Option<u64>` | balancer | 先淘汰；`None` → `None`（不限）；`Some(l)` → `l.saturating_sub(used)` |
| `remaining_tpm` | 同上 | balancer | 同上 |
| `snapshot` | `(&self, id: &str, protocols: &[Protocol], enabled, rpm_limit, tpm_limit) -> QuotaSnapshot` | admin | 先淘汰；`used` = 窗口内条数 / token 和；`protocols` 为供应商声明的全部协议（多协议供应商只此一条记录） |

不变式（并发）：`Mutex` 永远按 **ProviderTable 表序**获取；`select_and_record` 把所有候选 tracker 的锁**一次性全部持有**后再算权重，期间不做任何 await。

---

## balancer.rs

### `select_and_record(state: &AppState, protocol: Protocol) -> Result<Provider, ApiError>`

负载均衡的单一入口。

- 前置：`state.providers` 与 `state.llm_quotas` 可用。
- 后置（按判定顺序）：
  1. `list_enabled_by_protocol(protocol)` 为空 → `Err(NoProviderForProtocol(protocol))`（→ **503**）。
  2. 为每个候选取（缺则创建）`Arc<QuotaTracker>`，**按表序**全部加锁。
  3. 各供应商淘汰旧记录后算权重：`weight = min(rpm_ratio, tpm_ratio)`；未设限额的维度比例按 `1.0`；`weight == 0` 的供应商退出选择。
  4. **全部 weight == 0** → `Err(QuotaExhausted(protocol))`（→ **429**）。
  5. 其余按 weight 加权随机（`rand::WeightedIndex<f64>`）选中一个 → **同一锁内** `record_request()` → 返回该 `Provider` 克隆。
- 错误：除上述两个变体不产生其他响应形态。
- 并发：第 5 步的「选 + 记」处于同一临界区，因此并发请求不会重复选走只剩 1 RPM 的供应商。

---

## usage.rs

### `TailBuffer`

- `new()`：cap = 64 KiB。`push(&[u8])`：追加并从前截断到 cap。`tail() -> &[u8]`。

### `extract_tokens(protocol, content_type: Option<&str>, tail: &[u8]) -> u64`

从响应尾包提取 token 总数。**best-effort，永不失败**（解析不到返回 0）。

| 协议 | `content-type` 含 `text/event-stream` | 其余（JSON 等） |
|---|---|---|
| openai | 逐行取 `data:` 前缀的 JSON，每个含 `usage` 对象的块计入：`total_tokens`（有则取它），否则 `prompt_tokens + completion_tokens` | 从尾包找最后一个 `"usage"` 键，取其 JSON 对象，同上规则 |
| anthropic | 汇总所有含 `usage` 的块：`input_tokens` 取**首个出现的值**，`output_tokens` 取**最后出现的值**（Anthropic 的 message_delta usage 为累计值） | 同左（非流式响应只有一个 usage） |
| systemone | **桩：恒返回 0**（ADR-L8） | 同左 |

- `"usage"` 键定位：逐字节查找带引号字面量，其后跳过空白与 `:`，以花括号深度计数（含字符串状态机）截取完整 JSON 对象后反序列化；失败则继续找下一个出现位置。
- SSE 行解析：只处理以 `data:` 开头的行，去掉前缀与一个可选空格；跨缓冲区边界的截断行解析失败即跳过（尾包保留的是流末尾，用法块总在末尾完整）。

---

## forward.rs

### `forward(State<AppState>, Method, OriginalUri, HeaderMap, Request) -> Response`

`/llm` nest 的 fallback 处理器。

- 前置：无。
- 后置（按判定顺序）：
  1. 剥掉 `/llm` 前缀取剩余 path 与首段，按**两种入站形态**分流（ADR-L12）：
     - **显式协议段** `/llm/{protocol}/rest`（首段不是 `v1`，大小写不敏感）：首段空 → `404`（「missing protocol segment」）；`Protocol::from_url_segment` 为 `None` → `404`（消息含未知段名）。剩余路径 `rest` 原样转发，空则补 `/`。
     - **v1 推断** `/llm/v1/{端点}`（首段是 `v1`，大小写不敏感）：`v1/` 后无端点段 → `404`（「missing endpoint after v1」）；`infer_protocol(端点首段)` 为 `Err(Ambiguous)` → `404`（消息含端点名并提示改用显式协议段）；`Err(Unknown)` → `404`（消息含端点名）。推断出的协议只决定选池与凭证注入；**转发路径恒为 `/{首段原样}/{端点及剩余}`**（保留客户端的大小写写法）。systemone 永不由推断产出。
     无论哪种形态，查询串都原样拼接。
  2. `select_and_record` 失败按其错误映射（503 / 429，house 错误体）。
  3. `state.llm_stats_for(provider.id).guard()`（Drop 记账）。
  4. 上游 URL = `base_url.trim_end_matches('/') + 剩余路径`。
  5. 请求头：剥离集 = top-gateway 的 hop-by-hop + `content-length` + `host` + `authorization` + `x-api-key`；按协议注入供应商凭证：
     - `openai` / `systemone`：`Authorization: Bearer <api_key>`
     - `anthropic`：`x-api-key: <api_key>`；`anthropic-version` **不存在时**补 `2023-06-01`（客户端已传则保留原值）
  6. 附 `x-forwarded-host` / `x-forwarded-path`（同 proxy 行为）。
  7. `settings.default_timeout_secs` 包裹 `send()`：传输错误 → `502`（消息含 provider id）；超时 → `504`；均 `record_error()`。
  8. 响应头：剥 hop-by-hop + `content-length`；**注入 `x-llm-provider: <provider id>`**。
  9. body：`Body::from_stream(usage tap)`——每块向客户端转发的同时复制进 `TailBuffer`；流正常结束 / 上游流出错 / **消费者断开** 三种情形都触发一次 `extract_tokens → record_tokens`（互斥，只记一次）。
  10. 上游 5xx：状态与 body 原样透传，同时计一次错误；4xx 原样透传不计错误。
- 错误体：全部使用 `proxy::error_response` 的 house 格式。

---

## admin.rs（`/__admin/llm/*`）

挂载于既有 `admin_router()` 内的 `nest("/llm", ...)`。**鉴权与路由管理端点完全相同**（`require_admin`，同 `X-Admin-Key` / `Bearer` 密钥，同一 `GATEWAY_ADMIN_KEY`）。

| 方法 | 路径 | 成功响应 | 错误 |
|---|---|---|---|
| GET | `/__admin/llm/providers` | `200 {"providers":[...]}` | 401 |
| POST | `/__admin/llm/providers` | `201` + 新供应商 JSON；`id` 省略 → UUID v4 | 400（非法/重复 id）、401、422（JSON 体缺必填字段，axum `Json` 提取器标准行为，与路由 API 一致） |
| GET | `/__admin/llm/providers/{id}` | `200` + 单供应商 | 404、401 |
| PUT | `/__admin/llm/providers/{id}` | `200` + 合并后的供应商 | 400、404、401 |
| DELETE | `/__admin/llm/providers/{id}` | `200 {"deleted":<id>}` | 404、401 |
| GET | `/__admin/llm/quota` | `200 {"providers":[QuotaSnapshot...]}`，按表序 join 当前表 | 401 |
| GET | `/__admin/llm/metrics` | `200 {"providers":[{"provider":...,"stats":...}]}` | 401 |

- 所有变更端点成功后先 `state.persist_providers()` 再返回；持久化失败 → 500。
- `ProviderInput`（POST body）：`id`/`name` Option（id 省略生成），`protocols` 必填（接受单值或列表，见 ADR-L13），`base_url`/`api_key` 必填，`rpm_limit`/`tpm_limit`/`enabled` Option。
- `ProviderPatch`（PUT body）：全 Option，与 `RoutePatch` 同构的合并语义。**`rpm_limit` / `tpm_limit` 为 `Option<Option<u64>>`**：字段缺省 = 保留原值；显式 `null` = 设为不限；数字 = 设新限额。`protocols` 为 `Option<Vec<Protocol>>`：缺省或显式 `null` = 保留原值；给列表（或单值）= 整体替换。

---

## config.rs（新增）

### `Settings` 新增字段

| 环境变量 | 字段 | 默认 | 说明 |
|---|---|---|---|
| `GATEWAY_PROVIDERS_FILE` | `providers_file: PathBuf` | `providers.toml` | 供应商配置文件 |

### `load_providers(path: &Path) -> Result<Vec<Provider>, ApiError>`

- 后置：文件不存在 → `Ok(vec![])` 并告警（非致命，与 `load_routes` 同策略）；解析失败 → `ConfigLoad`（500）。
- 重复 id 只告警、全部保留。

### `save_providers(path: &Path, providers: &[Provider]) -> Result<(), ApiError>`

- 后置：pretty TOML + 固定托管头注释；同名目录临时文件 + rename 原子替换；**重写丢注释**。

### `run`（lib.rs）变更

- 启动顺序：`load_routes` → `load_providers` → 两个 `set_all`；两者都缺失不致命。
- `build_app` 新增 `nest("/llm", llm::llm_router())`，位于 admin nest 与 proxy fallback 之间；`/llm` 前缀保留（nest 优先于 fallback）。

---

## error.rs（新增变体）

| 变体 | HTTP 状态 | 触发场景 |
|---|---|---|
| `ProviderNotFound(id)` | 404 | GET/PUT/DELETE 未知供应商 id |
| `InvalidProvider(msg)` | 400 | `Provider::validate` 失败 |
| `DuplicateProviderId(id)` | 400 | POST id 已存在 / PUT 与其他供应商冲突 |
| `NoProviderForProtocol(protocol)` | 503 | 该协议没有启用供应商 |
| `QuotaExhausted(protocol)` | 429 | 该协议全部供应商 RPM 或 TPM 余量为 0 |

错误体沿用 `{"error":{"code":<u16>,"message":<thiserror 文本>}}`。

## 测试以本文件为准

`tests/llm_gateway.rs` 的每条断言对应上表某一条契约；映射见 `docs/tdd/mapping.md`。新增或修改行为前，先改本文件对应条目，再改代码与测试。
