# top-gateway

顶层网关：把多个子网关按路径前缀挂载在同一入口之下，并提供 REST 管理 API。
Rust 实现（axum + tokio + reqwest），与 `qqbot-api-gateway` 同一技术栈。

```
client ──► top-gateway:8000
              ├─ /qqbot/*        ──► 127.0.0.1:8080  (qqbot-api-gateway)
              ├─ /qqbot-admin/*  ──► 127.0.0.1:8081  (管理后台)
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
- **流式转发**：请求/响应 body 逐块透传，不缓存，可代理大文件/流式接口。
- **请求头**：剥离 hop-by-hop 头与 `Content-Length`，其余原样透传（含
  `Authorization`，由子网关自行鉴权）；附加 `x-forwarded-host` / `x-forwarded-path`；
  路由配置的 `headers` 在最后注入，可用来给子网关塞 `x-gateway-key`。
- **错误 JSON**：`{"error":{"code":502,"message":"..."}}` 统一格式。

## 持久化

每次经管理 API 的路由变更都会原子写回 `GATEWAY_ROUTES_FILE`（临时文件 +
rename），重启不丢失；注意重写会丢弃文件里的注释。
