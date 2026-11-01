# 测试与需求映射

> 每个测试钉住一条 `docs/sdd/` 里的规格条目。新增测试前先在规格里登记它要钉的行为，再在此登记映射。

## top-gateway（`tests/integration.rs`）

| 测试函数 | 覆盖需求（来源） | 验证要点 |
|---|---|---|
| `proxy_health_endpoint_answers` | `sdd/interfaces.md` 管理端点表：`GET /health` | 无需鉴权返回 `{"status":"ok"}` |
| `unmatched_path_returns_404` | `proxy` 后置第 1 步 | 无路由时 404，错误体含原始 path |
| `proxy_forwards_stripped_path_query_and_injected_headers` | `proxy` 后置第 4/5/7 步 | 剥前缀、查询串拼接、路由头注入、`x-forwarded-host/path` |
| `proxy_passes_client_authorization_through` | `proxy` 后置第 6 步 | `Authorization` 原样透传 |
| `proxy_forwards_post_body_and_method` | `proxy` 后置第 6/10 步 | 方法与请求 body 流式透传 |
| `strip_prefix_false_keeps_full_path` | `proxy` 后置第 4 步 | `strip_prefix=false` 时完整 path 转发 |
| `disabled_route_returns_503` | `proxy` 后置第 2 步 | 禁用路由 503，消息含 route id |
| `longest_matching_prefix_wins` | `RouteTable::match_route` 契约 | 最长前缀命中且仅命中者计数 |
| `unreachable_upstream_returns_502` | `proxy` 后置第 8 步 | 传输错误 502（本机回环 refused ~2s，故 timeout 15s） |
| `admin_crud_roundtrip_and_persistence` | `admin.rs` 端点表 + `config.rs` 持久化 | CRUD 全流程、合并式 PUT、401/400/404、原子写回 |
| `admin_without_key_configured_is_open` | `require_admin` 契约 | 未配置 key 放行、错误 key 401 |
| `routes_file_loads_at_startup` | `load_routes` 后置 | 启动从 TOML 加载 |
| `health_marks_enabled_route_healthy_and_disabled_separately` | `health_snapshot` 后置 | 启用=healthy / 禁用=disabled，首轮立即执行 |
| `probe_endpoint_reports_unhealthy_route` | `probe_all_now` 后置 | 立即探测，首个不健康 → 500 |
| `metrics_count_requests_and_errors` | `StatsSnapshot` 后置 | total / in_flight / errors / avg_latency |
| `route_input_round_trips_through_json` | `RouteInput` 反序列化 | 默认值（strip_prefix/enabled/id 生成）与必填字段拒绝 |
| `unknown_admin_route_is_404_not_proxied` | 管理端点表注 2 | 未知 `/__admin/*` 返 404，不落到代理 fallback |

## LLM 子网关（`tests/llm_gateway.rs`）

> 规格来源为 `../sdd/llm/interfaces.md`。quota 快照与 `x-llm-provider` 响应头未单列测试函数，分别由 `llm_rpm_exhaustion_returns_429` / 计量类 / 转发类测试内联断言（见各条「验证要点」）。

| 测试函数 | 覆盖需求（来源） | 验证要点 |
|---|---|---|
| `llm_forwards_openai_path_and_bearer_key` | `llm/forward` 后置第 1/6 步 | 路径拼接 `base_url + 剩余路径`、`Authorization: Bearer <provider key>`、`x-llm-provider` 头 |
| `llm_forwards_anthropic_key_and_version` | `llm/forward` 后置第 6 步 | `x-api-key` 注入、`anthropic-version` 缺省注入且不覆盖客户端值 |
| `llm_unknown_protocol_returns_404` | `llm/forward` 后置第 1 步 | 未知协议段 404，house 错误体 |
| `llm_missing_protocol_returns_404` | `llm/forward` 后置第 1 步 | `/llm` 无协议段 404 |
| `llm_query_string_is_preserved` | `llm/forward` 后置第 5 步 | 上游 URL 保留客户端查询串 |
| `llm_no_provider_for_protocol_returns_503` | `llm/forward` 后置第 2 步 | 协议无供应商 503 |
| `llm_disabled_provider_is_not_selected` | `select_and_record` 后置 | 禁用供应商不参与选择 |
| `llm_rpm_exhaustion_returns_429` | `llm/forward` 后置第 3 步 | 全部供应商 RPM 耗尽 429，house 错误体；内联 `quota_view` 钉住快照 used/remaining |
| `llm_exhausted_provider_falls_back_to_remaining` | `select_and_record` 后置 | 权重为 0 者被排除，请求落到仍有余量者，不误判 429 |
| `llm_rpm_accounting_is_per_provider` | `llm/quota` 配额计量 | RPM 按供应商计数，不影响其他供应商 |
| `llm_counts_tokens_from_openai_usage` | `extract_tokens` openai 契约 | 非流式 `usage.prompt+completion` 计入 TPM（`quota_view` 断言 tpm used） |
| `llm_counts_tokens_from_anthropic_usage` | `extract_tokens` anthropic 契约 | 非流式 `input_tokens + output_tokens` 计入 TPM |
| `llm_counts_tokens_from_streaming_final_chunk` | `extract_tokens` SSE 契约 | openai SSE 末尾 `data:` 块中的 usage 被计入，流式不被阻塞 |
| `llm_counts_tokens_from_anthropic_streaming` | `extract_tokens` SSE 契约 | anthropic SSE 取首个 `input_tokens` + 末个累计 `output_tokens` |
| `llm_usage_missing_records_zero_tokens` | `extract_tokens` 降级契约 | 无 usage 时记 0，不影响转发 |
| `llm_streams_response_body` | `llm/forward` 后置第 10 步 | SSE body 逐块透传（usage 提取不缓冲整包） |
| `llm_unreachable_provider_returns_502` | `llm/forward` 后置第 8 步 | 供应商不可达 502、计错误数 |
| `llm_systemone_stub_forwards_without_token_counting` | ADR-L8 桩契约 | SystemOne 可路由可转发（`x-llm-provider` 头），usage 记账为 0（钉住桩行为） |
| `llm_admin_provider_crud_and_persistence` | `llm/admin` 端点表 + `config.rs` | 供应商 CRUD、合并式 PUT（显式 `null` = 不限）、`providers.toml` 原子写回 |
| `llm_admin_llm_endpoints_require_admin_key` | `llm/admin` 鉴权 | 沿用 `require_admin`（`X-Admin-Key` / `Bearer`） |
| `llm_admin_rejects_invalid_provider` | `Provider::validate` 后置 + `llm/admin` 端点表 | 非法 base_url → 400；JSON 缺必填字段 → 422（axum 标准行为，与路由 API 一致） |
| `llm_metrics_view_reports_per_provider_stats` | `llm/admin` 指标端点 | 按供应商的请求数 / 并发 / 错误 / 延迟 |
| `llm_coexists_with_route_table` | `llm/lib` 集成契约 | `/llm/*` 与 `routes.toml` 前缀路由共存互不影响 |
