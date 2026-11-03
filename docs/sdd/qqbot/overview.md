# 概述（QQ Bot 子网关）

> **SDD 第 1 步**：系统是什么、边界在哪里。后续所有步骤以此为范围。
> 本文档是 [`../overview.md`](../overview.md)（top-gateway）的子系统规格，只描述 QQ Bot 子网关；路由转发、健康、指标等通用行为以 top-gateway 文档为准。

## 一句话定义

QQ Bot 子网关是**内嵌于 top-gateway 的 QQ 机器人平台接入层**：把 QQ 机器人开放平台的三条通道收进一个进程——客户端经 `/qqbot/{app_id}/...` 调用 OpenAPI（网关用 appId/clientSecret 自动换取并续期 `access_token`，注入 `Authorization: QQBot <token>`），平台经 `/qqbot/callback/{app_id}` 推送的 Webhook 事件被验签应答后转发给订阅者，或者由网关作为 WebSocket 客户端后台连入平台网关接收事件。一个机器人 = 一条配置记录（`bots.toml`）。

## 它解决什么

一个 QQ 机器人应用要同时面对三类平台交互，每件都是**与业务无关但又必须做对**的平台 plumbing：

1. **OpenAPI 调用前先换 token**：`access_token` 有效期 7200s，过期前 60s 内才能换到新 token，且换 token 接口**业务错误时 HTTP 仍是 200**、错误码在响应体 `code` 里；多机器人（多 appId）时每个机器人各一套凭证与刷新逻辑。
2. **Webhook 回调要做 ed25519 签名应答**：平台先发 op 13 验证请求，机器人必须用 botSecret 派生的私钥对 `event_ts + plain_token` 签名并回 `{"plain_token","signature"}`，否则回调地址注册不通过；之后每个事件还要即时回 op 12 ACK。
3. **WebSocket 接入要自己跑 Identify / 心跳 / Resume 状态机**：Hello → Identify → READY → 周期心跳 → 断线按关闭码决定 Resume 还是 Identify，重连还要指数退避。

QQ Bot 子网关把这三件事收进网关，应用后端只看到普通的 HTTP：

```
应用后端  POST http://gw:8000/qqbot/10203040/v2/users/<openid>/messages
        │     （无需管 token：网关自动换取/续期，注入 QQBot 头）
        ▼
QQ 开放平台  https://api.bot.qq.com/v2/users/<openid>/messages

平台推送  POST http://gw:8000/qqbot/callback/10203040   ← 管理端登记的回调地址
        │  op13 验证 → 网关签名应答；op0 事件 → 先回 op12，再转发给订阅者
        ▼
订阅者    POST <bot.event_url>  （可选 Bearer event_secret）

或：网关后台 wss 连入平台 → 收到事件 → POST <bot.event_url>
```

## 范围内

- **OpenAPI 透传转发**：`/qqbot/{app_id}/...` 剥前缀后拼到平台 API base（正式 `https://api.bot.qq.com`，沙箱 `https://sandbox.api.bot.qq.com`），body 与响应流式透传，网关注入 `Authorization: QQBot <access_token>`，客户端原 `Authorization` 一律替换。`api_base` 可逐机器人覆盖（指向自建 mock 或反代）。
- **access_token 自动管理**：按 appId/clientSecret 换 token，缓存到期前自动续期；**单飞**（同一机器人并发请求只触发一次换 token）+ **刷新冷静期**（避免在「临近过期但平台还没发新 token」的窗口里每次请求都打换 token 接口）；平台业务错误（响应体 `code`）→ 502，不复用旧 token。
- **Webhook 回调端点** `/qqbot/callback/{app_id}`：
  - op 13 回调验证：按 botSecret 派生 ed25519 私钥，对 `event_ts + plain_token` 签名（hex），回 `{"plain_token":..., "signature":...}`。
  - op 0 事件：先回 op 12 HTTP Callback ACK，再异步 POST 到该机器人的 `event_url`（可选 `Authorization: Bearer <event_secret>`），投递失败只计错误数与日志，不向平台暴露。
  - `X-Bot-Appid` 头存在时必须与路径 app_id 一致，否则 401。
- **WebSocket 网关客户端**（每机器人一个后台 supervisor 任务）：取平台 WSS 接入点 → 连接 → Hello → Identify（或断线后 Resume）→ READY 后按 Hello 周期发心跳（d=最新 seq，首拍 null）→ 收 op 0 事件即投递到 `event_url`；按平台关闭码表决定停止 / 重新 Identify / Resume；指数退避重连（上限 60s）；关闭/删除/禁用机器人时终止任务。`ws_url` 可逐机器人覆盖。
- **机器人配置 CRUD**（`/__admin/qqbot/bots*`，与 provider CRUD 同构、合并式更新）、**WS 连接状态查看**（`/__admin/qqbot/ws`）、**指标查看**（`/__admin/qqbot/metrics`，复用 `RouteStats`）。
- 机器人配置原子持久化到 `GATEWAY_BOTS_FILE`（默认 `bots.toml`），重启不丢失。`app_secret` 明文落盘（与 `providers.toml` 的 `api_key` 同策略）。

## 范围外

- **不做客户端鉴权**：`/qqbot/*` 端点本身不校验客户端 key；持有网关地址者即可消耗机器人配额（与 `/llm/*` 同定位，ADR-Q11）。部署侧绑回环 / 前置反代。
- **入站事件不做签名校验**：本地参考文档只给出**验证请求的签名算法**（己方私钥签名），未给出平台推送事件时的签名头与公钥；网关只做 `X-Bot-Appid` 与路径一致性校验（ADR-Q7、风险 R4）。
- **事件投递至少一次（at-most-once）**：ACK 之后异步投递，投递失败不重试、不补发（平台侧 Webhook 无重投约定，WebSocket 断线期间的事件靠 Resume 补发，属平台行为）。失败只计 metrics。
- **不做 WebSocket 分片编排**：`shard` 字段原样传给 Identify，但网关不为同一机器人自动建立多条分片连接；一个机器人一条连接（ADR-Q9）。
- **不做事件解析**：事件 payload 原样透传给 `event_url`，网关不理解也不改写事件结构（`t` 字段只用于识别 READY/RESUMED）。
- **不做 access_token 间互备**：机器人之间不共享 token；单机器人换 token 失败即 502，不在机器人间转移（OpenAPI 调用本身按 appId 隔离，无转移语义）。
- **不做 op 11 之外的下行能力**：网关不主动向 WebSocket 发心跳以外的东西（不做语音 / 互动回执等下行）。

## 用户与典型用法

| 用户 | 用法 |
|---|---|
| 机器人应用后端 | 直接 `POST http://gw:8000/qqbot/{app_id}/v2/users/{openid}/messages`，不需要管 token；或把第三方 SDK 的 base_url 指过来（SDK 的 Authorization 头会被网关替换，凭证由网关注入） |
| 机器人运维 | `POST /__admin/qqbot/bots` 登记 appId/appSecret，`event_source=webhook` 时把回调地址 `https://your-host/qqbot/callback/{app_id}` 填进管理端并等平台验证；`event_source=websocket` 时网关自动连 wss；`GET /__admin/qqbot/ws` 看连接状态 |
| 监控 | 轮询 `GET /__admin/qqbot/metrics`（按机器人的请求数 / 并发 / 错误 / 平均延迟）与 `GET /__admin/qqbot/ws`（session、seq、重连次数、最近错误） |

## 非功能约束

- 复用 top-gateway 进程与技术栈（axum / tokio / reqwest），**不新增进程、不新增端口**；`/qqbot` 前缀为网关保留（与 `/llm` 同构的保留前缀声明）。
- 响应 body **流式透传**；换 token 是唯一一次额外的同步 HTTP 往返（命中缓存时零往返）。
- WebSocket 连接是后台任务：删除 / 禁用机器人或切换 `event_source` 时立即终止；平台关闭码里的不可重试码（4001/4002/4010–4014/4914/4915 等）终止任务并记最近错误，不空转重连。
- 单进程、无外部依赖；机器人表存内存，持久化靠本地 TOML 文件。
- 错误体沿用 top-gateway 统一格式 `{"error":{"code":<u16>,"message":...}}`。
