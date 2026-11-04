# QQ Bot 事件投递接入文档（面向业务服务开发者）

**文档性质**：本文是给业务后端团队的接入契约文档。读者只需按本文实现一个 HTTP 接收端点（`event_url`）与回复调用，不需要了解 top-gateway 的内部实现。

**溯源约定**：本文每条契约细节均标注来源，优先级为——网关行为以实现权威 `docs/sdd/qqbot/interfaces.md` 为准（`CLAUDE.md` 指明）；平台协议（事件结构、API 枚举、错误码）以 `docs/reference/qq_bot_api/` 官方参考目录为准；`src/` 源码作为行为印证。材料间冲突在文中显式注明。凡未能从任何材料确认的事项，标注「（此处未从源码/文档确认，待核实）」。

---

## 1. 概述与角色

### 1.1 网关的位置

```
用户操作（在群里 @ 机器人 / 发单聊 / 点按钮 / 加好友 …）
  │
  ▼
QQ 开放平台
  │  事件下发（两条入站通道，由 bot 配置 event_source 决定）：
  │   ① webhook：平台 POST 网关的回调地址 /qqbot/callback/{app_id}（需公网入站可达）
  │   ② websocket：网关主动连平台的 wss 会话（无需公网入站）
  ▼
top-gateway（终结全部平台协议：op 13 回调验签、先 ACK 再投递、WS 心跳/断线重连、token 获取与刷新）
  │  出站投递：把平台下发的事件字节原样 POST 到 bot 配置的 event_url
  ▼
你的业务后端（event_url 指向的 HTTP 服务）——回 2xx 表示收到

……回复方向……
你的业务后端
  │  调网关的 OpenAPI 透传：/qqbot/{app_id}/v2/...（无需自带 Authorization）
  ▼
top-gateway（自动注入 Authorization: QQBot <token> 并缓存刷新）
  │
  ▼
QQ 开放平台
```

### 1.2 event_url 是什么

`event_url` 是**网关把事件投给业务后端时使用的出站 HTTP POST 地址**，由你在机器人配置（`bots.toml` 或 `POST /__admin/qqbot/bots`）中设置。路径完全由业务端自定义，网关按配置原样 POST（`src/qqbot/webhook.rs:115-116`）。

需要澄清的一点：网关**不做跨通道归一化**。webhook 通道投递的是平台回调的原始请求体字节，WS 通道投递的是原始 WS 文本帧字节，两者都进入同一个 `deliver_event` 实现（`src/qqbot/webhook.rs:92`、`src/qqbot/ws.rs:327-332` 的注释「事件字节原样投递（与 webhook 同一实现）」）。因此「接入形态对你不可见」指的是**投递行为一致**，而非网关会改写字节——字段顺序与多余字段以平台实际发送为准（见 §3.2）。

### 1.3 一条事件的完整路径（以群里 @ 机器人为例）

1. 用户在群里 @ 机器人发消息「 /今日天气 」。
2. 平台生成 `t = "GROUP_AT_MESSAGE_CREATE"` 的 Dispatch（op 0）事件。
3. 平台通过配置的通道下发：webhook 形态 POST 到 `http://<gateway>/qqbot/callback/<app_id>`；或 WS 形态推到网关维持的会话。
4. 网关**立即**给平台回 ACK（webhook 通道回 `{"op":12}`，`src/qqbot/webhook.rs:93`），随后异步把事件字节 POST 到 `event_url`。
5. 你的业务后端在 5 秒内回任意 2xx，即完成接收。之后业务端可用事件 `d.id` 作 `msg_id` 被动回复（见 §5）。

---

## 2. 接入契约（逐字精确）

### 2.1 方法与路径

| 项 | 值 | 来源 |
| --- | --- | --- |
| HTTP 方法 | **POST**（固定，业务端不要接受其它方法） | `src/qqbot/webhook.rs:115-116` |
| 路径 | 由 `event_url` 配置决定，业务端自定义（如 `http://10.0.1.20:9000/qq/events`） | `src/qqbot/bot.rs:87-90` |

### 2.2 请求头

网关向 `event_url` 发出的投递请求，**只注入两个契约头**（`src/qqbot/webhook.rs:117-121`）：

| 头 | 值 | 条件 | 来源 |
| --- | --- | --- | --- |
| `Content-Type` | webhook 通道：**取自平台回调请求的 `content-type` 头，缺失时回退 `application/json`**；WS 通道：固定 `application/json` | 始终携带 | `src/qqbot/webhook.rs:87-91`；`src/qqbot/ws.rs:332` |
| `Authorization` | `Bearer <event_secret>` | 仅当 bot 配置的 `event_secret` **非空**时附加（空串等同未设，`src/qqbot/bot.rs:91-93`、`218-227`） | `src/qqbot/webhook.rs:119-121` |

注意实际在 wire 上Received 的请求不止这两个头：投递共用网关的默认 reqwest 客户端（`src/state.rs:39-41`），该客户端给每个出站请求附加默认头 `accept: */*`，hyper 另自动添加 `host`、`content-length` 等 HTTP 框架头；**投递请求不带 `User-Agent`**。业务端应只把 `content-type` / `authorization` 当作网关契约头来解析，不要对请求头总数做严格断言。

关键事实：**平台的签名头（`X-Signature-Ed25519`、`X-Signature-Timestamp`、`X-Bot-Appid` 等）不会转发给业务端**——网关在 op 13 时自行回签（只签不验，ADR-Q7，`src/qqbot/webhook.rs:155-158`），投递请求是全新构造的。因此业务端**无法验证平台签名**，来源验证只能靠 `event_secret`（见 §8.1）。

### 2.3 响应要求

| 项 | 值 | 来源 |
| --- | --- | --- |
| 成功判据 | HTTP 状态码 **2xx**（任意 2xx，不要求 200） | `src/qqbot/webhook.rs:124` |
| 超时 | **5 秒**，包裹建连 + 发包 + 等响应头 | `src/qqbot/webhook.rs:17`（`const DELIVER_TIMEOUT`）、`:123` |
| 响应体 | 网关**只读状态码，从不读取业务端的响应体** | `src/qqbot/webhook.rs:124-152` |

业务端应在收到请求后尽快回 2xx；处理慢于 5 秒会触发超时失败（§2.4）。耗时处理应在回应之后异步进行。

### 2.4 可靠性语义（务必先读再设计）

- **异步 fire-and-forget**：投递由 `tokio::spawn` 发起，与平台的 ACK 响应互不等待（`src/qqbot/webhook.rs:92`）。webhook 通道是**先 ACK 再投递**——注释原文「先 ACK，再异步投递（先 ACK 再投递，避免平台侧超时，ADR-Q2）。投递任务自带 stats guard；ACK 本身不计 stats」（`src/qqbot/webhook.rs:85-86`）。**结论：你的业务端再慢，平台也不会因此重推砸向你。**
- **at-most-once，失败不重试**：非 2xx / 传输错误 / 超时一律 `record_error()` + `tracing::warn!`，**不重试、不向平台暴露**（ADR-Q1，`src/qqbot/webhook.rs:96-99`、`125-152`）。投递失败即事件**丢失**。
- **无 `event_url`（未配置 / 空串）**：计一次错误并丢弃，日志 `event dropped: bot has no event_url configured`（`src/qqbot/webhook.rs:104-112`）。
- **投递成功无任何日志**：成功分支为空实现（`src/qqbot/webhook.rs:124` 的 `Ok(Ok(response)) if response.status().is_success() => {}`）。业务端无法从网关日志确认「某条事件已投递成功」，只能看到失败侧的 warn。
- 可能的重复：单通道内每事件至多投一次，但**若平台侧同时存在 webhook 推送与 WS 会话**（本仓演示套件实测过该情形），两条通道都会把同一事件投给同一个 `event_url`——**去重是业务端的责任**（`observation/socket/qqbot/README.md`「webhook 与 WS 会重复投递同一事件」条目；可按事件外层 `id` 去重）。
- 平台侧本身也可能重推：官方事件文档明确「为确保消息可达，相同 msg_id 可能重复推送，开发者需结合 msg_seq 做去重」（`docs/reference/qq_bot_api/03-事件/群聊事件/群@机器人消息.md` 第 4 行）。

### 2.5 因此业务端必须做的

1. **幂等处理**：以事件外层 `id`（消息类事件中外层 `id` 与 `d.id` 相同，`docs/reference/qq-bot-mock-api.md` §9；官方信封字段见 §3.1）做去重键，同一 id 重复到达只处理一次。
2. **自己保证可靠性**：若不能接受丢失，在回 2xx 之前先把事件落入持久队列（数据库 / 消息队列），再异步消费。网关不提供重试、死信、回放。
3. **5 秒内响应**：或明确接受超时即丢失的语义。
4. **配置好 `event_url` 并监控 `errors`**（见 §6）。

### 2.6 event_url 的配置约束

- 必须 http/https scheme 且含 host，否则校验失败返回 400（`src/qqbot/bot.rs:206-208`、`313-333`；`src/qqbot/admin.rs:54`、`80`）。
- `None` / 空串 / 纯空白视为未设（校验通过，但事件会被丢弃，`src/qqbot/bot.rs:313-315`）。
- 修改用 `PUT /__admin/qqbot/bots/{id}`：字段缺省 = 保留原值，显式 `null` = 清除（`Option<Option<String>>` 合并语义，`src/qqbot/bot.rs:140-143`、`164-167`）。

---

## 3. 事件负载结构

### 3.1 统一信封

平台上下行消息同一结构（`docs/reference/qq_bot_api/01-接入准备/事件订阅通用数据结构.md`）：

```json
{
  "id": "event_id",
  "op": 0,
  "d": {},
  "s": 42,
  "t": "GATEWAY_EVENT_NAME"
}
```

| 字段 | 描述 |
| --- | --- |
| `id` | 事件 id（消息类事件中与 `d.id` 相同，可作被动回复的 `msg_id`） |
| `op` | opcode，见 §3.3 |
| `s` | 下行消息序列号，标识消息唯一性；**webhook 通道推送可能不带此字段**（见 §3.2） |
| `t` | 事件类型，仅在 op 为 0 Dispatch 时有值 |
| `d` | 事件内容，不同事件类型结构不同 |

网关侧解析只要求 `op`（u32）与 `d` 存在，`s`/`t` 可选（`src/qqbot/webhook.rs:184-194` 的 `CallbackPayload`）；**业务端应把 `s`/`t`/`id` 都当作可能缺失处理**，以平台实际下发为准。

### 3.2 两条通道投递的包是否一致

- 结构一致、字节不经一致化处理：webhook 通道投递**平台回调的原始 body 字节**（`src/qqbot/webhook.rs:26` 的 `body: Bytes` 原样传出，`:92`）；WS 通道投递**原始 WS 文本帧字节**（`src/qqbot/ws.rs:331`），Content-Type 固定 `application/json`（`:332`）。
- 两者的字段集相同（`webhook.rs:184-194` 与 `ws.rs:59-68` 均为 `{op, d, s?, t?}` 解析），但网关**不做跨通道归一化重序列化**，字段顺序 / 多余字段以平台实际发送为准。
- 实测差异（本仓演示套件对 QQ Bot Mock 的观察，可作参考）：**webhook 推送不带 `s`，只有 WS 帧带 `s`**——`s`（以及网关 WS 状态里的 `last_seq`）实质是 WS 信道一侧的概念（`observation/socket/qqbot/README.md` 第 78-82 行：两份副本可由外层 `s` 区分，webhook 推送不带 `s`，只有 WS 帧带；mock 文档 §9 的推送信封带 `s` 属期望形态，实现未跟上）。**（真实平台 webhook 推送是否一定携带 `s`，此处未从官方文档逐字确认，待核实。）**
- op 13（回调地址验证）**永远不会投递**（`src/qqbot/webhook.rs:66-83` 直接回签名 JSON）；WS 通道只在 `op == 0` 且 `t` 不是 `READY` / `RESUMED` 时投递（`src/qqbot/ws.rs:296-334`）。

### 3.3 op 码速查表

摘自 `docs/reference/qq_bot_api/01-接入准备/事件订阅通用数据结构.md`（OpCode 含义表）：

| CODE | 名称 | 接入方式 | 客户端行为 | 业务端是否可见 |
| --- | --- | --- | --- | --- |
| 0 | Dispatch | webhook/websocket | Receive | **是——这是唯一会投递到 event_url 的 op** |
| 1 | Heartbeat | websocket | Send/Receive | 否（网关 WS 心跳维护） |
| 2 | Identify | websocket | Send | 否（网关建连鉴权） |
| 6 | Resume | websocket | Send | 否（网关断线恢复） |
| 7 | Reconnect | websocket | Receive | 否（网关收到后重连，保留 session） |
| 9 | Invalid Session | websocket | Receive | 否（网关清 session 重连） |
| 10 | Hello | websocket | Receive | 否（建连首帧，含 heartbeat_interval） |
| 11 | Heartbeat ACK | websocket | Receive/Reply | 否（网关心跳应答） |
| 12 | HTTP Callback ACK | webhook | Reply | 否——这是**网关回给平台**的 ACK 体 `{"op":12}`（`src/qqbot/webhook.rs:93`） |
| 13 | 回调地址验证 | webhook | Receive | 否——网关直接回签 `{"plain_token":..., "signature":...}`（`src/qqbot/webhook.rs:77-83`、`203-208`） |

业务端的 event_url 处理器只需识别 `op == 0`，按 `t` 分发即可；建议对未知 `t` 回 2xx 并忽略，以免平台新增事件类型时丢失。

---

## 4. 事件清单

订阅由 bot 配置的 `intents` 位标记决定（`docs/reference/qq_bot_api/01-接入准备/事件订阅通用数据结构.md`「事件订阅 Intents」；`intents != 0` 是 bot 校验项之一，`src/qqbot/bot.rs:184-213`）。下表覆盖**群聊 / 单聊 / 互动域**的事件；其余域——频道（`GUILDS` 1<<0、`GUILD_MEMBERS` 1<<1、`GUILD_MESSAGES` 1<<9、`GUILD_MESSAGE_REACTIONS` 1<<10、`DIRECT_MESSAGE` 1<<12、`PUBLIC_GUILD_MESSAGES` 1<<30）、消息审核（`MESSAGE_AUDIT` 1<<27）、论坛（`FORUMS_EVENT` 1<<28，仅私域）、音频（`AUDIO_ACTION` 1<<29）——见同一文档位定义表与 `docs/reference/qq_bot_api/03-事件/频道事件/`。

### 4.1 事件总表

| 事件 `t` | Intent（位） | 触发条件 | 关键 `d` 字段 |
| --- | --- | --- | --- |
| `GROUP_AT_MESSAGE_CREATE` | `GROUP_AND_C2C_EVENT` (1<<25) | 用户在群里 @ 机器人发消息 | `id`、`author.member_openid`、`content`（已去除 @ 前缀）、`group_openid`、`timestamp`、`message_scene` |
| `GROUP_MESSAGE_CREATE` | (1<<25) | 机器人开启「接收所有消息」后，群里每条消息（不限于 @）；各字段含义与 `GROUP_AT_MESSAGE_CREATE` 完全一致（`03-事件/群聊事件/群消息-全量模式.md`） | 同上；`content` 中 @ 成员替换为 `<@member_openid>`、@ 全体替换为 `<@all>` |
| `C2C_MESSAGE_CREATE` | (1<<25) | 用户单聊发消息给机器人 | `id`、`author.user_openid`、`content`、`timestamp`、`message_scene` |
| `FRIEND_ADD` | (1<<25) | 用户添加机器人好友 | `openid`、`timestamp`（Unix 秒）、`scene`、`scene_param`、`author.union_openid` |
| `FRIEND_DEL` | (1<<25) | 用户删除机器人好友 | `openid`、`timestamp`（Unix 秒） |
| `C2C_MSG_RECEIVE` | (1<<25) | 用户在机器人资料卡开启「主动消息」推送 | `openid`、`timestamp`（Unix 秒） |
| `C2C_MSG_REJECT` | (1<<25) | 用户在机器人资料卡关闭「主动消息」推送 | `openid`、`timestamp`（Unix 秒） |
| `GROUP_ADD_ROBOT` | (1<<25) | 机器人被添加到群聊 | （本仓无独立事件文档，字段未从材料确认，待核实） |
| `GROUP_DEL_ROBOT` | (1<<25) | 机器人被移出群聊 | 同上（待核实） |
| `GROUP_MSG_RECEIVE` | (1<<25) | 群管理员在机器人资料页开启通知 | 同上（待核实） |
| `GROUP_MSG_REJECT` | (1<<25) | 群管理员在机器人资料页关闭通知 | 同上（待核实） |
| `INTERACTION_CREATE` | `INTERACTION` (1<<26) | 用户与机器人互动（按钮点击 / 快捷菜单 / 消息反馈 / 授权等） | `id`（**无 `INTERACTION_CREATE:` 前缀**，即 interaction_id）、`type`、`scene`、`chat_type`、`data.resolved`、`user_openid` / `group_openid` |

事件来源：`docs/reference/qq_bot_api/01-接入准备/事件订阅通用数据结构.md`、`05-速查附录/事件与Intents对照表.md`、`03-事件/群聊事件/群@机器人消息.md`、`03-事件/群聊事件/群消息-全量模式.md`、`03-事件/单聊事件/单聊消息事件.md`、`03-事件/单聊事件/用户添加好友.md`、`03-事件/单聊事件/用户删除好友.md`、`03-事件/单聊事件/单聊消息接收开启.md`、`03-事件/单聊事件/单聊消息接收关闭.md`、`03-事件/互动事件/互动事件.md`。

注：`GROUP_ADD_ROBOT` / `GROUP_DEL_ROBOT` / `GROUP_MSG_RECEIVE` / `GROUP_MSG_REJECT` 在 intents 位定义表中有定义（`01-接入准备/事件订阅通用数据结构.md` 第 88-91 行），但本仓 `03-事件/` 无独立事件体文档，字段结构未从材料确认。若需接收，请以 QQ 官方平台文档为准。

### 4.2 真实 JSON 示例

以下为**投递到 event_url 的完整信封**（外层信封 + `d` 取自官方事件示例）。

**群 @ 机器人消息（`GROUP_AT_MESSAGE_CREATE`）**——`d` 摘自 `docs/reference/qq_bot_api/03-事件/群聊事件/群@机器人消息.md` 示例 1：

```json
{
  "id": "ROBOT1.0_xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx",
  "op": 0,
  "s": 3,
  "t": "GROUP_AT_MESSAGE_CREATE",
  "d": {
    "id": "ROBOT1.0_xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx",
    "author": {
      "id": "A1B2C3D4E5F6A1B2C3D4E5F6A1B2C3D4",
      "member_openid": "A1B2C3D4E5F6A1B2C3D4E5F6A1B2C3D4",
      "member_role": "member",
      "username": "小明",
      "bot": false
    },
    "content": " /今日天气 ",
    "group_openid": "B2C3D4E5F6A1B2C3D4E5F6A1B2C3D4E5",
    "message_type": 0,
    "timestamp": "2026-07-21T10:00:00+08:00",
    "message_scene": {
      "source": "default",
      "ext": [
        "msg_idx=REFIDX_xxxxxxxxxxxxxxx==",
        "auth_token=xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"
      ]
    }
  }
}
```

注意 `content` 前后各有一个空格（平台 @ 前缀占位），业务端解析前建议 `trim`（官方文档注：`content` 字段已自动去除 @ 机器人的前缀；mock 文档 §9.1 亦注明「前后各补一个空格」）。

**带图片附件的群消息**——`d` 摘自同文件示例 2：

```json
{
  "id": "ROBOT1.0_yyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyy",
  "author": {
    "id": "C3D4E5F6A1B2C3D4E5F6A1B2C3D4E5F6",
    "member_openid": "C3D4E5F6A1B2C3D4E5F6A1B2C3D4E5F6",
    "member_role": "member",
    "username": "小红",
    "bot": false
  },
  "content": " 看看这张风景照 ",
  "group_openid": "B2C3D4E5F6A1B2C3D4E5F6A1B2C3D4E5",
  "message_type": 0,
  "timestamp": "2026-07-21T10:05:00+08:00",
  "attachments": [
    {
      "content_type": "image/jpeg",
      "filename": "photo.jpg",
      "url": "https://multimedia.nt.qq.com.cn/download?appid=xxx&fileid=xxx&rkey=xxx&spec=0",
      "width": 1920,
      "height": 1080,
      "size": 256000
    }
  ]
}
```

**单聊消息（`C2C_MESSAGE_CREATE`，纯文本）**——摘自 `03-事件/单聊事件/单聊消息事件.md` 示例 1：

```json
{
  "id": "ROBOT1.0_xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx",
  "author": {
    "id": "A1B2C3D4E5F6A1B2C3D4E5F6A1B2C3D4",
    "user_openid": "A1B2C3D4E5F6A1B2C3D4E5F6A1B2C3D4",
    "union_openid": "",
    "username": "",
    "bot": false
  },
  "content": "你好，今天有什么推荐的活动吗？",
  "message_type": 0,
  "message_scene": {
    "source": "default",
    "ext": ["msg_idx=REFIDX_xxxxxxxxxxxxxxx=="]
  },
  "timestamp": "2026-07-21T10:00:00+08:00"
}
```

**单聊卡片消息（`message_type` = 3，`ark_data` 有值）**——摘自同文件示例 2：

```json
{
  "id": "ROBOT1.0_yyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyy",
  "author": {
    "id": "B2C3D4E5F6A1B2C3D4E5F6A1B2C3D4E5",
    "user_openid": "B2C3D4E5F6A1B2C3D4E5F6A1B2C3D4E5",
    "union_openid": "B2C3D4E5F6A1B2C3D4E5F6A1B2C3D4E5",
    "username": "",
    "bot": false
  },
  "content": "[卡片消息] 小程序\n摘要: [每日打卡]快来完成今日学习打卡",
  "message_type": 3,
  "ark_data": {
    "ark_type": "miniapp",
    "ark_name": "小程序",
    "prompt": "[每日打卡]快来完成今日学习打卡",
    "fields": {
      "title": "快来完成今日学习打卡",
      "source": "学习助手",
      "tag": "微信小程序",
      "preview": "https://pubminishare-30161.picsz.qpic.cn/preview_a1b2c3d4",
      "source_logo": "https://miniapp.gtimg.cn/generated-icon/app_a1b2c3d4.png",
      "tag_icon": "https://miniapp.gtimg.cn/public/miniwx.png"
    }
  },
  "message_scene": {
    "source": "default",
    "ext": ["msg_idx=REFIDX_yyyyyyyyyyyyyyy=="]
  },
  "timestamp": "2026-07-21T10:01:00+08:00"
}
```

**互动事件（`INTERACTION_CREATE`，消息按钮点击，单聊场景）**——摘自 `03-事件/互动事件/互动事件.md` 示例 1：

```json
{
  "application_id": "1904842048",
  "chat_type": 2,
  "data": {
    "resolved": {
      "button_data": "confirm:once",
      "button_id": "allow-once"
    },
    "type": 11
  },
  "id": "1b13d569-4610-4ab9-bc51-feecc5def6d4",
  "scene": "c2c",
  "timestamp": "2026-07-20T21:53:54+08:00",
  "type": 11,
  "user_openid": "A1B2C3D4E5F6A1B2C3D4E5F6A1B2C3D4",
  "version": 1
}
```

群聊按钮点击示例（同文件示例 2）差异仅在 `chat_type: 1`、`scene: "group"`、携带 `group_openid` / `group_member_openid`、`id` 为 `06915133-7aef-46ed-94f7-c50939e285ae`。

**用户添加好友（`FRIEND_ADD`）**——摘自 `03-事件/单聊事件/用户添加好友.md`：

```json
{
  "openid": "A1B2C3D4E5F6A1B2C3D4E5F6A1B2C3D4",
  "timestamp": 1784570523,
  "scene": 1001,
  "scene_param": "",
  "author": {
    "union_openid": "A1B2C3D4E5F6A1B2C3D4E5F6A1B2C3D4"
  }
}
```

`scene` 枚举：`1000`=缺省默认、`1001`=网络搜索（全部 tab）、`1002`=网络搜索（机器人 tab）、`1003`=群场景、`1004`=空间场景、`2001`=站内分享资料页、`2002`=站外分享资料页、`2003`=开发者生成的分享链接（站内）、`2004`=开发者生成的分享链接（站外）（同文件事件体表）。

**用户删除好友（`FRIEND_DEL`）**——摘自 `03-事件/单聊事件/用户删除好友.md`：

```json
{
  "openid": "A1B2C3D4E5F6A1B2C3D4E5F6A1B2C3D4",
  "timestamp": 1784570524
}
```

**单聊消息接收开启 / 关闭（`C2C_MSG_RECEIVE` / `C2C_MSG_REJECT`）**——分别摘自对应事件文档：

```json
{ "openid": "A1B2C3D4E5F6A1B2C3D4E5F6A1B2C3D4", "timestamp": 1784570617 }
{ "openid": "A1B2C3D4E5F6A1B2C3D4E5F6A1B2C3D4", "timestamp": 1784570599 }
```

注意这两类（以及 `FRIEND_ADD` / `FRIEND_DEL`）事件的 `timestamp` 是 **Unix 秒整数**，而消息类事件的 `timestamp` 是 **RFC3339 字符串**（如 `2026-07-21T10:00:00+08:00`）——解析时必须区分类型。

**互动事件类型枚举（外层 `type`）**：`11`=消息按钮回调（INLINE_KEYBOARD）、`12`=单聊快捷菜单回调（CALLBACK_COMMAND）、`13`=消息反馈（MESSAGE_FEEDBACK）、`14`=清空会话（CLEAR_SESSION）、`15`=进出故事集（IN_OUT_STORY）、`16`=切换模型（SWITCH_MODEL）、`18`=用户授权（USER_AUTHORIZE）、`19`=群授权（GROUP_AUTHORIZE）、`20`=群授权状态变更（GROUP_AUTHORIZE_STATUS）（`03-事件/互动事件/互动事件.md` 事件体表第 20 行）。

---

## 5. 如何回复

### 5.1 经网关 OpenAPI 透传调用

业务端**不需要自己获取 / 缓存 access_token**，也不要自带 `Authorization` 头——网关会把客户端的 `authorization` 头剥掉并注入 `Authorization: QQBot <token>`（`src/qqbot/forward.rs:92-99`），token 由网关按 app_id 自动获取与刷新（`POST {api_base}/app/getAppAccessToken`，成功以响应体含 `access_token` 判定，`src/qqbot/token.rs:176-187`；缓存到剩余 ≤300s 才刷新，30s 冷却期，`token.rs:11-13`、`151-174`）。

等价路径映射（平台路径前加 `/qqbot/{app_id}`，query string 保留，`src/qqbot/forward.rs:25-28`、`80-87`）：

| 平台接口 | 经网关的调用路径 |
| --- | --- |
| `POST /v2/groups/{group_openid}/messages` | `POST http://<gateway>/qqbot/<app_id>/v2/groups/<group_openid>/messages` |
| `POST /v2/users/{user_openid}/messages` | `POST http://<gateway>/qqbot/<app_id>/v2/users/<user_openid>/messages` |
| `PUT /interactions/{interaction_id}` | `PUT http://<gateway>/qqbot/<app_id>/interactions/<interaction_id>` |
| `DELETE /v2/groups/{group_openid}/messages/{message_id}` | `DELETE http://<gateway>/qqbot/<app_id>/v2/groups/<group_openid>/messages/{message_id}` |

`Content-Type` 等其它头原样透传；响应回业务端时网关附加 `x-qqbot-bot: <bot id>` 头（`src/qqbot/forward.rs:157`）。

网关侧错误语义：找不到 bot → 404、bot 禁用 → 503、取 token 失败 → 502、超时 → 504（`src/qqbot/forward.rs:41-53`、`57-73`、`119-120`；`docs/sdd/qqbot/interfaces.md` 转发流程节）。**平台业务错误是 HTTP 200 + 响应体 `err_code`，网关原样透传不改码**（`docs/reference/qq_bot_api/01-接入准备/API调用指南.md`；判断成败请用 `err_code`，**不要用 `message`**——官方注明「message 可能会随时调整」）。

### 5.2 发送群聊消息

`POST /qqbot/{app_id}/v2/groups/{group_openid}/messages`，频率限制 100 QPS（`02-API接口/群聊/发送群聊消息.md:19`）。

`msg_type` 枚举（逐字）：`0=纯文本(content) 2=Markdown(markdown) 7=富媒体(media) 8=图文卡片(card)`（同文件 :31）。群消息不支持流式（同文件 :4）。

被动回复字段（逐字）：

- `msg_id`：「被动回复的消息 ID。从 GROUP_AT_MESSAGE_CREATE 等事件的 d.id 获取，5 分钟内有效」（:35）。
- `event_id`：「被动回复的事件 ID。从事件最外层的 id 获取。与 msg_id 二选一，支持事件：`INTERACTION_CREATE`、`GROUP_ADD_ROBOT`、`GROUP_MSG_RECEIVE`」（:36）。
- `msg_seq`：「回复消息的序号，与 msg_id 联合使用，避免相同消息 id 回复重复发送，不填默认是 1。相同的 msg_id + msg_seq 重复发送会失败。」（:37）。

**被动回复约束（逐字）：「被动消息有效时间 5 分钟，每个消息最多回复 5 次」**（:6）。

主动消息频控：Bot 维度「企业认证/个人身份证认证 60/qpm；未认证 30/qpm」；单关系维度「20/qpm，每个群 1 天最多接收 1000 条」（:8-9）。

示例（同文件 :142-150，文本被动回复）：

```
POST /qqbot/1020012345/v2/groups/B2C3D4E5F6A1B2C3D4E5F6A1B2C3D4E5/messages
Content-Type: application/json

{
  "msg_type": 0,
  "content": "欢迎使用本群助手，有什么可以帮你的吗？",
  "msg_id": "ROBOT1.0_xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx",
  "msg_seq": 1
}
```

Markdown + 内嵌键盘示例（:156-190，节选键盘部分）：

```
{
  "msg_type": 2,
  "markdown": {
    "content": "## 每日签到\n\n今日签到成功！获得 **50** 积分\n连续签到 **7** 天"
  },
  "keyboard": {
    "content": {
      "rows": [
        {
          "buttons": [
            {
              "id": "btn_signin",
              "render_data": { "label": "签到", "style": 1 },
              "action": { "type": 2, "permission": { "type": 2 }, "data": "/签到", "enter": true }
            }
          ]
        }
      ]
    }
  },
  "msg_id": "ROBOT1.0_xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx",
  "msg_seq": 1
}
```

图文卡片（`msg_type=8`）的 `card.content` 约束：`title`(1-50 字符)、`description`(1-200 字符)、`url` 与 `pic_url`（必须 https，长度 ≤1024 字符）（:135-136）。富媒体（`msg_type=7`）的 `media.file_info` 来自 `/v2/groups/{group_openid}/files` 文件上传接口返回值（:38、:121）。**（分片上传 / 预上传的完整请求结构未在本次材料范围内，待核实。）**

发送成功响应（同文件 :255-263）：

```
{
  "id": "ROBOT1.0_a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0c1d2e3f4a5b6c7d8e9f0a1b2",
  "timestamp": "2026-07-21T10:00:00+08:00",
  "ext_info": { "ref_idx": "REFIDX_xxxxxxxxxxxxxxx==" }
}
```

### 5.3 发送单聊消息

`POST /qqbot/{app_id}/v2/users/{user_openid}/messages`，频率限制「100 QPS，包括主动、被动等所有消息类型」（`02-API接口/单聊/发送单聊消息.md:20`）。

`msg_type` 枚举（逐字）：`0=纯文本(content) 2=Markdown(markdown) 6=输入中状态(input_notify) 7=富媒体(media) 8=图文卡片(card)`（:32）——比群聊多出 `6`。

单聊独有字段：

- `is_wakeup`（boolean）：「指明发送消息为互动召回消息，与 msg_id，event_id 互斥使用」（:41）。互动召回规则：用户主动对话后 30 天内可下发，每周期一条，周期为「当天、1-3 天、3-7 天、7-30 天，合计 4 个周期」（:10）。
- `input_notify`（`msg_type=6`）：`input_type`「填 1」，`input_second`「状态持续时间，最长 60s」（:43、:145-146）。

被动回复字段与群聊完全同构（:36-38），但事件来源不同：`msg_id`「从 C2C_MESSAGE_CREATE 等事件的 d.id 获取」（:36）；`event_id` 支持 `INTERACTION_CREATE`、`C2C_MSG_RECEIVE`、`FRIEND_ADD`（:37）。

**⚠️ 冲突提示**：同文件 :6 写「被动消息有效时间 **60 分钟**，每个消息最多回复 **4 次**」，但 :36 的 `msg_id` 字段说明仍写「5 分钟内有效」。两者不一致，**以哪个为准无法从这两份文件确认，待核实**——建议业务端按平台实际行为验证；这是接入时最容易踩的坑。群聊侧（5 分钟 / 5 次）两处表述一致，无冲突。

主动消息频控：Bot 维度「企业认证/个人身份证认证 10/qps；未认证 5/qps 且 30/qpm」；单关系维度「20/qpm，每个好友 1 天最多接收 1000 条」（:8-9）。

示例（同文件 :152-160）：

```
POST /qqbot/1020012345/v2/users/A1B2C3D4E5F6A1B2C3D4E5F6A1B2C3D4/messages
{
  "content": "你好，欢迎使用机器人助手！",
  "msg_type": 0,
  "msg_id": "ROBOT1.0_xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx",
  "msg_seq": 1
}
```

输入中状态示例（:184-194）：

```
{
  "msg_type": 6,
  "input_notify": { "input_type": 1, "input_second": 60 },
  "msg_id": "ROBOT1.0_xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx",
  "msg_seq": 1
}
```

### 5.4 互动事件响应

收到 `INTERACTION_CREATE` 后**需**调此接口回应，否则客户端一直 loading 直到超时（`02-API接口/通用/互动事件响应.md:4`）。

`PUT /qqbot/{app_id}/interactions/{interaction_id}`，50 QPS（:16）。`interaction_id`「从 INTERACTION_CREATE 事件的 d.id 字段获取，注意该 ID 不带 `INTERACTION_CREATE:` 前缀」（:22）。

请求体唯一字段 `code`，枚举（逐字）：`0=成功, 1=操作失败, 2=操作频繁, 3=重复操作, 4=没有权限, 5=仅管理员操作`（:28）。

回应约束（逐字）：「仅 type=11（消息按钮）和 type=12（快捷菜单）的互动事件需要调用此接口回应，其他类型无需回应（调用也不会报错）。需在事件触发的有效时间内回应，超时后 interaction_id 失效。同一 interaction_id 只能回应一次。code=0 时，对于 type=14（清空会话），后台会下发会话已清空小灰条提示用户。」（:6）。**有效时间的具体数值该文件未给出，待核实。**

示例（:34-39）：

```
PUT /qqbot/1020012345/interactions/1b13d569-4610-4ab9-bc51-feecc5def6d4
{ "code": 0 }
```

成功响应体为 `{}`（:48-52）。

### 5.5 撤回

- 群聊：`DELETE /qqbot/{app_id}/v2/groups/{group_openid}/messages/{message_id}`，10 QPS；「发送超过 2 分钟的消息不可撤回」「成功返回 HTTP 200，无响应体」（`02-API接口/群聊/撤回群聊消息.md`、:4、:6）。群管理员可撤回自己和普通成员消息，普通成员只能撤回自己发的（:7-8）。
- 单聊：`DELETE /qqbot/{app_id}/v2/users/{user_openid}/messages/{message_id}`，10 QPS；同样 2 分钟限制、200 无响应体（`02-API接口/单聊/撤回单聊消息.md`、:4-5）。

### 5.6 常见错误码（被动回复 / 去重相关）

| 错误码 | 含义 | 来源 |
| --- | --- | --- |
| `304103` | 消息ID已过期，不能回复 | 发送群聊消息.md:285 / 发送单聊消息.md:285 |
| `40034005` | 回复消息msg_id已过期 | 发送群聊消息.md:290 / 发送单聊消息.md:288 |
| `40034024` | 请求参数msg_id无效或越权 | 发送群聊消息.md:286 / 发送单聊消息.md:294 |
| `40034025` | 请求参数event_id无效 | 同上两文件 |
| `40034026` | 请求参数event_id已过期 | 同上两文件 |
| `40034027` | 该事件不支持回复消息 | 发送单聊消息.md:297 |
| `40034128` | 被动回复时间或次数超限 | 发送群聊消息.md:299 / 发送单聊消息.md:308 |
| `40054005` | 消息被去重（排查建议原文：「请确保每次请求使用不同的 msgseq 值」） | 发送群聊消息.md:302 |
| `304024` | （重复撤回时出现，演示套件实测） | `observation/socket/qqbot/README.md` |
| `40034101` | 群未加入（机器人不在该群） | 演示套件实测 |
| `306009` / `40061002` / `40064004` | 用户openid无效 / 请求参数msgid无效 / 已超出消息撤回时限 | 撤回单聊消息.md |
| `50059` / `40034122` / `40034123` / `40054004` / `40054013` | 输入类型错误 / 召回消息已达区间上限 / 不支持召回消息 / 无好友关系 / 用户拒收消息（单聊特有） | 发送单聊消息.md |

失败响应结构示例（`01-接入准备/API调用指南.md`）：

```json
{
  "err_code": 40034005,
  "message": "回复消息msg_id已过期",
  "trace_id": "4a8a61565b909f199b1ec169fdd6f49e"
}
```

TraceID 两个来源：HTTP 响应头 `X-Tps-trace-ID` 与响应体 `trace_id`（同文件）。

事件 `message_type`（事件体里的字段，0/3/101/102/103，`03-事件/单聊事件/单聊消息事件.md` 事件体表）与发送 API 的 `msg_type`（0/2/6/7/8）是**两套枚举**，请勿混用。（这两套枚举是否包含 1/3/4/5 等其它取值，材料未列全，待核实。）

---

## 6. 观测与排障

### 6.1 网关侧日志

| 场景 | 级别 | 日志指纹 | 来源 |
| --- | --- | --- | --- |
| `event_url` 未配置 / 空 | WARN | `event dropped: bot has no event_url configured`（带 `bot=` / `app_id=`） | `src/qqbot/webhook.rs:104-112` |
| 业务端非 2xx | WARN | 带 `status = <HTTP 状态码>` | `webhook.rs:127-132` |
| 传输错误 | WARN | 带 `error = <错误>` | `webhook.rs:134-141` |
| 超时 | WARN | `event delivery to subscriber timed out after 5s` | `webhook.rs:143-151` |
| 投递成功 | **无日志** | —— | `webhook.rs:124` 空分支 |

### 6.2 `GET /__admin/qqbot/ws`（WS 通道状态）

挂在 `/__admin/qqbot` 下，鉴权同其它管理端点（`X-Admin-Key` 或 `Authorization: Bearer`，`src/qqbot/admin.rs:13-14`、`CLAUDE.md`）。输出 `{"bots": [...]}`，**没有 supervisor 的 bot 不出列**（未用 WS / 已停止 / 已禁用，`admin.rs:107-122`）。字段清单（`src/qqbot/ws.rs:41-50`）：

| 字段 | 含义 |
| --- | --- |
| `bot_id` / `app_id` | 机器人标识 |
| `connected` | 当前 WS 是否在线 |
| `session_id` | 平台会话 id（Resume 语义：跨重连保留，`ws.rs:39-40`） |
| `last_seq` | 最新收到的事件 `s`（同上跨重连保留） |
| `reconnects` | 重连次数（注意：PUT 变更 bot 会清零并冷启动，`observation/socket/qqbot/README.md` demo_03 条目） |
| `last_error` | 最近一次停止原因（如命中不重连的 close code） |

WS 断线行为（网关自动处理，业务端无需关心，列出仅供排障对照）：4008/4009 重连（保留 session）；4006/4007/4900..=4913 清 session 重连；4001/4002/4010..=4014/4914/4915 停止不重连；其余（含 1006）保持 session；指数退避 2s → 60s（`CLAUDE.md` qqbot 段；`docs/sdd/qqbot/interfaces.md` WS 生命周期节）。

### 6.3 `GET /__admin/qqbot/metrics`（投递统计）

输出 `{"bots": [{"bot": <完整 Bot>, "stats": <StatsSnapshot>}]}`，遍历 bot 表全部 bot（`src/qqbot/admin.rs:126-141`）。`StatsSnapshot` 字段（`src/metrics.rs:17-23`、`46-59`）：

| 字段 | 口径 |
| --- | --- |
| `total_requests` | **投递尝试次数**（含被丢弃的未配置事件——guard 在 spawn 时即持有，`webhook.rs:101-102`；`metrics.rs:62-69`） |
| `in_flight` | 进行中的投递数 |
| `errors` | 投递失败 + 未配置丢弃 + **OpenAPI 透传失败**（见下） |
| `avg_latency_ms` | `total_latency_ms / total_requests`（含等待 5s 超时的时间；`total_requests == 0` 时为 `0.0`） |

**归因陷阱**：bot stats 表被 **OpenAPI 透传与事件投递共用**——转发路径同样 `record_error()` / `stats.guard()`（`src/qqbot/forward.rs:64`、`76-77`、`123`、`130`、`145`）。`errors` 与 `total_requests` **不区分来源**，排障时不能把 `errors` 全部归因于事件投递。被动的 4xx 透传不计错误（`interfaces.md` 转发流程第 11 步：上游 5xx 计错误，4xx 不计）。

### 6.4 业务端常见问题对照表

| 现象 | 可能原因 | 排查 |
| --- | --- | --- |
| 完全收不到事件 | `event_url` 未配置 / 空串 | 网关日志搜 `event dropped`；metrics `errors` 增长 |
| 完全收不到事件 | bot 未注册 / 已禁用 | webhook 通道：平台侧重配回调；网关侧回调返回 404（禁用 bot 回调 404 而非 503，ADR-Q8，`webhook.rs:40-53`）；WS 通道：bot 禁用不 spawn supervisor（`bot.rs:94-96`、`256-258`），`/__admin/qqbot/ws` 无该行 |
| 完全收不到事件 | `intents` 未订阅对应事件位 | 检查 bot 配置 `intents`（§4.1 位定义表）；订阅无权限的特殊事件会被平台关闭连接（事件订阅通用数据结构.md「权限」节） |
| 偶发丢失 | 业务端响应慢于 5s / 回非 2xx / 传输错误 | 网关日志按指纹定位（§6.1）；**失败不重试，即丢失**，需业务端补推 |
| 重复收到同一事件 | 平台重推（相同 msg_id 可能重复推送）或 webhook 与 WS 双通道同时活跃 | 以事件外层 `id` 做幂等键去重（官方提示结合 `msg_seq`，`群@机器人消息.md` 第 4 行） |
| WS 重连频繁 | 平台侧重连 / 鉴权失败 | 看 `/__admin/qqbot/ws` 的 `reconnects` 与 `last_error`；`last_seq` 是否在前进 |
| 回复失败 `40054005` | 相同 `msg_id + msg_seq` 重复发送 | 每次被动回复递增 `msg_seq`（不填默认 1） |
| 回复失败 `304103` / `40034005` | msg_id 已过期 | 群聊被动回复窗口 5 分钟（单聊存在 60 分钟与 5 分钟两处表述冲突，见 §5.3） |
| 平台 401 连不上回调地址 | —— | webhook 形态下平台侧地址需公网可达，且端口限 80/443/8080/8443（`01-接入准备/Webhook回调方式.md` 第 6 行） |

---

## 7. 本地联调

### 7.1 QQ Bot Mock 的角色

仓库配套的 **QQ Bot Mock**（`docs/reference/qq-bot-mock-api.md`，默认 `http://127.0.0.1:8080`，可用环境变量 `QQBOTMOCK_BASE` 覆盖）**模拟平台一侧**：它实现平台的回调推送、WS 会话协议与 OpenAPI。它**不扮演业务后端**——所以 **`event_url` 绝不能指向 mock**（mock 没有 `/events` 这类接收端点；`/events` 是业务后端该提供的端点）。

演示套件（`observation/socket/qqbot/`）把网关以控制台模式 `run` 拉在 `127.0.0.1:18198`、admin key 为 `demo-admin-key`，配置文件放临时目录、进程退出即清（`observation/socket/qqbot/README.md`「运行」节；`_common.py` 的 `Gateway` 类）。

### 7.2 EventSink 模式：自己起一个收事件的服务

参考实现见 `observation/socket/qqbot/_common.py` 的 `EventSink` 类：一个收 POST、记事件、回 200 的 HTTP 服务，三种模式——`ok`（恒 200）、`fail`（恒 500，演示投递失败不重试）、`slow`（延迟 N 秒再 200，演示先 ACK 后投递）。最小可复制版本：

```python
import json, threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

events = []

class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def do_POST(self):
        n = int(self.headers.get("Content-Length") or 0)
        raw = self.rfile.read(n)
        events.append({
            "path": self.path,
            "auth": self.headers.get("Authorization"),
            "ctype": self.headers.get("Content-Type"),
            "body": raw.decode("utf-8", "replace"),
        })
        body = b'{"ok":true}'
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *a): pass

srv = ThreadingHTTPServer(("127.0.0.1", 9000), Handler)
threading.Thread(target=srv.serve_forever, daemon=True).start()
# event_url 配为 http://127.0.0.1:9000/qq/events
```

注意：业务端在 keep-alive 连接上断开可能产生 `ConnectionResetError` 等噪声（reqwest 连接池回收），生产实现应容忍（`_common.py` 的 `_QuietHTTPServer`）。

### 7.3 驱动 Mock 的端点清单

注册 bot 后（`POST /__admin/qqbot/bots`，`api_base` 指向 mock），用下列端点**扮演平台侧触发事件**（`docs/reference/qq-bot-mock-api.md` §9、§10）：

| 触发的事件 `t` | 调用 |
| --- | --- |
| `GROUP_AT_MESSAGE_CREATE` | `POST /_mock/dispatch/group_message`（`group_openid`、`username`、`content`、`at_bot` 默认 true） |
| `GROUP_MSG_RECEIVE` | 同上，`receive=true`（全量接收模式） |
| `GROUP_ADD_ROBOT` / `GROUP_DEL_ROBOT` | `POST /_mock/groups/{id}/bot`（`{"joined": true/false}`） |
| `C2C_MESSAGE_CREATE` | `POST /_mock/dispatch/c2c_message`（`username`、`content`、`attachments[]`） |
| `FRIEND_ADD` | `POST /_mock/users/{id}/friend` 或 `POST /_mock/dispatch/friend_add`（`scene` 默认 1001） |
| `FRIEND_DEL` | `DELETE /_mock/users/{id}/friend` |
| `INTERACTION_CREATE` | `POST /_mock/dispatch/interaction`（`type` 默认 11、`scene`、`username`、`button_data`、`group_openid`） |
| 任意事件 | `POST /_mock/dispatch/raw`（`event_type` 必填、`data`） |

配套观察端点：

| 端点 | 用途 |
| --- | --- |
| `POST /_mock/webhook` | 把平台的 webhook 推送重定向到网关的 `/qqbot/callback/{app_id}` |
| `POST /_mock/webhook/validate` | 触发 op 13 回调地址验证（网关回签，可与 mock 的 `expected_signature` 比对） |
| `GET /_mock/events` | 平台视角：每条事件的 `webhook_ok` / `ws_sessions` |
| `GET /_mock/history?limit=100` | 明文调用流水；用**增量**验证 token 缓存（两次相同调用之间 `/app/getAppAccessToken` +1 然后 +0） |
| `GET /_mock/state` | 世界总览（appId、bot、webhook 注册） |
| `POST /_mock/inject` | 一次性错误码注入，如 `{"error_code": 304061}` |
| `GET /_mock/messages` | 机器人发出的消息、撤回状态 |

被动回复的联调要点：mock 用 `POST /_mock/dispatch/group_message` 返回的 `msg_id` 走被动回复；同 `msg_id` 再发 → `40054005`；撤回幂等，已撤回再撤 → `304024`（`observation/socket/qqbot/README.md` demo_01 条目）。`event_id` 被动回复仅支持 `INTERACTION_CREATE`、`GROUP_ADD_ROBOT`、`GROUP_MSG_RECEIVE`、`C2C_MSG_RECEIVE`、`FRIEND_ADD`，其余事件用 `event_id` 回复 → `40034027`（mock 文档 §9.4；与官方发送消息文档 :36/:37 的支持集合一致）。

### 7.4 已知差异与清理约定

- mock 文档 §9 的推送信封示例带 `s`，但**实现上 webhook 推送不带 `s`、只有 WS 帧带**（`README.md` 第 78-82 行）；据此，真实平台 webhook 推送是否携带 `s` 亦未从官方文档逐字确认（§3.2）。区分两份副本可利用此特征。
- `/_mock/info` 文档有列但实际访问 404（`README.md`「`/_mock/info` 文档有而实现无」条目）——可用信息从 `/_mock/state` 取。
- 本机 loopback 连接拒绝约 2 秒：平台向不可达 webhook 的推送是同步等待的，这段延迟在平台侧、不经过网关（同上「死 webhook 会推迟 WS 到达」条目）。
- **隔离与清理**（`README.md`「隔离与清理」节）：共享 mock 世界**不重置**——绝不 `POST /_mock/reset`（会清掉其他使用者的状态）；进入 / 退出演示时保存并还原平台 webhook 注册（含空 url）；演示注册的 bot 在结束时删除（WS supervisor 联动停止）；`/_mock/history` 与 `/_mock/events` 是共享流水，不做清理。

---

## 8. 安全与注意事项

### 8.1 `event_secret` 是业务端验来源的唯一手段

- `/qqbot/*` 通道**没有客户端认证**：网关的回调地址 `/qqbot/callback/{app_id}` 任何人都能 POST（网关只校验 `X-Bot-Appid` 头若存在则需与路径 app_id 一致——缺失则不校验，`src/qqbot/webhook.rs:29-38`；且网关**不验证入站事件签名**——op 13 只签不验，签名公钥不在参考文档内，ADR-Q7/R4）。
- 平台的签名头不会透传到 `event_url`（§2.2）。因此：**业务端收到的 POST 是否真来自网关，只能靠 `Authorization: Bearer <event_secret>` 判断**。校验失败应拒绝（回非 2xx 或 401 均可，语义上网关只计错误不重试）。
- 空串 / 纯空白等同未设（`bot.rs:218-227`、`309-311`）；**无最小长度等格式规则**，强度由你自己定（此处未从源码/文档确认平台侧是否有建议值，待核实）。建议生成高熵随机串并以恒定时间比较。
- 没配 `event_secret` 时，同网段任何能访问你 `event_url` 的人都能伪造事件——内网部署也不能省。

### 8.2 `event_url` 网络位置建议

- 优先**内网 / 可信地址**：投递是网关主动出站 POST，`event_url` 无需公网可达。
- **websocket 形态下整个部署可以完全没有公网入站**——网关主动出站连平台 wss，事件仍走同一个 `event_url` 投递（这是「适合没有公网入站的部署」的含义）。切换 `event_source` 不需要改业务代码。
- 若 `event_url` 必须暴露公网：限定来源 IP（网关出口）、启用 HTTPS、设置 `event_secret`，并避免把请求体落入公开日志。

### 8.3 secrets 明文落盘与暴露面

- `bots.toml` 中 `app_secret`、`event_secret` **明文存储**（`AppState::persist_bots` 序列化整个 bot 表，CLAUDE.md 持久化段）。文件属主权限应仅限运行网关的账号。
- 管理端 `GET /__admin/qqbot/bots`、`GET /__admin/qqbot/bots/{id}` 与 `GET /__admin/qqbot/metrics` 返回**完整 `Bot` 序列化，包含 `app_secret` 与 `event_secret` 明文**（`src/qqbot/admin.rs:31`、`44`、`135-137`；`Bot` 无跳过字段）。因此 `GATEWAY_ADMIN_KEY` 必须保密：未设置时管理端点**完全开放**并有启动告警（CLAUDE.md admin 段；比较使用恒定时间）。
- `event_url` / `event_secret` 支持 `PUT` 合并语义：字段缺省保留、显式 `null` 清除（`bot.rs:140-143`、`164-167`），变更后立即生效并持久化。

---

## 9. 接入检查清单（业务端上线前逐项 check）

**接收端点**
- [ ] 提供一个 HTTP POST 端点作为 `event_url`，路径自定义；只接受 POST（契约只保证 POST）。
- [ ] 对 `Content-Type` 两种取值都兼容：webhook 通道随平台回调头（通常 `application/json`，缺失时网关回退 `application/json`），WS 通道恒为 `application/json`。
- [ ] 按信封 `{"id", "op", "d", "s?", "t?"}` 解析；把 `s` / `t` / `id` 当作可能缺失处理；按 `op == 0` && `t` 分发；**未知 `t` 回 2xx 并忽略**。
- [ ] **5 秒内回 2xx**；耗时处理在回应之后异步进行（超时即丢失，网关不重试）。
- [ ] 网关不读响应体——不要用响应体向网关传任何信息。
- [ ] **幂等**：以事件外层 `id` 去重（消息类事件外层 `id` == `d.id`）；容忍双通道 / 平台重推导致的重复。
- [ ] 不能丢的事件：回 2xx 前先落持久队列。
- [ ] 区分消息类事件 `timestamp`（RFC3339 字符串）与好友 / 推送开关类事件 `timestamp`（Unix 秒整数）。
- [ ] 群消息 `content` 记得 `trim`（有 @ 前缀占位空格）。

**鉴权**
- [ ] 配置了高熵随机 `event_secret`，业务端校验 `Authorization: Bearer <event_secret>` 且恒定时间比较；未带或不符即拒绝。
- [ ] 若业务端伪造代价敏感，确认 `event_url` 不在公网裸露；暴露时限制来源 IP + HTTPS。

**回复**
- [ ] 回复走 `/qqbot/{app_id}/...` 透传，**不带** `Authorization` 头（网关注入 `QQBot <token>`）。
- [ ] 被动回复把事件 `d.id` 填入 `msg_id`，每次回复递增 `msg_seq`（不填默认 1，重复 `msg_id+msg_seq` 会 `40054005`）。
- [ ] 群聊被动回复在 5 分钟内、至多 5 次；单聊注意存在「60 分钟 / 4 次」与「msg_id 5 分钟」两处冲突表述（§5.3，待核实），按平台实际行为验证。
- [ ] `INTERACTION_CREATE`：仅 `type=11/12` 需 `PUT /interactions/{interaction_id}` 回应（`code` 枚举 0-5），同一 id 只能回应一次；`interaction_id` 取 `d.id` 且**不带前缀**。
- [ ] 用 `err_code`（而非 `message`、而非 HTTP 状态码——平台业务错误是 HTTP 200 + `err_code`）判断成败；记录 `trace_id` / `X-Tps-trace-ID` 供排障。
- [ ] 撤回在 2 分钟窗口内（群 / 单聊均如此）。

**订阅与观测**
- [ ] bot 的 `intents` 订阅了所需事件位（群 / 单聊 / 好友事件 = `1<<25`，互动 = `1<<26`，按位或）。
- [ ] `event_url` 配置为 `http(s)://<host>/...`（scheme 与 host 是硬校验项），`PUT` 变更时知道缺省保留 / `null` 清除语义。
- [ ] 上线前用 Mock + EventSink 跑通：事件接收、去重、投递失败（令业务端恒 500 验证不重试）、op 13 回签、被动回复与 `40054005`。
- [ ] 监控 `GET /__admin/qqbot/metrics` 的 `errors` 与 `avg_latency_ms`——注意 `errors` **混入了 OpenAPI 透传错误**，上涨时需结合日志指纹归因；投递成功无日志，不要指望用它对账。
- [ ] WS 通道部署：`GET /__admin/qqbot/ws` 看 `connected` / `session_id` / `last_seq` 是否前进、`reconnects` / `last_error` 是否异常。

**安全**
- [ ] `GATEWAY_ADMIN_KEY` 已设置并保密（未设置 = 管理端开放 + `bots` / `metrics` 明文返回 `app_secret` / `event_secret`）。
- [ ] `bots.toml` 文件权限仅限运行账号（secrets 明文落盘）。
- [ ] 知晓 `/qqbot/callback/{app_id}` 无客户端认证、网关不验入站签名——来源可信性完全由业务端的 `event_secret` 校验保证。
