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

### `Model`（结构体，ADR-L14）

字段与不变式见 `data-model.md`（P7/P8）。实现 `Serialize/Deserialize`（TOML 与 JSON 共用）；`upstream` 与 `priority` 带 `#[serde(default)]`。

### `Provider`（结构体）

字段与不变式见 `data-model.md`（P1–P10）。实现 `Serialize/Deserialize`（TOML 与 JSON 共用同一结构）。`protocols` 字段反序列化同时接受列表与**旧形态单值**（`#[serde(alias = "protocol")]`，见 ADR-L13）；序列化只产出列表形态。`models` / `timeout_secs` / `proxy_url` 均可缺省（`#[serde(default)]` / `Option`）——**不带模型表的旧 providers.toml 与旧测试继续通过**（兜底语义，ADR-L14）。

### `Provider::validate(&self) -> Result<(), String>`

- 前置：字段已填充。
- 后置：全部规则通过返回 `Ok(())`；任一失败 `Err(原因)`（管理 API 映射 400）。
- 规则（顺序即错误优先级）：`id` 非空 → `protocols` 非空 → `protocols` 无重复 → `base_url` scheme ∈ {http,https} → `base_url` host 非空 → `api_key` 非空 → `rpm_limit`/`tpm_limit` 为 `Some` 时 `> 0` → `timeout_secs` 为 `Some` 时 `> 0` → `proxy_url` 为 `Some` 时 scheme ∈ {http,https}、host 非空且可被 `reqwest::Proxy::all` 解析（P14，ADR-L19） → `models` 每项 `name` 非空 → `models` 内 `name` 无重复 → `models` 每项 `upstream` 为 `Some` 时非空。
- `proxy_url` 校验**不只做字符串检查**：同时尝试构造 `reqwest::Proxy::all(url)`，把 reqwest 能感知而字符串校验漏过的非法形态（如 userinfo 里带非法字符）拦在配置时（400），而不是请求时（500）。

### `Provider::apply_patch(&self, patch: ProviderPatch) -> Provider`

- 后置：`Some` 字段覆盖，`None` 保留；**`id` 永远来自 `self`**；不校验（调用方 `replace` 前调 `validate`）。`models` 为 `Option<Vec<Model>>`：缺省 / 显式 `null` = 保留原表；给列表 = 整体替换；给空列表 `[]` = 清空模型表（回到兜底）。`proxy_url` 为 `Option<Option<String>>`：缺省 = 保留原值；显式 `null` = 清除（回到共享直连 client）；字符串 = 设新代理。

### `ProviderTable::{set_all, list, get, add, replace, remove, list_enabled_by_protocol, pool_for, declares_model}`

| 方法 | 签名 | 后置 | 错误 |
|---|---|---|---|
| `set_all` | `(&self, Vec<Provider>)` | 整表替换（启动加载用） | 不抛 |
| `list` | `(&self) -> Vec<Provider>` | 全量克隆，表内顺序 | 不抛 |
| `get` | `(&self, id: &str) -> Option<Provider>` | 按 id 查 | 不抛 |
| `add` | `(&self, Provider) -> Result<(), ApiError>` | 追加表尾 | `DuplicateId`（400） |
| `replace` | `(&self, id: &str, Provider) -> Result<(), ApiError>` | 原地替换 | `ProviderNotFound`（404）/ `DuplicateId`（排除自身位置后比较） |
| `remove` | `(&self, id: &str) -> Option<Provider>` | 移除并返回 | 不抛 |
| `list_enabled_by_protocol` | `(&self, protocol) -> Vec<Provider>` | `enabled && protocols` **包含**该协议，表内顺序；多协议供应商同时出现在多个协议池里 | 不抛 |
| `pool_for` | `(&self, protocol, model: &str) -> Vec<Candidate>` | `enabled && protocols 含该协议 && (models 空 \|\| 声明了 model)`；`Candidate { provider, priority, upstream }`，`priority` = 声明条目的 `priority`（空表 = `i32::MIN`），`upstream` = 声明条目的 `upstream` 或 `None`（空表）；表内顺序 | 不抛 |
| `declares_model` | `(&self, model: &str) -> bool` | 全表**任意**供应商（含 disabled）声明了该 model | 不抛 |

---

## app.rs（新增，ADR-L15/L18）

### `App`（结构体）

字段与不变式见 `data-model.md`（P11–P13）。实现 `Serialize/Deserialize`（TOML 与 JSON 共用同一结构，与 `Provider` 同构的共享表示原则）。

### `App::validate(&self) -> Result<(), String>`

- 规则（顺序即错误优先级）：`id` 非空 → `key` 非空。`models` 允许任意内容（含空、含尚无供应商声明的模型名，P13）。
- 管理 API 映射 400（`InvalidApp`）。

### `App::apply_patch(&self, patch: AppPatch) -> App`

- 后置：`Some` 字段覆盖，`None` 保留；**`id` 永远来自 `self`**。`models` 为 `Option<Vec<String>>`：缺省 / 显式 `null` = 保留；给列表 = 整体替换；`[]` = 改为不限制。

### `AppTable::{set_all, list, get, find_by_key, add, replace, remove}`

| 方法 | 签名 | 后置 | 错误 |
|---|---|---|---|
| `set_all` | `(&self, Vec<App>)` | 整表替换（启动加载用） | 不抛 |
| `list` | `(&self) -> Vec<App>` | 全量克隆，表内顺序 | 不抛 |
| `get` | `(&self, id: &str) -> Option<App>` | 按 id 查 | 不抛 |
| `find_by_key` | `(&self, key: &str) -> Option<App>` | 按 key 查；**比较用常量时间**（复用 `admin::constant_time_eq`） | 不抛 |
| `add` | `(&self, App) -> Result<(), ApiError>` | 追加表尾 | `DuplicateAppId` / `DuplicateAppKey`（400） |
| `replace` | `(&self, id: &str, App) -> Result<(), ApiError>` | 原地替换 | `AppNotFound`（404）/ `DuplicateAppId` / `DuplicateAppKey`（均排除自身位置后比较） |
| `remove` | `(&self, id: &str) -> Option<App>` | 移除并返回 | 不抛 |

### 凭证解析约定（forward 使用）

- 入站凭证取值：`Authorization: Bearer <k>`（取 scheme 后内容，大小写不敏感于 `bearer`）或 `x-api-key: <k>`（两者都有时取 `Authorization`）。
- key 比较必须常量时间；**任何解析失败 / 空值都视为「无凭证」**（不与 401 混淆——只有「有凭证且不匹配任何 app」才 401）。
- 生成 key 形态：`sk-gw-<uuid v4 simple>`（`Uuid::new_v4().simple()`）。

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

### `Selection`（结构体，ADR-L14）

```rust
Selection { provider: Provider, upstream_model: Option<String> }
```

`upstream_model` = 选中供应商为该请求声明的上游模型名。`Some(name)` 且与请求 `model` 不同 → forward 需改写 body；`None` = 兜底供应商或无 model 请求（不改写）。

### `select_and_record(state: &AppState, protocol: Protocol, model: Option<&str>) -> Result<Selection, ApiError>`

负载均衡的单一入口。

- 前置：`state.providers` 与 `state.llm_quotas` 可用。
- 后置（按判定顺序）：
  1. **候选收集**：`model == None` → `list_enabled_by_protocol(protocol)`（每个供应商 effective priority = 0，`upstream_model = None`）；`model == Some(m)` → `pool_for(protocol, m)`，空池时：`declares_model(m)` 为真 → `Err(NoProviderForModel(m))`（→ **503**）；为假 → `Err(ModelNotFound(m))`（→ **404**）。`model == None` 且池空 → `Err(NoProviderForProtocol(protocol))`（→ **503**，保持原语义）。
  2. 为每个候选取（缺则创建）`Arc<QuotaTracker>`，**按表序**全部加锁。
  3. 各供应商淘汰旧记录后算权重：`weight = min(rpm_ratio, tpm_ratio)`；未设限额的维度比例按 `1.0`；`weight == 0` 的供应商退出选择。
  4. **分层（仅 `model == Some` 时，ADR-L16）**：按 effective priority **降序**分组；从高到低取第一个**存在 `weight > 0` 成员**的层；层内成员按 weight 加权随机。层内全 0 则看下一层；**所有层全 0** → `Err(QuotaExhausted(...))`（→ **429**，消息含 model）。
  5. `model == None` 时退化为扁平加权随机（无分层；全员 priority 0 同层）——即 ADR-L4 的原语义。
  6. 选中 → **同一锁内** `record_request()` → 返回 `Selection`（provider 克隆 + upstream_model）。
- 错误：除上述变体不产生其他响应形态。
- 并发：第 6 步的「选 + 记」处于同一临界区（所有候选 tracker 的锁一次性持有、期间无 await），并发请求不会重复选走只剩 1 RPM 的供应商——此不变式**不因分层改变**。

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

## client.rs（新增，ADR-L19）

### `ClientCache`（结构体）

按 `proxy_url` 复用 reqwest client（每个代理 url 一个自带连接池的 client），`None` 永远映射到 `AppState::client`（共享直连 client）。

| 方法 | 签名 | 后置 / 错误 |
|---|---|---|
| `new` | `() -> Self` | 空缓存 |
| `get` | `(&self, proxy_url: Option<&str>, direct: &reqwest::Client) -> Result<reqwest::Client, ApiError>` | `None` → 克隆共享直连 client；`Some(url)` → 命中缓存返回克隆，未命中构建 `Client::builder().proxy(Proxy::all(url))` 并写入缓存后返回克隆；构建失败 → `Err(Internal(...))`（→ **500**，防御性兜底——`validate` 已把绝大数非法 url 拦在配置时） |

- 并发：`RwLock<HashMap<String, reqwest::Client>>`，读路径（命中）只取读锁；「查不到再取写锁 or_insert」与 `AppState::stats_for` 同构。锁中毒 panic（与 `RouteTable` 同策略）。
- 缓存只增不删：供应商改/删代理 url 后旧 client 自然淘汰（无 leases 时由 reqwest 连接池回收），不做显式失效——代理 url 变更是低频管理操作。

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
  2. **解析 app**（ADR-L15）：入站凭证 = `Authorization: Bearer <k>`（scheme 大小写不敏感）或 `x-api-key: <k>`；无凭证 → `app = None`；有凭证但 `AppTable::find_by_key` 为 `None` → `401 UnknownAppKey`（fail-closed，消息提示 key 未注册）；命中且 `enabled == false` → `403 AppDisabled`。
  3. **缓冲请求 body**（上限 `MAX_REQUEST_BODY` = 16 MiB，ADR-L14）：超限 → `413 RequestBodyTooLarge`（不截断转发）；正常缓冲为 `Bytes` 后解析 JSON 取 `"model"` 字符串字段：非 JSON / 无 `model` / `model` 非字符串 → `model = None`（不拦截，风险 R11）。
  4. **组合校验**：`app` 为 `Some` 且其 `models` 非空且 `model` 为 `Some(m)` 且 `m` 不在组合内 → `403 ModelNotAllowedForApp`（消息含 model 与 app id）。`app == None` 或 `models` 为空 → 放行。
  5. `select_and_record(state, protocol, model.as_deref())` 失败按其错误映射（404 / 503 / 429，house 错误体）。
  6. `state.llm_stats_for(provider.id).guard()`（Drop 记账）。
  7. 上游 URL = `base_url.trim_end_matches('/') + 剩余路径`。
  8. **body 改写**（ADR-L14）：`upstream_model` 为 `Some(u)` 且 `Some(u) != model` → 解析 JSON、把 `model` 字段替换为 `u`、重新序列化后发送；`upstream_model == model`（或 `None`，或 `model == None`）→ **原始 bytes 零差异透传**。
  9. 请求头：剥离集 = top-gateway 的 hop-by-hop + `content-length` + `host` + `authorization` + `x-api-key`；按协议注入供应商凭证：
     - `openai` / `systemone`：`Authorization: Bearer <api_key>`
     - `anthropic`：`x-api-key: <api_key>`；`anthropic-version` **不存在时**补 `2023-06-01`（客户端已传则保留原值）
  10. 附 `x-forwarded-host` / `x-forwarded-path`（同 proxy 行为）。
  11. **选择 HTTP client**（ADR-L19）：`provider.proxy_url` 为 `None` → 用共享直连 client（`state.client`）；`Some(url)` → 向 `state.llm_clients` 按 url 取（缺则构建 `reqwest::Client::builder().proxy(Proxy::all(url))` 并缓存）**直连/代理 client 均复用连接池**。构建失败 → `500`（`ApiError::Internal`；`validate` 已拦过绝大多数非法 url，此处是防御性兜底）。
  12. 超时 = `provider.timeout_secs.unwrap_or(settings.default_timeout_secs)`（ADR-L17）包裹 `send()`：传输错误 → `502`（消息含 provider id）；超时 → `504`；均 `record_error()`。**超时包裹与 client 选择无关**——换 client 不改变超时语义。
  13. 响应头：剥 hop-by-hop + `content-length`；**注入 `x-llm-provider: <provider id>`**。
  14. body：`Body::from_stream(usage tap)`——每块向客户端转发的同时复制进 `TailBuffer`；流正常结束 / 上游流出错 / **消费者断开** 三种情形都触发一次 `extract_tokens → record_tokens`（互斥，只记一次）。**请求 body 不流式（已缓冲），响应 body 仍流式**。
  15. 上游 5xx：状态与 body 原样透传，同时计一次错误；4xx 原样透传不计错误。
- 错误体：全部使用 `proxy::error_response` 的 house 格式。
- 常量：`pub const MAX_REQUEST_BODY: usize = 16 * 1024 * 1024;`

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
| GET | `/__admin/llm/apps` | `200 {"apps":[...]}` | 401 |
| POST | `/__admin/llm/apps` | `201` + 新应用 JSON；`id` 省略 → UUID v4；`key` 省略 → `sk-gw-<uuid v4 simple>` | 400（非法/重复 id/重复 key）、401、422 |
| GET | `/__admin/llm/apps/{id}` | `200` + 单应用（含 `key`） | 404、401 |
| PUT | `/__admin/llm/apps/{id}` | `200` + 合并后的应用 | 400、404、401 |
| DELETE | `/__admin/llm/apps/{id}` | `200 {"deleted":<id>}` | 404、401 |

- 所有供应商变更端点成功后先 `state.persist_providers()`，应用变更端点先 `state.persist_apps()`，再返回；持久化失败 → 500。
- `ProviderInput`（POST body）：`id`/`name` Option（id 省略生成），`protocols` 必填（接受单值或列表，见 ADR-L13），`base_url`/`api_key` 必填，`rpm_limit`/`tpm_limit`/`timeout_secs`/`proxy_url` Option，`models` Option（省略 = 空表）、`enabled` Option。
- `ProviderPatch`（PUT body）：全 Option，与 `RoutePatch` 同构的合并语义。**`rpm_limit` / `tpm_limit` 为 `Option<Option<u64>>`**：字段缺省 = 保留原值；显式 `null` = 设为不限；数字 = 设新限额。`protocols` 为 `Option<Vec<Protocol>>`：缺省或显式 `null` = 保留原值；给列表（或单值）= 整体替换。`models` 为 `Option<Vec<Model>>`：缺省 / 显式 `null` = 保留原表；给列表 = 整体替换（`[]` = 清空，回到兜底）。`timeout_secs` 为 `Option<Option<u64>>`：缺省 = 保留；显式 `null` = 回落全局默认；数字 = 设新值。`proxy_url` 为 `Option<Option<String>>`：缺省 = 保留原值；显式 `null` = 清除代理（回到共享直连 client）；字符串 = 设新代理（同样要过 `validate` 的 P14 校验）。
- `AppInput`（POST body）：`id`/`name` Option，`key` Option（省略生成），`models` Option（省略 = 不限制），`enabled` Option；无必填字段（全缺省成一个启用、不限模型的 app，id/key 自动生成）。**省略（`None`/缺字段）= 自动生成；显式空串或纯空白 = 非法，`validate` 判 400**（`InvalidApp` 由此可达，与 `Provider` 的「空 id → 400」一致）。
- `AppPatch`（PUT body）：全 Option 合并语义；`models` 为 `Option<Vec<String>>`（缺省 / `null` = 保留；列表 = 替换；`[]` = 改为不限制）。

---

## config.rs（新增）

### `Settings` 新增字段

| 环境变量 | 字段 | 默认 | 说明 |
|---|---|---|---|
| `GATEWAY_PROVIDERS_FILE` | `providers_file: PathBuf` | `providers.toml` | 供应商配置文件 |
| `GATEWAY_APPS_FILE` | `apps_file: PathBuf` | `apps.toml` | 应用（模型组合）配置文件（ADR-L18） |

### `load_providers(path: &Path) -> Result<Vec<Provider>, ApiError>`

- 后置：文件不存在 → `Ok(vec![])` 并告警（非致命，与 `load_routes` 同策略）；解析失败 → `ConfigLoad`（500）。
- 重复 id 只告警、全部保留。

### `save_providers(path: &Path, providers: &[Provider]) -> Result<(), ApiError>`

- 后置：pretty TOML + 固定托管头注释；同名目录临时文件 + rename 原子替换；**重写丢注释**。

### `load_apps(path: &Path) -> Result<Vec<App>, ApiError>`（新增）

- 后置：文件不存在 → `Ok(vec![])` 并告警（非致命）；解析失败 → `ConfigLoad`（500）。重复 id 或重复 key 只告警、全部保留。

### `save_apps(path: &Path, apps: &[App]) -> Result<(), ApiError>`（新增）

- 后置：pretty TOML + 固定托管头注释；同名目录临时文件 + rename 原子替换；**重写丢注释**（与 `save_providers` 同构）。`key` 明文落盘（与 `api_key` 同策略，ADR-L11）。

### `run`（lib.rs）变更

- 启动顺序：`load_routes` → `load_providers` → `load_apps` → 三个 `set_all`；三者都缺失不致命。
- `build_app` 新增 `nest("/llm", llm::llm_router())`，位于 admin nest 与 proxy fallback 之间；`/llm` 前缀保留（nest 优先于 fallback）。

---

## error.rs（新增变体）

| 变体 | HTTP 状态 | 触发场景 |
|---|---|---|
| `ProviderNotFound(id)` | 404 | GET/PUT/DELETE 未知供应商 id |
| `InvalidProvider(msg)` | 400 | `Provider::validate` 失败 |
| `DuplicateProviderId(id)` | 400 | POST id 已存在 / PUT 与其他供应商冲突 |
| `NoProviderForProtocol(protocol)` | 503 | 该协议没有启用供应商（**无 model 请求**时） |
| `QuotaExhausted(msg)` | 429 | 全部候选供应商 RPM 或 TPM 余量为 0；有 model 时消息附带 model 名 |
| `ModelNotFound(model)` | 404 | 请求带的 model 无任何供应商声明（消息提示模型表在 `/__admin/llm/providers`） |
| `NoProviderForModel(model)` | 503 | 模型有供应商声明，但池空（全 disabled / 协议不匹配） |
| `ModelNotAllowedForApp(model, app)` | 403 | 请求 model 不在命中 app 的模型组合内 |
| `UnknownAppKey` | 401 | 入站凭证存在但未命中任何 app（fail-closed） |
| `AppDisabled(id)` | 403 | 凭证命中的 app `enabled = false` |
| `InvalidApp(msg)` | 400 | `App::validate` 失败 |
| `DuplicateAppId(id)` / `DuplicateAppKey(key)` | 400 | POST/PUT app 冲突（key 只回显 id 或脱敏形态，不回显完整 key） |
| `AppNotFound(id)` | 404 | GET/PUT/DELETE 未知 app id |
| `RequestBodyTooLarge` | 413 | 请求 body 超过 `MAX_REQUEST_BODY`（16 MiB） |
| `Internal(msg)` | 500 | 运行时构建代理 client 失败（ADR-L19；`validate` 已拦绝大多数非法 `proxy_url`，此为防御性兜底） |

`QuotaExhausted` 的消息从「protocol 名」改为「携带 model 信息的可读串」（有 model 时含 model 名）；无 model 时保持原语义（协议名）。错误体格式不变（house 格式）。

错误体沿用 `{"error":{"code":<u16>,"message":<thiserror 文本>}}`。

## 测试以本文件为准

`tests/llm_gateway.rs` 的每条断言对应上表某一条契约；映射见 `docs/tdd/mapping.md`。新增或修改行为前，先改本文件对应条目，再改代码与测试。
