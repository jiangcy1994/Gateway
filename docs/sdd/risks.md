# 风险登记

> **SDD 第 6 步**：风险与缓解。评估对象是第 1-5 步定下来的系统。每条风险必须有「怎么验证」，否则只是焦虑清单。

每条含可能性、影响、缓解与验证方式。新增风险追加，不覆盖已有条目的历史评估。

## R1 慢上游耗尽连接池 / 内存

- 可能性：中（子网关处理慢或挂起时）
- 影响：`default_timeout_secs`（默认 30s）内每个在途请求都占一个 reqwest 连接与一个 tokio 任务；并发足够高时资源吃紧
- 缓解：每路由可设 `timeout_secs` 收紧；`InFlightGuard` 使 in_flight 可观测，可在监控里设阈值告警
- 验证：`tests/integration.rs` 的 `unreachable_upstream_returns_502` 固定 504/502 语义边界；`metrics_count_requests_and_errors` 验证 in_flight 在请求完成后回落到 0

## R2 本机回环连接被安全软件拖延（~2s 才拒绝）

- 可能性：高（已在本机观测，见 `../../CLAUDE.md` 与记忆 `loopback-refused-takes-2s`）
- 影响：探测/连接 refused 的路径上，若 `timeout_secs` ≤ 2s，会把 502（拒绝）错报成 504（超时）；健康检查也可能误判
- 缓解：需要断言 502 的测试用 `dropped_port()`（bind 后丢弃的临时端口）+ `timeout_secs=15`；生产环境对可能被过滤的端口不做假设
- 验证：本机已用独立 rustc 探针测得拒绝耗时 ~2.04s；`unreachable_upstream_returns_502` 在该配置下稳定为 502

## R3 原子重写丢失手工注释

- 可能性：必然发生（每次管理变更都重写文件）
- 影响：运维在 `routes.toml` 里写的注释被静默丢弃，可能导致「为什么这样配」的知识丢失
- 缓解：文件头托管注释明示「rewrites drop comments」；`description` 字段承担「为什么挂这个路由」的表达；建议用 `description` 而非注释记原因
- 验证：`README.md` 与 `routes.toml` 头部均声明该行为；`admin_crud_roundtrip_and_persistence` 断言 delete 后磁盘内容消失（即重写路径被走到）

## R4 管理端点开放（`GATEWAY_ADMIN_KEY` 未设置）

- 可能性：中（开发和首次部署时容易忘记设置）
- 影响：任何能访问 `GATEWAY_BIND` 的人可增删路由、查上游地址
- 缓解：启动时必发 `warn` 日志；默认绑定 `127.0.0.1`（非 `0.0.0.0`），外网默认不可达；与 `qqbot-api-gateway` 同策略
- 验证：`admin_without_key_configured_is_open` 固定「未设置即开放」的契约；启动日志含 `GATEWAY_ADMIN_KEY is not set`

## R5 路由表增删时存在读到的快照不一致窗口

- 可能性：低（每个处理器克隆匹配到的 `Route` 后即与表解耦）
- 影响：并发变更期间，已匹配但未发出请求使用旧 upstream；`replace` 期间极短窗口内两次读可能拿到新旧混合——单次请求内部始终一致（只有一个 `match_route`）
- 缓解：写操作只在管理 API（串行、变更后立即持久化）；进程内无其他写者
- 验证：`longest_matching_prefix_wins` 通过 per-route 计数验证「同一请求只命中一个路由」；`admin_crud_roundtrip_and_persistence` 验证变更即时生效

## R6 健康探测串行执行，路由多时 `/probe` 耗时长

- 可能性：中（挂载数十个上游且普遍慢时）
- 影响：`POST /__admin/probe` 的响应时间 ≈ 路由数 × `health_timeout_secs`，可能触发客户端自身超时
- 缓解：后台 `check_all` 同样串行但异步，不阻塞转发；运维感受 latency 可直接看 `/__admin/health` 的 `latency_ms`
- 验证：`probe_endpoint_reports_unhealthy_route` 验证首个不健康立即返回 500（契约见 `interfaces.md` 的 `probe_all_now`）

## R7 SDD 与代码漂移

- 可能性：中（接口改动时先改代码是人的自然倾向）
- 影响：契约文档失去权威性，实现者依据错误签名工作
- 缓解：`interfaces.md` 定为实现唯一权威；`docs/design.md` 维护声明要求「接口变更先改 interfaces.md 再改代码」；TDD 循环以契约为准写断言
- 验证：`cargo test`（17 条集成测试）+ `cargo clippy --all-targets` 保持零警告；人工评审任何 PR 时对照 `interfaces.md` 的状态码表与 `match_route` 契约
