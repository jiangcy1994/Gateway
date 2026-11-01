# 数据模型

> **SDD 第 3 步**：核心类型与不变式。依赖第 2 步的模块划分——类型绝不超出架构边界。

top-gateway 的数据模型分三层：**配置值对象**（`Route`）、**运行期共享状态**（`RouteTable` / `AppState`）、**观测快照**（`HealthState` / `StatsSnapshot`）。

## 核心值对象：`Route`

一个挂载的子网关。同时是 TOML 文件、管理 API、内存路由表的共享表示——文件里能设的字段，API 里也能设，反之亦然。

| 字段 | 类型 | 约束 |
|---|---|---|
| `id` | `String` | 非空；路由表内唯一；`PUT` 不可改；POST 省略时自动生成 UUID v4 |
| `prefix` | `String` | 以 `/` 开头；非根前缀不能以 `/` 结尾；路由表内唯一 |
| `upstream` | `String` | `http://` 或 `https://` 且含 host |
| `strip_prefix` | `bool` | 默认 `true` |
| `enabled` | `bool` | 默认 `true`；`false` 时该前缀返回 503 |
| `description` | `String` | 默认空 |
| `headers` | `HashMap<String,String>` | 注入头；name 非空，value 必须是合法 header 值 |
| `timeout_secs` | `Option<u64>` | 为空时回落到 `GATEWAY_DEFAULT_TIMEOUT_SECS` |
| `health_path` | `String` | 以 `/` 开头；默认 `/health` |

### 不变式

| # | 不变式 | 由谁保证 |
|---|---|---|
| I1 | `prefix` 以 `/` 开头且（非 `/` 时）不以 `/` 结尾 | `Route::validate`（管理 API 与启动加载都会调） |
| I2 | `upstream` scheme ∈ {http, https} 且 host 非空 | `Route::validate` |
| I3 | 同一 `RouteTable` 内 `id` 唯一、`prefix` 唯一 | `RouteTable::add`/`replace`（写锁内检查） |
| I4 | `health_path` 以 `/` 开头 | `Route::validate` |
| I5 | `headers` 的 value 是合法 `HeaderValue` | `Route::validate`（不合法的注入头在代理层只告警并跳过） |

> 注意：`load_routes` 从磁盘读到重复 prefix 时只告警并全部保留（磁盘文件被手工改坏时的宽容策略）；经管理 API 写入则必然满足 I3。

## 运行期状态

### `RouteTable`（router.rs）

```rust
RouteTable { routes: Arc<RwLock<Vec<Route>>> }
```

- 无索引的线性表。`match_route` 取读锁做**最长前缀匹配**，克隆结果后释放锁。
- 匹配规则：`prefix == "/"` 匹配一切；否则 `path == prefix` 或 `path` 以 `prefix + "/"` 开头。

### `AppState`（state.rs）

```rust
AppState {
    settings: Settings,                 // 进程级配置，启动后不变
    client: reqwest::Client,            // 连接池共享
    routes: RouteTable,                 // 见上
    stats: Arc<RwLock<HashMap<String, Arc<RouteStats>>>>,
    health: Arc<RwLock<HashMap<String, HealthState>>>,
}
```

- `stats` map 的 key 是 route id；路由删除后其 stats 条目保留（历史计数不丢），但 `GET /__admin/metrics` 只 join 当前路由表，故不展示。
- `health` map 同理；`health_snapshot` 只输出当前仍挂载的路由，删过的路由不残留。

### `RouteStats`（metrics.rs）

```rust
RouteStats {
    total_requests: AtomicU64,
    in_flight:     AtomicI32,
    errors:        AtomicU64,
    total_latency_ms: AtomicU64,
}
```

- `in_flight` 用 `AtomicI32`（可为负的防御性选择）；快照序列化时转 `i64`。
- `avg_latency_ms` **不在运行期维护**，快照时用 `total_latency_ms / total_requests` 计算（无请求时为 0.0）。

## 观测快照（序列化形态）

### `HealthState`

| 字段 | 类型 | 说明 |
|---|---|---|
| `id` / `prefix` / `upstream` | `String` | 冗余自 `Route`，便于单条 JSON 可读 |
| `status` | `String` | `healthy` / `unhealthy` / `disabled` |
| `last_checked` | `Option<String>` | RFC3339 UTC；禁用路由为 `None` |
| `latency_ms` | `Option<u64>` | 仅 `healthy` 有值 |
| `last_error` | `Option<String>` | 仅 `unhealthy` 有值 |

### `StatsSnapshot`

`total_requests: u64`、`in_flight: i64`、`errors: u64`、`avg_latency_ms: f64`。

## 持久化形态

`RoutesFile { routes: Vec<Route> }` → `toml::to_string_pretty`，每个路由一个 `[[routes]]` 表。文件头部固定一行托管注释，**重写会丢弃文件中的其他注释**（原子替换：同目录临时文件 + rename）。

## 数据流上的形态

```
routes.toml ──load_routes──→ Vec<Route> ──set_all──→ RouteTable
                                   ▲                      │
                                   │                      ├─ match_route → 转发
                                   │                      ├─ add/replace/remove ← admin API
                                   └──── save_routes（原子写回）──┘
Route ──check_one──→ HealthState ──→ health map ──→ /__admin/health
代理热路径 ──原子计数──→ RouteStats ──快照──→ /__admin/metrics
```
