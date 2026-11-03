# QQ Bot 事件投递对接文档（`event_url`）

本文面向**接收 QQ 机器人事件的后端服务**开发者：你要实现一个 HTTP 端点，top-gateway
会把机器人收到的平台事件逐条 POST 到这个端点。读完本文你应当能：

- 知道 `event_url` 配置在哪、怎么改、有什么约束；
- 写出一个满足投递契约的接收端点（方法、头、响应时限）；
- 理解投递的可靠性语义（**at-most-once**）并据此做幂等与去重；
- 知道出问题时看什么指标、什么日志。

> 实现权威：投递逻辑是 `src/qqbot/webhook.rs` 的 `deliver_event`，
> 契约见 `docs/sdd/qqbot/interfaces.md` §webhook.rs。本文是对实现事实的转述，
> 两者冲突时以代码为准。平台事件结构参考 `docs/reference/qq_bot_api/03-事件/`。

---

## 1. 它是什么

`event_url` 是机器人配置（`bots.toml` / `/__admin/qqbot/bots` API）里的一个字段：
**事件投递目标**。机器人有两条入站通道，无论走哪条，事件最终都由网关用同一个
`deliver_event` 实现投到同一个 `event_url`：

| `event_source` | 入站通道 | 事件来源 |
| --- | --- | --- |
| `webhook`（缺省） | 平台主动 POST 网关的 `/qqbot/callback/{app_id}` | 平台推过来的事件字节 |
| `websocket` | 网关后台维持到平台的长连接（出站 wss） | WS 帧里的 op 0 事件 |

**业务端看不出事件来自哪条通道**——投递动作与通道无关。两条通道可以按部署条件选择：
没有公网入站的部署用 `websocket`；回调地址公网可达的用 `webhook`。

投递链路的关键点是**先 ACK 再投递**：网关收到平台事件后先同步给平台回 ACK
（`{"op":12}`），再把事件字节异步 POST 给你的 `event_url`。所以：

- 你的服务**慢或不通，都不会让平台侧超时重试**——平台看到的事件接收永远是成功的；
- 反过来，你的服务**收不到**事件时，平台不会知道、也不会补推——可靠性完全由
  你这一侧的可用性决定（见 §5）。

---

## 2. 配置

### 2.1 TOML 文件

配置文件位置由 `GATEWAY_BOTS_FILE` 环境变量决定，缺省 `bots.toml`。最小配置：

```toml
[[bots]]
id = "my-bot"                       # 配置 id，表内唯一，不可改
name = "我的机器人"
app_id = "102030405"                # 平台 appId，路由键
app_secret = "DG5g3B4j9X2KOErG"     # 平台 clientSecret
event_source = "webhook"            # 或 "websocket"
event_url = "http://10.0.0.20:9000/qq/events"   # ← 本文主题
event_secret = "your-shared-secret"             # 可选，见 §4.1
enabled = true
```

字段语义：

| 字段 | 必填 | 说明 |
| --- | --- | --- |
| `event_url` | 否 | 事件投递目标；须 `http(s)://host[:port][/path][?query]`。省略 / 空串 = 不投递（事件计一次错误后丢弃，见 §5.2） |
| `event_secret` | 否 | 非空时投递附 `Authorization: Bearer <event_secret>`；空串等同不设 |

注意 `bots.toml` 是**托管文件**：任何经管理 API 的变更都会重写它（重写丢注释，
只剩托管头）。直接改文件只在启动时生效一次；运行时要改配置请走管理 API。

### 2.2 管理 API（运行时变更）

`/__admin/qqbot/bots*`，鉴权用 `X-Admin-Key` 或 `Authorization: Bearer <管理密钥>`
（未设 `GATEWAY_ADMIN_KEY` 时端点开放）。

```bash
# 新增（event_url 可省略）
curl -X POST http://gateway:8000/__admin/qqbot/bots \
  -H "X-Admin-Key: $KEY" -H "Content-Type: application/json" \
  -d '{"app_id":"102030405","app_secret":"...","event_url":"http://10.0.0.20:9000/qq/events"}'

# 修改（合并语义：缺省字段保留原值）
curl -X PUT http://gateway:8000/__admin/qqbot/bots/my-bot \
  -H "X-Admin-Key: $KEY" -H "Content-Type: application/json" \
  -d '{"event_url":"http://10.0.0.21:9000/qq/events"}'

# 清除投递目标（显式 null ≠ 缺省）
curl -X PUT http://gateway:8000/__admin/qqbot/bots/my-bot \
  -H "X-Admin-Key: $KEY" -H "Content-Type: application/json" \
  -d '{"event_url":null}'
```

`PUT` 的合并语义对 `event_url` / `event_secret` 特别重要：

| PUT body 里的写法 | 效果 |
| --- | --- |
| 字段缺省 | 保留原值 |
| `"event_url": "..."` | 设新值 |
| `"event_url": ""`（空串） | 等同 `null`，清除 |
| `"event_url": null` | 显式清除 = 不投递 |

修改成功会立即落盘并生效；`event_source = "websocket"` 的机器人**会重建 WS
连接**（新会话、重新 Identify，不是 Resume）——变更事件通道等于一次干净冷启动。

### 2.3 校验规则

配置写入前会校验 `event_url`，失败返回 400：

- scheme 必须是 `http` 或 `https`；
- host 非空；
- 前后空白会被 trim；空串归一化为「未配置」（不报错，但事件会被丢弃）。

注意：**校验只管 URL 形态**，不探测可达性。端点能不能通要靠 §8 的指标确认。

---

## 3. 投递 HTTP 契约

网关对你的端点只做一件事：**把平台给的字节原样 POST 过去**，不解析、不重序列化、
不聚合。

### 3.1 请求

| 项 | 值 |
| --- | --- |
| 方法 | `POST`（固定） |
| 目标 | 配置的 `event_url` 整体（含你写的 path / query） |
| 请求体 | 平台回调 / WS 帧的**原始字节**，逐事件一次请求，无批量 |
| `Content-Type` | webhook 模式：透传平台回调请求里的同一值（缺省 `application/json`）；websocket 模式：恒为 `application/json` |
| `Authorization` | 仅当 `event_secret` 非空时附带，值为 `Bearer <event_secret>` |

**只有这两个头**。网关不附带签名头、不附带 bot 标识头、不附带平台原始头
（如 `X-Bot-Appid` / `X-Signature-Ed25519` 都不会透传过来）。

### 3.2 响应要求

- **HTTP 状态码 2xx** 即视为投递成功，body 任意（网关不读）；
- **整个请求（含你返回响应）须在 5 秒内完成**，超时即判定失败。

### 3.3 失败判定（不重试、不暴露）

下列任一情况，网关记一次错误（该 bot 的 `errors +1`）并打 `warn` 日志，**事件丢弃，
没有重试，也不会通知平台**：

- 非 2xx 响应；
- 传输错误（连接拒绝、DNS 失败、连接重置……）；
- 5s 超时；
- `event_url` 未配置（同样计错误丢弃）。

这条语义是**at-most-once**：一个事件最多被投递一次。重试要靠你自己——如果业务上
丢事件不能接受，参考 §7.2 的建议（端点尽量不丢、落库后由业务侧重放）。

---

## 4. 请求头细节与鉴权

### 4.1 `event_secret`

`event_secret` 是**你与网关共享的投递口令**，唯一的请求侧身份依据。建议：

- 给每个机器人配不同的 `event_secret`（或不同的 `event_url`），这样多机器人投到
  同一服务时能靠口令区分来源（见 §6）；
- 端点侧**必须校验** `Authorization: Bearer <event_secret>`——不配口令又把端点暴露
  在公网，等于任何人都能给你投伪造事件。

口令是明文比对：网关不做哈希、不做时间戳。比对时用常量时间比较，避免计时侧信道。

### 4.2 为什么没有签名头

网关在平台→网关这一段**不验签**（平台入站签名公钥不在本地参考文档内，ADR-Q7/R4），
因此也不会有签名可转发给下游。你的端点能依赖的身份证据只有 `event_secret` 和网络
边界。事件**真实性**的上限由平台→网关这段决定，不要在业务逻辑里假设事件已被平台
签名验证。

---

## 5. 事件载荷

### 5.1 信封结构

webhook 模式投递的是平台 POST `/qqbot/callback/{app_id}` 的完整 body；websocket
模式投递的是 op 0 帧的完整 body。两者都是平台信封 JSON：

```json
{
  "op": 0,
  "s": 4,
  "t": "GROUP_AT_MESSAGE_CREATE",
  "d": { /* 事件数据，结构随 t 而定 */ }
}
```

| 字段 | 含义 |
| --- | --- |
| `op` | 操作码；事件推送恒为 `0`（op 13 回调验证由网关直接回签，**不会投递给你**） |
| `t` | 事件类型，如 `GROUP_AT_MESSAGE_CREATE` / `C2C_MESSAGE_CREATE` / `INTERACTION_CREATE` |
| `s` | 平台序号（用于序号回执与断线续传）；**webhook 推送不带 `s`，只有 WS 信道带** |
| `d` | 事件数据。完整字段表见 `docs/reference/qq_bot_api/03-事件/`（群聊 / 单聊 / 互动 / 频道子目录） |

实现上「非 op 13 的一律投递」，所以原则上可能收到别的 op（平台当前只推 op 0 与
op 13）；解析时对未知 `t` / `op` 保持容忍，不要因此返回非 2xx。

最常见的事件类型见 §9 速查。

### 5.2 重复投递与去重

三种情况会让你的端点收到重复事件，**去重是业务端的事**：

1. **双通道重复**：平台的 webhook 仍指向网关、同时机器人又配成 `websocket` 时，
   一个事件会被两条通道各推一次，网关两侧都投给同一个 `event_url`。两份副本可靠
   `s` 区分：webhook 副本不带 `s`，WS 副本带。**生产上避免同时保留两条入站通道。**
2. **WS 断线续传**：`websocket` 模式下连接断开重连走 Resume 时，平台可能重放
   session 内已推过的事件（带相同的 `s`）。按 session + `s` 或 `d` 内的事件 id
   幂等即可。
3. **你自己重复处理**（崩溃重放等），同上。

建议统一按事件 id（`d` 内的 `id` / `msg_id` 等）做幂等键，所有通道共用。

---

## 6. 多机器人如何区分来源

投递请求**不携带任何 bot 标识**（没有头、body 里也没有 `app_id`）。同一端点服务
多个机器人时，按优先级：

1. 给每个 bot 配不同的 `event_url` 路径（如 `/qq/events/bot-102030405`）——最
   直观；
2. 或配不同的 `event_secret`，按 `Authorization` 头区分；
3. 兜底：按事件载荷里的群 / 用户上下文推断，但这不可靠（一个 bot 的私聊事件里没
   有能反推 bot 的字段）。

---

## 7. 端点实现建议

### 7.1 必做

- [ ] 接到请求**立即返回 2xx**，5s 内；耗时处理（落库、调用下游、回复消息）**异步化**；
- [ ] 校验 `Authorization: Bearer <event_secret>`（配了口令时），失败返回 401——
      401 与其他非 2xx 一样会被计为一次投递错误，便于在 §8 指标里暴露扫描行为；
- [ ] 按事件 id 幂等（§5.2）；
- [ ] 对未知 `t` / `op` 也返回 2xx，不要用非 2xx 表达「不支持」——非 2xx 只会
      平白增加错误计数。

### 7.2 可靠性

at-most-once 意味着**端点不可用的每一秒都在丢事件**，因此：

- 端点尽量轻：接事件 → 写本地队列 / 落库 → 返回。重活交给消费者；
- 监控网关侧的 `errors`（§8.1），它对投递失败比你的应用日志更全（传输错误、
  超时都计）；
- 真正不能丢的事件，业务侧自己留重放能力（平台没有「补推某个事件」的接口）。

### 7.3 回复消息

收到事件后要回复用户（如群聊被动回复）**不要在事件处理里同步调平台**，而是走
网关的 OpenAPI 透传通道：经 `POST http://gateway:8000/qqbot/{app_id}/...` 调平台
接口（如 `/v2/groups/{group_openid}/messages`），网关自动换 `access_token` 并替换
鉴权头。被动回复的时效窗口由平台决定，注意 `msg_id + msg_seq` 的去重约束。

---

## 8. 可观测性与排障

### 8.1 指标

`GET /__admin/qqbot/metrics`（需管理密钥）：每个 bot 一份统计，字段：

| 字段 | 含义 |
| --- | --- |
| `total_requests` | 投递任务次数（含被丢弃的：`event_url` 未配也计） |
| `errors` | 投递失败次数（非 2xx / 传输错误 / 超时 / 未配 `event_url`） |
| `in_flight` | 正在投递的任务数 |
| `avg_latency_ms` | 投递平均耗时 |

注意：**平台的 ACK 不计入这些指标**，它们只描述「网关 → 你的端点」这一段。
`errors` 突然增长而 `total_requests` 平稳 = 你的端点开始回非 2xx / 超时。

`GET /__admin/qqbot/ws`（`websocket` 模式）：每个 WS 机器人的
`connected` / `session_id` / `last_seq` / `reconnects` / `last_error`。
`last_seq` 是 **WS 信道一侧**的概念（webhook 副本不带 `s`）。

### 8.2 日志

投递失败统一 `warn` 级，关键字段 `bot`（配置 id）、`app_id`、失败原因
（`status = <HTTP码>` / `error = <传输错误>` / `timed out after 5s`）。
搜 `event delivery to subscriber failed` 与 `event dropped`。

### 8.3 常见症状

| 症状 | 排查 |
| --- | --- |
| 事件一次都没收到 | `event_url` 是否配了（`GET /__admin/qqbot/bots/{id}`）；`enabled = false` 时回调侧平台推不进来（网关回 404）、WS 也不会拉起 |
| 间歇收不到 | 查 `metrics.errors`；本机回环连接拒绝约 2s、慢端点 5s 都会算失败 |
| 事件重复 | 双通道（§5.2.1）？同 `s` 重复是 WS 续传重放，按事件 id 幂等 |
| 回调地址验证（op 13）失败 | 与 `event_url` 无关：网关用 `app_secret` 直接 ed25519 回签，检查 `app_secret` 是否与平台一致 |
| 改了配置不生效 | 直改 `bots.toml` 只在启动时读；运行时改走管理 API |

---

## 9. 速查

**投递契约**：`POST <event_url>`，body = 平台原始字节，`Content-Type` 见 §3.1，
可选 `Authorization: Bearer <event_secret>`；**5s 内回 2xx**；失败不重试。

**最小接收端**（伪代码）：

```python
@app.post("/qq/events")
async def events(request: Request):
    if request.headers.get("authorization") != f"Bearer {SECRET}":
        return Response(status_code=401)
    body = await request.body()      # 平台信封 JSON，原样字节
    envelope = json.loads(body)      # {"op":0,"t":...,"s":...,"d":...}
    queue.put(envelope)              # 异步处理，立即返回
    return Response(status_code=200)
```

**常见事件类型**（完整见 `docs/reference/qq_bot_api/03-事件/`）：

| `t` | 场景 |
| --- | --- |
| `GROUP_AT_MESSAGE_CREATE` | 群里 @ 机器人 |
| `C2C_MESSAGE_CREATE` | 单聊消息 |
| `INTERACTION_CREATE` | 按钮 / 菜单互动 |
| `GROUP_ADD_ROBOT` / `GROUP_DEL_ROBOT` | 机器人被加入 / 移出群 |

**与 `event_url` 无关但容易混淆的端点**：

- `POST /qqbot/{app_id}/...`——OpenAPI 透传（出站调平台），业务端调用方；
- `POST /qqbot/callback/{app_id}`——平台回调入口（网关的，不是你的 `event_url`）。
  不要把平台后台里填的回调地址写成你的业务端点——必须填网关的
  `http(s)://<gateway>/qqbot/callback/<app_id>`，否则 op 13 验证过不了。

---

## 10. 本地联调

用仓库自带的 QQ Bot Mock 作平台替身，可以完整跑通「平台 → 网关 → 业务端点」：

1. 启动 mock 与网关（详见 `observation/socket/qqbot/README.md`）；
2. 给 bot 配 `event_url` 指向你的本地端点；
3. 参考投递观察脚本 `observation/socket/qqbot/demo_02_webhook_callback.py`——
   它用 `EventSink` 实现了一个最小接收端，可对照「先 ACK 再投递 / 慢端点不阻塞
   平台 / 投递失败不重试 / 无 event_url 计错误丢弃」等行为。
