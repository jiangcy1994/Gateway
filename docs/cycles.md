# TDD 循环记录

top-gateway 的功能按 `sdd/architecture.md` 的依赖底向上实现：`router`（无依赖）→ `config`/`metrics`/`error` → `state` → `proxy`/`admin`/`health` → `lib`（组装）。每条循环先写测试、看到失败、再写最小实现。

> 记录说明：v0.1 的循环按集成测试的失败形态整理为标准记录，作为后续开发的**范式**；v0.1 实际会话中额外出现的红灯（均已在当时修复并保留对应测试）：
> - 大量 `State<Arc<AppState>>` 与 `Arc<AppState>` 的类型不匹配（E0308/E0599），最终重构为「全 Arc 字段 + derive Clone」的 `AppState`（ADR-007），所有处理器签名统一为 `State<AppState>`
> - 测试里用 `axum::extract::Method`（不存在）→ 改 `axum::http::Method`
> - mock 上游 `/health` 闭包直接返回 `serde_json::Value`（未实现 `IntoResponse`）→ 包 `Json(...)`
> - `health_marks_enabled_route_healthy_and_disabled_separately` 在首轮探测前用 `by_id["echo"]` 索引空 HashMap 而 panic → 改为 `let Some(echo) = by_id.get(...) else { continue; }` 的重试模式
>
> C1-C15 中标 [红] 的条目是当会话真实出现过的失败形态；未标 [红] 的是绿灯直接通过的钉子测试，保留以固化已决定行为。

---

### C1 路由表：最长前缀匹配（→ `match_route` 契约）
- 红[红]：`longest_matching_prefix_wins` —— `/echo/special/deep` 同时匹配 `/echo` 与 `/echo/special`，初始实现返回第一条命中，计数出现在错误路由上。
- 绿：`match_route` 遍历时记录 `best`，仅当新命中的 prefix 更长才替换。
- 重构：用 `best.is_none_or(|b| route.prefix.len() > b.prefix.len())` 取代手写 if 链。

### C2 代理：未匹配路径返回 404（→ interfaces 第 1 步）
- 红[红]：`unmatched_path_returns_404` —— 无路由表时 fallback 无响应形态；补 `error_response` 统一 JSON 错误体后通过。
- 绿：`proxy` 开头 `match_route` 为 `None` → `error_response(404, ...)`，消息含原始 path。
- 重构：抽 `error_response(status, message)` 为 `pub fn`，`error.rs` 的 `ApiError` 复用同一 body 形态。

### C3 代理：禁用路由返回 503
- 红[红]：`disabled_route_returns_503` —— 早期实现不检查 `enabled`，请求照常转发到上游并返回 200。
- 绿：匹配后先判 `!route.enabled` → 503，消息含 route id。
- 重构：无（与 404 共用 `error_response`）。

### C4 代理：剥前缀、保查询串、注入路由头
- 红[红]：`proxy_forwards_stripped_path_query_and_injected_headers` —— 断言 `path == "/status"`，首版把完整 `/echo/status` 透传；`x-gateway-key` 亦未到达上游。
- 绿：`strip_prefix` 为真时裁掉 `route.prefix`（剩余不以 `/` 开头时补 `/`）；查询串原样拼接；`route.headers` 最后 insert。
- 重构：把「剥前缀」抽成 `match route.strip_prefix` 表达式，顺带覆盖 `strip_prefix=false` 的分支（由 `strip_prefix_false_keeps_full_path` 钉住）。

### C5 代理：Authorization 原样透传（→ ADR 范围声明）
- 红[红]：`proxy_passes_client_authorization_through` —— 首版请求头过滤把 `authorization` 一并剥掉（为管理 API 的鉴权头着想）。
- 绿：剥离名单只含 hop-by-hop 与 `content-length`，`Authorization` 透传，子网关自行鉴权。
- 重构：剥离逻辑抽 `is_filtered(name)` 单一谓词，请求/响应两个方向共用。

### C6 代理：POST body 与方法透传
- 红[红]：`proxy_forwards_post_body_and_method` —— 首版 `reqwest::Body::default()` 导致上游收到空 body。
- 绿：`request.into_body().into_data_stream()` 包成 `wrap_stream`，方法名逐字节转换。
- 重构：无（为 ADR-002 的流式转发打基础）。

### C7 代理：不可达上游 502、超时 504（→ R2 的起点）
- 红[红]：`unreachable_upstream_returns_502` —— **三度失败**：先 504（超时 2s 太短）、再 504（换 `:1` 仍被过滤而非拒绝）、最后用独立 rustc 探针测出本机回环 **refused 耗时 ~2.04s**（安全软件拖延）。
- 绿：用 `dropped_port()`（bind 后丢弃的临时端口）保证「拒绝」语义，`timeout_secs` 提到 15。
- 重构：`dropped_port()` 抽成测试 helper；把该机器特性写入 CLAUDE.md 与记忆，避免下次重踩。

### C8 管理 API：CRUD 全流程 + 持久化
- 红[红]：`admin_crud_roundtrip_and_persistence` —— 首版缺密钥时全 401；`PUT` 用全量替换把未传字段清空；delete 后 `routes.toml` 内容未变。
- 绿：`require_admin` 支持 `X-Admin-Key` 与 `Bearer`；`RoutePatch` 全 `Option` 合并；每个变更端点调 `state.persist()`。
- 重构：鉴权抽 `constant_time_eq`，密钥比较防时序侧信道；`RouteInput`/`RoutePatch` 拆成两个结构（POST 必填、PUT 合并）。

### C9 管理 API：认证拒绝与开放两种形态
- 红[红]：`admin_without_key_configured_is_open` + 401 三连 —— 未配置 key 时应放行（启动告警），错误 key 应 401。
- 绿：`require_admin` 在 `admin_key == None` 时直接 `Ok(())`。
- 重构：无。

### C10 配置：TOML 文件在启动加载
- 红[红]：`routes_file_loads_at_startup` —— 早期 `run` 里 `AppState::new` 后直接空表启动。
- 绿：`run` 先 `load_routes` 再 `set_all`；`load_routes` 对缺失文件返回空 `Vec` + warn（非致命）。
- 重构：`spawn_gateway`（测试 helper）统一走「save → load → set_all」完整往返，杜绝路由状态与文件不一致（见 C14 的 harness bug）。

### C11 健康检查：启用/禁用分开上报
- 红[红]：`health_marks_enabled_route_healthy_and_disabled_separately` —— 首版首轮探测在 sleep 一个 interval 后才跑，测试 15s 内拿不到 `healthy`；禁用路由被当成 unhealthy 而非 `disabled`。
- 绿：`spawn` 先 `check_all` 再 sleep；`check_all` 对 `!enabled` 直接产出 `status="disabled"`、`last_checked=None`。
- 重构：`health_snapshot` 与当前路由表 join，删过的路由不留残影。

### C12 健康检查：`/probe` 立即探测并报错
- 红[红]：`probe_endpoint_reports_unhealthy_route` —— 首版无论上游死活都返回 200。
- 绿：`probe_all_now` 串行探测启用路由，首个非 healthy → `ApiError::Internal` → 500。
- 重构：无（与后台循环共用 `check_one`）。

### C13 指标：计数、并发、错误、平均延迟
- 红[红]：`metrics_count_requests_and_errors` —— 首版 `in_flight` 在请求结束后仍是 1（早返回路径漏减）。
- 绿：`InFlightGuard` 的 Drop 统一完成 `in_flight-1`、`total+1`、`latency+=`；`avg_latency_ms` 快照时计算。
- 重构：guard 模式替代手写「函数尾部的记账」（ADR-009）。

### C14 集成 harness：测试自身的状态加载 bug
- 红[红]：一次性 10 条测试失败 —— `spawn_gateway(routes, persistent=true)` 只把路由写进文件，从未 `set_all` 进 state，导致所有依赖具体路由的测试拿到 404。
- 绿：`spawn_gateway` 无条件执行完整 save → `config::load_routes` → `set_all` 往返（与 `run` 启动路径一致）。
- 重构：`persistent` 参数因此失去意义，改为 `_persistent` 保留签名；`Gateway` 结构体持有 `TempDir` 防止路由文件被提前清理。

### C15 钉子：未知 `/__admin/*` 不被代理
- 红：`unknown_admin_route_is_404_not_proxied` —— 钉住「nest 内未匹配路径返回 404，不落到代理 fallback」这一**已决定**行为，防止后续把管理路径泄漏给上游。
- 绿：无新实现（axum `nest` 语义即如此，测试即规格，直接通过）。
- 重构：无。这条循环的意义恰是「绿灯也是信息」。

---

## 循环与契约的对应

每条循环的断言都落在 `../sdd/interfaces.md` 的具体条目上（如 C7 → 502/504 状态码表，C8 → 管理端点表，C11 → `health_snapshot` 后置条件）。新增功能前先改 interfaces.md，再按上述范式开新循环。

---

## LLM 子网关（`src/llm/`，`tests/llm_gateway.rs`）

按 `sdd/llm/interfaces.md`（实现权威）实现：`provider`（共享表示）→ `quota`（计量）→ `usage`（提取）→ `balancer`（选择）→ `admin/forward`（端点）→ `state`/`config`/`lib`（装配）。依赖底向上、每条循环先改规格再写测试。

### C16 编译期红灯：模块与装配契约不存在（→ `lib.rs` 集成）
- 红[红]：23 条测试一次性产出 **11 条编译错误**——`Settings` 无 `providers_file`、`AppState` 无 `providers`、`llm` 模块不存在、测试里 `Route` 字面量缺 `description`/`timeout_secs`。这是「API 尚不存在」时的合法红灯形态（等价于运行期 FAIL）。
- 绿：按规格补 `llm` 模块与 `Settings`/`AppState` 字段；测试里补齐 `Route` 字面量。
- 重构：`build_app` 在 admin nest 与 fallback 之间插入 `nest("/llm", llm::llm_router())`，依赖 axum nest 优先于 fallback 保证保留前缀（ADR-L5）。

### C17 协议段路由与凭证注入（→ `llm/forward` 后置第 1/6 步）
- 红[红]：`llm_forwards_openai_path_and_bearer_key` —— 首版路径拼接把 `/llm` 段再次透传给上游；客户端 `Authorization` 未被替换成供应商凭证。
- 绿：剥 `/llm` 后按首个 `/` 切出协议段（空/未知 → 404 house 体），凭证按协议注入（openai/systemone → `Authorization: Bearer`；anthropic → `x-api-key` + 缺省才补的 `anthropic-version`）。
- 重构：剥离集合（hop-by-hop + content-length + host + authorization + x-api-key）复用 `proxy::is_filtered` 并追加三个名字，单一谓词两个方向共用。

### C18 加权选择：剩余配额加权 + 全满 429（→ `select_and_record` 后置）
- 红[红]：随机性下不能断言「分布」，首轮用「两个供应商各 rpm_limit=1」的边界形态：耗尽者权重 0 被排除、第三个供应商仍可选中（2×200 且 provider 不同 + 一次 429），替代分布断言。
- 绿：`weight = min(rpm_ratio, tpm_ratio)`（不限维度 = 1.0），权重 0 排除，全 0 → `QuotaExhausted`（429）。
- 重构：选择与 RPM 记账合并进**同一临界区**，按 `ProviderTable` 顺序一次性加锁全部 tracker（无 await、固定锁序），杜绝并发超额选择与死锁。

### C19 TPM 事后计量：usage 提取不缓冲（→ `extract_tokens` 契约）
- 红[红]：`llm_counts_tokens_from_streaming_final_chunk` —— 首版在转发前 `await bytes()` 收集整包，流式语义被破坏（客户端收不到逐块输出）。
- 绿：`UsageTap<S>` 手写 `Stream`，每块转发同时复制进 64 KiB `TailBuffer`；流结束 / 上游出错 / 消费者断开（Drop）三种情形都触发一次 `extract_tokens → record_tokens`（`done` 标志互斥）。
- 重构：借用排版事故两次：guard 借了临时 Arc（先收集 `Vec<Arc<QuotaTracker>>` 再加锁）与 `bytes_stream()` 移动 `upstream` 后再读其 headers（header 过滤循环提前到构造 tap 之前）。

### C20 管理端点挂载：12 条测试一次性失败（→ `llm/admin` 端点表）
- 红[红]：21 pass / 12 fail——quota/metrics/admin 全部 404，因为 `llm::admin::admin_router()` 从未挂进 `admin_router()`。
- 绿：`admin_router()` 尾部 `.nest("/llm", ...)`，与路由管理端点共用同一 `require_admin`。
- 重构：无（bug 纯属漏挂，测试即定位器）。

### C21 PUT 合并语义：显式 `null` 与缺省必须分开（→ `ProviderPatch` 契约）
- 红[红]：`llm_admin_provider_crud_and_persistence` —— `PUT {"rpm_limit": null}` 后 `rpm_limit` 仍是 60：serde 内置行为把 `null` 与字段缺省都映射成外层 `None`（保留原值），与规格「显式 `null` = 设为不限」冲突。
- 绿：`deserialize_some` 反序列化器（`#[serde(default, deserialize_with = ...)]`）：缺省走 default 仍是 `None`，`null` 变 `Some(None)`，数字变 `Some(Some(n))`。
- 重构：该配方提升为 `provider.rs` 的共有函数，注释说明 serde 行为差异（同 `RoutePatch` 的合并语义原则，但限额字段需要双层 Option）。

### C22 钉子：缺必填字段 422（→ `llm/admin` 端点表注）
- 红[红]：`llm_admin_rejects_invalid_provider` 期待 400 实得 **422**——axum 0.8 的 `Json` 提取器对缺必填字段返 422（与既有路由 API 同行为），`400` 只由 `validate()` 规则失败产生。
- 绿：测试断言改 422，规格端点表登记该状态（house 格式错误体的 400 保留给 `validate` 失败与重复 id）。
- 重构：无（接受框架标准行为，不为单端点定制 rejection 映射）。

### C23 钉子：SystemOne 桩与查询串（→ ADR-L8 / `llm/forward` 后置第 5 步）
- 红：`llm_systemone_stub_forwards_without_token_counting`、`llm_query_string_is_preserved` —— 两条绿灯钉子：前者钉住「SystemOne 全程可路由可转发，仅 usage 记账恒为 0」；后者钉住查询串拼接。均直接通过。
- 绿：无新实现（桩与查询串在 C16-C19 的实现中已就位）。
- 重构：无。
