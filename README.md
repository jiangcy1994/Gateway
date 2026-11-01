# top-gateway

顶层网关：把多个子网关按路径前缀挂载在同一入口之下，并提供 REST 管理 API。
Rust 实现（axum + tokio + reqwest），与 `qqbot-api-gateway` 同一技术栈。

```
client ──► top-gateway:8000
              ├─ /qqbot/*        ──► 127.0.0.1:8080  (qqbot-api-gateway)
              ├─ /qqbot-admin/*  ──► 127.0.0.1:8081  (管理后台)
              ├─ /llm/*          ──► 内嵌 LLM 网关（按协议选供应商，见下文）
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
| `GATEWAY_HEALTH_INTERVAL_SECS` | `10` | 健康检查轮询间隔 |
| `GATEWAY_HEALTH_TIMEOUT_SECS` | `2` | 单次健康探测超时 |
| `GATEWAY_DEFAULT_TIMEOUT_SECS` | `30` | 路由未指定时的上游超时 |
| `RUST_LOG` | `top_gateway=info` | 日志级别 |

## 路由配置（routes.toml）

```toml
[[routes]]
id = "qqbot"
prefix = "/qqbot"
upstream = "http://127.0.0.1:8080"
strip_prefix = true            # /qqbot/x -> /x
enabled = true
description = "QQ Bot API gateway"

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
- **`/llm` 为保留前缀**：`/llm/*` 由内嵌 LLM 网关处理（见下文），永远不会落
  到前缀路由；不要在 `routes.toml` 里挂 `/llm` 前缀。
- **流式转发**：请求/响应 body 逐块透传，不缓存，可代理大文件/流式接口。
- **请求头**：剥离 hop-by-hop 头与 `Content-Length`，其余原样透传（含
  `Authorization`，由子网关自行鉴权）；附加 `x-forwarded-host` / `x-forwarded-path`；
  路由配置的 `headers` 在最后注入，可用来给子网关塞 `x-gateway-key`。
- **错误 JSON**：`{"error":{"code":502,"message":"..."}}` 统一格式。

## LLM 网关（`/llm/{protocol}/*`）

内嵌子网关：同一入口下按协议段选择供应商池，客户端协议原样透传（不做协议
互转），按供应商的 RPM/TPM 剩余配额加权选择；配额按**整个供应商**（底下全
部模型合计）计算，60 秒滚动窗口。

支持的协议段：`openai`、`anthropic`、`systemone`（systemone 可路由可转发，
仅 usage 记账为桩，暂记 0）。凭证由网关按协议注入（openai/systemone 走
`Authorization: Bearer`，anthropic 走 `x-api-key`），客户端凭证一律被替换。

```bash
# 客户端照常调用（以 openai 协议为例）
curl http://127.0.0.1:8000/llm/openai/v1/chat/completions \
  -d '{"model":"gpt-4o","messages":[...]}'
# 响应头 x-llm-provider 指出本次选中的供应商
```

供应商在 `providers.toml` 配置（也可全程走管理 API）：

```toml
[[providers]]
id = "openai-official"
name = "OpenAI"
protocol = "openai"             # openai | anthropic | systemone
base_url = "https://api.openai.com"
api_key = "sk-..."
rpm_limit = 60                  # 每分钟请求数限额，缺省 = 不限
tpm_limit = 100000              # 每分钟 token 数限额，缺省 = 不限
enabled = true
```

LLM 管理端点（同样需要 `X-Admin-Key`）：

| 方法 | 路径 | 说明 |
|---|---|---|
| GET | `/__admin/llm/providers` | 列出全部供应商 |
| POST | `/__admin/llm/providers` | 新增供应商；`id` 省略时生成 UUID |
| GET/PUT/DELETE | `/__admin/llm/providers/{id}` | 查看 / 合并式更新 / 删除（PUT 显式 `null` = 设为不限） |
| GET | `/__admin/llm/quota` | 各供应商窗口内 RPM/TPM 用量与余量 |
| GET | `/__admin/llm/metrics` | 各供应商请求数 / 并发 / 错误数 / 平均延迟 |

选择行为：所有供应商满 RPM 或 TPM → `429`（`QuotaExhausted`）；协议下无启用
供应商 → `503`。TPM 由响应尾包里的 `usage` 字段事后计量（非流式取 JSON，
流式取末尾 `data:` 块），解析不到时记 0，永不失败。

## 持久化

每次经管理 API 的路由变更都会原子写回 `GATEWAY_ROUTES_FILE`（临时文件 +
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
  ├─ gateway.env       # KEY=VALUE，服务进程的唯一环境来源（install 时生成模板）
  └─ top-gateway.log   # 按日滚动（旧文件带日期后缀）
```

`gateway.env` 模板（**请填入真实密钥**——服务不读注册表，也不读用户环境变量）：

```
GATEWAY_ADMIN_KEY=change-me
GATEWAY_BIND=0.0.0.0:8000
# RUST_LOG=top_gateway=info
```

注意事项：

- 相对路径的 `GATEWAY_ROUTES_FILE` / `GATEWAY_PROVIDERS_FILE` 在服务模式下
  解析到服务基目录；控制台模式仍按 cwd 解析，行为不变。`GATEWAY_BASE_DIR`
  可覆盖基目录位置。
- 直接双击服务模式的 exe（无参数、未连上 SCM）会报 1063 错误并提示用
  `run` / `install`。
- 服务只接受 `SERVICE_CONTROL_STOP`，不接受系统关机通知——在关机时限内排空
  代理请求只会拖延关机（ADR-013）。
- `gateway.env` 里的 `GATEWAY_ADMIN_KEY` 是明文，和 `providers.toml` 里的
  `api_key` 一样（ADR-012 / ADR-L11）：依靠 %ProgramData% 的默认 ACL 限制
  访问，不要把密钥写进交给版本管理的模板。

