# 接口契约（QQ Bot 子网关）

> **SDD 第 4 步**：签名与契约。依赖第 3 步的数据类型——签名里出现的类型必须先定义过。
>
> 本文件是 QQ Bot 子网关**实现的唯一权威**。签名、前后置条件变化必须先改这里，再改代码；测试（`tests/qqbot_gateway.rs`）与映射（`docs/tdd/mapping.md`）以本文件为准。top-gateway 通用行为（鉴权、错误体格式、持久化原子性、header 剥离集）以 `../interfaces.md` 为准，此处不重复，只写差异与新增。

---

## bot.rs

### `EventSource`（枚举）

变体 `Webhook` / `Websocket`；serde 小写字符串 `"webhook"` / `"websocket"`；`Webhook` 为 `#[default]`；实现 `Display`（日志用）。未知值由 serde 报错，不实现 `FromStr`。

### `Bot`（结构体）

字段与不变式见 `data-model.md`（Q1–Q7）。实现 `Serialize/Deserialize`（TOML 与 JSON 共用同一结构；TOML 中 `None` 的 Option 字段被序列化时整体省略，空串在 `normalize` 里归一化为 `None`）。

### `Bot::validate(&self) -> Result<(), String>`

- 前置：字段已填充。
- 后置：全部规则通过返回 `Ok(())`；任一失败 `Err(原因)`（管理 API 映射 400）。
- 规则（顺序即错误优先级）：`app_id` 非空 → `app_secret` 非空 → `intents != 0` → `shard[1] > 0` → `shard[0] < shard[1]` → `api_base` / `event_url` 为 `Some` 时 scheme ∈ {http,https} 且 host 非空 → `ws_url` 为 `Some` 时 scheme ∈ {ws,wss} 且 host 非空。

### `Bot::{resolve_api_base, resolve_ws_url}(&self) -> String`

- 后置：覆盖值 `Some` 直接返回（`resolve_ws_url` 不做 trim）；否则按 `sandbox` 取平台常量。纯函数，无锁、无错误。

### `Bot::apply_patch(&self, patch: BotPatch) -> Bot`

- 后置：`Some` 字段覆盖，`None` 保留；**`id` 永远来自 `self`**；返回值经 `normalize()`（trim 空白、空串 Option → `None`）；不校验（调用方 `replace` 前调 `validate`）。

### `BotTable::{set_all, list, get, find, add, replace, remove}`

| 方法 | 签名 | 后置 | 错误 |
|---|---|---|---|
| `set_all` | `(&self, Vec<Bot>)` | 整表替换（启动加载用） | 不抛 |
| `list` | `(&self) -> Vec<Bot>` | 全量克隆，表内顺序 | 不抛 |
| `get` | `(&self, id: &str) -> Option<Bot>` | 按 **配置 id** 查 | 不抛 |
| `find` | `(&self, app_id: &str) -> Option<Bot>` | 按 **平台 app_id** 查（含 disabled） | 不抛 |
| `add` | `(&self, Bot) -> Result<(), ApiError>` | 追加表尾 | `DuplicateBotId`（400）/ `DuplicateBotAppId`（400） |
| `replace` | `(&self, id: &str, Bot) -> Result<(), ApiError>` | 原地替换 | `BotNotFound`（404）/ 重复 id 或 app_id（排除自身位置后比较，400） |
| `remove` | `(&self, id: &str) -> Option<Bot>` | 移除并返回 | 不抛 |

- `app_id` 上的唯一性是 QQ Bot 特有约束（平台身份不能二代）：`find` 的存在意义即「app_id 是转发与回调的路由键」。
- 表**不**提供 `find_enabled`：disabled 与不存在是两种状态，转发路径用 `find` + 显式 `enabled` 检查区分（503 vs 404），回调路径统一 404（ADR-Q8）。一个 `Option<Bot>` 无法承载这个区分，所以不做一个方法。

---

## token.rs

### `TokenStore`（结构体）

构造：`TokenStore::new()`。内部结构见 `data-model.md`。

| 方法 | 签名 | 调用方 | 后置 / 错误 |
|---|---|---|---|
| `get_or_fetch` | `(&self, client: &Client, bot: &Bot, timeout: Duration) -> Result<String, ApiError>` | forward、ws supervisor（两者传 `settings.default_timeout_secs`） | 见下面判定顺序 |
| `invalidate` | `(&self, app_id: &str)` | ws supervisor 收到 401 类关闭码后 | 清掉当前 token，下次强制刷新 |

### `get_or_fetch` 判定顺序（实现严格按此）

1. 快路径（无锁读 `inner`）：`current` 存在且 `remaining > 300s` → 返回 token 字符串。
2. `0 < remaining ≤ 300s`：
   - 距 `last_fetch` < 30s（刷新冷静期）→ 返回旧 token（**不打平台**，ADR-Q6）。
   - 否则持有 `fetch_lock`：**二次检查**（可能在等锁期间被别人刷新过）仍需刷新才执行 `fetch_token`：
     - 成功且 token 字符串**不同** → 回写新 `TokenEntry`、更新 `last_fetch`、返回新 token。
     - 成功但 token 字符串**相同** → 只更新 `last_fetch`，**保留原 `expires_at`**（平台在临近过期 60s 窗口外仍返旧 token，回写会错误延长有效期）。
     - 失败 → 告警并返回旧 token（旧 token 仍有效；若已过期则 `Err`）。
3. 无 token / 已过期：持有 `fetch_lock` 强制 `fetch_token`；失败 → `Err(TokenFetchFailed(...))`（→ **502**）。

### `fetch_token(client: &Client, bot: &Bot, timeout: Duration) -> Result<TokenEntry, ApiError>`（模块私有）

- 请求：`POST {bot.resolve_api_base()}/app/getAppAccessToken`，JSON body `{ "appId": bot.app_id, "clientSecret": bot.app_secret }`，调用方给的 `timeout` 包裹 `send()`（token 换发在用户请求热路径上，不能被平台挂死）。
- 响应判定：**HTTP 恒为 200**，以响应体 `access_token` 字段存在判成功（ADR-Q3）：
  - 成功：`access_token: String` + `expires_in`（number 或数字字符串，自定义反序列化两种都收，解析失败丢弃该字段并告警；缺失时按 7200 兜底）；`expires_at = now + expires_in`。
  - 失败： body 含 `code` / `message` → `Err`，错误消息含 code 与 message（便于排障，如 `100007` appid 无效）。
- 传输错误 / 超时 / JSON 解析失败（既无 `access_token` 也无 `code`）→ `Err`。
- `expires_in` 上限保护：`min(expires_in, 86400)`，防平台返回异常值把缓存钉死。

### 并发不变式

「检查 → fetch → 回写」全部在 `fetch_lock` 临界区内；快路径（情形 1）只读 `std::sync::Mutex` 保护的内层状态、不持 tokio 锁、不做 await。slot 按 **app_id** 键，惰性清理（删除机器人不主动清 slot，与 `QuotaTracker` 同策略）。

---

## forward.rs

### `forward(State<AppState>, Method, OriginalUri, HeaderMap, Request) -> Response`

`/qqbot` nest 的 fallback 处理器。

- 前置：无。
- 后置（按判定顺序）：
  1. 剥掉 `/qqbot` 前缀取剩余 path；首段为 app_id 段：空 → `404`（消息含提示「missing app_id segment」）。app_id 段之后为剩余路径，空则补 `/`。查询串原样拼接。
  2. `BotTable::find(app_id)`：`None` → `404`（消息含 app_id）；`enabled == false` → `503`（`BotDisabled`，与路由的同构语义一致）。
  3. `token::get_or_fetch` 失败 → `502`（消息含 bot id 与平台错误）。
  4. `state.bot_stats_for(bot.id).guard()`（Drop 记账）。
  5. 上游 URL = `bot.resolve_api_base().trim_end_matches('/') + 剩余路径`。
  6. 请求头：剥离集 = top-gateway 的 hop-by-hop + `content-length` + `host` + `authorization`；注入 `Authorization: QQBot <access_token>`（客户端凭证一律被替换）。
  7. 附 `x-forwarded-host` / `x-forwarded-path`（同 proxy 行为）。
  8. `settings.default_timeout_secs` 包裹 `send()`：传输错误 → `502`（消息含 bot id）；超时 → `504`；均 `record_error()`。
  9. 响应头：剥 hop-by-hop + `content-length`；**注入 `x-qqbot-bot: <bot id>`**。
  10. body：`Body::from_stream(stream)`——原样字节流式转发，**无缓冲、无 usage tap**（与 LLM 转发不同：QQ Bot 的 OpenAPI 无 usage 语义）。
  11. 上游 5xx：状态与 body 原样透传，同时计一次错误；4xx 原样透传不计错误。
- 错误体：全部使用 `proxy::error_response` 的 house 格式。

---

## webhook.rs

### `webhook(State<AppState>, Path<String>, HeaderMap, Bytes) -> Response`

`/qqbot/callback/{app_id}` 的处理器（POST；其他方法走 fallback → 404）。

- 前置：无。
- 后置（按判定顺序）：
  1. `X-Bot-Appid` 头存在且与路径 app_id **不一致** → `401`（消息含两值）。
  2. `BotTable::find(app_id)`：`None` → `404`（**enabled=false 也走 404**：回调地址不应再可达；与转发的 503 语义不同，见 ADR-Q8）。
  3. body 解析为 `CallbackPayload`：JSON 解析失败 → `400`。
  4. `op == 13`（回调地址验证）：
     - `d` 解析为 `ValidationData`（缺字段 → 400）。
     - `sign_validation(&bot.app_secret, event_ts, plain_token)` → 200 `{"plain_token":..., "signature":...}`（`ValidationResponse`，`application/json`）。
  5. 其他 op（含 op 0 事件）：**立即** 200 `{"op":12}`（HTTP Callback ACK；先 ACK 再投递，避免平台侧超时，ADR-Q2）。
     - 随后 `tokio::spawn` 异步投递（把 body 原样字节、bot、stats guard 一起 move 进去）：
       - `bot.event_url` 为 `None` → 计一次 stats（`record_error`）并 `warn`，结束。
       - `POST event_url`：body 原样字节；`event_secret` 非空时附 `Authorization: Bearer <event_secret>`；`Content-Type` 用请求头里同一个值。
       - `5s` 超时包裹；2xx → 成功（stats guard 正常 Drop）；非 2xx / 传输错误 / 超时 → `record_error` + `warn`（**不向平台暴露，不重试**，ADR-Q1）。
- 异步投递的 **stats 归属**：投递任务自带 `botStats` guard，失败计错误数；HTTP 响应（200 ACK）本身不计 stats。

### `sign_validation(secret: &str, event_ts: &str, plain_token: &str) -> String`（模块私有）

- ed25519 签名，与参考文档 Go 示例等价：
  - `seed = secret` 字节重复直至 ≥ 32 字节，取前 32 字节；
  - `SigningKey::from_bytes(&seed)`；
  - 签名消息 = `event_ts + plain_token`（字节拼接，**无分隔符**）；
  - 输出 hex 小写（自写 `to_hex`，不引 hex crate）。
- **只签名，不验签**：入站事件推送的签名头与平台公钥不在本地参考文档内（ADR-Q7、风险 R4）。

---

## ws.rs

### `WsTable`（结构体）

构造：`WsTable::new()`。内部结构见 `data-model.md`。

| 方法 | 签名 | 调用方 | 后置 / 错误 |
|---|---|---|---|
| `ensure` | `(&self, bot: &Bot, ctx: &SpawnCtx) -> Option<()>` | 启动 / POST / PUT | 形态需要 WS（`enabled && event_source==websocket`）且表内无该 bot id → spawn supervisor 并登记；已有 → 无操作；不需要 → 无操作（调用方负责先 `stop`） |
| `stop` | `(&self, bot_id: &str)` | DELETE / 禁用 / 切回 webhook | abort handle 并移除表项；不存在 → 无操作 |
| `snapshot` | `(&self) -> Vec<WsStatus>` | admin | 全量克隆（表内顺序） |

- `SpawnCtx`：spawn 所需的句柄集合（`BotTable` 克隆、`TokenStore` 克隆、`state.client`、消息投递用的 `BotStats` map 句柄 / `event_url` 投递函数）——`AppState` 本身即可，`ensure(bot, &state)` 直接传 `&AppState`。
- supervisor 每轮重连前从 `BotTable` 复核：记录消失 / `enabled=false` / `event_source != websocket` → **自行退出**（abort 只是加速，不依赖它）。

### supervisor 任务（`spawn_supervisor(bot_id, AppState)`，模块私有）

循环重连，指数退避 base=2s、上限 60s（成功 READY/RESUMED 后归零）。每轮：

1. 复核 `BotTable::get(bot_id)`：`None` / disabled / 非 websocket → 退出任务（先从 `WsTable` 摘除自己的表项为佳，但退出以表复核为准）。
2. 取接入点 `bot.resolve_ws_url()`；`connect_async`（wss 走 rustls）。失败 → 记 `last_error`、`reconnects += 1`、退避后下一轮。
3. 收 op 10 Hello：取 `d.heartbeat_interval`（ms，异常 → 30s 兜底）。
4. 有 `session_id && last_seq > 0` → 发 op 6 Resume `{token, session_id, seq}`；否则发 op 2 Identify `{token: "QQBot <access_token>", intents, shard, properties: WS_IDENTIFY_PROPERTIES}`。token 经 `TokenStore::get_or_fetch`，失败 → 记 `last_error` 并退出（凭证问题重连无用）。
5. `tokio::select!` 心跳 ticker / 入站消息 / 任务 abort 信号：
   - 每周期发 op 1，`d` = 最新 seq（**首拍 null**）。
   - 连续 **2** 周期未收到 op 11 ack → 主动断开重连（保留 session）。
   - op 0：`t == "READY"` → 存 `session_id`、退避归零、`connected = true`；`t == "RESUMED"` → 退避归零、`connected = true`（不重存 session_id）；其余 → 记最新 seq，**异步投递事件字节到 `event_url`**（投递逻辑与 webhook ② 完全相同，同一 `deliver_event` 函数）。
   - op 7 Reconnect → 断开重连（保留 session）。
   - op 9 Invalid Session → 清 session 后重连（重新 Identify）。
   - op 11 → 重置 ack 计数。
6. 关闭码分流（平台错误码表）：
   - 4008 / 4009 → 可 Resume 重连（保留 session）。
   - 4006 / 4007 / 4900~4913 → 清 session，重新 Identify。
   - 4001 / 4002 / 4010~4014 / 4914 / 4915 → 停止（记 `last_error`，**不重连**）。
   - 其他（含 1006 等传输层码）→ 保留 session 重连。
   - 实现注意：`tungstenite::Error::Protocol(Closed(code))` 或 `Message::Close` 携带的 code 才是平台语义码；传输层错误（`Io` / `AlreadyClosed`）一律走「保留 session 重连」。

### `deliver_event(state, bot, payload: Bytes)`（模块级，webhook 与 ws 共用）

- 与 webhook ②第 5 步的异步投递逻辑完全同一实现（event_url 为 None 计错误丢弃；Bearer event_secret；5s 超时；失败 `record_error`）。
- webhook 与 ws 两处调用都先 `tokio::spawn`。

---

## admin.rs（`/__admin/qqbot/*`）

挂载于既有 `admin_router()` 内的 `nest("/qqbot", ...)`。**鉴权与路由管理端点完全相同**（`require_admin`，同 `X-Admin-Key` / `Bearer` 密钥，同一 `GATEWAY_ADMIN_KEY`）。

| 方法 | 路径 | 成功响应 | 错误 |
|---|---|---|---|
| GET | `/__admin/qqbot/bots` | `200 {"bots":[...]}` | 401 |
| POST | `/__admin/qqbot/bots` | `201` + 新机器人 JSON；`id` 省略 → UUID v4 | 400（非法/重复 id/重复 app_id）、401、422（JSON 体缺必填字段） |
| GET | `/__admin/qqbot/bots/{id}` | `200` + 单机器人 | 404、401 |
| PUT | `/__admin/qqbot/bots/{id}` | `200` + 合并后的机器人 | 400、404、401 |
| DELETE | `/__admin/qqbot/bots/{id}` | `200 {"deleted":<id>}` | 404、401 |
| GET | `/__admin/qqbot/ws` | `200 {"bots":[WsStatus...]}`，按 bot 表序 join | 401 |
| GET | `/__admin/qqbot/metrics` | `200 {"bots":[{"bot":...,"stats":...}]}` | 401 |

- 所有变更端点成功后先 `state.persist_bots()` 再返回；持久化失败 → 500。
- `BotInput`（POST body）：`id`/`name` Option（id 省略生成），`app_id`/`app_secret` 必填，其余字段（`sandbox`/`api_base`/`ws_url`/`intents`/`shard`/`event_source`/`event_url`/`event_secret`/`enabled`) Option。
- `BotPatch`（PUT body）：全 Option，与 `ProviderPatch` 同构的合并语义。`api_base`/`ws_url`/`event_url`/`event_secret` 为 **`Option<Option<String>>`**：字段缺省 = 保留原值；显式 `null` = 清除覆盖（用平台默认 / 不投递）；字符串 = 设新值（空串同 null，由 `apply_patch` 内部调 `normalize` 收敛）。**`id` 不可改**（与路由 / 供应商一致）。
- **WS 生命周期联动**（变更端点内，持久化成功之后）：
  - POST 成功且 `enabled && event_source == Websocket` → `bot_ws.ensure(&bot, &state)`。
  - PUT 成功 → `bot_ws.stop(&bot.id)`（旧 supervisor 必须重连：配置已变），若新形态需要 WS → `ensure`。
  - DELETE 成功 → `bot_ws.stop(&bot.id)`。

---

## config.rs（新增）

### `Settings` 新增字段

| 环境变量 | 字段 | 默认 | 说明 |
|---|---|---|---|
| `GATEWAY_BOTS_FILE` | `bots_file: PathBuf` | `bots.toml` | QQ 机器人配置文件 |

### `load_bots(path: &Path) -> Result<Vec<Bot>, ApiError>`

- 后置：文件不存在 → `Ok(vec![])` 并告警（非致命，与 `load_routes` 同策略）；解析失败 → `ConfigLoad`（500）。
- 重复 id 或 app_id 只告警、全部保留（`load_routes` / `load_providers` 同策略）。

### `save_bots(path: &Path, bots: &[Bot]) -> Result<(), ApiError>`

- 后置：pretty TOML + 固定托管头注释；同目录临时文件 + rename 原子替换；**重写丢注释**；`app_secret` 明文落盘（与 `Provider.api_key` 同策略）。

### `run`（lib.rs）变更

- 启动顺序：`load_routes` → `load_providers` → `load_bots` → 三个 `set_all`；三者缺失都不致命。
- `build_app` 新增 `nest("/qqbot", qqbot::qqbot_router())`，位于 `/llm` nest 与 proxy fallback 之间；`/qqbot` 前缀保留（nest 优先于 fallback，ADR-Q5）。
- 启动 spawn：`set_all` 之后，对每个 `enabled && event_source == Websocket` 的机器人调 `bot_ws.ensure`。

### `state.rs`（新增）

| 字段 | 类型 | 说明 |
|---|---|---|
| `bots` | `BotTable` | 机器人配置表 |
| `bot_stats` | `Arc<Mutex<HashMap<String, Arc<RouteStats>>>>` | 按机器人 id 存（与路由 / LLM 供应商 stats 隔离，防 id 碰撞） |
| `bot_tokens` | `TokenStore` | 按 app_id 的 token 缓存 |
| `bot_ws` | `WsTable` | WS supervisor 表 |

- `bot_stats_for(&self, bot_id: &str) -> Arc<RouteStats>`：缺则创建，与 `stats_for` / `llm_stats_for` 同构。
- `persist_bots(&self) -> Result<(), ApiError>`：序列化 `bots.list()` → `save_bots(settings.bots_file)`，与 `persist` / `persist_providers` 同构。

---

## error.rs（新增变体）

| 变体 | HTTP 状态 | 触发场景 |
|---|---|---|
| `BotNotFound(id)` | 404 | GET/PUT/DELETE 未知机器人 id |
| `InvalidBot(msg)` | 400 | `Bot::validate` 失败 |
| `DuplicateBotId(id)` | 400 | POST id 已存在 / PUT 与其他机器人 id 冲突 |
| `DuplicateBotAppId(app_id)` | 400 | POST app_id 已存在 / PUT 与其他机器人 app_id 冲突 |
| `BotDisabled(app_id)` | 503 | 转发路径机器人被禁用（回调路径不用它，见 ADR-Q8） |
| `TokenFetchFailed(msg)` | 502 | 换 token 失败（传输错误 / 超时 / 平台响应体 `code` 业务错误 / 已过期无旧 token 可回退） |

错误体沿用 `{"error":{"code":<u16>,"message":<thiserror 文本>}}`。

## 测试以本文件为准

`tests/qqbot_gateway.rs` 的每条断言对应上表某一条契约；映射见 `docs/tdd/mapping.md`。新增或修改行为前，先改本文件对应条目，再改代码与测试。
