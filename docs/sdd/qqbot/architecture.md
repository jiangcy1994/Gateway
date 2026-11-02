# 架构（QQ Bot 子网关）

> **SDD 第 2 步**：模块划分与请求流程。只使用第 1 步（`overview.md`）定义过的概念。

## 模块划分

QQ Bot 子网关是 top-gateway 内嵌模块，代码在 `src/qqbot/`，与既有模块的集成点只有三个：`state.rs`（共享状态）、`config.rs`（持久化）、`lib.rs`（路由组装与后台任务启动）。

| 模块 | 职责 | 依赖 |
|---|---|---|
| `qqbot/bot.rs` | `EventSource` 枚举、`Bot` 值对象、`BotInput`/`BotPatch`、`BotTable`（与 `ProviderTable` 同构）、平台 base URL 常量 | `error.rs` |
| `qqbot/token.rs` | `TokenStore`：按机器人 app_id 换取 / 缓存 / 单飞续期 access_token；解析响应体 `code` 业务错误 | `bot.rs`、`state.rs`、`error.rs` |
| `qqbot/forward.rs` | `/qqbot/{app_id}/*` 转发处理器：选机器人 → 取 token → 注入 `QQBot` 头 → 流式转发 | `bot.rs`、`token.rs`、`state.rs`、`proxy.rs` 的 `error_response` |
| `qqbot/webhook.rs` | `/qqbot/callback/{app_id}` 回调端点：op 13 签名应答、op 0 事件 ACK + 异步投递 | `bot.rs`、`state.rs` |
| `qqbot/ws.rs` | WebSocket 客户端 supervisor（连接 / Identify / Resume / 心跳 / 关闭码分流 / 退避重连）与 `WsTable` 连接状态 | 以上全部 + `tokio-tungstenite` |
| `qqbot/admin.rs` | `/__admin/qqbot/*` 端点（机器人 CRUD / WS 状态 / 指标），复用 `require_admin` | `bot.rs`、`ws.rs`、`state.rs`、`metrics.rs` |

## 请求流程

### ① 客户端调用 OpenAPI（`/qqbot/{app_id}/...`）

```
build_app 把 qqbot::qqbot_router() nest 在 "/qqbot"
  │  其余路径照旧落到 proxy fallback（routes.toml 前缀转发）
  ▼
forward handler（qqbot/forward.rs）
 1. 解 path：剥 "/qqbot"，取首段为 app_id 段；空 → 404（消息含提示）
 2. BotTable::find(app_id)：未找到 → 404；enabled=false → 503（BotDisabled；回调路径则统一 404，见 ADR-Q8）
 3. token::get_or_fetch(bot)：
      ├─ 缓存命中（剩余 > 300s）→ 零 HTTP 往返
      ├─ 快过期（0 < 剩余 ≤ 300s）且过了冷静期 → 单飞刷新；失败回退旧 token
      ├─ 无/已过期 → 强制刷新；失败（含平台响应体 code 业务错误）→ 502
      └─ 并发请求只触发一次换 token（单飞锁）
 4. bot_stats_for(bot.id).guard()：RouteStats in-flight / 延迟 / 计数（RAII）
 5. 上游 URL = api_base.trim_end('/') + 剩余路径 + 原查询串
 6. 请求头：剥 hop-by-hop / content-length / host / authorization；
      注入 Authorization: QQBot <access_token>（客户端凭证一律被替换）
    附 x-forwarded-host / x-forwarded-path
 7. default_timeout_secs 包裹 send()：传输错误 → 502；超时 → 504（均 record_error）
 8. 响应头：剥 hop-by-hop / content-length；附 x-qqbot-bot: <bot id>
 9. 响应 body：Body::from_stream（流式，无缓冲）
10. 上游 5xx → record_error 后原样透传
```

### ② 平台 Webhook 回调（`/qqbot/callback/{app_id}`）

```
webhook handler（qqbot/webhook.rs）
 1. X-Bot-Appid 头存在且与路径 app_id 不一致 → 401
 2. BotTable::find(app_id) 未找到 → 404（enabled=false 也走 404：回调地址不应再可达）
 3. 读 body → 解析 payload：
      ├─ op == 13（回调验证）→ 用 bot.app_secret 派生 ed25519 私钥，
      │     对 event_ts + plain_token 签名（hex 小写）
      │     → 200 {"plain_token":..., "signature":...}
      └─ 其他（op 0 事件等）→ 立即回 200 {"op":12}（HTTP Callback ACK）
           → 异步任务：POST bot.event_url（可选 Bearer event_secret）
              ├─ 没配 event_url → 计一次 stats 后丢弃（并在日志记 warn）
              ├─ 2xx → 成功
              └─ 非 2xx / 传输错误 / 5s 超时 → record_error
```

### ③ WebSocket 客户端（后台 supervisor，每机器人一个）

```
启动 / 机器人创建 / PUT 改为 event_source=websocket 时 spawn（ws.rs）
 supervisor（循环重连，指数退避 base=2s，上限 60s）
  ├─ 每轮重新读 BotTable 的最新配置（删除/禁用/切回 webhook → 退出）
  ├─ 取接入点：bot.ws_url 覆盖值，否则按 sandbox 取平台常量
  ├─ connect_async（wss 用 rustls）→ 收 op 10 Hello（心跳周期，ms）
  ├─ 有 session_id + seq → 发 op 6 Resume；否则发 op 2 Identify
  │     {token: "QQBot <access_token>", intents, shard, properties}
  ├─ 收 op 0：t == READY → 存 session_id、退避归零、状态置 connected
  │             t == RESUMED → 同上（不重存 session_id）
  │             其余 → 记最新 seq；异步投递事件到 event_url（同 ②）
  ├─ select：心跳 ticker / 入站消息
  │     ├─ 每周期发 op 1（d = 最新 seq，首拍 null）
  │     ├─ 连续 2 周期未收到 op 11 ack → 主动断开重连（保留 session）
  │     ├─ op 7 Reconnect → 重连（保留 session）
  │     ├─ op 9 Invalid Session → 清 session 后重连（重新 Identify）
  │     └─ op 11 ack → 重置 ack 计数
  └─ 关闭码分流（平台错误码表）：
        4008/4009 → 可 Resume 重连
        4006/4007/4900~4913 → 清 session，重新 Identify
        4001/4002/4010~4014/4914/4915 → 停止（记最近错误，不重连）
        其他（含 1006 等传输层码） → 保留 session 重连
```

### 管理请求（`/__admin/qqbot/*`）

挂载在既有 `admin_router()` 内（`nest("/qqbot", qqbot::admin_router())`），鉴权复用 `require_admin`（同一下 `X-Admin-Key` / `Bearer` 密钥）。变更端点成功后调 `state.persist_bots()`——与路由持久化同构（同目录临时文件 + rename，托管头注释，重写丢注释）；创建 / 更新 / 删除同时维护 WS supervisor 的生命周期（见下）。

## WS 生命周期管理

`AppState.bot_ws`（`WsTable`）存每个机器人的 supervisor `JoinHandle` 与 `WsStatus` 镜像。规则：

| 触发 | 动作 |
|---|---|
| 启动加载完成 | 对每个 `enabled && event_source=websocket` 的机器人 spawn supervisor |
| `POST /__admin/qqbot/bots` 且目标形态需要 WS | spawn |
| `PUT /__admin/qqbot/bots/{id}` | abort 旧 supervisor；若新形态需要 WS 则 spawn 新的（配置已变，必须重连） |
| `DELETE` / `enabled=false` / `event_source=webhook` | abort 并移除表项 |

supervisor 自己在每轮重连前复核 BotTable，因此**配置被删除时任务自然退出**，abort 只是加速。

## 与 top-gateway 既有行为的关系

- `/qqbot` 前缀**保留**：`nest` 优先于 fallback，即使 `routes.toml` 挂了同名前缀也不会被代理转发（ADR-Q5）。
- `Bot` 是独立配置面，不与 `Route` / `Provider` 混用：`routes.toml` 管「前缀 → 上游」，`providers.toml` 管「协议 → LLM 供应商池」，`bots.toml` 管「appId → QQ 机器人凭证与事件通道」。
- 指标复用 `metrics::RouteStats`（按机器人 id 存在独立 map `bot_stats`，与路由 / 供应商 stats 隔离，防 id 碰撞）。
- 健康检查不覆盖机器人：OpenAPI 可用性由 502 状态码与 `/__admin/qqbot/metrics` 的错误数反映；WebSocket 连接状态由 `/__admin/qqbot/ws` 反映（连接状态不是 HTTP 健康，不进 `/__admin/health`）。
- 换 token 与事件投递复用 `state.client`（reqwest 连接池共享，与 LLM 转发同池）。
