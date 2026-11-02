# 风险登记（QQ Bot 子网关）

> **SDD 第 6 步**：评估前 5 步定下来的系统。每条风险必须有「怎么验证」。

| # | 风险 | 影响 | 缓解 | 怎么验证 |
|---|---|---|---|---|
| R1 | **换 token 的单飞粒度是 app_id**：同一 app_id 的并发刷新只跑一次，刷新失败时所有并发请求一起拿到旧 token 或一起 502 | 单机器人换 token 故障（如平台 100001 限流、100007 appid 无效）期间，该机器人的全部调用不可用 | 刷新失败时**先回退仍有效的旧 token**（仅完全过期才 502）；冷静期 30s 限制打平台接口的频率；`TokenFetchFailed` 错误消息含平台 `code`，便于排障 | 集成测试：换 token 上游返 200+`code` 业务错误 → 转发 502；`expires_in` 字符串形态可解析；并发请求只触发一次换 token（mock 上游计数） |
| R2 | **`expires_in` 字符串/数字双形态**（ADR-Q4）：自定义反序列化两种都收，解析失败丢弃字段按 7200 兜底 | 平台若返回一个完全无法解析的值，缓存有效期按 7200s 算，可能比真实值长或短 | 7200s 是平台文档约定值；`expires_at` 到期后的强制刷新路径不受影响（过期即重取）；`min(expires_in, 86400)` 上限保护防异常值钉死缓存 | 单测：number / 字符串 / 缺失 / 畸形四种 `expires_in` 形态各自的 `expires_at` 行为 |
| R3 | **token 刷新窗口外的同 token 回写陷阱**（ADR-Q6）：若错误回写 `expires_at`，旧 token 被当成「还有 7200s」 | 旧 token 真正过期后的一段时间内，所有请求带过期 token 被平台拒（401），且缓存不会自我修正 | 同 token 字符串不回写 `expires_at`，只更新 `last_fetch`；刷新冷静期 30s内直接用旧 token | 单测：mock 换 token 上游在 `remaining > 60s` 时返回同 token → `expires_at` 不变、`last_fetch` 更新 |
| R4 | **入站事件无签名校验**（ADR-Q7）：平台推送事件时的签名头与验签公钥不在本地参考文档内 | 能向 `/qqbot/callback/{app_id}` POST 者可伪造事件让订阅者收到（`X-Bot-Appid` 头一致性校验挡不住自己持有 app_id 的攻击者） | 网关只做 `X-Bot-Appid` 与路径一致性校验（401）；部署侧义务：回调端点只暴露给平台出口 IP / 前置反代做来源限制 | 集成测试：`X-Bot-Appid` 与路径不一致 → 401；缺该头时不拒绝（平台某些推送可能不带）；该缺口已记录在 ADR-Q7，不在 v1 修复 |
| R5 | **`/qqbot/*` 无客户端鉴权 + `app_secret` 明文**（ADR-Q11） | 能访问端口者可消耗全部机器人配额；`GET /__admin/qqbot/bots` 泄露 app_secret | 部署侧：绑回环、前置反代加鉴权、设 `GATEWAY_ADMIN_KEY`；README 环境变量表注明 | 回归测试钉住管理端点鉴权（`__admin/qqbot/*` 缺 key → 401）；残留风险记录于 ADR-Q11 |
| R6 | **事件投递 at-most-once**（ADR-Q13）：ACK 之后异步投递，失败不重试不补发 | 订阅者短暂不可用时事件永久丢失（webhook 形态）；WebSocket 形态断线期间靠平台 Resume 补发，但「收到 op 12 ACK 之后、投递之前」网关崩溃也丢 | 投递失败计 `record_error` + 日志，可由 `/__admin/qqbot/metrics` 的错误数监控；订阅者侧应做幂等 + 自身兜底 | 集成测试：订阅者返 500 → 事件统计记一次错误、平台侧 ACK 仍是 200；无 `event_url` → 计错误并告警 |
| R7 | **WebSocket supervisor 无限重连窗口**：传输层错误（1006 等）按「保留 session 重连」退避重试，上限 60s | 平台长时间不可达时每机器人每分钟一次重连尝试（可接受）；不可重试码（4001/4002/4010-4014/4914/4915）会停止并记 `last_error`，需人工介入 | `GET /__admin/qqbot/ws` 的 `reconnects` / `last_error` 让空转可见；监控告警可基于 `last_error` 非空 | 集成测试（mock wss 上游）：关闭码 4014 → 任务停止、`last_error` 有值、不再重连；1006 → 保留 session 重连 |
| R8 | **心跳漏检的假阳性**：连续 2 周期未收 op 11 就断开重连——平台若临时拉长 ack 间隔（如自身拥塞），会触发不必要的 Resume | 频繁 Resume 换来的是事件重放（平台行为，可能重复投递） | 2 周期阈值是「保守」侧（等两个完整心跳周期才动）；Resume 语义让重连代价可控；订阅者应做事件幂等 | 集成测试（mock wss）：连上后不发 ack 两周期 → 触发重连且使用同一 session_id |
| R9 | **`app_id` 既是路由键又是 token 缓存键**：删除机器人后 slot 残留（惰性清理） | 删除再重建同 app_id 的机器人会命中旧缓存 token；若旧 token 已过期则强制刷新，无实质影响 | slot 惰性清理与 `QuotaTracker` 同策略；删除机器人后同 app_id 重建属于边缘操作 | 单测：删除 → 重建同 app_id → 缓存命中过期 token 时触发强制刷新 |
| R10 | **沙箱 WSS 接入点是推断值**：本地参考文档只示例了正式环境 `wss://api.bot.qq.com/websocket/`，沙箱值按 HTTP base 推断为 `wss://sandbox.api.bot.qq.com/websocket/` | 沙箱机器人连 websocket 形态时可能连不上（接入点不存在） | `ws_url` 逐机器人覆盖提供 escape hatch（ADR-Q12）：沙箱实机连不上时手工填接入点；常量集中于一处便于修正 | 代码评审：常量集中在 `bot.rs`；平台文档更新后改一行即可；无自动化验证（沙箱接入点不在本地文档内） |
| R11 | **`shard` 字段透传但不编排**（ADR-Q9）：配错分片（如 `[1,1]`）会被 `validate` 拦下，但 `[0,4]` 单连接场景实际只有 1/4 事件 | 高频机器人按 4 分片配置后，网关只建一条连接，事件覆盖不完整 | 文档注明「一个机器人一条连接」；需要分片能力时配多个 app_id 或前置平台 SDK | `validate` 单测：`[1,1]` → 400；集成测试：Identify 包携带配置的 `shard` 原值 |
| R12 | **回调端点不限方法外的滥用面**：`/qqbot/callback/{app_id}` 只接受 POST（其他方法走 fallback → 404），但 POST 体不做结构限制 | op 不是 13/0 的畸形 payload 会落到「立即回 op 12 + 异步投递」分支，把垃圾事件投给订阅者 | op 12 ACK 保证平台侧不重试；投递体原样字节由 `event_url` 订阅者自行判别（网关不理解事件结构是声明过的范围外） | 集成测试：op 99 的 payload → 200 `{"op":12}` 且投递计数 +1；JSON 解析失败 → 400 |
