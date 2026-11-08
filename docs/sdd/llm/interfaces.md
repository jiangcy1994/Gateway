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
| `lock` | `(&self) -> QuotaGuard<'_>` | balancer / forward / admin | **创建时即淘汰**窗口外记录；锁中毒 panic（与 `RouteTable` 同策略） |

### `QuotaGuard`（`lock()` 的返回：已淘汰、持有期间一直有效的临界区句柄）

| 方法 | 签名 | 调用方 | 后置 / 错误 |
|---|---|---|---|
| `rpm_used` | `(&self) -> u64` | balancer | 窗口内请求数（`requests.len()`，创建时已淘汰） |
| `tpm_used` | `(&self) -> u64` | balancer | 窗口内 token 和 |
| `record_request` | `(&mut self)` | balancer（**同一临界区内**） | 追加 `Instant::now()`；不再加锁、不再淘汰 |
| `retry_after` | `(&self, rpm_limit: Option<u64>, tpm_limit: Option<u64>) -> Option<Duration>` | balancer（**429 路径，guard 仍全持有时**） | 该供应商「最早可能恢复」还需多久（ADR-L24）；语义见下 |

### 恢复时长语义（`retry_after`，ADR-L24）

- **返回 `Some` 的条件**：RPM、TPM 至少一个维度设了限额**且已耗尽**（`rpm_used >= rpm_limit` / `tpm_used >= tpm_limit`）。两维度都不限或都未耗尽 → `None`。`weight == 0`（429 的成因）意味着至少一维耗尽，故 429 时每个候选必返回 `Some`；池级聚合时对 `None` 防御性跳过。
- **RPM 恢复瞬间**：耗尽即 `len >= limit`。恢复时刻 = 第 `len - limit` 条（0-based；`len == limit` 时即 `requests[0]`）离开窗口的瞬间——此后窗口内只剩 `limit - 1 < limit` 条：

  ```
  rpm_recovery = requests[len - limit] + WINDOW
  ```

- **TPM 恢复瞬间**：`sum >= limit` 时从最旧条目起逐条累加被淘汰量，直到剩余量首次 `< limit`——**触发该落点的那条**决定恢复瞬间（必终止：P4 保证 `limit > 0`，全部淘汰后剩余为 0）：

  ```
  tpm_recovery = tokens[i] + WINDOW，i = 最小的令 sum - Σ(tokens[0..=i]) < limit 的下标
  ```

- **供应商级 = max**：`weight = min(rpm_ratio, tpm_ratio)`，两维度都恢复才算恢复，故取较慢者；未耗尽的维度不参与。
- **池级 = min**（balancer）：429 意味着**每个**候选都有耗尽维度，任一候选恢复即出现可调度对象（`weight > 0`），故取全部候选中的最早者。
- **边界**：guard 创建时已淘汰窗口外条目，保留条目均满足 `now - entry < WINDOW`（严格小于，淘汰谓词为 `>=`），故恢复时刻恒在未来、`retry_after ∈ (0, WINDOW)`；`error.rs` 的 `ceil` + clamp `[1, 60]` 是**防御性格式化**，不承担语义。
- **advisory 语义（最小退避下界，非预约）**：头值是「最早**可能**恢复」的时刻而非保留座位的承诺——并发请求可抢先占走刚空出的槽位（选择无排队，L6/L4/L16），在途请求的 token 回账也可能重新压满 TPM（与 R2 同源的时序缺口）。按头值**精确**重试的客户端仍可能再撞 429（风险 R19）；客户端应视其为最小退避下界，按「头值 + 抖动」退避。

不变式（并发）：`Mutex` 永远按 **ProviderTable 表序**获取；`select_and_record` 把所有候选 tracker 的锁**一次性全部持有**后再算权重与 `retry_after`，期间不做任何 await。

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
  4. **分层（仅 `model == Some` 时，ADR-L16）**：按 effective priority **降序**分组；从高到低取第一个**存在 `weight > 0` 成员**的层；层内成员按 weight 加权随机。层内全 0 则看下一层；**所有层全 0** → `Err(QuotaExhausted(msg, retry_after))`（→ **429**，消息含 model）：`retry_after` = 全部候选 `guard.retry_after(rpm_limit, tpm_limit)` 的 **min**（第 2 步获取的 guard 仍全部持有——**同一临界区、零额外加锁**；ADR-L24，`None` 候选跳过）。
  5. `model == None` 时退化为扁平加权随机（无分层；全员 priority 0 同层）——即 ADR-L4 的原语义。
  6. 选中 → **同一锁内** `record_request()` → 返回 `Selection`（provider 克隆 + upstream_model）。
- 错误：除上述变体不产生其他响应形态。
- 并发：第 6 步的「选 + 记」处于同一临界区（所有候选 tracker 的锁一次性持有、期间无 await），并发请求不会重复选走只剩 1 RPM 的供应商——此不变式**不因分层改变**；429 的恢复时长计算（第 4 步）亦在该临界区内，不产生新的跨请求时序缺口。

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

### `Usage`（结构体，ADR-L20）

token 分桶明细，供 ledger 记账。**best-effort，永不失败**；`total` 恒等于各桶之和（**饱和**求和——极端 / 恶意上游值不回绕、不 panic，ADR-L22）。

| 字段 | 类型 | 说明 |
|---|---|---|
| `input` | `u64` | 输入（prompt）token |
| `output` | `u64` | 输出（completion）token |
| `cache_read` | `u64` | 命中缓存被读取的 token（openai `prompt_tokens_details.cached_tokens`；anthropic `cache_read_input_tokens`） |
| `cache_write` | `u64` | 写入缓存的 token（anthropic `cache_creation_input_tokens`；openai 无对应字段，恒 0） |

- `Default` = 全 0（`Usage::default()`）。
- 桶划分**跨协议对齐**：同一字段名在两种协议下语义一致（openai 的 `prompt_tokens` 含 cached，取数时先减去 `cached_tokens` 归一到 `input`；anthropic 的 `input_tokens` 本就不含缓存）。
- `reasoning_tokens`（openai 的 `completion_tokens_details`）**不计入任何桶**：它是 `output` 的子集而非独立桶，单列会造成重复计数（风险 R16）。
- systemone 的 `extract_usage` 与 `extract_tokens` 同为**桩：恒为全 0**（ADR-L8 的同构扩展，ADR-L20）。

### `extract_usage(protocol, content_type: Option<&str>, tail: &[u8]) -> Usage`

从响应尾包提取分桶明细。与 `extract_tokens` 同流的 best-effort 契约（解析不到返回全 0），但**记账维度不同**：本函数返回分桶明细供 ledger 记账，`extract_tokens` 返回总数供 TPM 配额记账。两函数各自独立、互不影响——TPM 配额语义（含「`total_tokens` 优先」规则）保持不变。

| 协议 | `content-type` 含 `text/event-stream` | 其余（JSON 等） |
|---|---|---|
| openai | 逐行取 `data:` 前缀的 JSON，汇总所有含 `usage` 对象的块 | 从尾包找最后一个 `"usage"` 键，取其 JSON 对象 |
| anthropic | 汇总所有含 `usage` 的块：`input`/`cache_read`/`cache_write` 取**首个出现的值**，`output` 取**最后出现的值**（累计值语义，与 `extract_tokens` 同规则） | 同左（非流式响应只有一个 usage） |
| systemone | **桩：恒全 0**（ADR-L8/L20） | 同左 |

openai 取数规则：`input = prompt_tokens - prompt_tokens_details.cached_tokens`（`cached_tokens` 缺省 0，减法用 `saturating_sub`）；`output = completion_tokens`；`cache_read = prompt_tokens_details.cached_tokens`；`cache_write` 恒 0。`prompt_tokens` 或 `completion_tokens` 缺失的块按该桶 0 处理（存在的桶仍计入）。

anthropic 取数规则：`input = input_tokens`、`output = output_tokens`、`cache_read = cache_read_input_tokens`（缺省 0）、`cache_write = cache_creation_input_tokens`（缺省 0）；四个字段全缺的 usage 对象按全 0 计。

---

## ledger.rs（新增，ADR-L20）

### `UsageLedger`（结构体）

按**日（UTC）× 应用**聚合 token 用量的账本，与 `QuotaTracker` 并列的运行期状态。

构造：`UsageLedger::new()`。

| 方法 | 签名 | 调用方 | 后置 / 错误 |
|---|---|---|---|
| `record` | `(&self, app: Option<&str>, date: NaiveDate, usage: Usage)` | forward 的 usage tap | 以 `(date, app)` 为主键合并累加各桶；锁中毒 panic（与 `RouteTable` 同策略）；`app = None`（无凭证）记为空串键 |
| `snapshot` | `(&self, filter: UsageFilter) -> Vec<UsageEntry>` | admin | 按 filter 过滤后按 `(date, app)` **升序**返回克隆；`app` 空串键在视图边界转成 `None` |
| `flush_if_dirty` | `(&self, path: &Path) -> Result<(), ApiError>` | flush 任务 / shutdown | 先持 `flush_lock`（串行化所有 flush，见下）；dirty 时全量序列化落盘（原子写）并清 dirty；不 dirty 时 **no-op 不写文件**（锁内早退）；序列化/写盘失败 → `Persist`（500） |

- 并发：内部 `RwLock<BTreeMap<(NaiveDate, String), Usage>>` + `AtomicBool dirty` + `Mutex<()>` flush 锁（ADR-L22）。`record` 只取写锁、**不持有任何 quota 锁**（在 `UsageTap::finish` 里于 quota 记账之后调用）；`flush_if_dirty` 持 `flush_lock` 全程（含写盘），其内在**同一把 map 写锁**里完成「快照 + 清 dirty」，序列化在锁外进行。
- 并发不变式：`flush_lock` 串行化所有 flush（周期 flush × 末次 flush 互斥）；「清 dirty」与「快照」同锁同临界区，并发的 `record` 要么落在快照里（清位之前），要么在清位之后重新置位——**不存在「已记账但 dirty = false」的夹缝**。快照后新记的条目不在本次落盘内容里，但 dirty 已置位，下一次 flush 会带上——**不丢条目，只可能延迟一个 flush 窗口**。
- `UsageFilter`：`{ app: Option<String>, date: Option<NaiveDate>, from: Option<NaiveDate>, to: Option<NaiveDate> }`，全部可选；过滤语义为 AND（`app` 精确匹配，`date` 精确匹配，`from`/`to` 闭区间）。

### `UsageEntry`（序列化形态）

`record` 的视图投影，也是 admin 响应与持久化 TOML 的共享行结构。

| 字段 | 类型 | 说明 |
|---|---|---|
| `date` | `String`（`YYYY-MM-DD`） | UTC 日期；序列化为字符串以避开 toml 对 chrono 的 feature 要求 |
| `app` | `Option<String>` | 应用 id；`None` = 无凭证流量（默认策略）；TOML 行内为空串 |
| `input` / `output` / `cache_read` / `cache_write` / `total` | `u64` | `total` 序列化时由各桶**饱和**求和填入（ADR-L22），反序列化时**忽略**（以各桶为准，兼容手工编辑） |

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
  5. `select_and_record(state, protocol, model.as_deref())` 失败直接走 `ApiError::into_response()` 渲染（404 / 503 / 429，house 错误体；429 时 `Retry-After` 头一并带上，ADR-L24）。**不再经 `proxy::error_response`**——两者错误体字节相同，`ApiError::into_response` 是 `ApiError` 渲染的唯一权威（429 的头只有前者能带出）。
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
  14. body：`Body::from_stream(usage tap)`——每块向客户端转发的同时复制进 `TailBuffer`；流正常结束 / 上游流出错 / **消费者断开** 三种情形都触发一次（互斥，只记一次）：先 `extract_tokens → record_tokens`（TPM 配额），再 `extract_usage → ledger.record(app_id, today, usage)`（ADR-L20）。**请求 body 不流式（已缓冲），响应 body 仍流式**。
  15. 上游 5xx：状态与 body 原样透传，同时计一次错误；4xx 原样透传不计错误。
- 错误体：全部使用 house 格式（`ApiError::into_response`，与 `proxy::error_response` 同形；见第 5 步）。
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
| GET | `/__admin/llm/usage` | `200 {"usage":[UsageEntry...]}`，全部可选 query 参数（见下） | 400（非法日期格式 / 结构非法的 query string，均走 `InvalidUsageQuery`）、401 |

- 所有供应商变更端点成功后先 `state.persist_providers()`，应用变更端点先 `state.persist_apps()`，再返回；持久化失败 → 500。
- GET（列表与单项）返回的供应商 JSON **总是带 `proxy_url` 键**：未配置时为 `null`（与 `rpm_limit`/`tpm_limit`/`timeout_secs` 同构）——对 0.2.2 之前的消费者是纯新增键，不影响既有解析。
- `ProviderInput`（POST body）：`id`/`name` Option（id 省略生成），`protocols` 必填（接受单值或列表，见 ADR-L13），`base_url`/`api_key` 必填，`rpm_limit`/`tpm_limit`/`timeout_secs`/`proxy_url` Option，`models` Option（省略 = 空表）、`enabled` Option。
- `ProviderPatch`（PUT body）：全 Option，与 `RoutePatch` 同构的合并语义。**`rpm_limit` / `tpm_limit` 为 `Option<Option<u64>>`**：字段缺省 = 保留原值；显式 `null` = 设为不限；数字 = 设新限额。`protocols` 为 `Option<Vec<Protocol>>`：缺省或显式 `null` = 保留原值；给列表（或单值）= 整体替换。`models` 为 `Option<Vec<Model>>`：缺省 / 显式 `null` = 保留原表；给列表 = 整体替换（`[]` = 清空，回到兜底）。`timeout_secs` 为 `Option<Option<u64>>`：缺省 = 保留；显式 `null` = 回落全局默认；数字 = 设新值。`proxy_url` 为 `Option<Option<String>>`：缺省 = 保留原值；显式 `null` = 清除代理（回到共享直连 client）；字符串 = 设新代理（同样要过 `validate` 的 P14 校验）。
- `AppInput`（POST body）：`id`/`name` Option，`key` Option（省略生成），`models` Option（省略 = 不限制），`enabled` Option；无必填字段（全缺省成一个启用、不限模型的 app，id/key 自动生成）。**省略（`None`/缺字段）= 自动生成；显式空串或纯空白 = 非法，`validate` 判 400**（`InvalidApp` 由此可达，与 `Provider` 的「空 id → 400」一致）。
- `AppPatch`（PUT body）：全 Option 合并语义；`models` 为 `Option<Vec<String>>`（缺省 / `null` = 保留；列表 = 替换；`[]` = 改为不限制）。

### `GET /__admin/llm/usage` 查询参数（ADR-L20）

全部可选，过滤语义为 AND：

| 参数 | 格式 | 语义 |
|---|---|---|
| `app` | 应用 id 字符串 | 精确匹配；不可用于匹配无凭证流量（无凭证条目无 app id） |
| `date` | `YYYY-MM-DD` | 精确匹配单日 |
| `from` | `YYYY-MM-DD` | 闭区间起点 |
| `to` | `YYYY-MM-DD` | 闭区间终点 |

- **空串 = 未传**：四个参数均以「参数出现但值为空」等同于未传来解析——`app` 空串不过滤任何应用，`?date=` / `?from=&to=` 不过滤任何日期（空串既不是合法 id 也不是合法日期，但不报 400，语义与省略一致）。
- 日期解析用 `NaiveDate::parse_from_str(s, "%Y-%m-%d")`，且解析成功后再按同一格式回写比对原串——`%Y` 单独接受 1-4 位年份（`"26-11-06"` 会解析成 0026 年），只有规范 4 位年份的入串合法；任一参数格式或形态非法 → `400 InvalidUsageQuery`（消息含参数名与原值）。
- **query string 本身结构非法**（如重复参数 `?date=a&date=b`、非法百分号转义）同样 → `400 InvalidUsageQuery`（消息 `malformed query string: ...`）。端点用 `RawQuery` + 手工 `serde_urlencoded` 解析而非 axum 的 `Query` 提取器——后者拒绝时回**纯文本** 400，会破坏 house JSON 错误契约（ADR-L23）。
- 无参数 = 全量。返回按 `(date, app)` 升序；`app` 缺失的条目（无凭证流量）排在同日内最前。
- 只读端点：不写盘、不清 dirty（落盘由 flush 任务与 shutdown 负责）。

---

## config.rs（新增）

### `Settings` 新增字段

| 环境变量 | 字段 | 默认 | 说明 |
|---|---|---|---|
| `GATEWAY_PROVIDERS_FILE` | `providers_file: PathBuf` | `providers.toml` | 供应商配置文件 |
| `GATEWAY_APPS_FILE` | `apps_file: PathBuf` | `apps.toml` | 应用（模型组合）配置文件（ADR-L18） |
| `GATEWAY_USAGE_FILE` | `usage_file: PathBuf` | `usage.toml` | 用量账本持久化文件（ADR-L20） |
| `GATEWAY_USAGE_FLUSH_SECS` | `usage_flush_secs: u64` | `60` | 账本 flush 周期秒数（ADR-L20；`0` = 不启用周期 flush，仅 shutdown 落盘） |

### `load_providers(path: &Path) -> Result<Vec<Provider>, ApiError>`

- 后置：文件不存在 → `Ok(vec![])` 并告警（非致命，与 `load_routes` 同策略）；解析失败 → `ConfigLoad`（500）。
- 重复 id 只告警、全部保留。
- **加载不跑 `Provider::validate`**（与 `load_routes` 同构）：手工编辑写入的非法值（如 socks5 的 `proxy_url`、base_url 空串）能加载、启动不失败，只在请求时暴露——`validate` 的 400 闸门只覆盖管理 API 路径（风险 R13/R14）。

### `save_providers(path: &Path, providers: &[Provider]) -> Result<(), ApiError>`

- 后置：pretty TOML + 固定托管头注释；同名目录临时文件 + rename 原子替换；**重写丢注释**。

### `load_apps(path: &Path) -> Result<Vec<App>, ApiError>`（新增）

- 后置：文件不存在 → `Ok(vec![])` 并告警（非致命）；解析失败 → `ConfigLoad`（500）。重复 id 或重复 key 只告警、全部保留。

### `save_apps(path: &Path, apps: &[App]) -> Result<(), ApiError>`（新增）

- 后置：pretty TOML + 固定托管头注释；同名目录临时文件 + rename 原子替换；**重写丢注释**（与 `save_providers` 同构）。`key` 明文落盘（与 `api_key` 同策略，ADR-L11）。

### `load_usage(path: &Path) -> Result<Vec<UsageEntry>, ApiError>`（新增，ADR-L20）

- 后置：文件不存在 → `Ok(vec![])` 并告警（非致命，与 `load_routes` 同策略）；解析失败 → `ConfigLoad`（500）。
- 同一 `(date, app)` 出现多行时**合并累加**（文件被手工编辑出重复行也收敛），合并后按 `(date, app)` 排序。
- 不跑任何 `validate`：用量文件是网关自己写的观测数据，没有配置面语义。

### `save_usage(path: &Path, entries: &[UsageEntry]) -> Result<(), ApiError>`（新增，ADR-L20）

- 后置：pretty TOML + 固定托管头注释；同名目录临时文件 + rename 原子替换；**重写丢注释**。
- 全量写：不做增量追加（账本规模为 日 × 应用 数，全量写的代价可接受，且避免追加文件与 compaction 的复杂度）。

### `run`（lib.rs）变更

- 启动顺序：`load_routes` → `load_providers` → `load_apps` → `load_usage` → 四个 `set_all`；四者都缺失不致命。
- 启动后按 `usage_flush_secs` spawn 周期 flush 任务（dirty 时才写盘）；`axum::serve` 结束（shutdown）后**先 abort 周期 flush 任务**再执行**最后一次 flush**——abort 只在 await 点生效，进行中的写盘会先完成，并发安全由 `flush_if_dirty` 内部的 `flush_lock` 保证，末次写不会被周期任务的旧快照覆盖（ADR-L22）；进程被 SIGKILL 强杀时丢失最近一个 flush 窗口（风险 R15）。
- `build_app` 新增 `nest("/llm", llm::llm_router())`，位于 admin nest 与 proxy fallback 之间；`/llm` 前缀保留（nest 优先于 fallback）。

---

## error.rs（新增变体）

| 变体 | HTTP 状态 | 触发场景 |
|---|---|---|
| `ProviderNotFound(id)` | 404 | GET/PUT/DELETE 未知供应商 id |
| `InvalidProvider(msg)` | 400 | `Provider::validate` 失败 |
| `DuplicateProviderId(id)` | 400 | POST id 已存在 / PUT 与其他供应商冲突 |
| `NoProviderForProtocol(protocol)` | 503 | 该协议没有启用供应商（**无 model 请求**时） |
| `QuotaExhausted(msg, retry_after)` | 429 | 全部候选供应商 RPM 或 TPM 余量为 0；有 model 时消息附带 model 名；`retry_after: Option<Duration>` = 池级最早可能恢复时长（ADR-L24），`Some` 时响应附 `Retry-After` 头 |
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
| `InvalidUsageQuery(msg)` | 400 | `/__admin/llm/usage` 的 `date`/`from`/`to` 参数不是 `YYYY-MM-DD` 格式（消息含参数名与原值），或 query string 本身结构非法（消息 `malformed query string: ...`）——两种形态共用同一变体以保 JSON 信封（ADR-L23） |

`QuotaExhausted(msg, retry_after)` 的消息从「protocol 名」改为「携带 model 信息的可读串」（有 model 时含 model 名）；无 model 时保持原语义（协议名）。错误体格式不变（house 格式）。

**`Retry-After` 头**（ADR-L24）：`retry_after` 为 `Some(d)` 时 `IntoResponse` 插入 `Retry-After: <秒>`——值 = `ceil(d)` 后 clamp 到 `[1, 60]`（delta-seconds 形态）。`#[error(...)]` 的 Display **只读第一个字段**，故消息体字节不因第二字段而变——`retry_after` 对 JSON 响应是**纯加性**的。其他变体不带该头。头值是 advisory 最小退避下界而非预约（语义见「恢复时长语义」小节，风险 R19）。

错误体沿用 `{"error":{"code":<u16>,"message":<thiserror 文本>}}`。

## 测试以本文件为准

`tests/llm_gateway.rs` 的每条断言对应上表某一条契约；映射见 `docs/tdd/mapping.md`。新增或修改行为前，先改本文件对应条目，再改代码与测试。
