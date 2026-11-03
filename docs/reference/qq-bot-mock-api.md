# QQ Bot Mock 对接参考（API 规格）

本文是 **QQ 机器人开放平台本地 Mock 服务** 的完整 API 规格，面向要接入 QQ Bot API 的程序：
所有路径、字段、错误码都与本 Mock 的实现逐条对齐，可直接作为**接入测试的目标规格**。

- 真实平台文档见 `docs/qq-bot/`（`https://bot.q.qq.com/wikidev` 同步整理版）。
- 本 Mock 覆盖平台能力的一个子集（群/单聊收发、富媒体、互动、菜单面板、网关、Webhook）；
  **未实现**的接口见文末「未实现清单」。
- 启动：`uv run python -m qqbotmock --port 8187`，下列示例沿用 `http://127.0.0.1:8187`。

---

## 0. 通用约定

| 项目 | 约定 |
| --- | --- |
| Base URL | `http://127.0.0.1:<port>`（真实平台为 `https://api.bot.qq.com`） |
| 请求格式 | `Content-Type: application/json`，除上传外全部 JSON |
| 鉴权头 | `Authorization: QQBot <access_token>`（注意 `QQBot ` 前缀必填） |
| 成功响应 | HTTP **200** + 业务 JSON 体 |
| 业务错误 | HTTP **200** + `{"err_code": <int>, "message": "...", "trace_id": "<32位hex>"}`，响应头同带 `X-Tps-trace-ID` |
| 鉴权失败 | HTTP **401** + 上面的错误体（`err_code=401`）——这是唯一的 HTTP 非 200 情形 |
| 例外 | `POST /app/getAppAccessToken` 的错误放在响应体 `code` 字段（`{"code":100016,...}`），HTTP 仍为 200 |
| 时间戳 | 全部东八区 RFC3339 秒精度，如 `2026-11-02T15:30:00+08:00` |
| 消息 ID | `ROBOT1.0_` + 64 位 hex；消息引用索引 `REFIDX_` 开头 |
| 用户/群 ID | 32 位 hex OpenID |

**事件通道二选一**（可同时使用）：WebSocket 网关（主动长连接）或 Webhook 回调（平台 POST 推送）。

### 默认凭据

| appId | clientSecret |
| --- | --- |
| `1020012345` | `mock_client_secret` |

启动时可用 `--app-id` / `--client-secret` / `--bot-username` 覆盖。

---

## 1. 鉴权

### 1.1 获取访问凭证

```
POST /app/getAppAccessToken
{"appId": "1020012345", "clientSecret": "mock_client_secret"}
```

成功（200）：

```json
{"access_token": "mocktoken_<32位hex>", "expires_in": 7200}
```

失败（仍然是 200，错误在 `code`）：

```json
{"code": 100016, "message": "invalid appid or secret"}
```

- `appId` 或 `clientSecret` 缺失 / 不匹配都会返回 100016。
- Mock 的 token 不会真正过期；`expires_in` 固定 7200。

### 1.2 后续请求

除 `/app/getAppAccessToken` 外，所有 OpenAPI 接口都需要：

```
Authorization: QQBot mocktoken_xxxx
```

token 不正确 → HTTP 401 `{"err_code":401,"message":"access token 校验不通过",...}`。

### 1.3 获取机器人详情

```
GET /users/@me
```

```json
{"id": "5777414462219517083", "username": "MockBot", "avatar": "https://thirdqq.qlogo.cn/...",
 "bot": true, "union_openid": "", "union_user_account": "",
 "share_url": "https://qun.qq.com/qunpro/robot/qunshare?robot_appid=1020012345&biz_type=0",
 "welcome_msg": "欢迎使用 QQ Bot Mock"}
```

---

## 2. 群聊 API

### 2.1 接口清单

| 接口 | Method | 路径 |
| --- | --- | --- |
| 获取群基本信息 | GET | `/v2/groups/{group_openid}/info` |
| 获取群成员列表 | GET | `/v2/groups/{group_openid}/members?cursor=` |
| 获取群成员详情 | GET | `/v2/groups/{group_openid}/members/{member_openid}` |
| 获取机器人群内状态 | GET | `/v2/groups/{group_openid}/bot_state` |
| 查询群禁言状态 | GET | `/v2/groups/{group_openid}/restrict_chat_setting` |
| 设置群成员禁言 | POST | `/v2/groups/{group_openid}/restrict_chat_setting` |
| 群黑名单操作 | POST | `/v2/groups/{group_openid}/member_blacklist` |
| 群黑名单查询 | GET | `/v2/groups/{group_openid}/member_blacklist` |
| 群成员批量移除 | POST | `/v2/groups/{group_openid}/batch_remove_members` |
| 发送群聊消息 | POST | `/v2/groups/{group_openid}/messages` |
| 撤回群聊消息 | DELETE | `/v2/groups/{group_openid}/messages/{message_id}` |
| 群聊富媒体预上传 | POST | `/v2/groups/{group_openid}/upload_prepare` |
| 群聊分片上传完成 | POST | `/v2/groups/{group_openid}/upload_part_finish` |
| 群聊富媒体上传 | POST | `/v2/groups/{group_openid}/files` |

**前置条件**：机器人必须在群内，否则所有群接口返回 `40034101`（机器人非群成员）。

### 2.2 群信息与成员

```
GET /v2/groups/3E5D8A1F7B2C9E4D6A0F1B3C5D7E9F2A/info
```

```json
{"group_openid": "3E5D8A1F7B2C9E4D6A0F1B3C5D7E9F2A",
 "group_name": "读书分享会", "group_finger_memo": "每周共读一本好书",
 "group_class_text": "文化", "group_tags": ["阅读","文学","成长"], "group_member_num": 3}
```

成员列表分页（每页 30 条，`next_cursor` 为空表示到底）：

```
GET /v2/groups/{group_openid}/members?cursor=<base64>
```

```json
{"members": [
   {"member_openid": "A1B2...C3D4", "username": "小明", "member_role": "member",
    "bot": false, "joined_at": "2026-11-02T10:00:00+08:00", "union_openid": "9F2E...DF22"}],
 "next_cursor": ""}
```

成员详情同上单个对象；成员不在群内 → `40034101`。

机器人群内状态：

```json
{"member_openid": "7A3B9C1D5E2F4A6B8C0D1E3F5A7B9C2D",
 "joined_at": "2026-11-02T10:00:00+08:00",
 "allow_proactive_msg": true, "recv_msg_setting": "all", "member_role": "member"}
```

### 2.3 禁言与黑名单

**查询群禁言状态**（只返回当前仍被禁言的成员）：

```json
{"global_rule": {"mode": "none", "schedule_rules": [], "recurring_rules": []},
 "members": [{"member_openid": "...", "mute_expire_at": "2026-11-02T16:00:00+08:00",
              "username": "小明", "union_openid": "..."}]}
```

**设置群成员禁言**：

```json
POST /v2/groups/{group_openid}/restrict_chat_setting
{"members": [
   {"op": "add", "member_openid": "A1B2...C3D4", "mute_expire_at": "2026-11-02T16:00:00+08:00"},
   {"op": "update", "member_openid": "C3D4...E5F6", "mute_expire_at": "..."},
   {"op": "del", "member_openid": "D4E5...F6A1"}]}
```

- `op` 取值 `add` / `update` / `del`，非法 → `10000`。
- `mute_expire_at` 缺失或解析失败按 **30 分钟后**处理。
- 超 20 个成员 → `10000`；成员不在群内 → `40034101`。成功返回 `{}`。

**群黑名单**：

```json
POST /v2/groups/{group_openid}/member_blacklist
{"op": "add", "member_openids": ["..."]}
```

- `op` = `add` / `del`，非法 → `10000`。
- **加黑时成员仍需不在群中**：`add` 一个在群成员 → `10000`（成员仍在群中，无法加黑）。
- 查询：`GET .../member_blacklist` → `{"members":[{"member_openid","username"}]}`。

**群成员批量移除**：

```json
POST /v2/groups/{group_openid}/batch_remove_members
{"member_openids": ["A...", "B..."], "add_to_member_blacklist": true}
```

```json
{"remove_members_result": "success",
 "add_to_member_blacklist_fail_openids": ["<移除失败的 openid>"]}
```

- 单次最多 20 个，超过 → `10000`。
- 不在群内的 openid 会进入 fail 列表（不报错）；配合 `add_to_member_blacklist` 可一并拉黑。

### 2.4 发送群聊消息

```json
POST /v2/groups/{group_openid}/messages
{"msg_type": 0, "content": "你好", "msg_id": "ROBOT1.0_..."}
```

成功：

```json
{"id": "ROBOT1.0_<64hex>", "timestamp": "2026-11-02T15:00:00+08:00",
 "ext_info": {"ref_idx": "REFIDX_xxxxx=="}}
```

**msg_type 取值**（群聊）：`0` 文本 / `2` markdown / `7` 富媒体 / `8` card。
单聊额外支持 `6`（系统消息）。非法 msg_type → `340069`。

**参数校验顺序与错误码**（这是最值得写测试的部分）：

| 场景 | err_code | message 关键词 |
| --- | --- | --- |
| 一次性错误注入开启 | (注入值) | `注入的错误码` |
| 机器人不在群 / 群不存在 | 40034101 | 机器人非群成员 |
| 用户不是好友（单聊）/ 用户不存在 | 40054004 | 无好友关系 |
| msg_type 非法 | 340069 | 消息类型无效 |
| 同时传 markdown 和 content | 22006 | 消息类型与内容不匹配 |
| msg_type=2 但 markdown.content 空 | 40034008 | markdown参数有空值 |
| msg_type=0 但 content 空 | 304061 | 消息内容无效 |
| msg_type=8 但 card 空 | 22006 | 必须填写 card |
| msg_type=7 但 file_info 无效/过期 | 304080 | 文件信息无效 |
| msg_id 与 event_id 同时传 | 22006 | 不能同时传入 |
| msg_id 无效 / 回复机器人自己的消息 | 40034024 | msg_id无效或越权 |
| msg_id 超出被动消息有效期 | 40034005 | msg_id已过期 |
| event_id 无效 | 40034025 | event_id无效 |
| event_id 对应事件不支持回复 | 40034027 | 该事件不支持回复消息 |
| event_id 已过期 | 40034026 | event_id已过期 |
| msg_id+msg_seq 重复发送 | 40054005 | 消息被去重 |
| 主动消息但群未开通权限 | 40034105 | 主动消息发送失败，无权限 |

**被动消息有效期**：群聊 **300 秒**，单聊 **3600 秒**。
**去重键**：`msg_id + msg_seq`（默认 msg_seq=1），同一组合只能用一次。

### 2.5 撤回消息

```
DELETE /v2/groups/{group_openid}/messages/{message_id}
```

成功返回 `{}`；消息不存在或已撤回 → `304024`。

---

## 3. 单聊 API

| 接口 | Method | 路径 |
| --- | --- | --- |
| 发送单聊消息 | POST | `/v2/users/{user_openid}/messages` |
| 撤回单聊消息 | DELETE | `/v2/users/{user_openid}/messages/{message_id}` |
| 流式发送单聊消息 | POST | `/v2/users/{user_openid}/stream_messages` |
| 单聊富媒体预上传 | POST | `/v2/users/{user_openid}/upload_prepare` |
| 单聊分片上传完成 | POST | `/v2/users/{user_openid}/upload_part_finish` |
| 单聊富媒体上传 | POST | `/v2/users/{user_openid}/files` |

前置条件：对方需是机器人好友（`40054004`）。消息体、错误码、有效期、去重与群聊一致（被动窗口为 3600s）。

### 3.1 流式发送单聊消息

```json
POST /v2/users/{user_openid}/stream_messages
{"stream_msg_id": null, "content_raw": "正在思考",
 "input_mode": "append", "input_state": 1, "is_wakeup": false}
```

```json
{"id": "<stream_msg_id>", "timestamp": "...", "ext_info": {"ref_idx": "REFIDX_..."},
 "remain_msg_len": 4}
```

- 首片不带 `stream_msg_id`，平台生成并返回；后续分片回传该 id。
- `input_mode`：`append`（追加）或 `replace`（前缀不可修改：`content_raw` 必须以已下发内容开头，否则 `40007`）。
- `input_state=10` 表示生成结束，整条流式消息落库。
- `is_wakeup=true` 时跳过被动来源校验（主动流式消息）。
- 非法的 `stream_msg_id` → `50001`。
- 群聊没有流式接口（真实平台同样仅单聊支持，字段口径向 `docs/qq-bot/02-API接口/单聊/流式发送单聊消息.md` 对齐）。

---

## 4. 富媒体上传

三种方式：`url` 直传、`upload_id`（分片合并）、`file_data`（base64 二进制）。

### 4.1 预上传（分片）

```json
POST /v2/{groups|users}/{openid}/upload_prepare
{"file_type": 1, "file_size": 31457280, "file_name": "demo.jpg"}
```

- `file_type`：`1` 图片 / `2` 视频 / `3` 语音 / `4` 文件；非法 → `850019`。
- `file_size` ≤ 0 或非法 → `10000`；> 200MB → `850031`。
- 分块固定 **10MB**（10485760）。

```json
{"upload_id": "upload_<16hex>",
 "block_size": "10485760",
 "parts": [{"index": 0, "presigned_url": "http://<host>/_mock/upload/<upload_id>/0",
            "block_size": "10485760"}],
 "upload_config": {"concurrency": 1, "retry_timeout": 300, "retry_delay": 1}}
```

### 4.2 分片上传完成

```json
POST /v2/{groups|users}/{openid}/upload_part_finish
{"upload_id": "upload_...", "part_index": 0}
```

成功 `{}`；`upload_id` 无效 → `40093001`；`part_index` 越界 → `10000`。

> Mock 附加便利：直接 `PUT presigned_url`（即 `/_mock/upload/{upload_id}/{part_index}`）也会标记该分片完成。

### 4.3 合并 / 直传

```json
POST /v2/{groups|users}/{openid}/files
{"file_type": 1, "upload_id": "upload_...", "srv_send_msg": true}
```

或 `{"file_type":1, "url":"https://example.com/a.jpg"}`，
或 `{"file_type":4, "file_data":"<base64>", "file_name":"a.txt"}`（≤ 5MB，否则 `850031`）。

成功：

```json
{"file_uuid": "<32hex>", "file_info": "<32hex大写>", "ttl": 300,
 "raw_url": "http://<host>/_mock/media/<upload_id>"}
```

- `url` 方式要求以 `http` 开头，否则 `850026`。
- `file_data` 必须给 `file_name`（默认 `upload.bin`）。
- 媒体类型（1/2/3）且分片合并时返回 `raw_url`。
- `srv_send_msg=true` 时直接发送一条 msg_type=7 消息，响应附带 `id`。
- `file_info` 有效期 **300 秒**，用于发消息（msg_type=7）时携带，过期/无效 → `304080`。
- 三种方式都没给 → `10000`。

---

## 5. 通用 API

### 5.1 互动事件响应

```
PUT /interactions/{interaction_id}
{"code": 0}
```

`interaction_id` 即互动事件的 `id`（事件包外层 `id`）。成功 `{}`；无效 → `40034025`。

### 5.2 生成分享链接

```json
POST /v2/generate_url_link
{"expire_in": 60}
```

```json
{"url": "https://qun.qq.com/qunpro/robot/qunshare?robot_appid=1020012345&biz_type=0",
 "url_link_id": "<32hex>", "expire_in": 60}
```

`expire_in` 缺省 30。

---

## 6. 菜单与面板

| 接口 | Method | 路径 |
| --- | --- | --- |
| 查询全局自定义菜单 | GET | `/v2/menu` |
| 修改全局自定义菜单 | POST | `/v2/menu` |
| 查询指令面板列表 | GET | `/v2/panels` |
| 创建指令面板 | POST | `/v2/panels` |
| 查询指令面板详情 | GET | `/v2/panels/{panel_id}` |
| 修改指令面板 | POST | `/v2/panels/{panel_id}` |
| 修改指令面板关联对象 | POST | `/v2/panels/{panel_id}/target` |
| 删除指令面板 | DELETE | `/v2/panels/{panel_id}` |

```json
GET /v2/menu            →  {"menu": {...} | null, "version": 1}
POST /v2/menu {"menu": {...}}  →  {"menu": {...}, "version": 2}   // version 每次写 +1
POST /v2/panels {"scope": "c2c", "panel": {...}}  →  {"panel_id": "p_1020012345_xxxx"}
GET /v2/panels          →  {"records": [...], "next_cursor": "", "is_end": true}
GET /v2/panels/{id}     →  {"panel_id","scope","target_type","panel","version"}
POST /v2/panels/{id} {"panel": {...}}  →  {}      // panel 变更时 version +1
POST /v2/panels/{id}/target {"target_type": "all"}  →  {}
DELETE /v2/panels/{id}  →  {}
```

面板不存在一律 `10000`（message 含「面板不存在」）。

---

## 7. WebSocket 网关

### 7.1 获取接入点

```
GET /gateway/bot
{"url": "ws://127.0.0.1:8187/websocket", "shards": 1,
 "session_start_limit": {"total": 1000, "remaining": 1000, "reset_after": 86400000,
                         "max_concurrency": 1}}
```

HTTP 服务返回 `ws://`（本机），真实平台为 `wss://api.bot.qq.com/websocket`。

### 7.2 连接流程

```
connect ws://127.0.0.1:8187/websocket

<-- {"op": 10, "d": {"heartbeat_interval": 45000}}
--> {"op": 2, "d": {"token": "QQBot mocktoken_xxxx",
                    "intents": 100663296, "shard": [0, 1]}}
<-- {"id": "", "op": 0, "s": 1, "t": "READY",
     "d": {"version": 1, "session_id": "<uuid>",
           "user": {"id": "5777414462219517083", "username": "MockBot", "bot": true},
           "shard": [0, 1]}}
--> {"op": 1, "d": null}        # 每 45s 一次心跳
<-- {"op": 11}                  # 心跳 ACK
<-- {"id": "ROBOT1.0_...", "op": 0, "s": 2, "t": "GROUP_AT_MESSAGE_CREATE", "d": {...}}
```

**Identify 要点**：

- `token` **必须带 `QQBot ` 前缀**（即 `"QQBot " + access_token`），否则 `4004`。
- `intents` 必须是允许位掩码的子集，否则 `4013`。
- `shard` 为 `[shard_id, shard_count]` 两整数，且 `0 ≤ shard_id < shard_count`，否则 `4010`。

### 7.3 OpCode 速查

| op | 方向 | 含义 |
| --- | --- | --- |
| 0 | ← 平台 | 事件推送（`t` 为事件类型，`s` 为序列号） |
| 1 | → 平台 | 心跳（`d` 可带上报的 s） |
| 2 | → 平台 | Identify |
| 6 | → 平台 | Resume |
| 10 | ← 平台 | Hello（含 heartbeat_interval） |
| 11 | ← 平台 | 心跳 ACK |
| 13 | ↔ | Webhook 回调地址验证（见 §8） |

### 7.4 Intents

| Intent | 值 | 说明 |
| --- | --- | --- |
| `GUILDS` | `1<<0` | 频道增删改 |
| `GUILD_MEMBERS` | `1<<1` | 频道成员 |
| `GUILD_MESSAGES` | `1<<9` | 频道消息（私域） |
| `GUILD_MESSAGE_REACTIONS` | `1<<10` | 频道表情表态 |
| `DIRECT_MESSAGE` | `1<<12` | 频道私信 |
| **`GROUP_AND_C2C_EVENT`** | **`1<<25`** | **群与单聊事件（群/单聊必备）** |
| **`INTERACTION`** | **`1<<26`** | **互动事件** |
| `MESSAGE_AUDIT` | `1<<27` | 消息审核 |
| `FORUMS_EVENT` | `1<<28` | 论坛事件 |
| `AUDIO_ACTION` | `1<<29` | 音频控制 |
| `PUBLIC_GUILD_MESSAGES` | `1<<30` | 频道消息（公域） |

群+单聊+互动场景常用 `(1<<25) | (1<<26)` = `100663296`。
如果识别不到怎么办 A: 事件未下发（非错误）——请检查 intents 位。

### 7.5 断线补发（Resume）

鉴权成功的会话断线后保留 **5 分钟**，此期间事件会缓冲不下发。重连后：

```
--> {"op": 6, "d": {"token": "QQBot mocktoken_xxxx",
                    "session_id": "<原 session_id>", "seq": 3}}
<-- （重放所有 s > seq 的事件，s 序号延续）
<-- {"id": "", "op": 0, "s": 8, "t": "RESUMED", "d": ""}
```

约束：`token` 仍然必填（`4004`）；`session_id` 找不到（含已过期）→ `4006`；
`seq` 非整数 / 负 / 大于当前 s → `4007`。Resume 后 session_id 保持不变。

### 7.6 关闭码

| 关闭码 | 含义 |
| --- | --- |
| 1001 | 服务端关闭 |
| 4001 | 无效 opcode |
| 4002 | payload 不是合法 JSON / 不是 JSON 对象 |
| 4004 | token 无效（含缺 `QQBot ` 前缀） |
| 4006 | session 无效 / 已过期，请重新 Identify |
| 4007 | seq 错误 |
| 4009 | 连接过期：鉴权前 **20 秒**未 Identify，或心跳超时（**112.5 秒**未收到心跳） |
| 4010 | shard 无效 |
| 4013 | intent 无效 |

---

## 8. Webhook 回调

### 8.1 配置与验证流程

```
POST /_mock/webhook
{"url": "http://127.0.0.1:9000/callback", "secret": "DG5g3B4j9X2KOErG"}
```

```json
{"url": "http://127.0.0.1:9000/callback", "secret": "DG5g3B4j9X2KOErG",
 "note": "secret 用于 ed25519 签名，不填时回退到 clientSecret"}
```

发起地址验证：

```
POST /_mock/webhook/validate
```

Mock 会向回调地址 POST：

```json
{"op": 13, "d": {"plain_token": "<20位hex大写>", "event_ts": "1793591849"}}
```

业务端需在 5 秒内返回 200：

```json
{"plain_token": "<原样回传>", "signature": "<ed25519 hex 签名>"}
```

验证结果（查询用，不影响推送）：

```json
{"ok": true, "status": 200, "response": {...},
 "expected_signature": "<正确签名>", "event_ts": "...", "plain_token": "..."}
```

### 8.2 签名算法

```
seed   = secret 循环翻倍到 ≥32 字节，取前 32 字节
私钥   = ed25519 种子即该 32 字节
op=13: signature = hex(ed25519.sign(event_ts + plain_token))
事件推送: signature = hex(ed25519.sign(event_ts + 请求体原文))
```

事件推送请求头：

| Header | 值 |
| --- | --- |
| `X-Signature-Ed25519` | 上述 hex 签名（小写） |
| `X-Signature-Timestamp` | 秒级时间戳（与签名输入一致） |
| `X-Bot-Appid` | 机器人 appId |
| `Content-Type` | `application/json` |
| `User-Agent` | `QQBot-Callback` |

`secret` 为空时回退到 `clientSecret`。业务端验签：`ed25519.verify(X-Signature-Ed25519, X-Signature-Timestamp + 原始请求体)`。

回调返回**非 2xx** 或连接失败 → 该事件 `webhook_ok=false`（事件仍会计入历史，不重试）。

---

## 9. 事件

事件包统一结构（外层 `id` 与消息类事件的 `d.id` 相同，可作 `msg_id` 被动回复）：

```json
{"id": "ROBOT1.0_...", "op": 0, "s": 3, "t": "GROUP_AT_MESSAGE_CREATE",
 "d": {"author": {...}, "content": " 你好 ", "group_openid": "...",
       "message_type": 0, "timestamp": "...",
       "message_scene": {"source": "default",
                         "ext": ["msg_idx=REFIDX_...", "auth_token=..."]}}}
```

### 9.1 群聊事件

| 事件 t | 需订阅 Intent | 触发方式 |
| --- | --- | --- |
| `GROUP_AT_MESSAGE_CREATE` | `1<<25` | `POST /_mock/dispatch/group_message`（`at_bot=true`，默认） |
| `GROUP_MSG_RECEIVE` | `1<<25` | 同上，`receive=true`（全量接收模式） |
| `GROUP_ADD_ROBOT` | `1<<25` | `POST /_mock/groups/{id}/bot {"joined":true}` |
| `GROUP_DEL_ROBOT` | `1<<25` | 同上，`{"joined":false}` |

`GROUP_AT_MESSAGE_CREATE` 的 `content` 前后各补一个空格（对齐真实平台的 @ 前缀占位）。

### 9.2 单聊事件

| 事件 t | 需订阅 Intent | 触发方式 |
| --- | --- | --- |
| `C2C_MESSAGE_CREATE` | `1<<25` | `POST /_mock/dispatch/c2c_message` |
| `FRIEND_ADD` | `1<<25` | `POST /_mock/users/{id}/friend` 或 `/_mock/dispatch/friend_add` |
| `FRIEND_DEL` | `1<<25` | `DELETE /_mock/users/{id}/friend` |

### 9.3 互动事件

| 事件 t | 需订阅 Intent | 触发方式 |
| --- | --- | --- |
| `INTERACTION_CREATE` | `1<<26` | `POST /_mock/dispatch/interaction` |

事件 `d` 含 `type`（互动类型）、`scene`（`c2c`/`group`）、`chat_type`、
`data.resolved.{button_data, button_id, message_id}` 等。
外层 `id` 既是 `interaction_id`（用于 `PUT /interactions/{id}`），也可作 `event_id` 被动回复。

### 9.4 支持被动回复的事件

`event_id` 被动回复仅支持：`INTERACTION_CREATE`、`GROUP_ADD_ROBOT`、
`GROUP_MSG_RECEIVE`、`C2C_MSG_RECEIVE`、`FRIEND_ADD`。
其余事件用 `event_id` 回复 → `40034027`。

---

## 10. 驱动 Mock 的管理端（`/_mock`）

测试时用来「扮演用户和平台」——这是写集成测试的核心工具。

### 10.1 事件模拟

| 接口 | 作用 | 关键参数 |
| --- | --- | --- |
| `POST /_mock/dispatch/group_message` | 用户在群里发消息 | `group_openid`、`username`（或 `user_openid`）、`content`、`at_bot`(默认 true)、`receive`、`attachments[]`、`event_id`（可指定） |
| `POST /_mock/dispatch/c2c_message` | 用户单聊发消息 | `username`、`content`、`attachments[]` |
| `POST /_mock/dispatch/interaction` | 互动事件（按钮点击） | `type`(默认 11)、`scene`、`username`、`button_data`、`group_openid` |
| `POST /_mock/dispatch/friend_add` | 添加好友事件 | `username`、`scene`(默认 1001) |
| `POST /_mock/dispatch/raw` | 推送任意事件 | `event_type`（必填）、`data` |

返回形如 `{"ok": true, "records": [{"t": "...", "ws_sessions": 1, "webhook_ok": null|true|false, ...}], "msg_id": "<可被动回复的 id>"}`。

### 10.2 世界管理

| 接口 | 作用 |
| --- | --- |
| `GET /_mock/state` | 世界总览：会话、用户、群、webhook、注入开关 |
| `GET /_mock/users` / `/_mock/groups` | 种子数据 |
| `POST /_mock/users` | 建用户（`username`、`is_friend`、`user_openid` 可选） |
| `DELETE /_mock/users/{id}` | 删用户 |
| `POST /_mock/users/{id}/friend` / `DELETE` | 加/删好友（同时推 `FRIEND_ADD`/`FRIEND_DEL`） |
| `POST /_mock/groups` | 建群（`group_name`、`bot_joined` 默认 false） |
| `POST /_mock/groups/{id}/bot` | 机器人入群/退群（`joined`、`allow_proactive_msg`、`user_openid` 触发者） |
| `POST /_mock/groups/{id}/members` / `DELETE .../members/{uid}` | 加/删群成员（`role`: member/admin/owner） |
| `POST /_mock/inject` | 错误注入：`{"error_code": 304061}`（一次性，下条发消息生效）、`{"expire_replies": true}` |
| `GET /_mock/messages?scene=group&limit=50` | 查全部消息（含 `from_bot`、`deleted`、机器人请求的 `extra` 字段） |
| `GET /_mock/events` | 事件历史（含 `webhook_ok`） |
| `GET /_mock/media` | 已上传的富媒体文件 |
| `POST /_mock/reset` | 重置世界（**保留** appId/secret/webhook 配置） |
| `GET /_mock/info` | 服务总览 |

### 10.3 断言数据源

- `GET /_mock/messages`：机器人发出的消息（`from_bot=true`）、撤回状态（`deleted`）、
  请求透传字段（`extra`：`markdown`/`keyboard`/`message_reference`/`card`/`media`）。
- `GET /_mock/events`：每条事件的 `ws_sessions`（推给了几个会话）与 `webhook_ok`。
- `GET /_mock/history?limit=100` / `DELETE /_mock/history`：明文调用历史
  （每行含 method、path、status、req_body、resp_body、err_code、duration_ms）。

---

## 11. 种子数据

### 群

| 群名 | group_openid | 机器人 | 成员 |
| --- | --- | --- | --- |
| 读书分享会 | `3E5D8A1F7B2C9E4D6A0F1B3C5D7E9F2A` | **已加入** | 小明（member）、小红（admin）、小华（owner） |
| 技术交流群 | `4F6E8B2A9C1D3E5F7A9B1C3D5E7F9A1B` | **未加入**（用于测 40034101） | 小测试（owner） |

### 用户（均为机器人好友）

| 用户名 | user_openid | union_openid |
| --- | --- | --- |
| 小明 | `A1B2C3D4E5F6A1B2C3D4E5F6A1B2C3D4` | `9F2E872045CCCC5948BEAF5B5FCCDF22` |
| 小红 | `C3D4E5F6A1B2C3D4E5F6A1B2C3D4E5F6` | `FE003FAF76C4817251FDC128A16753BB` |
| 小华 | `D4E5F6A1B2C3D4E5F6A1B2C3D4E5F6A1` | （空） |
| 小测试 | `B2C3D4E5F6A1B2C3D4E5F6A1B2C3D4E5` | `1C99D1A0F6DAAE4BA1B2C3D4E5F6A7B8` |

机器人：username `MockBot`，bot_user_id `5777414462219517083`，
bot_member_openid `7A3B9C1D5E2F4A6B8C0D1E3F5A7B9C2D`。

---

## 12. 错误码速查

| err_code | 含义 | 何时出现 |
| --- | --- | --- |
| 401 | access token 校验不通过 | Authorization 头缺失/错误（HTTP 401） |
| 10000 | 不支持的操作 | 通用参数非法：JSON 解析失败、非 JSON 对象、op 取值非法、单次超过 20 个、`event_type` 必填、面板不存在等 |
| 100016 | invalid appid or secret | 换 token 接口（在响应体 `code` 字段） |
| 22006 | 消息类型与内容不匹配 | markdown+content 同传；msg_type=8 缺 card；msg_id 与 event_id 同传 |
| 304024 | 消息不存在 / 消息已被撤回 | 撤回接口 |
| 304061 | 消息内容无效 | msg_type=0 且 content 为空 |
| 304080 | 文件信息无效 | msg_type=7 的 file_info 无效或过期 |
| 340069 | 消息类型无效 | msg_type 不在允许集合 |
| 40007 | 已下发内容前缀不可修改 | 流式消息 replace 模式 |
| 50001 | 服务内部错误 | stream_msg_id 无效 |
| 850019 | 不支持的文件格式 | file_type 非 1~4 |
| 850026 | 下载原始文件失败 | url 不以 http 开头 |
| 850031 | 上传文件超过大小限制 | >200MB；file_data >5MB |
| 40034101 | 机器人非群成员 | 群接口前置校验（含建群/加成员时的「群不存在」） |
| 40034105 | 主动消息发送失败，无权限 | 群未开通主动消息 |
| 40054004 | 无好友关系 | 单聊前置校验；管理端用户不存在 |
| 40054005 | 消息被去重 | msg_id+msg_seq 重复 |
| 40034005 | 回复消息 msg_id 已过期 | 超被动窗口；或注入 `expire_replies` |
| 40034008 | markdown 参数有空值 | msg_type=2 且 markdown.content 空 |
| 40034024 | 请求参数 msg_id 无效或越权 | msg_id 不存在／回复机器人自己消息 |
| 40034025 | 请求参数 event_id 无效 | event_id 不存在；互动回复 interaction_id 无效 |
| 40034026 | 请求参数 event_id 已过期 | 超被动窗口 |
| 40034027 | 该事件不支持回复消息 | 事件类型不在被动回复白名单 |
| 40093001 | 文件上传失败 | upload_id 无效 / 分片未完成 |

网关关闭码见 §7.6。

---

## 13. 与真实平台的差异（未实现清单）

对接真实平台时下列接口在 Mock 中**不存在**，调用会是 404：

- 入群自动审批策略：创建 / 修改 / 删除 / 执行 / 白名单 / 列表，入群申请审批与列表
- 群聊流式消息（真实平台是群流式；Mock 只实现了单聊流式，口径见 `docs/qq-bot`）
- 频道（公会）域全部：子频道、消息收发、成员身份组、权限、禁言、小程序、素材、日程、公告、音频
- 全局自定义菜单为简化版（无校验、无版本号控制语义），指令面板是单机存储

另有若干 Mock 简化：

- access_token 不过期，不校验 scope / 频率限制（文档里的 QPS/QPM 限制一律不生效）
- 错误码 message 文案与平台近似但不保证逐字一致
- Webhook 推送失败不重试，只记 `webhook_ok=false`
- `session_start_limit` / `shards` 为固定值

---

## 14. 最小接入清单（冒烟顺序）

1. `POST /app/getAppAccessToken` 拿 token；
2. `GET /users/@me` 验证 `Authorization: QQBot <token>`；
3. `GET /v2/groups/{读书分享会}/info` + `members`；
4. `POST /v2/groups/{群}/messages {"msg_type":0,"content":"hi"}`（主动消息）→ 记下返回的 `id`；
5. `DELETE` 上面那条消息 → `304024` 二次撤回；
6. 网关：`GET /gateway/bot` → 连 `/websocket` → 收 op=10 → 发 op=2（intents 至少含 `1<<25`）→ 收 READY → 起心跳；
7. `POST /_mock/dispatch/group_message {"group_openid":..., "username":"小明","content":"你好"}` → 网关收到 `GROUP_AT_MESSAGE_CREATE`（`s` 递增）；
8. 用事件 `id` 作 `msg_id` 发被动回复 → 用同一 `msg_id` 再发一次验证 `40054005` 去重；
9. `POST /_mock/inject {"error_code":304061}` → 下一条发消息返回注入错误码；
10. `POST /_mock/webhook` + `POST /_mock/webhook/validate` 走完 op=13 与验签；
11. 断线重连：5 分钟内 `op=6` Resume 验证断线补发。

完整分用例清单见 `docs/tdd.md`（107 项检查，含三个可运行脚本 `scripts/smoke_http.py`、`scripts/smoke_ws.py`、`scripts/demo_extra_tests.py`）。
