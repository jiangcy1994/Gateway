# 测试与需求映射

> 每个测试钉住一条 `docs/sdd/` 里的规格条目。新增测试前先在规格里登记它要钉的行为，再在此登记映射。
>
> 例外：`llm_example_configs_parse_and_validate`（LLM 表末行）与 `qqbot_example_configs_parse_and_validate`（QQ Bot 表末行）是示例配置维护条目——钉的是 `examples/` 三个模板不随代码演进腐坏，不对应 SDD 规格。

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
| `llm_v1_inference_routes_by_endpoint` | ADR-L12 / `infer_protocol` 闭表 | `/llm/v1/chat/completions` → openai 池 + Bearer；`/llm/v1/messages` → anthropic 池 + `x-api-key`；`/llm/V1/...` 保留原大小写转发且查询串拼接 |
| `llm_v1_shared_provider_serves_both_protocols` | ADR-L13（`protocols` 多选） + P6 | 多协议供应商同时进两个协议池；RPM 按供应商记一份（跨协议共用）；配额快照 `protocols` 为数组且只有一条记录；耗尽 → 429 |
| `llm_v1_ambiguous_endpoint_returns_404` | `llm/forward` 后置第 1 步（`InferError::Ambiguous`） | `/llm/v1/models` → 404 且消息提示显式协议段；随后 `/llm/openai/v1/models` → 200（显式段绕过歧义） |
| `llm_v1_unknown_endpoint_returns_404` | `llm/forward` 后置第 1 步（`InferError::Unknown`） | 未知端点 404，消息含端点名 |
| `llm_v1_missing_endpoint_returns_404` | `llm/forward` 后置第 1 步（v1 后无端点） | `/llm/v1` → 404「missing endpoint」 |
| `llm_legacy_single_protocol_field_still_loads` | ADR-L13 兼容契约 + `load_providers` 后置 | 旧形态 `protocol = "openai"` 仍可加载；下次 `save_providers` 迁移为 `protocols = [...]`；两键都缺 → `ApiError::ConfigLoad` |
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
| `llm_admin_rejects_invalid_provider` | `Provider::validate` 后置 + `llm/admin` 端点表 | 非法 base_url → 400；`protocols` 空 / 重复 → 400；JSON 缺必填字段 → 422（axum 标准行为，与路由 API 一致） |
| `llm_metrics_view_reports_per_provider_stats` | `llm/admin` 指标端点 | 按供应商的请求数 / 并发 / 错误 / 延迟 |
| `llm_coexists_with_route_table` | `llm/lib` 集成契约 | `/llm/*` 与 `routes.toml` 前缀路由共存互不影响 |
| `llm_rewrites_model_to_upstream_name` | ADR-L14（模型映射改写）+ `llm/forward` 后置第 8 步 | 两供应商声明同一网关 model、不同 upstream 名与 priority → mock 上游收到的是重写后的 upstream 名；响应 `x-llm-provider` 指向高优先级者 |
| `llm_model_rewrite_zero_diff_passthrough` | ADR-L14（值相同时零差异透传） | upstream 与请求 model 相同时，上游收到的 body 与客户端发送的逐字节一致（不重序列化） |
| `llm_priority_tier_demotes_on_exhaustion` | ADR-L16（分层选择）+ `select_and_record` 后置 | 高优先级供应商 `rpm_limit=1` 耗尽后，第二次请求降层到低优先级供应商（断言 `x-llm-provider`） |
| `llm_unknown_model_returns_404` | `llm/forward` 后置第 5 步（`ModelNotFound`） | 无任何供应商声明的 model → 404，消息提示模型表位置 |
| `llm_model_all_disabled_returns_503` | `llm/forward` 后置第 5 步（`NoProviderForModel`） | 模型有声明但池空（disabled / 协议不匹配）→ 503 |
| `llm_all_tiers_exhausted_returns_429` | `llm/forward` 后置第 5 步（分层 429） | 所有层 weight 全 0 → 429，house 错误体含 model |
| `llm_model_request_skips_empty_models_provider` | ADR-L14 显式声明优先于兜底 | 并存空模型表 provider 与声明该 model 的 provider 时，声明者被优先选中 |
| `llm_catchall_provider_passes_model_through` | ADR-L14 空表兜底 | 只有空模型表 provider 时，带 model 的请求 model 原样透传、路由命中 |
| `llm_app_combo_allows_models_without_model_field` | ADR-L15（组合盲区）+ 风险 R11 | app 组合非空但请求 body 无 `model` 字段 → 放行（不拦截） |
| `llm_app_rejects_model_outside_combo` | `llm/forward` 后置第 4 步（`ModelNotAllowedForApp`） | 组合外 model → 403，消息含 model 与 app id |
| `llm_unknown_app_key_returns_401` | `llm/forward` 后置第 2 步（`UnknownAppKey`） | 无效凭证 → 401；去掉凭证后同一请求放行 |
| `llm_disabled_app_key_returns_403` | `llm/forward` 后置第 2 步（`AppDisabled`） | 命中但 `enabled=false` 的 app key → 403 |
| `llm_request_without_credential_uses_default_policy` | ADR-L15 默认策略 | 无凭证请求按全部已映射模型放行（历史 3 处 `Bearer client-key` 站点改无头后的回归基线） |
| `llm_oversized_body_returns_413` | `llm/forward` 前置常量（`MAX_REQUEST_BODY`）+ 风险 R12 | 超过 16 MiB 的请求 body → 413，不转发 |
| `llm_provider_timeout_overrides_global` | ADR-L17 | provider `timeout_secs=1` 指向 sleep 3s 的慢上游 → 504；不设 timeout → 走全局默认（此用例不被超时误杀） |
| `llm_provider_proxy_routes_through_proxy` | ADR-L19（按供应商代理）+ `llm/forward` 后置第 11 步 | 配了 `proxy_url` 的供应商经代理转发：base_url 不可达却 200，且代理端读到 absolute-form URI（带 scheme+host），与直连的 origin-form 相区分 |
| `llm_unreachable_proxy_returns_502` | ADR-L19 / 风险 R13 | 代理不可达与上游不可达同语义：502、消息含供应商 id、错误数 +1 |
| `llm_admin_rejects_invalid_proxy_url` | `Provider::validate` 后置（P14） | proxy_url 为 socks5 / 非 http(s) scheme / 无 scheme / 无 host → 400（配置时拒绝，非请求时 500） |
| `llm_admin_provider_proxy_merge_semantics` | `llm/admin` 端点表（`ProviderPatch.proxy_url`） | PUT 缺省保留代理、显式 `null` 清除回共享直连 client；清除后落盘 TOML 不含该键、重载等价无代理；GET（单项与列表）未配置时 `proxy_url` 恒为 `null`——对旧消费者是纯新增键 |
| `llm_legacy_production_file_round_trips_without_proxy_key` | `config.rs` 持久化后置 + ADR-L13/L14 | 0.2.1 生产形态文件（单值 `protocol`、空模型表兜底、禁用供应商带模型表、注释、无代理键）在 0.2.2 下加载仅发生已知迁移；重写落盘**不多出 `proxy_url` 键**——未配代理的既有部署落盘形态与旧版一致，可原地替换 |
| `llm_file_level_proxy_url_routes_request_through_proxy` | ADR-L19 + `config.rs` 加载后置 | 文件态端到端：往 providers.toml 直接写 `proxy_url` → 启动加载后请求**实际走该代理**（代理端读到 absolute-form URI），钉住「磁盘配置 → 启动加载 → 转发」整条链路，而非只有管理 API 一条配置路径 |
| `llm_hand_edited_invalid_proxy_loads_but_fails_at_request_time` | `config.rs` 加载后置（加载路径不跑 `validate`）+ 风险 R13/R14 | 手工写的 socks5 `proxy_url` 能加载、启动不失败（`load_providers` 不断言代理合法），首个请求以 502「provider ... unreachable」暴露——400 闸门只在管理 API 路径上，文件态没有这道闸 |
| `llm_admin_app_crud_and_persistence` | `llm/admin` apps 端点表 + `config.rs` 持久化 | app CRUD、合并式 PUT（models 列表替换）、key 自动生成 `sk-gw-...`、`apps.toml` 原子写回（重读断言落盘） |
| `llm_legacy_provider_without_models_still_works` | ADR-L14 兜底兼容契约 | 无模型表的历史 provider 配置（`provider()` helper）在带 model 的请求下仍可路由——向后兼容回归 |
| `llm_example_configs_parse_and_validate` | 示例配置维护条目（非 SDD 契约，见 `examples/README.md`） | `examples/` 两个 LLM 示例 TOML 经真实 `load_providers`/`load_apps` 解析与 `validate` 通过；分层（priority 10/1）、按供应商超时、按供应商代理、组合引用已声明模型的演示不变式成立——防示例文档随代码演进腐坏 |

## QQ Bot 子网关（`tests/qqbot_gateway.rs`）

> 规格来源为 `../sdd/qqbot/interfaces.md`。三个 mock 平台（OpenAPI 上游 / 事件订阅者 / mock WSS）由测试自带；token 缓存的并发不变式与关闭码全表未单列测试函数，由 WS 类测试内联断言（见各条「验证要点」）。

| 测试函数 | 覆盖需求（来源） | 验证要点 |
|---|---|---|
| `forwards_openapi_with_qqbot_token` | `forward` 后置第 1/3/5/6/9 步 + `fetch_token` 契约 | 首段 app_id 路由、token 由 `POST /app/getAppAccessToken` 换取且缓存复用、`Authorization: QQBot <token>` 注入（客户端凭证被替换）、`x-qqbot-bot` 响应头、`x-forwarded-host/path` |
| `forward_rejects_unknown_or_disabled_bots` | `forward` 后置第 2 步 + ADR-Q8 | 未知 app_id → 404；disabled → 503（`BotDisabled`）；回调路径 disabled 统一 404 |
| `forward_502_on_dead_upstream` | `forward` 后置第 8/11 步 | 传输错误 502 且计一次错误（本机回环 refused ~2s，故 timeout 30s） |
| `forward_504_on_timeout` | `forward` 后置第 8 步 | 上游超时 504 且计一次错误 |
| `webhook_validation_and_error_paths` | `webhook` 后置第 1/2/3/4 步 + `sign_validation` | `X-Bot-Appid` 与路径不一致 → 401；未知/禁用 bot → 404；非 JSON → 400；op 13 回 `plain_token` + 签名（独立 ed25519 重算镜像种子构造与拼接顺序） |
| `webhook_acks_then_delivers_event` | `webhook` 后置第 5 步 + ADR-Q2 | 先 ACK（`{"op":12}`）后异步投递；订阅者收到原样字节、`Bearer <event_secret>` 与原 `Content-Type` |
| `event_without_event_url_counts_error` | `webhook` 后置第 5 步（`event_url` 为 None 分支） | 无 event_url → 计一次错误并丢弃，ACK 仍 200 |
| `admin_qqbot_endpoints_require_key` | `admin` 鉴权（沿用 `require_admin` 契约） | 无/错 key → 401；正确 `X-Admin-Key` 放行 |
| `bot_admin_crud_and_persistence` | `admin` 端点表 + `config.rs` 持久化 + WS 生命周期联动 | CRUD 全流程、合并式 PUT（`Option<Option<String>>`：缺省保留 / 显式 `null` 清除）、`bots.toml` 原子写回、POST/PUT/DELETE 联动 supervisor ensure/stop |
| `metrics_view_reports_forward_stats` | `admin` 指标端点 + `StatsSnapshot` 后置 | 按 **bot id** 的 total / errors / avg_latency；bot id 与 route id stats 不串号（ADR-L9 同构） |
| `qqbot_prefix_is_reserved_from_route_table` | ADR-Q5（`lib.rs` 集成契约） | `/qqbot` 前缀路由永不生效（nest 优先于 fallback），route 与 bot 同名共存 |
| `ws_supervisor_identifies_heartbeats_and_delivers` | supervisor 第 3/4/5 步 | Hello → Identify（`token: "QQBot <token>"` / intents / shard）→ READY 存 session → 心跳 op 1（`d` = 最新 seq，首拍 null）→ op 11 ack；op 0 事件异步投递到 event_url |
| `ws_supervisor_stops_on_unretryable_close` | supervisor 第 6 步关闭码分流 | 4014 类不可重试码 → 停止不重连：等待后连接数不增、无 op 6 Resume 帧 |
| `ws_supervisor_resumes_session_after_abrupt_drop` | supervisor 第 4/5 步（Resume 分支） | 断线后重连发 op 6（带 session_id + seq），收到 `RESUMED`，session 跨重连不丢失 |
| `qqbot_example_configs_parse_and_validate` | 示例配置维护条目（非 SDD 契约，见 `examples/README.md`） | `examples/bots.example.toml` 经真实 `load_bots` 解析与 `validate` 通过；两通道形态（webhook/websocket）、沙箱切换、api_base 覆盖、无 event_url 丢弃分支的演示不变式成立，且占位凭证保持占位形态——防示例文档随代码演进腐坏 |
