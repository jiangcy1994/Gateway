# 示例配置说明（examples/）

本目录给出 LLM 子网关两个配置面与 QQ Bot 子网关一个配置面的**示例 TOML**
与逐字段说明，覆盖模型映射、应用组合、按供应商超时与按供应商代理
（ADR-L14 / L15 / L16 / L17 / L18 / L19）以及 QQ 机器人三通道接入
（ADR-Q8…Q13）。模板里的凭证全是
占位符，复制到部署位置后必须替换。路由表（routes.toml）沿用仓库根目录的既有
文件与既有文档，不在本目录重复。

| 示例文件 | 正式文件 | 环境变量 | 管理 API |
|---|---|---|---|
| `providers.example.toml` | `providers.toml` | `GATEWAY_PROVIDERS_FILE`（默认 `providers.toml`） | `/__admin/llm/providers*` |
| `apps.example.toml` | `apps.toml` | `GATEWAY_APPS_FILE`（默认 `apps.toml`） | `/__admin/llm/apps*` |
| `bots.example.toml` | `bots.toml` | `GATEWAY_BOTS_FILE`（默认 `bots.toml`） | `/__admin/qqbot/bots*` |

字段权威定义见 `src/llm/provider.rs` / `src/llm/app.rs` / `src/qqbot/bot.rs`
与 `docs/sdd/llm/data-model.md` / `docs/sdd/qqbot/data-model.md`；行为契约见
`docs/sdd/llm/interfaces.md` / `docs/sdd/qqbot/interfaces.md`。本文件
是说明文档，契约若与本文件冲突以 SDD 为准。

## 1. 快速上手

```bash
# 1) 复制模板到网关的配置目录（控制台模式按 cwd 解析相对路径；服务模式按
#    服务基目录，见第 6 节），改名成正式文件名
copy providers.example.toml  providers.toml
copy apps.example.toml       apps.toml
copy bots.example.toml       bots.toml

# 2) 替换全部占位凭证（providers 的 api_key、apps 的 key、bots 的
#    app_secret 与 event_secret）
notepad providers.toml

# 3) 启动网关（或重启：配置只在启动时从文件加载）
cargo run --release
```

管理端点需要 `GATEWAY_ADMIN_KEY`：控制台模式是普通环境变量，Windows 服务模式
写在配置目录的 `gateway.env`（见 README.md「部署为 Windows 服务」）。**未设置
= 端点完全开放 + 启动告警**——本文的 curl 都假设你已经设了。

此后三个文件就交给管理 API 维护：任何经 `/__admin/llm/providers*` /
`/__admin/llm/apps*` / `/__admin/qqbot/bots*` 的变更都会**原子写回**对应文件
（临时文件 + rename），**重写会丢弃注释**。想保留注释就只在启动文件里写，
热修改走管理 API——两套途径不要混着对同一个文件用。写语义（curl 示例见
第 9 节，端点表见 README.md）：

| 操作 | 语义 |
|---|---|
| `POST`（providers / apps / bots） | 新增；providers 的 `id`、apps 的 `id`/`key`、bots 的 `id` 省略时生成；providers 的 `models` 省略 = 空表（协议级兜底）；bots 的 `app_id`/`app_secret` 必填（缺 → 422） |
| `PUT`（providers / apps） | **合并**：字段缺省 = 保留原值；`rpm_limit`/`tpm_limit` 显式 `null` = 不限，`timeout_secs` 显式 `null` = 回落全局默认，`proxy_url` 显式 `null` = **清除代理**（回到共享直连 client）；`protocols`/`models` 给列表 = **整体替换**（providers 的 `models` 给 `[]` = 清回空表兜底；apps 的 `models` 给 `[]` = 不限制） |
| `PUT`（bots） | **合并**：字段缺省 = 保留原值；`api_base`/`ws_url`/`event_url`/`event_secret` 显式 `null` = **清除覆盖**（回落平台默认 / 不投递 / 不附 secret）；`event_source` 给值 = 切通道，联动 WS supervisor（websocket 且 enabled → 起连接，否则停） |

## 2. providers.example.toml 字段

`[[providers]]`：

| 字段 | 类型 | 缺省 | 说明 |
|---|---|---|---|
| `id` | string | 必填 | 表内唯一；管理 API 的路由键，消息里回显它 |
| `name` | string | 空串 | 展示名，无功能语义 |
| `protocols` | 列表 | 必填 | `openai` / `anthropic` / `systemone`，可多选；旧形态单值 `protocol = "openai"` 仍可读（ADR-L13），但列表是规范形态。**systemone 只能经显式段 `/llm/systemone/...` 访问**（`/llm/v1` 不推断它），且其 usage 记账是桩（恒记 0）——`tpm_limit` 对它不生效、TPM 加权选不到它（R8） |
| `base_url` | string | 必填 | 上游地址，scheme 必须 http/https；转发路径直接拼在它后面 |
| `api_key` | string | 必填 | 明文落盘（ADR-L11）；按协议注入（openai/systemone → `Authorization: Bearer`，anthropic → `x-api-key` + 缺省补 `anthropic-version: 2023-06-01`，客户端已传则保留原值），客户端凭证一律被替换 |
| `rpm_limit` | u64 | 不限 | 每分钟请求数限额；`Some` 时必须 > 0 |
| `tpm_limit` | u64 | 不限 | 每分钟 token 数限额，**事后**按响应尾包的 `usage` 计量（best-effort，R1） |
| `timeout_secs` | u64 | 全局 `GATEWAY_DEFAULT_TIMEOUT_SECS` | 该供应商的上游超时（ADR-L17） |
| `proxy_url` | string | 不代理 | 该供应商的 HTTP 代理（ADR-L19）：`http://` / `https://`，host 非空；`socks5://` 不支持（validate 直接判非法）。缺省 / 显式 `null` = 共享直连 client；代理只影响这一家供应商 |
| `enabled` | bool | true | false 时退出所有选择池 |
| `models` | 表列表 | 空表 | 模型映射表，见下；**空表 = 协议级兜底** |

`[[providers.models]]`：

| 字段 | 类型 | 缺省 | 说明 |
|---|---|---|---|
| `name` | string | 必填 | **网关侧**模型名：客户端请求里写它，apps 组合里也写它；provider 内唯一 |
| `upstream` | string | = `name` | 上游真实模型名；与 `name` 相同时原始请求 body **零差异透传**（不重序列化） |
| `priority` | i32 | 0 | 越大越优先；同一网关模型名可被多个供应商声明，分层选择（ADR-L16） |

配额是**供应商级**的：一个供应商底下全部模型、全部协议合计一份 RPM/TPM（60 秒
滚动窗口）。多协议供应商（`protocols` 写两个）同时进入两个协议池，但配额只记
一份——跨协议共用。

## 3. 示例文件里的选择行为（分层，ADR-L16）

以本目录的 providers 为例，请求 `model = "chat-pro"` 时：

1. 候选池 = 声明了该模型、说该协议、`enabled` 的供应商：
   `shared-primary`（priority 10）与 `shared-fallback`（priority 1）。
2. 按 priority **降序分层**：最高层（10）里只剩 `shared-primary`。只要该层
   还有 `weight > 0` 的成员（`weight = min(rpm 余量比例, tpm 余量比例)`），请求
   就在这一层内按 weight 加权随机——单成员即必然选中它。
3. 主用层配额耗尽（weight 归零）→ 降层到 priority 1，用 `shared-fallback`；
   两层都耗尽 → `429`（`QuotaExhausted`，house 错误体）。
4. 选中后：`model` 被改写成 `chat-pro-2026-04`，注入 `shared-primary` 凭证，
   响应头回 `x-llm-provider: shared-primary` 便于排错。上游响应原样回吐，
   **不做反向改写**——响应 body 里的模型名仍是上游名（`chat-pro-2026-04`）。

其他情形：

- `reason-large` 只有 `open-models` 声明 → 直接选中它，用 `timeout_secs = 60`
  （比全局默认宽，因为这条路径慢），上游收到 `hub/reason-large`。
- 客户端写了一个没人声明的模型（如 `chat-ultra`）→ **404** `ModelNotFound`，消息
  提示去 `/__admin/llm/providers` 登记模型表；想给所有模型一个结尾安全感，就
  挂一个**空模型表**的协议级兜底供应商（文件末尾有注释掉的示例）——它的
  effective priority 是 `i32::MIN`，只要有一家显式声明了该模型，兜底永远排最后，
  且不改写 `model` 字段。
- 模型**有**供应商声明，但候选池空（声明者全部 `enabled = false`，或请求的
  协议段与声明的协议不符）→ **503** `NoProviderForModel`。注意 `enabled = false`
  把供应商**移出所有选择池**，不是把它降层——排错时把两层都关掉会直接 503。
- 请求体不带 `model`（或不是 JSON）→ 不分层、不映射，按协议下所有启用供应商
  扁平加权随机（即旧行为）。

## 4. apps.example.toml 与凭证

客户端把 app key 填在 SDK 原本填供应商 key 的位置即可（OpenAI SDK 填
`api_key`、Anthropic SDK 填 `api_key` 或 `x-api-key`），base_url 指网关：

```
http://127.0.0.1:8000/llm/v1          # 两家 SDK 都能零配置接入（按端点推断协议）
http://127.0.0.1:8000/llm/openai/v1   # 显式协议段（共用端点 / 未知端点用这个）
```

`/llm/v1` 的推断是**闭表**：两协议共用的端点（`models`、`files`）与未登记
端点返回 404 并提示改用显式段；`systemone` 同样不经 `/llm/v1`（见第 2 节）。

凭证矩阵（ADR-L15）：

| 入站凭证 | 结果 |
|---|---|
| 无凭证 | 默认策略：放行全部已映射模型 |
| 不命中任何 app | **401**（fail-closed：写错的 key 不能换来权限） |
| 命中但 `enabled = false` | 403 |
| 命中且 `models` 非空，请求 `model` 不在组合内 | 403 `ModelNotAllowedForApp` |

组合校验的已知盲区（风险 R11）：只覆盖 body 里带 `model` 字段的 JSON 请求——
无 `model` / 非 JSON 的请求**不拦截**，按协议级路由放行；超出 16 MiB 缓冲
上限的请求直接 **413 拒绝**（不进入盲区，上游收不到）。`models` 是**约束面
不是安全面**：真要把某个 app 隔离出去，给它配专属供应商（或前缀路由）而不
是只靠组合。

示例文件里三条覆盖三种形态：限定组合（`cli-code`）、混搭组合
（`cherry-studio`）、空表不限制（`open-all`）。

## 5. bots.example.toml 与三通道

一个机器人配置同时服务三条接入通道（README 的 QQ Bot 段是入口）：

1. **OpenAPI 透传** `/qqbot/{app_id}/...`：路径首段是平台 `app_id`，其余拼到
   有效 `api_base`；网关自动换发并缓存 `access_token`（到期前 5 分钟刷新、
   临近过期 30s 冷却防惊群），注入 `Authorization: QQBot <token>`——客户端
   凭证一律被替换，响应头附 `x-qqbot-bot: <bot id>`。
2. **Webhook 回调** `/qqbot/callback/{app_id}`（仅 POST）：平台推 `op == 13`
   回调地址验证时按 `app_secret` 做 ed25519 签名应答（**只签不验**，平台公钥
   不在本地参考文档内，ADR-Q7）；其他事件先回 `{"op":12}` ACK，再异步投递到
   `event_url`（at-most-once，失败只计错误不重试、不向平台暴露，ADR-Q13）。
3. **WebSocket 客户端**（`event_source = "websocket"`）：网关后台 supervisor
   主动连 wss——Identify → READY（存 session）→ 周期心跳 → 事件投递到同一个
   `event_url`；断线按平台关闭码分流（可恢复尝试 Resume、不可恢复停止不重连），
   指数退避 2s→60s。

`[[bots]]`：

| 字段 | 类型 | 缺省 | 说明 |
|---|---|---|---|
| `id` | string | TOML 必填 | 配置 id：表内唯一、管理 API 的路由键（仅 `POST` 可省略，生成 UUID v4；`PUT` 不可改）。**与平台无关**——别和 `app_id` 混了 |
| `name` | string | 空串 | 展示名，无功能语义 |
| `app_id` | string | 必填 | 平台 appId：三通道的路由键，全表唯一；请求首段对不上 → 透传 404 |
| `app_secret` | string | 必填 | 平台 clientSecret：换 `access_token` 的凭证，也是回调 op 13 签名的 ed25519 种子；明文落盘（ADR-Q11，与 `api_key` 同级） |
| `sandbox` | bool | false | true → 内置 api/ws 地址同时切到 `sandbox.api.bot.qq.com`；正式与沙箱机器人可混跑在同一网关里 |
| `api_base` | string | 按 `sandbox` 取平台常量 | 覆盖 OpenAPI base；须 `http(s)://host`；`PUT` 显式 `null` = 清除覆盖 |
| `ws_url` | string | 按 `sandbox` 取平台常量 | 覆盖 WSS 接入点；须 `ws(s)://host[:port][/path]`；**沙箱 WSS 地址是推断值**（R10），实机连不上时用它逐机器人覆盖（ADR-Q12） |
| `intents` | u32 | `100663296` = `(1<<25)\|(1<<26)` | 事件订阅位掩码，透传给 Identify；`0` 非法；位定义以平台文档为准 |
| `shard` | `[u32;2]` | `[0, 1]` | `[分片 id, 总数]`，要求 `id < total`；**透传给 Identify 但网关不做分片编排**——一机器人一条连接（ADR-Q9 / R11），要水平扩展请配多个 app_id |
| `event_source` | string | `webhook` | `webhook` = 平台推、网关被动等回调；`websocket` = 网关后台连 wss。网关不校验与平台权限的组合——配错形态由平台用 op 9 / 关闭码 4014 拒绝（ADR-Q10） |
| `event_url` | string | 不投递 | 事件投递目标，须 `http(s)://host`；webhook 与 websocket 两通道投递到同一地址，带 `Authorization: Bearer <event_secret>`，5s 超时；不设 = 事件计错误后丢弃，`/__admin/qqbot/metrics` 的错误数可见（R6） |
| `event_secret` | string | 不附 | 投递时附的 Bearer 值；空串等同不设 |
| `enabled` | bool | true | false → 透传 **503**、回调统一 **404**（回调地址从此「摘除」，平台侧重试不会继续打，ADR-Q8）、不 spawn WS supervisor |

示例文件里三条覆盖三种形态：正式 webhook 全字段（`group-qa`）、沙箱
websocket（`sandbox-ws`）、内网 `api_base` 覆盖且不投递事件（`internal-relay`）；
另附注释掉的「临时停用」条目（`enabled = false`）。

排错速查（详细错误语义见 README）：

- `GET /__admin/qqbot/ws` 看 websocket 形态机器人的连接 / session / 心跳 /
  重连状态：`reconnects` 持续上涨或 `last_error` 非空 = 需要人工介入
  （不可重试的关闭码会停止不重连，R7）。
- 透传 404 但配置存在？路由键是请求路径首段的 `app_id`，不是配置 `id`——
  两个对不上就是 404。
- ws 形态连不上沙箱？沙箱 WSS 接入点是推断值（R10），用 `ws_url` 显式覆盖。

## 6. 部署路径与文件查找

- **控制台模式**：相对路径按 cwd 解析——直接在配置目录里启动，或写绝对路径
  到 `GATEWAY_PROVIDERS_FILE` / `GATEWAY_APPS_FILE` / `GATEWAY_BOTS_FILE`。
- **Windows 服务模式**：相对路径解析到服务基目录（默认
  `%ProgramData%\top-gateway\`，可用 `GATEWAY_BASE_DIR` 覆盖）；服务进程不读
  用户环境变量，密钥放 `gateway.env`。
- `install` 只从 exe 同目录**补种** `routes.toml` 与 `providers.toml`（不覆盖
  已有文件）——`apps.toml` / `bots.toml` 不参与补种，放一份到 exe 同目录也不会
  被复制，要么手动放进基目录，要么直接用 `POST /__admin/llm/apps` /
  `POST /__admin/qqbot/bots` 建。
- 配置只在**启动时**从文件加载；运行期变更一律走管理 API（会写回文件）。

## 7. 从旧配置迁移

示例拓扑是从没有模型表的旧形态升上来的，兼容性是刻意保留的（ADR-L14）：

| 旧形态 | 新写法 | 注意 |
|---|---|---|
| `protocol = "openai"`（单值） | `protocols = ["openai"]` | 单值仍可读，不启动失败；下一次经管理 API 写回即归一化为列表 |
| 没有 `[[providers.models]]` | 加模型表 | 不加也能继续工作（空表 = 协议级兜底，参与任意 model 选择、不改写）——渐进迁移路径 |
| 无 `timeout_secs` | 按供应商加 | 不加就走全局 `GATEWAY_DEFAULT_TIMEOUT_SECS`（默认 30） |
| 无 `proxy_url` | 按供应商加 | 不加 = 共享直连 client；只有个别上游需要代理时按供应商配，不影响其余供应商 |
| （新增文件）`apps.toml` | 见本目录示例 | 没有该文件不致命（启动告警 + 无 app）；无凭证时行为与升级前完全一致 |
| （新增文件）`bots.toml` | 见本目录示例 | 没有该文件不致命（启动告警 + 无 bot）；`/qqbot/*` 行为与升级前完全一致 |

迁移建议：先只加 `providers` 的模型表与 `timeout_secs`（客户端零改动即可拿到
改写与分层），再按需建 `apps.toml`。模型名以供应商 `/v1/models` 实际返回为准，
示例里的模型名仅作格式演示。

## 8. 安全说明

- 这三个示例文件里的所有 `api_key` / `key` / `app_secret` / `event_secret`
  都是**占位符**，正式部署必须替换；真实凭证明文落盘，依靠部署位置的访问控制
  （服务模式靠 `%ProgramData%` 的默认 ACL），不要提交进版本管理
  （ADR-012 / ADR-L11 / ADR-L18 / ADR-Q11）。
- `/llm/*` 与 `/qqbot/*` 本身**不做客户端鉴权**：app key 只是模型组合的
  约束面。部署侧务必绑定回环或前置带鉴权的反代，并设置 `GATEWAY_ADMIN_KEY`
  （设置位置见第 1 节）。
- `GET /__admin/llm/providers`、`/__admin/llm/apps` 与 `/__admin/qqbot/bots`
  的响应**包含明文 key / app_secret**，这些端点因此与管理 API 一样需要
  `X-Admin-Key`。

## 9. 验证配置生效

```bash
# 启动后列出已加载的供应商 / app（响应含明文 key，注意终端历史）
curl -s -H "X-Admin-Key: $GATEWAY_ADMIN_KEY" http://127.0.0.1:8000/__admin/llm/providers
curl -s -H "X-Admin-Key: $GATEWAY_ADMIN_KEY" http://127.0.0.1:8000/__admin/llm/apps

# 窗口内配额用量 / 各供应商请求数·错误数·平均延迟（分层降级有没有发生看这两个）
curl -s -H "X-Admin-Key: $GATEWAY_ADMIN_KEY" http://127.0.0.1:8000/__admin/llm/quota
curl -s -H "X-Admin-Key: $GATEWAY_ADMIN_KEY" http://127.0.0.1:8000/__admin/llm/metrics

# 打一发请求看实际选中和改写（x-llm-provider 指明命中的供应商）
curl -s -D - http://127.0.0.1:8000/llm/v1/chat/completions \
  -H "Authorization: Bearer sk-gw-00000000-0000-0000-0000-000000000000" \
  -H "Content-Type: application/json" \
  -d '{"model":"chat-pro","messages":[{"role":"user","content":"hi"}]}'
# 上游收到 model=chat-pro-2026-04；组合外模型（如 reason-large）→ 403

# 机器人表（响应含明文 app_secret）、WS supervisor 状态与按 bot 的指标
curl -s -H "X-Admin-Key: $GATEWAY_ADMIN_KEY" http://127.0.0.1:8000/__admin/qqbot/bots
curl -s -H "X-Admin-Key: $GATEWAY_ADMIN_KEY" http://127.0.0.1:8000/__admin/qqbot/ws
curl -s -H "X-Admin-Key: $GATEWAY_ADMIN_KEY" http://127.0.0.1:8000/__admin/qqbot/metrics

# 透传一发 OpenAPI（首段是 app_id；Authorization 由网关替换为 QQBot <token>）
curl -s -D - http://127.0.0.1:8000/qqbot/1020304050/users/@me
# 响应头 x-qqbot-bot 指明命中的配置 id

# 热修改不经文件，直接管理 API（合并语义见第 1 节）
curl -X POST http://127.0.0.1:8000/__admin/llm/providers \
  -H "X-Admin-Key: $GATEWAY_ADMIN_KEY" -H "Content-Type: application/json" \
  -d '{"protocols":["openai"],"base_url":"https://api.example.com","api_key":"sk-..."}'
curl -X PUT http://127.0.0.1:8000/__admin/llm/providers/open-models \
  -H "X-Admin-Key: $GATEWAY_ADMIN_KEY" -H "Content-Type: application/json" \
  -d '{"rpm_limit": null}'
# ↑ 显式 null = 设为不限；字段缺省 = 保留原值（open-models 其余字段不变）
curl -X POST http://127.0.0.1:8000/__admin/qqbot/bots \
  -H "X-Admin-Key: $GATEWAY_ADMIN_KEY" -H "Content-Type: application/json" \
  -d '{"app_id":"1020304099","app_secret":"DG5g...","event_url":"http://127.0.0.1:9000/qq/events"}'
curl -X PUT http://127.0.0.1:8000/__admin/qqbot/bots/group-qa \
  -H "X-Admin-Key: $GATEWAY_ADMIN_KEY" -H "Content-Type: application/json" \
  -d '{"enabled": false}'
# ↑ 临时停用：透传 503、回调 404、WS supervisor 停止；id 不可改，
#   清除 url 覆盖用显式 null（如 {"event_url": null} = 恢复不投递）
```

> `$GATEWAY_ADMIN_KEY` 是 POSIX shell 写法；PowerShell 用
> `$env:GATEWAY_ADMIN_KEY`，cmd 用 `%GATEWAY_ADMIN_KEY%`。

启动日志会告警缺失的文件与重复 id/key/app_id（不致命）；解析失败才是致命错误
（`ConfigLoad` → 启动失败，会带上文件名与 TOML 错误位置）。

## 10. 相关文档

- `README.md` LLM 段 / QQ Bot 段——入口概览与命令示例
- `docs/sdd/llm/overview.md`——范围与非功能约束
- `docs/sdd/llm/data-model.md`——`Provider` / `Model` / `App` 值对象与不变式
- `docs/sdd/llm/interfaces.md`——实现权威（本文件若与之冲突以它为准）
- `docs/sdd/llm/decisions.md`——ADR-L14…L19 的决策背景
- `docs/sdd/llm/risks.md`——R1 / R11 / R13 / R14 等已知风险与验证方式
- `docs/sdd/qqbot/data-model.md`——`Bot` 值对象与不变式 Q0…Q7
- `docs/sdd/qqbot/interfaces.md`——QQ Bot 模块的实现权威
- `docs/sdd/qqbot/decisions.md`——ADR-Q1…Q13 的决策背景
- `docs/sdd/qqbot/risks.md`——R4 / R6 / R7 / R10 等已知风险
