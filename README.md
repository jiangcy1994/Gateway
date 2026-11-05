# top-gateway

顶层网关：把多个子网关按路径前缀挂载在同一入口之下，并提供 REST 管理 API。
Rust 实现（axum + tokio + reqwest），与 `qqbot-api-gateway` 同一技术栈。

```
client ──► top-gateway:8000
              ├─ /qqbot/*        ──► 内嵌 QQ Bot 网关（OpenAPI 透传 / webhook 回调 / WS 客户端）
              ├─ /llm/*          ──► 内嵌 LLM 网关（/llm/v1/{端点} 或 /llm/{protocol}/...，见下文）
              ├─ /svc/*          ──► 127.0.0.1:9000  （routes.toml 里挂载的子网关）
              └─ /__admin/*      ──► 本网关管理 API（不转发）
```

## 快速开始

```bash
cargo run --release          # 监听 GATEWAY_BIND（默认 127.0.0.1:8000）
cargo test                   # 集成测试：mock 上游 + 完整 CRUD/持久化/健康/指标
cargo fmt && cargo clippy --all-targets
```

| 环境变量 | 默认值 | 说明 |
|---|---|---|
| `GATEWAY_BIND` | `127.0.0.1:8000` | 监听地址 |
| `GATEWAY_ADMIN_KEY` | 未设置 | `/__admin/*` 的密钥；未设置时告警且放行 |
| `GATEWAY_ROUTES_FILE` | `routes.toml` | 路由配置文件 |
| `GATEWAY_PROVIDERS_FILE` | `providers.toml` | LLM 供应商配置文件 |
| `GATEWAY_APPS_FILE` | `apps.toml` | LLM 应用（模型组合）配置文件 |
| `GATEWAY_BOTS_FILE` | `bots.toml` | QQ 机器人配置文件 |
| `GATEWAY_HEALTH_INTERVAL_SECS` | `10` | 健康检查轮询间隔 |
| `GATEWAY_HEALTH_TIMEOUT_SECS` | `2` | 单次健康探测超时 |
| `GATEWAY_DEFAULT_TIMEOUT_SECS` | `30` | 路由未指定时的上游超时 |
| `RUST_LOG` | `top_gateway=info` | 日志级别 |

## 路由配置（routes.toml）

```toml
[[routes]]
id = "svc"
prefix = "/svc"
upstream = "http://127.0.0.1:9000"
strip_prefix = true            # /svc/x -> /x
enabled = true
description = "an external sub-gateway mounted by prefix"

[routes.headers]               # 注入到每个转发请求，覆盖客户端同名头
x-gateway-key = "..."
```

## 管理 API（/__admin/*）

前缀刻意不用 `/admin`——子网关通常自己占用该路径。所有端点需要
`X-Admin-Key`（或 `Authorization: Bearer <key>`）。

| 方法 | 路径 | 说明 |
|---|---|---|
| GET | `/__admin/routes` | 列出全部路由（配置视图） |
| POST | `/__admin/routes` | 新增路由，立即生效；`id` 省略时自动生成 |
| GET | `/__admin/routes/{id}` | 查看单个路由 |
| PUT | `/__admin/routes/{id}` | 合并式更新，只改传入字段，其余保留 |
| DELETE | `/__admin/routes/{id}` | 删除路由 |
| GET | `/__admin/health` | 各上游健康状态（healthy/unhealthy/disabled） |
| GET | `/__admin/metrics` | 各路由请求数 / 并发 / 错误数 / 平均延迟 |
| POST | `/__admin/probe` | 立即探测全部上游一次 |
| GET | `/__admin/version` | 网关版本号（编译期取自 `Cargo.toml`） |
| GET | `/health` | 本网关自身健康检查（无需鉴权） |

示例：

```bash
# 挂载一个子网关（变更立即生效并写回 routes.toml）
curl -X POST http://127.0.0.1:8000/__admin/routes \
  -H "X-Admin-Key: $GATEWAY_ADMIN_KEY" \
  -H "Content-Type: application/json" \
  -d '{"prefix":"/svc","upstream":"http://127.0.0.1:9000"}'

# 禁用
curl -X PUT http://127.0.0.1:8000/__admin/routes/svc \
  -H "X-Admin-Key: $GATEWAY_ADMIN_KEY" -d '{"enabled":false}'
```

## 转发行为

- **最长前缀匹配**：`/echo/special` 同时匹配 `/echo` 与 `/echo/special` 时取后者。
  未匹配任何前缀返回 `404`，路由被禁用返回 `503`，上游不可达 `502`，超时 `504`。
- **`/llm` 与 `/qqbot` 为保留前缀**：这两个前缀由内嵌子网关处理（见下文），永远不会
  落到前缀路由；不要在 `routes.toml` 里挂 `/llm` 或 `/qqbot` 前缀。
- **流式转发**：请求/响应 body 逐块透传，不缓存，可代理大文件/流式接口。
- **请求头**：剥离 hop-by-hop 头与 `Content-Length`，其余原样透传（含
  `Authorization`，由子网关自行鉴权）；附加 `x-forwarded-host` / `x-forwarded-path`；
  路由配置的 `headers` 在最后注入，可用来给子网关塞 `x-gateway-key`。
- **错误 JSON**：`{"error":{"code":502,"message":"..."}}` 统一格式。

## LLM 网关（`/llm/{protocol}/*`）

内嵌子网关：同一入口下按**协议 + 模型**选择供应商池，客户端协议原样透传
（不做协议互转）；请求带 `model` 时按「供应商-模型」条目的 `priority` **分层**
——高优先级层先用，其配额耗尽才降层，层内按 RPM/TPM 剩余配额加权随机。
配额按**整个供应商**（底下全部模型合计）计算，60 秒滚动窗口。两种入站形态：

- **`/llm/v1/{端点}`**：SDK 的 base_url 直接指 `http://gw:8000/llm/v1`，网关按
  端点名推断协议（`chat/completions` 等 → openai，`messages` 等 → anthropic），
  两家 SDK 无需任何配置即可接入。
- **`/llm/{protocol}/rest`**：显式协议段，无歧义场景的逃生口。两协议共用的
  端点（`models`、`files`）与未知端点在 `/llm/v1` 下返回 404 并提示改用此形态。

一个供应商可以同时侍奉多个协议（`protocols = ["openai", "anthropic"]`，常见于
共享网关型上游）：它同时进入两个协议池，但 RPM/TPM 配额只按供应商记一份。

支持的协议段：`openai`、`anthropic`、`systemone`（systemone 仅可经显式协议段
访问，可路由可转发，仅 usage 记账为桩，暂记 0）。凭证由网关按协议注入
（openai/systemone 走 `Authorization: Bearer`，anthropic 走 `x-api-key`），
客户端凭证一律被替换。

```bash
# OpenAI / Anthropic SDK 直接指 /llm/v1（按端点推断协议）
curl http://127.0.0.1:8000/llm/v1/chat/completions -d '{"model":"chat-pro",...}'
curl http://127.0.0.1:8000/llm/v1/messages -d '{"model":"chat-max",...}'

# 显式协议段（无歧义；v1 下推断不了的端点用这个）
curl http://127.0.0.1:8000/llm/openai/v1/chat/completions \
  -d '{"model":"chat-pro","messages":[...]}'

# 响应头 x-llm-provider 指出本次选中的供应商
```

供应商在 `providers.toml` 配置（也可全程走管理 API）：

```toml
[[providers]]
id = "primary-upstream"
name = "主用上游"
protocols = ["openai"]           # openai | anthropic | systemone；可多选共享配额
base_url = "https://api.example.com"
api_key = "sk-..."
rpm_limit = 60                   # 每分钟请求数限额，缺省 = 不限
tpm_limit = 100000               # 每分钟 token 数限额，缺省 = 不限
timeout_secs = 30                # 该供应商的上游超时，缺省 = GATEWAY_DEFAULT_TIMEOUT_SECS
enabled = true

  [[providers.models]]           # 模型映射表，缺省 = 空表（协议级兜底，见下）
  name = "chat-pro"              # 网关侧模型名：客户端请求里写它（app 组合里也写它）
  upstream = "chat-pro-2026-04"  # 上游真实模型名，缺省 = name（相同时零差异透传）
  priority = 10                  # 越大越优先；缺省 = 0
```

**模型映射**：客户端始终写网关侧模型名，网关在转发前把请求 body 里的 `model`
字段改写成上游真实名（上游响应原样回吐，不做反向改写）。同一条网关模型名可
以被多个供应商声明（各配不同 `upstream` 与 `priority`）：请求按 `priority`
降序分层，高优先级层有剩余配额就不会落到低层。**空模型表 = 协议级兜底**：
该供应商参与任意 model 请求的选择、不改写 `model` 字段，但 effective priority
为 `i32::MIN`——只要有一个供应商显式声明了该模型，兜底就永远排在最后。这让
不带模型表的旧部署继续工作，也给了渐进迁移路径。

**应用组合**（`apps.toml`）：入站凭证即应用身份——客户端把网关签发的 key 填在
SDK 原本填供应商 key 的位置（`Authorization: Bearer <key>` 或 `x-api-key`），
网关据此识别来源 app 并限制其可用模型集合；同时客户端凭证一律被替换成选中
供应商的凭证。

> 完整示例配置（按真实拓扑示范模型映射、分层兜底、按供应商超时与应用组合）
> 见 [examples/](examples/)——示例文件由 `llm_example_configs_parse_and_validate`
> 钉住解析与 `validate`，不会随代码演进腐坏。

```toml
[[apps]]
id = "cli-code"                  # 省略时生成 UUID；显式空串/纯白空白 = 400
name = "Claude Code CLI"         # 展示名，缺省空串
key = "sk-gw-cli-code-xxxx"      # 省略时生成 sk-gw-<uuid v4 simple>；表内唯一
models = ["chat-pro", "chat-max"]        # 空表 = 不限制
enabled = true                   # false 时该 key 得 403
```

凭证语义：**无凭证 = 默认策略放行**（全部已映射模型）；有凭证但不命中任何
app → `401`（fail-closed：瞎写的 key 不能换来权限）；命中但请求 model 不在
组合内 → `403`。key 比较用常量时间。注意组合限制只覆盖 body 里带 `model`
字段的 JSON 请求——无 model / 非 JSON 的请求不拦截，按协议级路由放行
（已知盲区，见 `docs/sdd/llm/risks.md`）；超出 16 MiB 缓冲上限的请求直接
`413` 拒绝，不进入盲区。

LLM 管理端点（同样需要 `X-Admin-Key`）：

| 方法 | 路径 | 说明 |
|---|---|---|
| GET | `/__admin/llm/providers` | 列出全部供应商 |
| POST | `/__admin/llm/providers` | 新增供应商；`id` 省略时生成 UUID |
| GET/PUT/DELETE | `/__admin/llm/providers/{id}` | 查看 / 合并式更新 / 删除（PUT 显式 `null` = 设为不限） |
| GET | `/__admin/llm/apps` | 列出全部应用（含 key） |
| POST | `/__admin/llm/apps` | 新增应用；`id`/`key` 省略时生成 |
| GET/PUT/DELETE | `/__admin/llm/apps/{id}` | 查看 / 合并式更新 / 删除 |
| GET | `/__admin/llm/quota` | 各供应商窗口内 RPM/TPM 用量与余量 |
| GET | `/__admin/llm/metrics` | 各供应商请求数 / 并发 / 错误数 / 平均延迟 |

选择行为：带 `model` 时，无任何供应商声明该模型 → `404`（`ModelNotFound`，
提示去 `/__admin/llm/providers` 登记模型表）；模型有声明但池空（全 disabled /
协议不匹配）→ `503`（`NoProviderForModel`）；所有层都满 RPM 或 TPM → `429`
（`QuotaExhausted`）；不带 `model` 时维持原语义——协议下无启用供应商 → `503`。
`/llm/v1` 下两协议共用或未登记的端点 → `404`。TPM 由响应尾包里的 `usage`
字段事后计量（非流式取 JSON，流式取末尾 `data:` 块），解析不到时记 0，永不失败。

## QQ Bot 网关（`/qqbot/*`）

内嵌子网关，三路接入同一机器人配置：

1. **OpenAPI 透传**（`/qqbot/{app_id}/...`）：路径首段是平台 `app_id`，其余部分
   拼到 `api_base`。网关自动换发 `access_token`（`POST /app/getAppAccessToken`），
   缓存到过期前 5 分钟、临近过期冷却 30s 防惊群，注入
   `Authorization: QQBot <token>`——客户端凭证一律被替换。响应头附
   `x-qqbot-bot: <bot id>` 指明命中配置。
2. **Webhook 回调**（`/qqbot/callback/{app_id}`，POST）：平台推 `op == 13`
   回调地址验证时，按 `app_secret` 做 ed25519 签名回 `{"plain_token","signature"}`
   （只签不验：入站事件签名的平台公钥不在本地参考文档内）；其他事件**先 ACK**
   `{"op":12}` 再异步转发到 `event_url`（带 `Authorization: Bearer <event_secret>`，
   5s 超时，失败只计错误不重试、不向平台暴露）。
3. **WebSocket 网关客户端**：`event_source = "websocket"` 的机器人启动后台
   supervisor——Identify → READY → 周期心跳（op 1/11）→ 事件投递到 `event_url`；
   断线按平台关闭码分流（4008/4009 可 Resume 重连、4900 段重新 Identify、
   4010 段停止不重连、其余保留 session 重连），指数退避 2s→60s。`GET /__admin/qqbot/ws`
   可看每个连接的 session/心跳/重连状态。

```bash
# 客户端照常调用平台 OpenAPI（app_id 由网关路由到对应凭证）
curl http://127.0.0.1:8000/qqbot/1020304050/users/@me
```

机器人在 `bots.toml` 配置（也可全程走管理 API）：

```toml
[[bots]]
id = "my-bot"                     # 配置 id（TOML 必填；仅管理 API 可省略生成 UUID）
name = "我的机器人"
app_id = "102030405"              # 平台 appId（路由键，全表唯一）
app_secret = "DG5g3B4j9X2KOErG"   # 平台 clientSecret，明文落盘（同 api_key 策略）
sandbox = false                   # true → 沙箱环境 api/ws 地址
intents = 100663296               # 缺省 (1<<25)|(1<<26)
# event_source = "websocket"      # webhook（缺省）| websocket
# event_url = "http://127.0.0.1:9000/events"   # 事件投递地址
# event_secret = "..."                         # 投递时附 Bearer
# enabled = true
```

> 完整示例配置（三通道形态、沙箱切换、逐机器人 URL 覆盖与临时停用）见
> [examples/](examples/)——示例文件由 `qqbot_example_configs_parse_and_validate`
> 钉住解析与 `validate`，不会随代码演进腐坏。

QQ Bot 管理端点（同样需要 `X-Admin-Key`）：

| 方法 | 路径 | 说明 |
|---|---|---|
| GET | `/__admin/qqbot/bots` | 列出全部机器人 |
| POST | `/__admin/qqbot/bots` | 新增机器人；`id` 省略时生成 UUID |
| GET/PUT/DELETE | `/__admin/qqbot/bots/{id}` | 查看 / 合并式更新 / 删除（PUT 显式 `null` = 清除该覆盖） |
| GET | `/__admin/qqbot/ws` | 各 WS supervisor 的连接 / session / 心跳状态 |
| GET | `/__admin/qqbot/metrics` | 各机器人的请求数 / 并发 / 错误数 / 平均延迟 |

错误语义：未知 `app_id` → `404`；机器人被禁用 → 转发 `503`、回调统一 `404`
（回调地址不应再可达）；换 token 失败 / 上游不可达 → `502`；上游超时 → `504`。

## 持久化

每次经管理 API 的路由 / 供应商 / 应用 / 机器人变更都会原子写回对应文件
（`routes.toml` / `providers.toml` / `apps.toml` / `bots.toml`，临时文件 +
rename），重启不丢失；注意重写会丢弃文件里的注释。

## 部署为 Windows 服务

同一份二进制既是控制台程序也是服务程序：SCM 不带参数启动它即进入服务模式，
`install / start / stop / status / uninstall` 是管理控制台。全部命令需要
管理员权限。

```powershell
.\target\release\top-gateway.exe install            # 注册为按需启动的服务
.\target\release\top-gateway.exe install --autostart # 开机自启
.\target\release\top-gateway.exe start
.\target\release\top-gateway.exe status              # 打印状态；退出码 0 = 运行中
.\target\release\top-gateway.exe stop                # 等待在途连接排空（最长 30s）
.\target\release\top-gateway.exe uninstall           # 需先停止；配置文件保留
.\target\release\top-gateway.exe run                 # 前台运行（等同非 Windows 构建）
```

服务以 LocalSystem 身份运行，工作目录是 `C:\Windows\System32`，也不继承用户
环境变量，因此配置与日志固定在服务基目录，而不是 cwd：

```
%ProgramData%\top-gateway\
  ├─ routes.toml       # install 时从 exe 同目录补种缺失文件，之后由管理 API 维护
  ├─ providers.toml
  ├─ apps.toml
  ├─ bots.toml
  ├─ gateway.env       # KEY=VALUE，服务进程的唯一环境来源（install 时生成模板）
  └─ top-gateway.log.YYYY-MM-DD   # 按日滚动（当前文件即带日期后缀）
```

`gateway.env` 模板（**请填入真实密钥**——服务不读注册表，也不读用户环境变量）：

```
GATEWAY_ADMIN_KEY=change-me
GATEWAY_BIND=0.0.0.0:8000
# RUST_LOG=top_gateway=info
```

注意事项：

- 相对路径的 `GATEWAY_ROUTES_FILE` / `GATEWAY_PROVIDERS_FILE` / `GATEWAY_APPS_FILE`
  / `GATEWAY_BOTS_FILE` 在服务模式下解析到服务基目录；控制台模式仍按 cwd
  解析，行为不变。`GATEWAY_BASE_DIR` 可覆盖基目录位置。
- 直接双击服务模式的 exe（无参数、未连上 SCM）会报 1063 错误并提示用
  `run` / `install`。
- 服务只接受 `SERVICE_CONTROL_STOP`，不接受系统关机通知——在关机时限内排空
  代理请求只会拖延关机（ADR-013）。
- `gateway.env` 里的 `GATEWAY_ADMIN_KEY` 是明文，和 `providers.toml` 里的
  `api_key`、`apps.toml` 里的 `key`、`bots.toml` 里的 `app_secret` 一样
  （ADR-012 / ADR-L11 / ADR-L18 / ADR-Q11）：依靠 %ProgramData% 的默认 ACL
  限制访问，不要把密钥写进交给版本管理的模板。
  另注意 `/qqbot/*` 本身不做客户端鉴权（`/llm/*` 同理）——部署侧绑定回环或前置反代。

