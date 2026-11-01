# 架构

> **SDD 第 2 步**：模块划分与请求流程。依赖第 1 步划定的范围——范围外的能力（负载均衡、熔断）不成立模块。

## 模块依赖图

```
main.rs ──► lib.rs (run / build_app)
              ├─► admin.rs   ──► router.rs (Route, RouteTable, RoutePatch)
              ├─► proxy.rs   ──► router.rs
              ├─► health.rs  ──► router.rs, state.rs
              ├─► state.rs   ──► config.rs, router.rs, health.rs, metrics.rs
              ├─► config.rs  ──► router.rs (Route 的 TOML 序列化)
              ├─► metrics.rs
              └─► error.rs   （被除 main.rs 外所有模块引用）
```

依赖单向往下、无环：`proxy` 与 `admin` 都只读写 `router` 的路由表，互不直接调用。`error.rs` 是叶子模块。

## 请求流程（代理路径）

```
HTTP 请求
   │
   ▼
build_app 的 Router
   ├─ /health            → lib.rs::root_health → 200 {"status":"ok"}
   ├─ /__admin/*         → admin.rs（require_admin 鉴权后处理）
   └─ 其他（fallback）   → proxy.rs::proxy
        ├─ match_route(path) 找不到          → 404
        ├─ route.enabled == false            → 503
        ├─ 记 InFlightGuard（Drop 时统计）
        ├─ strip_prefix 决定转发路径
        ├─ 复制请求头（过滤 hop-by-hop/host/content-length）
        ├─ 注入 x-forwarded-host / x-forwarded-path
        ├─ 注入 route.headers（最后注入，可覆盖客户端同名头）
        ├─ tokio::time::timeout(send())
        │     ├─ 传输错误                   → 502（记 errors）
        │     └─ 超时                        → 504（记 errors）
        └─ 响应头过滤 + Body::from_stream    → 原样回传
```

## 请求流程（管理路径）

```
/__admin/*  →  require_admin
                ├─ settings.admin_key == None → 放行（启动时已告警）
                ├─ X-Admin-Key 或 Bearer 匹配（常数时间比较）→ 放行
                └─ 否则                        → 401
              │
   ├─ GET    /routes            → 路由表快照
   ├─ POST   /routes            → 校验 → add → persist → 201
   ├─ GET    /routes/{id}       → 单条或 404
   ├─ PUT    /routes/{id}       → apply_patch 合并 → 校验 → replace → persist
   ├─ DELETE /routes/{id}       → remove → persist
   ├─ GET    /health            → health_snapshot（与当前路由表 join）
   ├─ GET    /metrics           → 每路由 stats 快照
   └─ POST   /probe             → 同步探测全部启用路由，首个不健康 → 500
```

## 关键架构性质

1. **状态即 Arc**：`AppState` 所有字段都是 `Arc`（或 Arc 包的锁），`Clone` 廉价，处理器直接拿 `State<AppState>`，不需要再套一层 `Arc<AppState>`。
2. **读多写极少**：路由表是 `Vec<Route>` + `RwLock`；代理热路径只取读锁、克隆匹配到的 `Route` 后立即释放锁。挂载数十个网关时线性扫描最快（ADR-003）。
3. **持久化和内存一致性**：每个变更先改内存路由表、再原子写文件，文件写失败则管理 API 返回 500。重启时从文件重建。
4. **健康检查与转发解耦**：健康循环在独立 tokio 任务里按间隔跑，结果写 `health` map；代理不读它（转发与否不依赖健康状态，只影响 `/__admin/health` 的展示）。
5. **指标无锁**：代理热路径只触 `AtomicU64/I32`；`InFlightGuard` 靠 Drop 完成记账，早返回也漏不掉。

## 部署形态

单二进制（`cargo build --release` → `target/release/top-gateway`），环境变量配置，`routes.toml` 与二进制同目录（可由 `GATEWAY_ROUTES_FILE` 改）。无外部依赖。
