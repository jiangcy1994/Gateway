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

### C30 审查驱动加固：flush 并发 / 关停顺序 / 饱和求和 / 查询契约（→ ADR-L22 / ADR-L23）
- 红：**无**——本轮改动来自一次系统性**代码审查**而非需求 / 规格先行，没有先行的失败测试（审查发现的是并发时序与契约一致性问题，非功能回归）。如实记录：这是「测试是验证者而非驱动者」的形态，与 C16-C23 的需求驱动路径不同。
- 绿：四处实现加固，各自由既有或扩展后的测试钉住：
  1. **flush 并发**（`ledger.rs`）：新增 `flush_lock: Mutex<()>`，`flush_if_dirty` 持锁全程（含写盘）；「快照 + 清 dirty」收进同一把 map 写锁临界区——消除「周期 flush 与末次 flush 并发时旧快照覆盖新数据」与「快照/清位夹缝」两个缺口。
  2. **关停顺序**（`lib.rs`）：shutdown 时先 `abort()` 周期 flush 任务再末次 `flush_if_dirty`。
  3. **饱和求和**（`usage.rs` / `config.rs`）：`Usage::total()`、`Usage + Usage`、`save_usage` 的 total 全改 `saturating_add`——上游返回接近 `u64::MAX` 的极端 / 恶意值不再 debug panic / release 回绕。
  4. **usage 查询契约**（`admin.rs`）：`RawQuery` + 手工 `serde_urlencoded` 取代 axum `Query` 提取器（其拒绝路径回纯文本 400，破坏 house JSON 信封）；四个过滤参数统一「空串 = 未传」。
- 验证：`llm_usage_view_filters_by_app_date_range` 内联扩展（空串不过滤、结构非法 query string → 400 JSON 信封）；新增 src 内单测 `usage_total_and_addition_saturate`（饱和语义）。**flush 的三条并发不变式没有确定性回归测试**（竞态无法稳定复现），靠锁序推理成立并留在代码注释里，验证缺口登记为风险 R18。
- 重构：无（加固性改动，不动外部接口形态）。

### C31 429 的 `Retry-After` 头：恢复时长可算（→ ADR-L24 / `quota.rs` `retry_after`）
- 红：`llm_429_response_carries_retry_after`——在 `QuotaExhausted` 只带消息串的形态下断言 `Retry-After` 头存在，头不存在而失败；同批 src 内单测 `quota_retry_after_none_when_unlimited_or_not_exhausted` / `quota_retry_after_rpm_tracks_evicting_entry` / `quota_retry_after_tpm_accumulates_from_front` 是**编译期红**（`retry_after` 方法不存在，等价于运行期 FAIL）。
- 绿：`QuotaGuard::retry_after(rpm_limit, tpm_limit)`（RPM = `requests[len - limit] + WINDOW`，TPM = 从前累加至剩余 `< limit` 的条目 `+ WINDOW`，供应商级 max）；balancer 的 429 分支在**既有临界区**内取候选 min 传入 `QuotaExhausted(String, Option<Duration>)`；`error.rs` 的 `IntoResponse` 在 `Some` 时输出 `Retry-After: <ceil 秒，clamp [1,60]>`（Display 只读第一字段 → 消息体字节不变）；`forward.rs` 的选择错误改走 `ApiError::into_response()`（与 `error_response` 错误体字节相同，头才能带到 `/llm` 响应）。
- 重构：无（纯加性：guard 新方法 / 变体扩字段 / 一处渲染路径统一；429 语义与消息体不变）。ADR-L24；R6 mitigation 更新为已实现，新增 R19（advisory 语义）。

---

## QQ Bot 子网关（`src/qqbot/`，`tests/qqbot_gateway.rs`）

按 `sdd/qqbot/interfaces.md`（实现权威）实现：`bot`（配置表示）→ `token`（访问令牌缓存）→ `forward`（OpenAPI 透传）→ `webhook`（回调接收）→ `ws`（网关客户端）→ `admin`/`config`/`state`/`lib`（装配）。外部事实来源是 `docs/reference/qq_bot_api`（平台 OpenAPI、回调地址验证、WS 协议）。

> 与 C1-C23 的流程差异（如实记录）：本子系统**规格先行**——SDD 六步子套件全部写完才动手写代码，实现按 phase 2-5 推进（每 phase 一个提交），**测试在 phase 6 集中补齐**。因此红灯形态不是「每功能一条运行期 FAIL」，而是「整套契约一次性用 14 条测试校验」：一次编译期红（C24）+ 两条运行期红（C28），其余为绿灯钉子（固化已决定行为，与 C15/C23 同性质——绿灯也是信息）。

### C24 编译期红灯：14 条测试一次性产出 12 条编译错误（→ 全模块）
- 红[红]：12 条编译错误，集中在三族：
  - `E0599` ×7 —— mock WSS 的 `WebSocketStream` 上找不到 `send` / `next`：tungstenite 0.29 的这两个方法分别来自 `SinkExt` / `StreamExt`，测试未引入（实现侧 `src/qqbot/ws.rs` 引过，测试侧漏了）。
  - `E0600` —— `assert!(soon(move || ...))` 缺 `.await`，rustc 把它报成「不能对 `impl Future` 应用一元 `!`」（宏展开造成的迷惑诊断）。
  - `E0716` ×3 —— `frame_jsons(&shared).iter().find(...)` 直接遍历临时 Vec，借用在分号处结束。
- 绿：引入 `use futures_util::{SinkExt, StreamExt};`；`soon(...).await`；先 `let frames = frame_jsons(&shared);` 再遍历。
- 重构：`cargo clippy --all-targets` 顺带挖出两个**真**问题——mock echo 未回显 `x-forwarded-host`（dead_code，且会让 C25 的透传断言隐性比对 `Value::Null`，属于「测试自身的断言失效」）；`posts.lock().unwrap().len() >= 1`（len_zero，改 `!is_empty()`）。

### C25 钉子：OpenAPI 透传与错误路径（→ `forward` 后置第 1/2/3/8/9 步）
- 红：`forwards_openapi_with_qqbot_token`、`forward_rejects_unknown_or_disabled_bots`、`forward_502_on_dead_upstream`、`forward_504_on_timeout` —— 实现先于测试（phase 3），补齐 C24 的编译错误与 echo 回显后全部直接通过。
- 绿：无新实现。
- 重构：无。四条测试钉住：app_id 段路由 + `Authorization: QQBot <token>` 替换客户端凭证 + `x-qqbot-bot` 响应头；404（未知）/ 503（禁用，ADR-Q8 的转发侧）；502 / 504 均计错误数。502 用 `timeout_secs=30`（本机回环 refused ~2s，见 C7）。

### C26 钉子：Webhook 签名镜像与先 ACK 后投递（→ `webhook` 后置 + `sign_validation`）
- 红：`webhook_validation_and_error_paths`、`webhook_acks_then_delivers_event`、`event_without_event_url_counts_error` —— 同上，直接通过。
- 绿：无新实现。
- 重构：无。op 13 的签名由测试**独立重算**（种子循环补齐 32 字节、`event_ts + plain_token` 拼接、hex 小写）镜像实现，任一侧改算法即红；先 ACK（`{"op":12}`）后异步投递、无 `event_url` 计错误丢弃（ADR-Q2 / ADR-Q1）。

### C27 钉子：管理端点、持久化与保留前缀（→ `admin` 端点表 + `config.rs` + ADR-Q5）
- 红：`admin_qqbot_endpoints_require_key`、`bot_admin_crud_and_persistence`、`metrics_view_reports_forward_stats`、`qqbot_prefix_is_reserved_from_route_table` —— 同上，直接通过。
- 绿：无新实现。
- 重构：无。钉住：沿用 `require_admin`；`BotPatch` 的 `Option<Option<String>>` 合并语义（缺省保留 / 显式 `null` 清除）；`bots.toml` 原子写回；stats 按 bot id 与 route id 分表；`/qqbot` nest 优先于路由表 fallback。

### C28 WS supervisor：心跳断言读了过期快照（→ supervisor 第 3/4/5 步）
- 红[红]：`ws_supervisor_identifies_heartbeats_and_delivers` —— `assert!(!beats.is_empty())` 失败：`beats` 过滤的是**为 Identify 断言拍的旧 `frames` 快照**（心跳一拍都还没发出），而 `soon(...)` 轮询本身成功了（闭包内每次重读）。典型的「快照在轮询前固定、断言在轮询后用旧值」。
- 绿：轮询后重新 `let all_frames = frame_jsons(&wss_shared);` 再过滤 op 1；Identify 断言改用早先的独立绑定。
- 重构：由此沉淀本测试文件的固定写法——**凡 `soon` 轮询后的帧断言，一律重取快照**；且心跳只断 `d >= 1`（op 0 事件与 READY 的到达顺序与 timing 不固定，断 `d == 2` 会竞态）。

### C29 钉子：关闭码分流与 Resume（→ supervisor 第 4/6 步）
- 红：`ws_supervisor_stops_on_unretryable_close`、`ws_supervisor_resumes_session_after_abrupt_drop` —— 直接通过。
- 绿：无新实现。
- 重构：无。前者刻意避开「空调轮询」陷阱：先等帧出现、再轮询表项消失、**再 sleep 3s 断言连接数不增且无 op 6**——若实现暗中重连，轮询过了也会在最后一关失败。后者钉住断线后 op 6 Resume 携带原 session_id + seq 并收到 `RESUMED`。
