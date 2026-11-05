# 风险登记（LLM 子网关）

> **SDD 第 6 步**：评估前 5 步定下来的系统。每条风险必须有「怎么验证」。

| # | 风险 | 影响 | 缓解 | 怎么验证 |
|---|---|---|---|---|
| R1 | **TPM 记账是 best-effort**：无 `usage` 字段（模型/供应商不返回）、流被中断、或 tail 缓冲（64 KiB）没覆盖到 usage 时记 0，TPM 计量偏低 | 配额比实际宽松，窗口末端可能轻微超限 | 限额配置留余量；`extract_tokens` 对无 usage 情况显式记 0 条而非静默跳过（`record_tokens(0)`），使请求次数仍精确 | 单测 + 集成测试 `llm_usage_missing_records_zero_tokens`；`llm_counts_tokens_from_streaming_final_chunk` 覆盖 SSE 末尾 usage |
| R2 | **在途请求令牌未预留**：派遣时只扣 RPM，token 要等响应回来才入账 | 高并发 + 大 TPM 限额时窗口内实际占用被低估 | 可接受（LLM 响应延迟通常 << 60s 窗口）；后续可加预估预扣 | 集成测试：同一供应商并发发若干请求后查 `/__admin/llm/quota` 的 `tpm.used` 与已完成响应累计一致 |
| R3 | **加权随机的短样本波动**：权重 0.9 / 0.1 的两个供应商，10 次请求里小权重者可能一次都没选到（的概率约 35%） | 分布式流量在分钟级自然均衡，单测样本不可作为分布断言 | 测试只钉确定性边界（权重为 0 即排除、耗尽即 429、全满前不误拒） | `llm_exhausted_provider_falls_back_to_remaining`、`llm_rpm_exhaustion_returns_429` |
| R4 | **错误体非原生协议格式**：OpenAI/Anthropic SDK 收到 429/502 时走通用错误分支，拿不到原生 `error.type` 等字段 | SDK 侧的错误处理代码要按状态码而非错误体结构写 | house 格式 `{"error":{"code","message"}}` 信息足够 SDK 按 status code 重试；已写入 overview 非功能约束 | 集成测试断言 429/503/404 体为此格式 |
| R5 | **`/llm/*` 无客户端鉴权 + api_key 明文**（ADR-L11） | 能访问端口者可消耗全部供应商配额；`GET /__admin/llm/providers` 泄露 key | 部署侧：绑回环、前置反加鉴权、设 `GATEWAY_ADMIN_KEY`；README 环境变量表注明 | 回归测试 `llm_admin_llm_endpoints_require_admin_key`（仅钉管理端点鉴权）；残留风险记录于 ADR-L11 |
| R6 | **全满即 429，无排队**：客户端看到 429 的时间窗内所有供应商都在限额内 | 高峰期瞬时拒绝；客户端需自带退避重试 | 与需求「全满返 429」一致；`Retry-After` 头未在 v1 提供（后续可加，窗口剩余时间可算） | 集成测试 `llm_rpm_exhaustion_returns_429` 钉住语义 |
| R7 | **无失败转移**（ADR-L7）：单供应商故障 → 502 | 挂多家供应商并不能躲开单家故障 | 监控 `/__admin/llm/metrics` 的 errors 计数及时下线故障供应商（`PUT` 设 `enabled:false`）；客户端重试会落到其他仍有配额的供应商 | `llm_unreachable_provider_returns_502` |
| R8 | **`Systemone` 计量桩**（ADR-L8） | SystemOne 供应商的 TPM 计量恒为 0，按 TPM 的加权对其无效 | 桩行为有测试钉住；规格明确后只改 `usage.rs` 一个函数 | `llm_systemone_stub_forwards_without_token_counting` |
| R9 | **同一供应商被多协议误配**：`protocols` 字段决定选择池，配错协议的供应商会被打到它不支持的协议路径 | 上游返回 4xx，透传给客户端 | `ProviderInput.protocols` 必填且 `validate` 校验（非空、无重复）；`x-llm-provider` 响应头让排错时能看到实际打到哪家 | `llm_forwards_openai_path_and_bearer_key` 断言响应头；多协议池由 `llm_v1_shared_provider_serves_both_protocols` 钉住 |
| R10 | **v1 端点推断是闭表**：新端点（OpenAI/Anthropic 后续新增的 API）未登记时 `/llm/v1/{新端点}` 返回 404 | 新端点要等网关升级才能经 `/llm/v1` 访问 | 闭表即确定性（ADR-L12 拒绝前缀/模糊匹配）；逃生口是无歧义的显式协议段 `/llm/{protocol}/v1/{新端点}`，不经推断 | `llm_v1_unknown_endpoint_returns_404`（404）+ `llm_v1_ambiguous_endpoint_returns_404`（404 后显式段仍 200） |
| R11 | **组合校验存在盲区**（ADR-L15）：请求 body 无 `model` 字段 / 不是 JSON / 超出 16 MiB 缓冲上限时，app 的模型组合**无法校验** | 无 `model` 的请求（如 `POST /v1/embeddings` 变体、文件上传）可绕过组合限制打到任意协议级供应商；app 配置给出「已限制」的假象 | 文档明确「组合只覆盖带 `model` 的请求」（范围外条目）；超 16 MiB 的请求直接 413（不放进盲区）；`App.models` 只是**约束面**不是安全面——真正的隔离靠供应商分流（给不同 app 配不同供应商，而非只靠组合） | `llm_app_combo_allows_models_without_model_field`（无 model 字段的请求不因组合被拒）；`llm_oversized_body_returns_413` |
| R12 | **请求 body 缓冲的内存占用**（ADR-L14）：每个并发请求最多缓冲 16 MiB | 高并发下内存线性增长（100 并发 = 1.6 GiB 上限） | 16 MiB 是 LLM 请求体（含图片/文件 base64）的经验上限；响应方向仍流式；超限即 413 拒绝，不存在无限缓冲；网关部署规模（本地 / 小团队）下可接受 | 单测 + 集成测试 `llm_oversized_body_returns_413` 钉住上限；`llm_model_rewrite_zero_diff_passthrough` 钉住值相同时不复制不重写 |
