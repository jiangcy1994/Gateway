# 数据模型（QQ Bot 子网关）

> **SDD 第 3 步**：核心类型与不变式。依赖第 2 步的模块划分——类型绝不超出架构边界。

数据模型分三层：**配置值对象**（`EventSource` / `Bot` / `BotInput` / `BotPatch`）、**运行期状态**（`BotTable` / `TokenStore` / `WsTable`）、**观测快照**（`WsStatus`）。

## 核心枚举：`EventSource`（bot.rs）

```rust
enum EventSource { Webhook, Websocket }
```

- 序列化 / TOML / JSON：`"webhook"` / `"websocket"`（全小写）。
- 决定事件通道：`Webhook` 时平台往 `/qqbot/callback/{app_id}` 推、网关被动等；`Websocket` 时网关后台连 wss。两者**不互斥地共存**：回调端点始终存在（`Webhook` 形态才真正会被平台调用），但只有 `Websocket` 形态 spawn supervisor。
- **不变式 Q0**：`POST /__admin/qqbot/bots` 与 `PUT` 接受任意值；`Websocket` 形态在平台侧未获 Webhook 权限时才有意义，网关不做权限组合校验（平台用 op 9 / 关闭码 4014 拒绝，ADR-Q10）。

## 核心值对象：`Bot`（bot.rs）

一个 QQ 机器人。同时是 TOML 文件、管理 API、内存表的共享表示（与 `Route` / `Provider` 同构的共享表示原则）。

| 字段 | 类型 | 约束 |
|---|---|---|
| `id` | `String` | 非空；表内唯一；`PUT` 不可改；POST 省略时自动生成 UUID v4（配置 id，与平台无关） |
| `name` | `String` | 展示名，默认空串 |
| `app_id` | `String` | 非空；表内唯一（平台身份；`/qqbot/{app_id}/...` 与 `/qqbot/callback/{app_id}` 用它路由） |
| `app_secret` | `String` | 非空（换 access_token 的 clientSecret，也是 ed25519 签名种子来源） |
| `sandbox` | `bool` | 默认 `false`；选择内置 base（正式 / 沙箱） |
| `api_base` | `Option<String>` | 覆盖 OpenAPI base；`Some` 时须为 `http(s)://host` 形式 |
| `ws_url` | `Option<String>` | 覆盖 WSS 接入点；`Some` 时须为 `ws(s)://host[:port][/path]` 形式 |
| `intents` | `u32` | 默认 `GROUP_AND_C2C_EVENT \| INTERACTION` = `(1<<25) \| (1<<26)`；`0` 非法（至少订阅一类事件） |
| `shard` | `[u32; 2]` | 默认 `[0, 1]`；必须 `shard[1] > 0` 且 `shard[0] < shard[1]`（`[id, total]`，无分片场景填 `[0,1]`） |
| `event_source` | `EventSource` | 默认 `Webhook` |
| `event_url` | `Option<String>` | 事件投递目标；`Some` 时须为 `http(s)://host` 形式；`None` 时事件计 stats 后丢弃 |
| `event_secret` | `Option<String>` | 投递时附 `Authorization: Bearer <event_secret>`（可为空串，空串等同不设） |
| `enabled` | `bool` | 默认 `true`；`false` 时转发返回 503、回调返回 404、不 spawn WS |

### 不变式

| # | 不变式 | 由谁保证 |
|---|---|---|
| Q1 | `app_id` / `app_secret` 非空 | `Bot::validate`（管理 API 与启动加载都调） |
| Q2 | 同一 `BotTable` 内 `id` 唯一 | `BotTable::add` / `replace`（写锁内检查） |
| Q3 | 同一 `BotTable` 内 `app_id` 唯一 | `BotTable::add` / `replace`（写锁内检查） |
| Q4 | `intents != 0` | `Bot::validate` |
| Q5 | `shard` 满足 `total > 0 && id < total` | `Bot::validate` |
| Q6 | `api_base` / `ws_url` / `event_url` 为 `Some` 时是合法 `http(s)://` / `ws(s)://` URL | `Bot::validate` |
| Q7 | 有效 base 由 `sandbox` 决定，覆盖值优先 | `Bot::resolve_api_base()` / `resolve_ws_url()`（短小纯函数，覆盖值 `Some` 直接用） |

> 与 `Route` / `Provider` 同宽容策略：`load_bots` 从磁盘读到重复 `id` 或 `app_id` 时只告警并全部保留（后续管理变更以先写表者为先，持久化重写后归一）。

## 平台常量（bot.rs）

| 常量 | 值 |
|---|---|
| `API_BASE_PRODUCTION` | `https://api.bot.qq.com` |
| `API_BASE_SANDBOX` | `https://sandbox.api.bot.qq.com` |
| `WS_URL_PRODUCTION` | `wss://api.bot.qq.com/websocket/` |
| `WS_URL_SANDBOX` | `wss://sandbox.api.bot.qq.com/websocket/` |
| `WS_IDENTIFY_PROPERTIES` | `{"$os":"top-gateway","$browser":"top-gateway","$device":"top-gateway"}` |

沙箱的 WSS 接入点本地参考文档只示例了正式环境地址，沙箱值为按 HTTP base 的推断（风险 R10）。

## 运行期状态

### `BotTable`（bot.rs）

```rust
BotTable { bots: Arc<RwLock<Vec<Bot>>> }
```

线性表（与 `ProviderTable` 同构）。热路径入口是 `find(app_id)`（转发路径再自行检查 `enabled`：未找到 404、禁用 503）。

### `TokenStore`（token.rs）

```rust
TokenStore { slots: Arc<RwLock<HashMap<String /* app_id */, Arc<TokenSlot>>>> }
TokenSlot {
    fetch_lock: tokio::sync::Mutex<()>,              // 单飞：并发刷新只跑一次
    inner:      std::sync::Mutex<TokenInner>,
}
TokenInner {
    current:     Option<TokenEntry>,
    last_fetch:  Option<Instant>,
}
TokenEntry { token: String, expires_at: Instant }
```

- **换 token 接口**：`POST {api_base}/app/getAppAccessToken`，JSON body `{appId, clientSecret}`；**HTTP 恒为 200**，成功体 `{access_token, expires_in}`，失败体 `{code, message}`——以**响应体 `access_token` 存在**判成功（ADR-Q3）。
- `expires_in` 规范为 number，但官方返回示例写成字符串 `"7200"`——用自定义反序列化**两种都收**（number → u64；字符串 → 解析为 u64，失败则丢弃该字段）（ADR-Q4）。
- **取 token 判定顺序**：
  1. `remaining > 300s` → 直接用（不经 `fetch_lock`，纯内存读）；
  2. `0 < remaining ≤ 300s`：若距 `last_fetch` ≥ 30s（刷新冷静期）则持有 `fetch_lock` 单飞刷新，**同 token 字符串不回写**（临近过期 60s 窗口外平台仍返旧 token，回写会错误延长有效期，ADR-Q6）；刷新失败 → 回退旧 token 并告警；距 `last_fetch` < 30s → 直接用旧 token；
  3. `无 token / 已过期`：持有 `fetch_lock` 强制刷新，失败 → `Err`（→ 502）。
- 并发：情形 2/3 的「检查 → 刷新 → 回写」都在 `fetch_lock` 内，后续并发请求拿到的是刷新结果；情形 1 是无锁快路径。
- token 按 **app_id** 存：同一 app_id 的配置 id 变化（重建配置行）不丢失缓存；删除机器人后 slot 残留（惰性清理，与 `QuotaTracker` 同策略）。

### `WsTable` / `WsStatus`（ws.rs）

```rust
WsTable { entries: Arc<std::sync::Mutex<HashMap<String /* bot id */, WsEntry>>> }
WsEntry { handle: tokio::task::JoinHandle<()>, status: Arc<RwLock<WsStatus>> }
WsStatus {
    bot_id: String, app_id: String,
    connected: bool, session_id: Option<String>, last_seq: u64,
    reconnects: u64, last_error: Option<String>,
}
```

- `WsStatus.session_id` / `last_seq` 跨重连保留（Resume 语义），`reconnects` 每次失败重连 +1；`connected` 在 READY / RESUMED 时置 `true`，断开时置 `false`。
- `WsTable::stop(bot_id)`：abort handle 并移除表项（删除 / 禁用 / 切回 webhook 时调用）。
- supervisor 每轮重连前从 `BotTable` 复核：记录消失 → 自行退出（abort 只是加速，不依赖它）。

### Webhook payload（webhook.rs）

```rust
struct CallbackPayload { op: u32, d: serde_json::Value, s: Option<u64>, t: Option<String> }
struct ValidationData { plain_token: String, event_ts: String }
struct ValidationResponse { plain_token: String, signature: String }
```

- op 13 的 `d` 含 `plain_token` / `event_ts`（均为字符串）；回包为 `ValidationResponse`。
- op 0 事件 payload **原样字节**透传给 `event_url`，不反序列化成具体事件类型（overview 范围外）。

### ed25519 签名（webhook.rs）

```rust
fn bot_secret_seed(secret: &str) -> [u8; 32]
fn sign_validation(secret: &str, event_ts: &str, plain_token: &str) -> String  // hex 小写
```

与参考文档 Go 示例等价：`seed = secret` 重复到 ≥ 32 字节后截前 32 字节；`SigningKey::from_bytes(&seed)`；签名消息 = `event_ts + plain_token`（字节拼接，无分隔符）；输出 hex 小写（自写 `hex` 编解码，不引 hex crate）。**只签名，不验签**（验签需要平台公钥，本地文档未提供，ADR-Q7）。

## 观测快照

`GET /__admin/qqbot/ws` 每条 = `WsStatus`（`connected` / `session_id` / `last_seq` / `reconnects` / `last_error`）。
`GET /__admin/qqbot/metrics` 每条 = `Bot` + `RouteStats::snapshot()`（与 `/__admin/metrics`、`/__admin/llm/metrics` 的 stats 形态完全一致）。

## 持久化形态

```toml
# top-gateway bots - managed file, rewrites drop comments
[[bots]]
id = "bot-primary"
name = "群答疑机器人"
app_id = "1020304050"
app_secret = "DG5g3B4j9X2KOErG"
sandbox = false
intents = 100663296           # (1<<25) | (1<<26)
shard = [0, 1]
event_source = "webhook"
event_url = "http://127.0.0.1:9000/qq/events"
enabled = true
```

`BotsFile { bots: Vec<Bot> }` → `toml::to_string_pretty`；原子替换语义与 routes / providers 文件相同（同目录临时文件 + rename，重写丢注释）。值为 `None` 的 Option 字段（上例的 `api_base` / `ws_url` / `event_secret`）**在 TOML 里被整体省略**（`to_string_pretty` 跳过 `None` 键，不是写成空串——已被 `toml_omits_none_option_keys` 测试钉住），读回时经 `serde(default)` + `Bot::normalize` 归一化为 `None`；JSON 管理 API 中 `None` 保持 `null`，`Some("")` 在 `normalize` 里也被收敛为 `None`（空串等同未设）。`app_secret` 以明文落盘（与 `Provider.api_key` 同策略，ADR-Q11）。

## 数据流上的形态

```
bots.toml ──load_bots──→ Vec<Bot> ──set_all──→ BotTable
                                  ▲                     │
                                  │                     ├─ find(app_id) → forward（enabled 检查） → token::get_or_fetch
                                  │                     ├─ find(app_id) → webhook 回调
                                  │                     ├─ add/replace/remove ← /__admin/qqbot/bots
                                  │                     └──── save_bots（原子写回）──┘
                                  │
换 token ──单飞──→ TokenStore.slots[app_id] ──→ Authorization: QQBot <token>
平台 wss ──supervisor──→ 事件字节 ──deliver_event──→ POST bot.event_url
平台 webhook ──op13 签名应答 / op0 ACK──→ 事件字节 ──deliver_event──→ 同上
RouteStats（bot_stats map）──快照──→ /__admin/qqbot/metrics
WsStatus ──快照──→ /__admin/qqbot/ws
```
