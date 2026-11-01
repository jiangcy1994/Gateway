# 接口契约

> **SDD 第 4 步**：签名与契约。依赖第 3 步的数据类型——签名里出现的类型必须先定义过。
>
> 本文件是**实现的唯一权威**。签名、前后置条件变化必须先改这里，再改代码；测试以本文件为准。

## lib.rs

### `build_app(state: AppState) -> Router`

组装整个应用。

- 前置：`state` 已加载路由（`run` 中完成；测试里手动 `set_all`）。
- 后置：`GET /health` 与 `/__admin/*` 由本网关处理，其余路径落到 `proxy` fallback。
- 错误：不抛（纯组装）。

### `run(settings: Settings) -> Result<(), Box<dyn Error + Send + Sync>>`

进程主体。

- 前置：`settings` 由 `Settings::from_env()` 构造。
- 后置：加载路由文件（缺失不致命，告警后以零路由启动）→ `health::spawn`（首轮立即执行）→ 绑定 `settings.bind` → 阻塞服务直到 Ctrl+C / SIGTERM，再优雅关闭。
- 错误：路由文件解析失败、客户端构建失败、绑定失败均向上传播。

---

## router.rs

### `Route`（结构体）

字段与不变式见 `data-model.md`。实现 `Serialize/Deserialize`（TOML 与 JSON 共用同一结构）。

### `Route::validate(&self) -> Result<(), String>`

- 前置：字段已填充（id/prefix/upstream/health_path/headers）。
- 后置：全部规则通过返回 `Ok(())`；任一失败返回 `Err(原因)`（管理 API 映射为 400）。
- 规则（顺序即错误优先级）：`id` 非空 → `prefix` 以 `/` 开头 → `prefix` 非 `/` 时结尾无 `/` → `upstream` scheme ∈ {http,https} → `upstream` host 非空 → `health_path` 以 `/` 开头 → 每个 header 名非空且 value 合法。

### `Route::apply_patch(&self, patch: RoutePatch) -> Route`

- 前置：`self` 为当前路由。
- 后置：`patch` 中 `Some` 的字段覆盖，`None` 的保留原值；**`id` 永远来自 `self`**（`patch` 无 id 字段）。
- 错误：不校验（调用方在 `replace` 前调 `validate`）。

### `RouteTable::{set_all, list, get, match_route, add, replace, remove}`

| 方法 | 签名 | 前置 | 后置 | 错误 |
|---|---|---|---|---|
| `set_all` | `(&self, Vec<Route>)` | 无 | 整表替换（启动加载用） | 不抛（`std::sync::PoisonError` 会 panic；本进程无手撕锁场景） |
| `list` | `(&self) -> Vec<Route>` | 无 | 全量克隆，表内顺序 | 不抛 |
| `get` | `(&self, id: &str) -> Option<Route>` | 无 | 按 id 查 | 不抛 |
| `match_route` | `(&self, path: &str) -> Option<Route>` | `path` 为 URL path | **最长匹配前缀**的路由；无则 `None` | 不抛 |
| `add` | `(&self, Route) -> Result<(), ApiError>` | route 已填字段 | 追加到表尾 | `DuplicateId` / `DuplicatePrefix`（均 400） |
| `replace` | `(&self, id: &str, Route) -> Result<(), ApiError>` | id 存在 | 原地替换，保持位置 | `RouteNotFound`（404）/ `DuplicatePrefix`（排除自身位置后比较） |
| `remove` | `(&self, id: &str) -> Option<Route>` | 无 | 移除并返回；`None` 表示不存在 | 不抛 |

`match_route` 匹配语义（契约核心）：

```
prefix == "/"            → 匹配一切 path
path == prefix           → 精确匹配
path.starts_with(prefix + "/")  → 前缀匹配
否则不匹配；多条命中时取 prefix 最长者
```

---

## proxy.rs

### `proxy(State<AppState>, Method, OriginalUri, HeaderMap, Request) -> Response`

fallback 处理器，所有非 `/health`、非 `/__admin/*` 请求都到这里。

- 前置：无。
- 后置（按判定顺序）：
  1. `match_route(path)` 为 `None` → `404`，体 `{"error":{"code":404,"message":"no route mounted for path {path}"}}`。
  2. `route.enabled == false` → `503`，消息含 route id。
  3. 取 `stats.guard()`（Drop 时记账，含早返回路径）。
  4. `strip_prefix == true`：`prefix` 之后的部分作为上游 path，若剩余不以 `/` 开头则补一个（`/qqbot` → `/`）；`false`：原 path 透传。
  5. 查询串原样拼接。
  6. 请求头：剥离 `HOP_BY_HOP`（connection/keep-alive/proxy-authenticate/proxy-authorization/te/trailer/transfer-encoding/upgrade）与 `content-length`；`host` 不转发，改为写 `x-forwarded-host`；附 `x-forwarded-path`（完整原始 path）；**`Authorization` 原样透传**。
  7. `route.headers` **最后注入**（`insert` 语义：覆盖客户端同名头）；非法头名/值只 `warn` 并跳过。
  8. `route.timeout_secs` 或 `default_timeout_secs` 包裹上游请求：传输错误 → `502`；超时 → `504`；两者都调 `record_error()`。
  9. 上游 5xx 响应：原样透传状态码与 body，同时计一次 error。
  10. 响应头同样剥离 hop-by-hop 与 `content-length`；body 以 `bytes_stream` 流式回传（不缓存）。
- 错误：除上述状态码外不产生其他响应。

### `error_response(status: StatusCode, message: &str) -> Response`

- 后置：体为 `{"error":{"code":<status as u16>,"message":<message>}}`。全项目统一错误格式。

---

## admin.rs

### 鉴权：`require_admin(&AppState, &HeaderMap) -> Result<(), ApiError>`

- 前置：无。
- 后置：`settings.admin_key == None` → 放行；否则取 `X-Admin-Key`，或 `Authorization: Bearer <key>`，与配置 key **常数时间**比较（长度不同先短路），相等放行。
- 错误：不匹配或未提供 → `ApiError::Unauthorized`（401），体消息固定 `"unauthorized: a valid X-Admin-Key is required"`。

### HTTP 端点（挂载于 `/__admin`）

| 方法 | 路径 | 鉴权 | 成功响应 | 错误 |
|---|---|---|---|---|
| GET | `/__admin/routes` | 需要 | `200 {"routes":[...]}` | 401 |
| POST | `/__admin/routes` | 需要 | `201` + 新路由 JSON | 400（非法/重复 id/重复 prefix）、401 |
| GET | `/__admin/routes/{id}` | 需要 | `200` + 单路由 | 404、401 |
| PUT | `/__admin/routes/{id}` | 需要 | `200` + 合并后的路由 | 400、404、401 |
| DELETE | `/__admin/routes/{id}` | 需要 | `200 {"deleted":<id>}` | 404、401 |
| GET | `/__admin/health` | 需要 | `200 {"routes":[HealthState...]}`（按路由表顺序 join，删过的路由不残留） | 401 |
| GET | `/__admin/metrics` | 需要 | `200 {"routes":[{"route":...,"stats":...}]}` | 401 |
| POST | `/__admin/probe` | 需要 | `200 {"status":"all routes healthy"}` | 500（首个不健康路由，消息含原因）、401 |
| GET | `/health` | **不需要** | `200 {"status":"ok"}` | — |

- `GET /__admin/routes/{id}` 的 `id` 是 URL path 段（`{id}` axum 语法），不做 URL 解码以外的处理。
- 未知 `/__admin/*` 路径返回 404（**不会被 fallback 代理**——`nest` 优先于 fallback）。
- 所有变更端点成功后先 `state.persist()` 再返回；持久化失败 → 500。

### `RouteInput`（POST body / PUT body 的反序列化目标）

| 字段 | POST | PUT |
|---|---|---|
| `id` | `Option`，省略 → UUID v4 | **不接受**（`RoutePatch` 无此字段） |
| `prefix` | 必填 | `Option` |
| `upstream` | 必填 | `Option` |
| `strip_prefix` / `enabled` | 默认 `true` | `Option` |
| `description` | 默认空 | `Option` |
| `headers` | 默认空 map | `Option` |
| `timeout_secs` | `Option<u64>` | `Option` |
| `health_path` | 默认 `/health` | `Option` |

- 前置（POST）：JSON 可反序列化且 `prefix`/`upstream` 非缺。
- 后置：`From<RouteInput> for Route` 填默认值；随后必须经 `validate()`。
- 错误：JSON 缺必填字段 → axum 直接 400（反序列化拒绝），消息为反序列化错误。

---

## config.rs

### `Settings::from_env() -> Settings`

| 环境变量 | 字段 | 默认 | 说明 |
|---|---|---|---|
| `GATEWAY_BIND` | `bind` | `127.0.0.1:8000` | 监听地址 |
| `GATEWAY_ADMIN_KEY` | `admin_key` | `None` | 未设置或空串 → 端点开放 + 启动告警 |
| `GATEWAY_ROUTES_FILE` | `routes_file` | `routes.toml` | 路由文件路径 |
| `GATEWAY_HEALTH_INTERVAL_SECS` | `health_interval_secs` | `10` | 非正数回落默认 |
| `GATEWAY_HEALTH_TIMEOUT_SECS` | `health_timeout_secs` | `2` | 单次探测超时 |
| `GATEWAY_DEFAULT_TIMEOUT_SECS` | `default_timeout_secs` | `30` | 路由无 `timeout_secs` 时使用 |

- 后置：`admin_key` 为空串时视为未设置（过滤）。

### `load_routes(path: &Path) -> Result<Vec<Route>, ApiError>`

- 前置：无。
- 后置：文件**不存在 → `Ok(vec![])` 并告警**（非致命）；解析成功返回 `Vec<Route>`。
- 错误：读取 IO 错误（非 NotFound）或 TOML 解析失败 → `ConfigLoad`（500）。重复 prefix 只告警、全部保留。

### `save_routes(path: &Path, routes: &[Route]) -> Result<(), ApiError>`

- 前置：`path` 的父目录存在且可写。
- 后置：pretty TOML + 固定头部注释；同目录临时文件写好后 `persist`（rename）原子替换。
- 错误：临时文件创建/写入/rename 任一失败 → `Persist`（500）。**成功则文件中既有注释全部丢失**（除托管头）。

---

## health.rs

### `spawn(state: AppState) -> JoinHandle<()>`

- 后置：独立任务循环 `check_all` → `sleep(interval)`，**首轮立即执行**。
- 错误：任务内 panic 仅终止该任务。

### `AppState::health_snapshot(&self) -> Vec<HealthState>`

- 后置：按当前 `routes.list()` 顺序 join `health` map；路由表里没有的条目不出现（删除的路由不留残影）。首轮探测完成前可能为空数组。

### `AppState::probe_all_now(&self) -> Result<(), ApiError>`

- 后置：同步逐个探测所有**启用**路由并写入 health map；首个非 healthy 立即返回 `Internal`（消息 `"route {id} unhealthy: {原因}"`），其余不再探测。
- 注意：探测是串行的，路由多时耗时 = 路由数 × health_timeout_secs。

---

## metrics.rs

### `RouteStats::guard(self: &Arc<Self>) -> InFlightGuard`

- 后置：`in_flight +1`；guard drop 时 `in_flight - 1`、`total_requests + 1`、`total_latency_ms += 经过毫秒`。**早返回也记账**（RAII）。

### `RouteStats::record_error(&self)`

- 后置：`errors +1`。代理在 502/504 与上游 5xx 时调用。

### `RouteStats::snapshot(&self) -> StatsSnapshot`

- 后置：四个原子量各读一次；`avg_latency_ms = total_requests == 0 ? 0.0 : total_latency_ms / total_requests`。

---

## error.rs

### `ApiError` → 状态码映射（契约）

| 变体 | HTTP 状态 | 触发场景 |
|---|---|---|
| `RouteNotFound(id)` | 404 | GET/PUT/DELETE 未知 id |
| `InvalidRoute(msg)` | 400 | `validate()` 失败 |
| `DuplicateId(id)` | 400 | POST 时 id 已存在 |
| `DuplicatePrefix(prefix)` | 400 | POST/PUT 时 prefix 已被其他路由占用 |
| `Unauthorized` | 401 | 密钥缺失或不对 |
| `ConfigLoad(ctx)` | 500 | 启动加载（`run` 中直接转字符串向上传播） |
| `Persist(ctx)` | 500 | 管理变更写盘失败 |
| `Internal(ctx)` | 500 | 客户端构建失败、`/probe` 探测失败 |

所有错误体统一为 `{"error":{"code":<u16>,"message":<thiserror 文本>}}`。

## 测试以本文件为准

`tests/integration.rs` 的每条断言对应上表某一条契约；新增或修改端点行为前，先改本文件对应条目，再改代码与测试。
