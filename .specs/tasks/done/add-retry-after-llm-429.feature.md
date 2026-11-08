---
title: Add Retry-After header to LLM 429 quota-exhausted responses
---

# Add Retry-After header to LLM 429 quota-exhausted responses

## Initial User Prompt

为 LLM 子网关 429 响应加 Retry-After 头：QuotaExhausted 携带恢复时长，house IntoResponse 输出 Retry-After 头（ADR-L24，R6 后续项）

## Description

> **Required Skill**: You MUST use and analyse `http-retry-after-semantics` skill before doing any modification to task file or starting implementation of it!

> Skill location: `.claude/skills/http-retry-after-semantics/SKILL.md`

ADR-L6 把「全部候选供应商的 RPM 或 TPM 用尽」定为 429——语义上是「可重试」，却没告诉客户端**等多久**（风险 R6 登记的「后续可加」缺口）。客户端只能盲退避：退避短了反复撞 429（高峰期无效请求堆积），退避长了白等（容量闲置）。而窗口剩余时间**恰好可算**：所有记账都在 `QuotaTracker` 的双端队列里，触发阈值的那条记录即决定窗口重新可用的瞬间（ADR-L24）。

本任务把该信息从内部状态变成客户端可见的标准 HTTP 头。`QuotaGuard` 新增 `retry_after(rpm_limit, tpm_limit) -> Option<Duration>`：RPM 耗尽时恢复时刻 = `requests[len - limit] + WINDOW`（len == limit 时即队首）；TPM 耗尽时 = 从前累加被淘汰量直至剩余量 `< limit` 的那条 `+ WINDOW`；供应商级取两维的 **max**（`weight = min`，两维都恢复才算恢复）。`select_and_record` 判 429 时所有候选 guard 仍全部持有，池级取全部候选的 **min**（`None` 候选防御性跳过）并随 `QuotaExhausted` 携带——整个计算在既有临界区内完成，零额外加锁、无 await、不延后任何请求。

渲染侧 `ApiError::QuotaExhausted` 变为 `(String, Option<Duration>)` 二元组：house 的 `IntoResponse` 在 `Some(d)` 时插入 `Retry-After: <ceil(d) 的秒数>`（delta-seconds 形态，clamp `[1, 60]`——纯防御性格式化，理论值恒 ∈ (0, WINDOW)）。forward 的 `select_and_record` 错误改走 `ApiError::into_response()`（与 `proxy::error_response` 错误体字节相同），否则头带不到 `/llm` 响应。

三条硬约束贯穿本任务：(1) **消息体字节不变**——thiserror 的 `Display` 只读第一字段，第二字段对 JSON body 纯加性，错误文案与 house 信封格式均不得修改；(2) **既有临界区不变式不破坏**——锁按 ProviderTable 表序、选择与 RPM 记账同一临界区，恢复时长只读已淘汰过的 deque、不在读取时二次淘汰；(3) **advisory 语义**——头值是最早**可能**恢复的时刻而非预约（并发请求可抢先占走刚空出的槽位、在途 token 回账可重新压满 TPM，风险 R19），客户端应视为最小退避，文档须如实陈述。

**Scope:**
- Included: retry_after 计算、`QuotaExhausted` 变体二元组、house `IntoResponse` 渲染、forward 渲染路径切换、mapping.md 与 README / risks.md（R6 / R19）同步、`/__admin/llm/quota` 视图不变
- Excluded: 排队 / 槽位预留（L6 已定无排队）、透传上游 429 的 `Retry-After`（耗尽是网关侧决定）、修改 429 消息体或选择 / 加权 / 分层语义

**User Scenarios:**
1. **主流程（全池耗尽）**: 全部候选供应商 RPM 或 TPM 用尽 → 429 + `Retry-After: <秒>` → 客户端以头值为最小退避下界、加抖动重试
2. **替代流程（维度不限额 / 未耗尽）**: 该维度未设限额或未耗尽 → 无头 → 客户端回退自身退避策略
3. **错误时序（按头值精确重试仍撞 429）**: 客户端按头值精确重试（不加抖动）仍可能因并发抢槽 / 在途 token 回账（R19）再次收到 429——该时序如实呈现，不掩盖

## Architecture Overview

### Solution Strategy

**Approach**: 最小介入的「单源计算 + 顺层透传」——恢复时长在 `QuotaGuard::retry_after` 一处计算（域数学），经 `balancer` 的 429 分支聚合为池级 min，作为 `ApiError::QuotaExhausted` 的第二字段，由 house `IntoResponse` 渲染为 `Retry-After` 头，`forward` 仅切换一处渲染调用。**Why**: 设计已由 ADR-L24（`docs/sdd/llm/decisions.md:30`）锁定并预登记于 `docs/sdd/llm/interfaces.md:126/170/425` 与 `docs/sdd/llm/data-model.md:139-142`——本任务不是开放设计而是落地，架构自由度只剩「如何在既有分层与临界区不变式下零摩擦接入」。答案是复用既有契约链：`QuotaGuard` 是唯一的临界区句柄（guard 即锁，`src/llm/quota.rs:32-49`）、`ApiError::IntoResponse` 是唯一的 house 错误渲染权威（`src/error.rs:138-149`）、thiserror Display 的字段选择性天然保证消息体字节不变。不新增模块、不新增锁、不引入跨组件抽象——三条硬约束（体字节不变 / 临界区不变式 / advisory 语义）全部由既有结构承载，符合仓库分层单体与「ApiError 单一错误契约」的既有模式。

**外部语义依据**（RFC 9110 §10.2.3 的 delay-seconds 形态、RFC 6585 §4 对 429 的 MAY 授权、OpenAI SDK 的真实消费行为）见 `.claude/skills/http-retry-after-semantics/SKILL.md`；影响面与逐条 file:line 依据见 `.specs/analysis/phase2b-codebase-impact.md`。

### Component Decomposition（429 路径数据流）

```
QuotaGuard::retry_after(rpm_limit, tpm_limit)        [src/llm/quota.rs — NEW，唯一计算点]
   ├─ RPM 耗尽 → requests[len - limit] + WINDOW
   ├─ TPM 耗尽 → 从前累加淘汰量至剩余 < limit 的那条 + WINDOW
   └─ 供应商级 = max(两维耗尽者)
        ▼ （guards 仍全部持有——既有临界区，零新锁、无 await）
balancer::select_and_record 429 分支                  [src/llm/balancer.rs:76-78 — UPDATE]
   └─ 池级 = 全部候选之 min（None 防御性跳过）→ exhausted(protocol, model, retry_after)
        ▼
ApiError::QuotaExhausted(String, Option<Duration>)    [src/error.rs:41-42 — UPDATE]
   └─ thiserror Display 只读 {0} → JSON message 字节不变（对 body 纯加性）
        ▼
ApiError::IntoResponse                                [src/error.rs:138-149 — UPDATE]
   └─ 仅该变体且 Some(d) → Retry-After: ceil(d) 秒 clamp [1,60]（delta-seconds）
        ▼
forward 的 select_and_record 错误改走 e.into_response()  [src/llm/forward.rs:160-165 — UPDATE]
   └─ 其余变体渲染字节与 proxy::error_response 逐字节相同（已核验），仅 429 多一头
```

### Key Decisions

1. **`retry_after` 挂在 `QuotaGuard` 而非 tracker / state**：guard 就是锁本身，在 `select_and_record` 既有临界区内调用即满足「零额外加锁、无 await、不延后任何请求」（guards 于 `balancer.rs:68` 一次性获取、`:76` 判 429 时仍全部持有、直至 `:106` 记账）；`&self` 足够——只读**已淘汰过**的 deque，读取时不二次淘汰。
2. **恢复数学单点实现于 `quota.rs` 模块私有助手**：`balancer` 只做池级 min 聚合、`error` 只携带与渲染，任何处不重算 `requests[len - limit] + WINDOW` 或 TPM 从前累加式（CK-15 / Single Source of Truth）。RPM 助手以 `limit.is_some() && requests.len() >= limit` 为门（耗尽即该维是约束），否则下标越界；TPM 累加因 P4 保证 `limit > 0` 必终止。
3. **变体二元组而非新变体或通用字段**：`QuotaExhausted(String, Option<Duration>)` 承袭仓库两字段先例（`ModelNotAllowedForApp(String, String)`）；`#[error(...)]` 只引用 `{0}`，Display 输出逐字节不变 → JSON body 纯加性。全仓库构造点仅 2 处（`balancer.rs:117/:120`）、穷尽匹配仅 1 处（`error.rs:116`）——grep 核验，`tests/` 侧无编译点。
4. **头部在 `IntoResponse` 内按「变体 + `Some(d)`」双重门插入**：不引入通用的 `e.retry_after()` 访问器，从构造上排除 404/503/401/403/413 携带该头（CK-5 陷阱）；用类型常量 `axum::http::header::RETRY_AFTER`（axum 0.8 `pub use http`）消除拼写类错误。
5. **`ceil` + clamp `[1, 60]` 的渲染形态**：`as_secs()` 截断会产出 0，而 OpenAI Python SDK 仅当 `0 < retry_after <= 120` 才 honor 头值（0 被丢弃、退回盲退避）；ceil 保证 ≥1，上界 60 留在 SDK honor 窗口内且理论值恒 ∈ (0, 60)，clamp 纯防御性格式化。
6. **`forward` 渲染路径切换经字节等价性核验**：`proxy::error_response` 与 `ApiError::into_response` 输出逐字节相同（同 status 来源 `ApiError::status()`、同 `json!` 信封、同 message 字节——调用方传入的 `&e.to_string()` 即实现内部所用的同一 Display），切换对其余变体零字节变化，仅 429 多出一头；这是头能到达 `/llm` 响应的唯一路径。forward 内其余 `error_response` 调用点（含 `:242` 的 client 构建 500 错误）一律不动。
7. **单测以私有注入钉住恢复数学**：在 `src/llm/quota.rs` 新增 `#[cfg(test)] mod tests`（该文件当前无测试块，照 `src/llm/usage.rs:319` 的内文件模式），借子模块可见私有项之便直接向 `QuotaInner` 的 deque 注入回溯 `Instant`——确定性、不依赖墙上时钟 sleep；集成测试只断言头存在、值 ∈ [1,60]、消息体不变（真实时间下不断言精确秒数）。

### Trade-offs（如实陈述）

- **advisory 而非预约（R19）**：头值是最早**可能**恢复的时刻——并发请求可抢先占走刚空出的槽位（选择无排队，L6/L4/L16），在途请求的 token 回账可重新压满 TPM（与 R2 同源时序缺口）；客户端按头值精确重试仍可能再撞 429。无代码层面缓解（排队 / 预留已被 L6 与 ADR-L24 否决），由文档与用户场景 3 如实呈现。
- **`ceil` 牺牲亚秒精度、最多高估 1s**：换取整数 delta-seconds（客户端标准库可直接解析）且永不为 0。
- **池级 min 可低于实际下次可调度时刻**：最快恢复的供应商可能处于更低 priority 层（ADR-L16），min 表达「池内最早可能恢复」而非「分层策略下次会选谁」——ADR-L24 认定此为 429 的正确语义。
- **临界区增加 O(deque) 只读遍历**：长度被 60s 窗口封顶、无 await、不取第二把锁——与「选择 + RPM 记账同临界区」的既有不变式相容。

### Expected Changes

```
Gateway/
├── src/
│   ├── error.rs                 # UPDATE: QuotaExhausted(String) → (String, Option<Duration>)（#[error] 仍只读 {0}）；
│   │                            #   status 匹配臂补第二模式；IntoResponse 仅在该变体 Some(d) 时插 Retry-After
│   │                            #   （ceil 秒 clamp [1,60]）；新增 std::time::Duration 与 axum::http 导入
│   ├── llm/
│   │   ├── quota.rs             # UPDATE: QuotaGuard::retry_after(&self, rpm_limit, tpm_limit) -> Option<Duration>
│   │   │                        #   + 模块私有 RPM/TPM 恢复助手（供应商级 max）；NEW #[cfg(test)] mod tests
│   │   │                        #   （3 条确定性单测，注入回溯 Instant）；Duration/Instant 导入已齐备
│   │   ├── balancer.rs          # UPDATE: 429 分支（:76-78，guards 仍持有时）算池级 min 后传入
│   │   │                        #   exhausted(protocol, model, retry_after)；exhausted() 签名扩展；
│   │   │                        #   新增 use std::time::Duration（该文件当前未导入）
│   │   └── forward.rs           # UPDATE: :164 的 select_and_record 错误改 e.into_response()，并更新
│   │                            #   :162-163 注释（其余 error_response 调用点一律不动）
│   └── ...
├── tests/
│   └── llm_gateway.rs           # UPDATE: 新增 llm_429_response_carries_retry_after（rpm_limit = Some(1)，
│                                #   首请求 200 / 次请求 429 → 头存在、值为整数秒 ∈ [1,60]、消息体不变，
│                                #   非 429 错误不带头，quota 视图不回归；复用 spawn_llm_gateway /
│                                #   authed / error_body / quota_view 既有 harness）
└── docs/tdd/
    └── mapping.md               # UPDATE: LLM 表新增 llm_429_response_carries_retry_after 一行
                                 #   （列：测试函数 / 覆盖需求（ADR-L24、llm/forward 后置第 3 步）/ 验证要点）
```

无需改动：`src/proxy.rs`、`src/llm/admin.rs`（`/__admin/llm/quota` 视图走 `QuotaTracker::snapshot`，不触及 `retry_after`，视图形态不变）、`src/llm/provider.rs`、`src/llm/ledger.rs`。文档侧 `README.md`、`docs/sdd/llm/{interfaces,decisions,data-model,risks}.md`（R6 已转「已实现」、R19 已登记 advisory 语义）、`docs/cycles.md`（C31 红绿）均已预登记为目标态，实现期只需核对一致性（若有漂移按已登记文本校正，不重写语义）。

---

## Implementation Process

You MUST launch for each step a separate agent, instead of performing all steps yourself. And for each step marked as parallel, you MUST launch separate agents in parallel.

**CRITICAL:** For each agent you MUST:
1. Use the **Model** and **Agent** type specified in the step's sub-task file (e.g., `haiku`, `sonnet`, `tech-writer`)
2. Provide the path to THIS task file AND the path to that step's sub-task file
3. Require agent to implement exactly that step, not more, not less, not other steps

**CRITICAL:** Verification is done at PHASE level, not per step. When every step of a phase is complete, you MUST launch the code reviewer ONCE for that phase, at the **Reviewer model** named for that phase in the Phase Overview.

### Parallelization Overview

```
Step 01-quota-retry-after [sonnet, sdd:developer]
    │
    ▼
═══════ end of Phase 1 (review: opus) ═══════
    │
    ▼
Step 02-429-path-wiring [opus, sdd:developer]
    │
    ├──────────────────────────┐
    ▼                          ▼
Step 03-integration-test-      Step 04-docs-consistency
and-mapping [sonnet,           [sonnet, sdd:tech-writer]
sdd:developer]
    (parallel, width 2)
    │                          │
    └────────────┬─────────────┘
                 ▼
═══════ end of Phase 2 (review: opus) ═══════
```

| Step | Phase | Model | Agent | Depends on | Parallel with | Sub-Task File |
|------|-------|-------|-------|------------|---------------|---------------|
| `01-quota-retry-after` | Phase 1 | sonnet | sdd:developer | None | None | `.specs/sub-tasks/add-retry-after-llm-429/01-quota-retry-after.md` |
| `02-429-path-wiring` | Phase 2 | opus | sdd:developer | `01-quota-retry-after` | None | `.specs/sub-tasks/add-retry-after-llm-429/02-429-path-wiring.md` |
| `03-integration-test-and-mapping` | Phase 2 | sonnet | sdd:developer | `02-429-path-wiring` | `04-docs-consistency` | `.specs/sub-tasks/add-retry-after-llm-429/03-integration-test-and-mapping.md` |
| `04-docs-consistency` | Phase 2 | sonnet | sdd:tech-writer | `02-429-path-wiring` | `03-integration-test-and-mapping` | `.specs/sub-tasks/add-retry-after-llm-429/04-docs-consistency.md` |

### Phase Overview

#### Phase 1: Recovery Math Foundation

Steps: `01-quota-retry-after`
Reviewer model: `opus`
Acceptance Criteria that should be fulfiled:
Checklist items:
- `CK-7` — RPM 耗尽时恢复时刻是否为 `requests[len - limit] + WINDOW`（guard 创建时已淘汰窗口外条目）？
- `CK-8` — TPM 耗尽时恢复时刻是否为「从前累加被淘汰量直至剩余量 `< limit`」的那条 `+ WINDOW`？
- `CK-9`（供应商级部分）— 供应商级是否取两维耗尽者的 max？
- `CK-10` — `retry_after` 对未设限额或未耗尽的维度是否返回 `None`？
- `CK-12` — `src/llm/quota.rs` 是否存在 3 条单测钉住恢复数学（不限 / 未耗尽 → None；RPM；TPM）？

Rubrics:
- `Recovery Math Correctness`
- `Test Coverage Quality`
- `Single Source of Truth`

#### Phase 2: Wiring, End-to-End Proof, and Docs Consistency

Steps: `02-429-path-wiring`, `03-integration-test-and-mapping`, `04-docs-consistency`
Reviewer model: `opus`
Acceptance Criteria that should be fulfiled:
Checklist items:
- `CK-1` — `ApiError::QuotaExhausted` 是否为 `(String, Option<Duration>)` 二元组，且 `#[error]` 的 Display 只读第一字段？
- `CK-2` — house 的 `IntoResponse` 是否仅在该变体携带 `Some(d)` 时插入 `Retry-After` 头？
- `CK-3` — 头值是否为 `ceil(d)` 秒并 clamp 到 `[1, 60]` 的整数（delta-seconds）？
- `CK-4` — 429 的 JSON body 是否与变更前逐字节相同？
- `CK-5` — 任何非 429 的错误响应（404 / 503 / 401 / 403 / 413）是否携带了 `Retry-After` 头？（是 = 缺陷）
- `CK-6` — 池级 `retry_after` 是否在 `select_and_record` 既有临界区内、全部 guard 仍持有时计算（无新锁、无 await、不延后请求）？
- `CK-9`（池级部分）— 池级取全部候选的 min 且 `None` 候选防御性跳过？
- `CK-11` — forward 的 `select_and_record` 错误是否改走 `ApiError::into_response()`，使头到达 `/llm` 响应？
- `CK-13` — `tests/llm_gateway.rs` 的 `llm_429_response_carries_retry_after` 是否断言头存在、值 ∈ [1,60]、消息体不变？
- `CK-14` — `docs/tdd/mapping.md` 是否为该集成测试新增一行？
- `CK-15` — 恢复数学是否只实现一次（quota / balancer / error 之间无重复推导）？
- `CK-16` — README / interfaces 的恢复时长语义是否明确其为最小退避而非预约（R19）？
- `CK-17` — `cargo build` 是否零错误完成？
- `CK-18` — `cargo fmt` 与 `cargo clippy --all-targets` 是否零告警？
- `CK-19` — `cargo test` 是否全部通过且既有 148+ 条保持绿色？
- `CK-20` — 每个 `selected_types` 条目是否都至少有一条已实现测试？
- `CK-21` — 每条可测 checklist 条目是否都解析到至少一条通过测试？
- `CK-22` — 每个 Test Cases to Cover 条目（3 条 src 单测 + 1 条集成测试）是否都已实现并通过？
- `CK-23` — 客户端收到 429 时能否从头值读出「最小退避秒数」作为退避下界？
- `CK-24` — 客户端按头值精确重试（不加抖动）时仍可能再次收到 429——该 R19 时序是否在文档与测试场景中如实呈现而非被掩盖？

Rubrics:
- `Recovery Math Correctness`
- `Rendering Conformance`
- `429 Body Backward Compatibility`
- `Critical-Section Discipline`
- `Test Coverage Quality`
- `Advisory Semantics Documentation`
- `Project Guidelines Alignment`
- `Single Source of Truth`

## Acceptance Criteria

**Checklist:**

| ID | Question | Category | Importance |
|---|---|---|---|
| CK-1 | `ApiError::QuotaExhausted` 是否为 `(String, Option<Duration>)` 二元组，且 `#[error]` 的 Display 只读第一字段？ | hard_rule | essential |
| CK-2 | house 的 `IntoResponse` 是否仅在该变体携带 `Some(d)` 时插入 `Retry-After` 头？ | hard_rule | essential |
| CK-3 | 头值是否为 `ceil(d)` 秒并 clamp 到 `[1, 60]` 的整数（delta-seconds）？ | hard_rule | essential |
| CK-4 | 429 的 JSON body 是否与变更前逐字节相同（错误文案与 `{"error":{"code","message"}}` 信封不变）？ | hard_rule | essential |
| CK-5 | 任何非 429 的错误响应（404 / 503 / 401 / 403 / 413）是否携带了 `Retry-After` 头？（是 = 缺陷） | pitfall | important |
| CK-6 | 池级 `retry_after` 是否在 `select_and_record` 既有临界区内、全部 guard 仍持有时计算（无新锁、无 await、不延后请求）？ | hard_rule | essential |
| CK-7 | RPM 耗尽时恢复时刻是否为 `requests[len - limit] + WINDOW`（guard 创建时已淘汰窗口外条目）？ | hard_rule | essential |
| CK-8 | TPM 耗尽时恢复时刻是否为「从前累加被淘汰量直至剩余量 `< limit`」的那条 `+ WINDOW`？ | hard_rule | essential |
| CK-9 | 供应商级是否取两维耗尽者的 max，池级取全部候选的 min 且 `None` 候选防御性跳过？ | hard_rule | important |
| CK-10 | `retry_after` 对未设限额或未耗尽的维度是否返回 `None`？ | hard_rule | important |
| CK-11 | forward 的 `select_and_record` 错误是否改走 `ApiError::into_response()`，使头到达 `/llm` 响应？ | hard_rule | essential |
| CK-12 | `src/llm/quota.rs` 是否存在 3 条单测钉住恢复数学（不限 / 未耗尽 → None；RPM；TPM）？ | hard_rule | essential |
| CK-13 | `tests/llm_gateway.rs` 的 `llm_429_response_carries_retry_after` 是否断言头存在、值 ∈ [1,60]、消息体不变？ | hard_rule | essential |
| CK-14 | `docs/tdd/mapping.md` 是否为该集成测试新增一行（遵循 src 单测按 risks.md 惯例不入表）？ | hard_rule | important |
| CK-15 | 恢复数学是否只实现一次（quota / balancer / error 之间无重复推导）？ | principle | important |
| CK-16 | README / interfaces 的恢复时长语义是否明确其为**最小退避而非预约**（R19）？ | principle | important |
| CK-17 | `cargo build` 是否零错误完成？ | hard_rule | essential |
| CK-18 | `cargo fmt` 与 `cargo clippy --all-targets` 是否零告警？ | hard_rule | essential |
| CK-19 | `cargo test` 是否全部通过，且既有 148+ 条测试（含 `llm_rpm_exhaustion_returns_429`）保持绿色？ | hard_rule | essential |
| CK-20 | 每个 `selected_types` 条目（unit / integration）是否都至少有一条已实现测试？ | hard_rule | essential |
| CK-21 | 每条可测 checklist 条目是否都解析到至少一条通过测试（结构性不变式除外，见 Definition of Done）？ | hard_rule | essential |
| CK-22 | 每个 Test Cases to Cover 条目（3 条 src 单测 + 1 条集成测试）是否都已实现并通过？ | hard_rule | essential |
| CK-23 | 客户端收到 429 时能否从头值读出「最小退避秒数」作为退避下界（整数秒、delta-seconds 形态、客户端标准库可直接解析）？ | principle | important |
| CK-24 | 客户端按头值精确重试（不加抖动）时仍可能再次收到 429——该 R19 时序是否在文档与测试场景中如实呈现而非被掩盖？ | principle | important |

**Regular Checks:**

- [ ] 429 响应（全部候选 RPM 或 TPM 用尽）携带 `Retry-After` 头，值为整数秒且 ∈ [1, 60]
- [ ] 429 的 JSON body 与变更前逐字节一致：断言 `code == 429` 且 `message` 与实现前快照完全相同（以当前代码的实际输出为基准——`exhausted()` 的文案经 `ApiError::QuotaExhausted` 的 thiserror Display 包装后才是真实 body，见 `src/llm/balancer.rs` 与 `src/error.rs`）
- [ ] 非耗尽 / 不限额场景的响应（如 503 无供应商、常规 200）不携带 `Retry-After`
- [ ] `cargo build` 成功
- [ ] `cargo fmt` 无 diff
- [ ] `cargo clippy --all-targets` 零告警
- [ ] `cargo test` 全绿（既有 148+ 条 + 新增 4 条：`quota_retry_after_none_when_unlimited_or_not_exhausted`、`quota_retry_after_rpm_tracks_evicting_entry`、`quota_retry_after_tpm_accumulates_from_front`、`llm_429_response_carries_retry_after`）
- [ ] `docs/tdd/mapping.md` 新增 `llm_429_response_carries_retry_after` 一行（src 单测按 risks.md 惯例不入表）
- [ ] 无代码重复：新代码不重复已存在的函数 / 逻辑 / 概念（恢复时刻的推导只在 `QuotaGuard` 一处，`balancer` / `error` 引用而非重算）

**Rubric:**

| Criterion | Weight |
|---|---|
| Recovery Math Correctness（恢复数学正确性） | 0.18 |
| Rendering Conformance（ADR-L24 渲染规则一致性） | 0.14 |
| 429 Body Backward Compatibility（429 响应体向后兼容） | 0.14 |
| Critical-Section Discipline（临界区纪律） | 0.14 |
| Test Coverage Quality（测试覆盖质量） | 0.14 |
| Advisory Semantics Documentation（advisory 语义文档化） | 0.09 |
| Project Guidelines Alignment（项目规范一致性） | 0.09 |
| Single Source of Truth（恢复数学单点实现） | 0.08 |

**Rubric Score Definitions:**

### Recovery Math Correctness

恢复时刻是否由淘汰谓词（`entry + window <= now`，即 `now - entry >= WINDOW`）直接反推为数学上严格的下界，涵盖 RPM 下标式（`requests[len - limit] + WINDOW`，len == limit 时即队首）、TPM 从前累加式、供应商级 max、池级 min（None 跳过）与 `(0, WINDOW)` 边界。

本维度评估实现是否复现 ADR-L24 定义的恢复瞬间数学。分类：essential。评审指引：对照 `docs/sdd/llm/interfaces.md` 恢复时长语义段与 `docs/sdd/llm/data-model.md` 恢复段逐式核对；近似估计、经验缓冲、下标基准错位或聚合方向（供应商 max / 池级 min）颠倒均落在本维度；`as_secs()` 截断属渲染维度，不在本维度评分。

#### Anchors

**contrast**: 同一池、同一时刻，池级 `retry_after` 取全部候选的 min（最先可再调度的瞬间）还是误取 max（最晚恢复）。

**score_2**:
```text
// RPM：淘汰掉该条后窗口内只剩 limit-1 < limit 条（len == limit 时即 requests[0]）
let rpm = requests.get(len - limit).map(|t| *t + WINDOW);
// TPM：从前累加被淘汰量，首个令剩余量 < limit 的条目（P4 保证 limit > 0，必终止）
let tpm = tokens.iter().scan(0u64, |evicted, (t, n)| { *evicted += *n; Some((*t, *evicted)) })
    .find(|(_, evicted)| sum.saturating_sub(*evicted) < limit).map(|(t, _)| *t + WINDOW);
let provider = rpm.max(tpm);                              // weight = min 的对偶：两维都恢复才算恢复
let pool = candidates.filter_map(retry_after).max();     // ← 池级误取 max：头值变成「等最晚恢复的供应商」
```

**score_4**:
```text
// RPM：淘汰掉该条后窗口内只剩 limit-1 < limit 条（len == limit 时即 requests[0]）
let rpm = requests.get(len - limit).map(|t| *t + WINDOW);
// TPM：从前累加被淘汰量，首个令剩余量 < limit 的条目（P4 保证 limit > 0，必终止）
let tpm = tokens.iter().scan(0u64, |evicted, (t, n)| { *evicted += *n; Some((*t, *evicted)) })
    .find(|(_, evicted)| sum.saturating_sub(*evicted) < limit).map(|(t, _)| *t + WINDOW);
let provider = rpm.max(tpm);                              // weight = min 的对偶：两维都恢复才算恢复
let pool = candidates.filter_map(retry_after).min();     // 池级取 min：最先可能恢复的瞬间（None 候选防御性跳过）
```

### Rendering Conformance

头的渲染规则是否符合 ADR-L24 / interfaces.md：仅 `QuotaExhausted` 变体且 `Some(d)` 时出现；值 = `ceil(d)` 秒 clamp `[1, 60]` 的整数（delta-seconds 形态）；forward 的 `select_and_record` 错误经 `ApiError::into_response()` 渲染（ApiError 渲染的唯一权威）。

本维度评估头的作用域、值形态与渲染路径是否符合规格。分类：essential。评审指引：其余变体（404 / 503 / 401 / 403 / 413）不得带该头；`None` 时不得插头；头值不得为小数、0 或 > 60；forward 不得仍走 `proxy::error_response`（头带不到 `/llm` 响应）。本锚点对只对比插头作用域一项——头值形态由 CK-3 的边界单测钉住、forward 渲染路径由 CK-11 的集成测试钉住，两者不进入本维度锚点对比。

#### Anchors

**contrast**: 头是否仅在 `QuotaExhausted` 携带 `Some(d)` 时出现——其余变体（404 / 503 / 401 / 403 / 413）的响应绝无此头。

**score_2**:
```text
// error.rs —— 对所有 ApiError 无差别插头：404 / 503 / 401 / 403 / 413 的响应也带上 Retry-After
if let Some(d) = e.retry_after() {
    let secs = (d.as_secs_f64().ceil().clamp(1.0, 60.0)) as u64;
    resp.headers_mut().insert(RETRY_AFTER, HeaderValue::from(secs));
}
```

**score_4**:
```text
// error.rs —— 仅该变体且 Some 时插头，其余变体（404 / 503 / 401 / 403 / 413）绝无此头
if let ApiError::QuotaExhausted(_, Some(d)) = e {
    let secs = (d.as_secs_f64().ceil().clamp(1.0, 60.0)) as u64;
    resp.headers_mut().insert(RETRY_AFTER, HeaderValue::from(secs));
}
```

### 429 Body Backward Compatibility

消息体是否逐字节保持不变（thiserror Display 只读第一字段、`exhausted()` 文案生成逻辑与 house 信封不变），且既有 429 语义（状态码、消息、quota 视图）无回归——纯加性是本任务对 R4（SDK 按状态码而非错误体结构写处理代码）承诺的延续。

本维度评估「加头」是否真的只是加头。分类：essential。评审指引：变更不得修改 `exhausted()` 的消息生成或 body 组装；既有测试 `llm_rpm_exhaustion_returns_429` 必须零改动通过；把时长信息拼进 message 字符串（如 `{1:?}`）即违反本维度。

#### Anchors

**contrast**: 第二字段是否渗入 Display / message，既有客户端断言是否被破坏。

**score_2**:
```text
#[error("quota exhausted for protocol '{0}': retry after {1:?}")]
QuotaExhausted(String, Option<Duration>),
// 头加上了，但 message 多出 "(retry after ...)"——既有测试断言、客户端日志解析、SDK 透传体感全变了
```

**score_4**:
```text
#[error("quota exhausted for protocol '{0}': all providers are at their rpm or tpm limit")]
QuotaExhausted(String, Option<Duration>),
// Display 与变更前逐字节相同；时长走独立字段、头独立于 body，对 JSON 响应纯加性
```

### Critical-Section Discipline

恢复时长计算是否落在既有临界区内：复用 `select_and_record` 第 2 步已持有的全部 guard、无新锁、无 await、不延后任何请求；`retry_after` 只读已淘汰过的 deque，不在读取时二次淘汰。

本维度评估并发不变式是否被破坏。分类：essential。评审指引：锁按 ProviderTable 表序获取、临界区内无 await 是硬边界；把计算移出临界区（释放 guard 后重锁、或新开锁）即违反本维度，即使数学本身正确。

#### Anchors

**contrast**: 恢复时长计算是在 guard 全持有的同一段同步代码内，还是释放锁后重新加锁重算。

**score_2**:
```text
{
    let guards = candidates.iter().map(|c| c.tracker.lock().guard()).collect::<Vec<_>>(); // 取锁
    let weights = /* 算权重 */;
}                                                                                         // ← guards drop，临界区结束
// 临界区外重新逐张加锁算 retry_after：锁序成为新风险面，且已不是「零额外加锁」
let ra = providers.iter().map(|p| p.tracker.lock().retry_after(rpm, tpm)).min();
```

**score_4**:
```text
if weights.iter().all(|&w| w <= 0.0) {
    // 第 2 步取的 guard 仍全部持有——同一临界区、零额外加锁、无 await
    let ra = guards.iter().filter_map(|g| g.retry_after(rpm_limit, tpm_limit)).min();
    return Err(exhausted(protocol, model, ra));
}
```

### Test Coverage Quality

恢复数学与头行为是否被确定性测试钉住：3 条 src 单测覆盖 None / RPM / TPM 三分支（直接构造双端队列 + 可控 `Instant`，不依赖墙上时钟 sleep），集成测试端到端断言头存在、值域与消息体不变，`docs/tdd/mapping.md` 登记。

本维度评估测试的确定性与规格钉住程度。分类：important。评审指引：用 `sleep` 等真实窗口的测试属脆弱覆盖；不断言值域或消息体的头测试只是半覆盖；src 单测按 risks.md 惯例不入 mapping.md（集成测试才入）。

#### Anchors

**contrast**: 断言是否覆盖头存在、值域、消息体不变三点（测试均为确定性构造，不依赖墙上时钟）。

**score_2**:
```text
// 确定性构造：VecDeque + 可控 Instant，无 sleep
let mut requests = VecDeque::new();
requests.push_back(t0);
trigger_exhaustion(&guard, requests, now);
assert!(resp.headers().contains_key("retry-after"));            // 只断言头存在——值域与消息体不变没钉
```

**score_4**:
```text
// 确定性构造：VecDeque + 可控 Instant，无 sleep
let mut requests = VecDeque::new();
requests.push_back(t0);
trigger_exhaustion(&guard, requests, now);
assert!(resp.headers().contains_key("retry-after"));            // 头存在
let secs: u64 = resp.headers()["retry-after"].to_str().unwrap().parse().unwrap();
assert!((1..=60).contains(&secs));                              // 值为整数秒且 ∈ [1, 60]
assert_eq!(body_message, pre_change_body_message);              // 消息体与实现前逐字节一致
```

### Advisory Semantics Documentation

README / interfaces / data-model 是否如实陈述头是 advisory：最早**可能**恢复、最小退避而非预约（并发请求可抢先占用刚空出的槽位、在途请求的 token 回账可能重新压满 TPM——风险 R19）；R6 的 mitigation 由「后续可加」转为「已实现」是否同步。

本维度评估文档的诚实性。分类：important。评审指引：把头值说成「保证可重试时刻」或暗示预留座位即违反本维度；客户端 SDK 的标准做法本就是「头值 + 抖动」退避，文档应引导而非误导。

#### Anchors

**contrast**: 文档是明示最小退避语义，还是暗示预约。

**score_2**:
```text
429 响应附 Retry-After 头，客户端可按该值准时重试。      // 暗示预约：客户端做精确重试，高峰期必然再次撞 429
```

**score_4**:
```text
头值是池级「最早可能恢复」的秒数——最小退避而非预约：并发请求可能抢先占用刚空出的槽位，
在途请求的 token 回账也可能重新压满 TPM（风险 R19）。客户端应按「头值 + 抖动」退避。
```

### Project Guidelines Alignment

实现与 CLAUDE.md / SDD 文档体系约定的一致性：错误体走 house `{"error":{"code","message"}}` 信封、spec-first 流程（interfaces.md「新增或修改行为前，先改本文件对应条目，再改代码与测试」——本任务规格已预登记，实现须与之逐条核对）、`docs/tdd/mapping.md` 登记新集成测试、README 与 risks.md（R6 / R19）同步。

本维度评估项目规范的落地程度。分类：optional。评审指引：新增错误形态、绕过 mapping 登记、实现与 interfaces.md 的恢复时长语义段不一致、README 误导性描述均落在本维度。

#### Anchors

**contrast**: `docs/tdd/mapping.md` 是否登记 `llm_429_response_carries_retry_after` 一行。

**score_2**:
```text
// mapping.md 表尾——集成测试已写、登记缺失：
| llm_rpm_exhaustion_returns_429 | RPM 耗尽 → 429 | 状态码与消息体断言 |
（表在此终止，无 llm_429_response_carries_retry_after 行）
```

**score_4**:
```text
// mapping.md 新增表行（列：测试名 → 覆盖需求 → 验证要点）：
| llm_429_response_carries_retry_after | interfaces.md 的 Retry-After 条 / ADR-L24 | 头存在、值 ∈ [1,60]、消息体不变 |
```

### Single Source of Truth

恢复时刻的推导是否只在 `QuotaGuard::retry_after` 一处实现：balancer 的 429 分支与 error 的渲染只引用其结果（`Option<Duration>` / 二元组第二字段），任何地方都不重算 `requests[len - limit] + WINDOW` 或 TPM 从前累加式。

本维度评估恢复数学是否存在重复实现（点名承接 CK-15）。分类：important。评审指引：在 quota / balancer / error 三处搜索恢复时刻的推导痕迹；只要 balancer 或 error 内联了 RPM 下标式或 TPM 累加式（哪怕只是再算一遍用作 fallback），即落向 score_2；三处均只引用 `retry_after` 结果则落向 score_4。

#### Anchors

**contrast**: balancer 的 429 分支是内联重算恢复时刻，还是只调用 `QuotaGuard::retry_after` 取结果。

**score_2**:
```text
if weights.iter().all(|&w| w <= 0.0) {
    // 临界区内现场重算 RPM 恢复时刻——与 QuotaGuard::retry_after 的推导重复，两处必然一起漂移
    let ra = guards.iter().map(|g| {
        let reqs = g.requests();
        reqs.get(reqs.len() - rpm_limit).map(|t| *t + WINDOW)
    }).min();
    return Err(exhausted(protocol, model, ra));
}
```

**score_4**:
```text
if weights.iter().all(|&w| w <= 0.0) {
    // 只引用结果：恢复时刻的推导只在 QuotaGuard::retry_after 一处
    let ra = guards.iter().filter_map(|g| g.retry_after(rpm_limit, tpm_limit)).min();
    return Err(exhausted(protocol, model, ra));
}
```

**Test Strategy:**

**Criticality:** MEDIUM-HIGH — 429 是面向客户端的公共 HTTP 响应契约（状态码 + 错误体 + 头形态），多客户端可见，且改动触碰并发临界区；无鉴权 / 资金 / 数据损毁面，故不到 HIGH。

**Test Matrix:**

| Type | Size | Framework | Dependencies | Gate |
|---|---|---|---|---|
| unit（恢复数学：None / RPM / TPM 三分支 + 供应商级 max） | small | `cargo test`（`src/llm/quota.rs` 内 `#[test]`） | 无（直接构造 `VecDeque` + 可控 `Instant`） | Gate 1 |
| integration（429 头的端到端可见性） | medium | `cargo test`（`tests/llm_gateway.rs` 既有网关 harness） | 内嵌网关（临时端口）+ mock 上游（axum） | Gate 2 |

**Test Cases to Cover:**

文档登记（CK-14）与质量门（CK-17..19）非测试类型，由 Regular Checks 承载，此处不列；以下仅 unit / integration 用例。

#### CK-11: forward 的 `select_and_record` 错误是否改走 `ApiError::into_response()`，使头到达 `/llm` 响应？

- [integration] `llm_429_response_carries_retry_after` 的头存在断言即覆盖 CK-11——头只有经 `ApiError::into_response()` 渲染才能到达 `/llm` 响应，forward 若仍走 `proxy::error_response` 头即缺失（该集成测试同时承载 CK-13，测试总数不变）

#### CK-12: src/llm/quota.rs 是否存在 3 条单测钉住恢复数学？

- [unit] `quota_retry_after_none_when_unlimited_or_not_exhausted`：两维不限 → None；一维不限、另一维未耗尽 → None（同时钉住 CK-10）
- [unit] `quota_retry_after_rpm_tracks_evicting_entry`：`len == limit` 与 `len > limit` 两种长度下恢复时刻 = `requests[len - limit] + WINDOW`；双维同时耗尽时供应商级 = max（同时钉住 CK-7 / CK-9）
- [unit] `quota_retry_after_tpm_accumulates_from_front`：累加落点位于中间条目（非队首非队尾）时恢复时刻 = 该条 `+ WINDOW`（钉住 CK-8；P4 保证 `limit > 0` 故必终止）

#### CK-13: 集成测试是否断言头存在、值域与消息体不变？

- [integration] `llm_429_response_carries_retry_after`：供应商 `rpm_limit = Some(1)`，首请求 200、次请求 429 → 断言 `Retry-After` 头存在、值为整数秒且 ∈ [1, 60]、`code == 429` 且 `message` 与实现前快照逐字节一致（以当前代码实际输出的复合消息为基准，不在规格中硬编码字面量）；并在同测试内断言一个非 429 错误（如 503 无供应商）不携带该头（钉住 CK-2 / CK-3 / CK-4 / CK-5）
- [integration] 同场景下 `/__admin/llm/quota` 的 rpm remaining 仍为 0（既有 `llm_rpm_exhaustion_returns_429` 行为不回归）
- 边界说明（不新增测试）：`rpm_limit = 1` 且两次请求紧邻时恢复时刻 ≈ 记录时刻 + 60s → d ≈ 60 − ε → 头值命中 clamp 上界 60；d → 0+ 时头值命中下界 1（由既有「值 ∈ [1,60]」断言兜底）

**Definition of Done:**

- [ ] 全部 Checklist 条目为是（结构性条目 CK-1 / CK-6 保持结构性豁免，经代码审查与锁序推理验证，与 R18 登记的「审查驱动、测试是验证者」先例一致；CK-15 移出豁免，已由 Single Source of Truth rubric 维度承接；文档登记 CK-14 与质量门 CK-17..19 由 Regular Checks 承载；其余经测试验证）
- [ ] 3 条 src 单测与 1 条集成测试全部实现并通过
- [ ] `cargo build` / `cargo fmt` / `cargo clippy --all-targets` / `cargo test` 全部干净，既有 148+ 条测试零回归
- [ ] `docs/tdd/mapping.md` 新增对应行；README / risks.md（R6 转为「已实现」、R19 advisory 语义）与实现一致
- [ ] 429 消息体字节与变更前逐字节一致；`Retry-After` 头仅在 429 携带时长时出现，值为整数秒 ∈ [1, 60]
