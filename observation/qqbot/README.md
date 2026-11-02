# QQ Bot Observatory —— 对着 QQ Bot Mock 的三路通道观察

这个目录用 [QQ Bot Mock](../../docs/reference/qq-bot-mock-api.md)（`http://127.0.0.1:8080`）
作为平台替身，**观察本仓库 `top-gateway` 内嵌的 QQ Bot 子网关如何把同一个机器人
配置同时服务三条通道**：

1. OpenAPI 透传（出站）：业务端经 `/qqbot/{app_id}/...` 调平台，网关自动取 token
   并替换鉴权头；
2. Webhook 回调（入站）：平台 POST `/qqbot/callback/{app_id}`，网关先 ACK
   （`{"op":12}`）再异步把事件投到业务端 `event_url`；
3. WebSocket 客户端（入站）：`event_source=websocket` 的机器人由网关替它维持到
   平台的长连接，收到的事件走与 webhook 同一个 `deliver_event`。

与 `observation/socket` 不同，这里的 upstream mock 是**共享的外部服务**（不由脚本
自起），所以脚本遵守「不重置 mock 世界」的约定——详见下文「隔离与清理」。

## 目录组织

| 脚本 | 主题 | 网关行为（被测） | mock 观测点 |
| --- | --- | --- | --- |
| `demo_00_three_channels.py` | 三路接入总览 | 同一 bot 配置下出站透传、webhook 入站、WS 入站各走一遍；webhook 与 WS 并行时的重复投递；op 13 回签 | `/_mock/events` 的 `ws_sessions`/`webhook_ok`；`/__admin/qqbot/ws` 快照 |
| `demo_01_openapi_passthrough.py` | OpenAPI 透传 | token 缓存（换一次 vs 两次）、凭证替换（直连 401 vs 经网关 200）、状态码映射（未知 app_id 404 / 禁用 503）、业务错误码原样透传（40034101 / 304061）、被动回复与 msg_id 去重、撤回幂等 | `/_mock/history` 里 `/app/getAppAccessToken` 的**增量**；`/_mock/inject` 一次性错误码；`/__admin/qqbot/metrics` |
| `demo_02_webhook_callback.py` | Webhook 回调 | 先 ACK 再异步投递（慢业务端 3s 不阻塞平台）、op 13 ed25519 即时回签、投递失败不重试（恒 500 → errors +1 一次）、无 event_url 计错误丢弃、禁用 bot 回调 404（vs 转发 503） | `/_mock/events` 的 `webhook_ok`（平台视角） vs `/__admin/qqbot/metrics`（网关视角） |
| `demo_03_websocket_supervisor.py` | WS 接入 | 注册/变更/禁用/删除联动 supervisor、Identify → READY 记 session_id、last_seq 随事件 s 前进、PUT 重建 = 干净冷启动（新 session、走 Identify 而非 Resume） | `dispatch` records 的 `ws_sessions`；`/__admin/qqbot/ws` 的 connected/session_id/last_seq/reconnects/last_error |
| `_common.py` | 公共库 | — | mock 管理端封装、网关子进程（控制台 `run` 模式）、EventSink 事件投递接收器、输出格式化、共享世界的保存/还原约定 |

## 运行

前置条件：QQ Bot Mock 已在 `http://127.0.0.1:8080` 运行（可用环境变量
`QQBOTMOCK_BASE` 覆盖）；网关二进制（`cargo build` 出的
`target/debug/top-gateway.exe`）由脚本自动构建。

```bash
uv run python observation/qqbot/demo_00_three_channels.py
uv run python observation/qqbot/demo_01_openapi_passthrough.py
uv run python observation/qqbot/demo_02_webhook_callback.py
uv run python observation/qqbot/demo_03_websocket_supervisor.py

# mock 在别的地址时：
QQBOTMOCK_BASE=http://127.0.0.1:9090 uv run python observation/qqbot/demo_03_websocket_supervisor.py
```

网关子进程固定绑定 `127.0.0.1:18198`、admin key 为演示统一的 `demo-admin-key`，
以**控制台模式 `run`**启动（无参数启动会尝试进入 Windows SCM 服务模式），
routes/providers/bots 文件放在系统临时目录、进程退出即清——与本机 18000 端口的
既有部署互不干扰。

## 拓扑

```
业务端(脚本)
  │ OpenAPI 出站
  ▼
127.0.0.1:18198 top-gateway(run)  ──token 注入 QQBot──▶  127.0.0.1:8080 QQ Bot Mock
  │                                                          │
  │ ▲ webhook 入站：平台 POST /qqbot/callback/{app_id}       │ 平台侧动作：
  │ │   立即 ACK {"op":12}，tokio::spawn 异步投递            │  · /app/getAppAccessToken
  │ └────────────────────────────────────────────────────────┘  · webhook 推送（可失败）
  │                    EventSink(业务端，本机随机端口)              · WS 会话推送
  ▼                          ▲                          ▲
ws 状态/统计：/__admin/qqbot/* │ 事件字节原样投递        │ 平台 ──WS 长连接─▶ 网关(WsTable)
                              （webhook 与 WS 同一个 deliver_event）
```

观察的本质：**平台视角（mock 的 `/_mock/events`、`webhook_ok`/`ws_sessions`）与
网关视角（`/__admin/qqbot/metrics`、`/__admin/qqbot/ws`）描述同一批事件的两侧**，
演示处处以两者互为对照。

## 沉淀的观察事实

- **ACK 与投递是两件事**（demo_02）：慢业务端（3s 才回响应）在 dispatch 后 0.6s
  时，平台侧已是 `webhook_ok=true`——ACK 在投递之前同步完成；投递任务自带 guard，
  且**不计入 stats**，只有投递失败才 `errors +1`。
- **投递是 at-most-once**（demo_02）：业务端恒 500 时，网关只投递一次、记一次错误，
  没有二次投递，平台侧始终 `webhook_ok=true`——失败不重试、也不向平台暴露
  （ADR-Q13）。无 `event_url` 的事件同样计错误后丢弃。
- **webhook 与 WS 会重复投递同一事件**（demo_00）：平台 webhook 仍指向网关时，
  一次 dispatch 会被 webhook 与 WS 各推一次，网关两侧都投给同一个 `event_url`
  ——去重是业务端的事（按事件 id）。两份副本可由外层 `s` 区分：**webhook 推送
  不带 `s`，只有 WS 帧带**，所以 `last_seq` 天然是 WS 信道一侧的概念（mock 文档
  §9 的推送信封里带 `s` 属期望形态，实现未跟上）。
- **token 缓存要看增量**（demo_01）：mock 的 `/_mock/history` 是全局累积的明文流水
  （跨演示、含管理端调用），绝对值证明不了缓存；两次相同调用之间
  `/app/getAppAccessToken` 的增量是 +1 然后 +0——第二次命中网关内缓存。
- **凭证替换在直连/经网关两侧可对照**（demo_01）：同一假
  `Authorization: Bearer client-fake-token` 直连 mock → 401，经网关 → 200；
  网关把客户端鉴权头整个换成 `QQBot <平台 token>`，并在响应上加
  `x-qqbot-bot: <bot id>`。
- **「未注册 / 禁用 / 回调」三种状态码不同**（demo_01、demo_02）：转发路径上
  未知 app_id → 404、bot 禁用 → 503；回调路径上 bot 禁用 → **404**（ADR-Q8：
  平台应理解为「地址不存在」而不是「服务不可用」）。
- **业务错误是 HTTP 200 + err_code，网关原样透传不改码**（demo_01）：群未加入
  40034101、一次性注入 304061、重复 msg_id 40054005、二次撤回 304024——网关不解释
  err_code，业务语义属于平台（平台连取 token 的接口都恒为 HTTP 200、只在体里给
  err_code，网关据此判断成败——ADR-Q3）。
- **被动回复的去重键是 msg_id+msg_seq**（demo_01）：同 msg_id 再发 → 40054005；
  撤回幂等，已撤回再撤 → 304024。
- **op 13 回签是只签不验**（demo_00、demo_02）：网关按 `bot.app_secret`（种子循环
  补齐到 32 字节）对 `event_ts + plain_token` 做 ed25519 签名，回签与 mock 的
  `expected_signature` 一致；回调路径本身不校验 `X-Signature-Ed25519`，因为平台的
  入站签名公钥不在参考文档内（ADR-Q7/R4）。
- **WS 生命周期全由 bot 表驱动**（demo_03）：注册/启用 → supervisor 自动拉起
  （Identify → READY 记 session_id）；PUT 变更 → 停旧按新配置重建，内存里的
  session/seq 清零、新连接走 Identify 而非 Resume、reconnects 归零——一次 PUT 相当于
  一次干净的冷启动；禁用/删除 → 行从 `/__admin/qqbot/ws` 消失（而不是留一行
  connected=false）。
- **投递路径与通道无关**（demo_03）：webhook 与 WS 收到的事件经同一个
  `deliver_event` 投给 `event_url`，业务端看不出事件来自哪条通道。
- **死 webhook 会推迟 WS 到达**（demo_00）：平台向不可达 webhook 的推送是同步等待的，
  本机 loopback 连接拒绝约 2s（机器特性），这段延迟在平台侧、不经过网关；网关收到
  WS 帧后的投递仍是毫秒级。
- **入群状态是 40034101 的世界前提**（demo_01）：mock seed 世界里技术交流群是
  「未加入」，但共享 mock 的实际状态可能已被此前的试用改写；demo_01 先经网关
  `GET .../bot_state` 读现状、用管理端钉成未加入再演示，退出时还原。注意
  `/_mock/state` 的群成员列表**不含机器人**，入群状态以
  `POST /_mock/groups/{id}/bot` 的回执为准。
- **`/_mock/info` 文档有而实现无**（demo 套件）：mock 文档（§目录）列出了
  `/_mock/info`，实际访问 404；可用信息从 `/_mock/state` 取（appId、bot 名、
  webhook 注册）。

## 隔离与清理

- **共享的 mock 世界不重置**：脚本从不 `POST /_mock/reset`（它会清掉群成员、
  会话与流水中其他使用者留下的状态）。
- **平台 webhook 注册**：进入演示前保存、退出时还原为原值（含空 url）；只有
  demo_00/02/03 需要重定向它，demo_01 全程不动。
- **群成员状态**：demo_01 会临时把机器人移出技术交流群以制造 40034101，退出时
  按演示前的原样还回去（退群/入群动作本身会向平台 webhook 推
  GROUP_DEL_ROBOT/GROUP_ADD_ROBOT，演示时 webhook 处于不可达或指向本网关，无副作用）。
- **演示注册的 bot 在结束时删除**（WS supervisor 联动停止）；`/_mock/history` 与
  `/_mock/events` 是共享流水，不做清理。
- **网关子进程**：控制台模式 `run`、固定 `127.0.0.1:18198`、临时配置文件随进程
  退出清空，与本机 18000 端口的既有部署、以及 `observation/socket` 的 18099 互不干扰。
