# Step 03: Integration Test `llm_429_response_carries_retry_after` + `docs/tdd/mapping.md` Row

**Task File:** `.specs/tasks/todo/add-retry-after-llm-429.feature.md`

> The task file moves between `.specs/tasks/{draft,todo,in-progress,done}/` as work progresses; if it is not at this path, resolve it by its filename under `.specs/tasks/`.

**Phase:** Phase 2
**Model:** sonnet
**Agent:** sdd:developer
**Depends on:** `02-429-path-wiring`
**Parallel with:** `04-docs-consistency`
**Note:** None

**Goal:** Prove end-to-end that a 429 from `/llm/*` carries a standard `Retry-After` header while
the response body stays byte-identical to the pre-change composite — and register that test in
`docs/tdd/mapping.md` so the requirement stays nailed to a spec entry.

Add ONE integration test to `tests/llm_gateway.rs` and ONE row to `docs/tdd/mapping.md`. Reuse
the file's existing harness — `spawn_llm_gateway` (~line 94), `gateway_url` (~line 131),
`authed()` (~line 144), `error_body` (~line 152), `quota_view` (~line 248) — and mirror the
shape of `llm_rpm_exhaustion_returns_429` (~lines 594-623), which is the closest existing 429
test. Do not modify any existing test; this step is additive.

Test `llm_429_response_carries_retry_after` (name fixed by the task file — do not rename):

- Provider configured with `rpm_limit = Some(1)`, mock upstream returning 200.
- First `POST /llm/openai/v1/chat/completions` with `{"model": "..."}` → 200 (consumes the single
  RPM slot).
- Second identical request → 429, then assert:
  - the `Retry-After` header IS present;
  - its value parses as **integer** seconds and `1 <= secs <= 60`;
  - `code == 429` and the `message` equals the pre-change composite MESSAGE BYTE-FOR-BYTE — the
    baseline is what `exhausted()`'s text yields after thiserror's `Display` wrapping (see
    `src/llm/balancer.rs` and `src/error.rs`); capture the current 429 body from THIS scenario
    (run it against the step-02 code) and pin that captured string in the test. Do not
    paraphrase, do not derive it from a spec sentence (CK-4 / rubric "429 Body Backward
    Compatibility").
- In the same test, assert one non-429 error response (e.g. a 503 model-declared-but-pool-empty
  case, mirroring `llm_model_all_disabled_returns_503`) carries NO `Retry-After` header (CK-5).
- Inline assertion that `/__admin/llm/quota` still reports rpm `remaining == 0` for the scenario
  (existing `llm_rpm_exhaustion_returns_429` behavior does not regress).

Boundary note (do NOT write a separate test for it): with `rpm_limit = 1` and two requests back
to back, recovery is `t_recorded + WINDOW`, so `d ≈ 60 − ε` and the header hits the clamp ceiling
`60`; the `d → 0+` lower bound `1` is covered by the same `∈ [1, 60]` assertion.

`docs/tdd/mapping.md`: add ONE row to the LLM 表 (`## LLM 子网关（\`tests/llm_gateway.rs\`）`
table, rows start ~line 33; place it next to the other 429 rows, e.g. after
`llm_all_tiers_exhausted_returns_429` at ~line 69) with the canonical three columns
`| 测试函数 | 覆盖需求（来源） | 验证要点 |`:

- 测试函数: `` `llm_429_response_carries_retry_after` ``
- 覆盖需求（来源）: `ADR-L24 / interfaces.md 的 Retry-After 条 / llm/forward 后置第 3 步`
- 验证要点: `429 携带 Retry-After（整数秒 ∈ [1,60]，delta-seconds）；消息体与变更前逐字节一致；同测试内非 429 错误不带该头；内联 quota_view 钉住 remaining == 0`

Per the file's own header rule (and `docs/sdd/llm/risks.md` convention) the 3 src unit tests from
step `01-quota-retry-after` do NOT go in this table — only integration tests are registered
(CK-14).

#### Expected Output

- `tests/llm_gateway.rs` — new test `llm_429_response_carries_retry_after` (plus any private helper reuse; no new harness)
- `docs/tdd/mapping.md` — one new row in the LLM table

#### Success Criteria

- [X] `llm_429_response_carries_retry_after` exists, passes, and asserts header presence, integer value ∈ [1, 60], `code == 429`, and the exact pre-change composite message
- [X] A non-429 error in the same test carries no `Retry-After` header
- [X] The test asserts `/__admin/llm/quota` rpm remaining == 0 for the exhausted scenario (no regression of the existing snapshot behavior)
- [X] The test is deterministic — no `sleep` waiting on the 60s window (the header value is asserted by RANGE, never an exact second, since `Instant` is not injectable here)
- [X] `docs/tdd/mapping.md` gains exactly one row in the LLM table with the three canonical columns, and src unit tests are NOT registered
- [X] Full `cargo test` green: existing 148+ tests plus this one; no existing test edited

#### Subtasks

- [X] Write `llm_429_response_carries_retry_after` in `tests/llm_gateway.rs` reusing `spawn_llm_gateway` / `gateway_url` / `authed` / `error_body` / `quota_view`
- [X] Capture the current 429 composite message for this scenario (run the new test's scenario against the step-02 code) and pin it byte-for-byte in the assertion
- [X] Add the non-429 no-header assertion and the inline `quota_view` remaining == 0 assertion
- [X] Add the `llm_429_response_carries_retry_after` row to `docs/tdd/mapping.md`'s LLM table next to the other 429 rows
- [X] Run `cargo test`, `cargo fmt`, `cargo clippy --all-targets` — all clean, zero existing-test edits

#### Blockers & Risks

| Type | Item | Impact | Likelihood | Mitigation / Resolution |
|------|------|--------|------------|-------------------------|
| Risk | Message snapshot asserted from a paraphrase instead of the actual composite, so a real body change would slip past | High | Medium | Capture the body from the running scenario (`error_body` → `message`), then pin that exact string; the composite is `exhausted()`'s text wrapped by `ApiError`'s thiserror `Display` |
| Risk | Test asserts an exact `Retry-After` second value, making it wall-clock flaky | Medium | Medium | Assert integer-parseable and `1..=60` only; the clamp ceiling note explains why `60` is the expected value without asserting it exactly |
| Risk | Second 429 request observes a slot freed by the first request's TTL — impossible within a 60s window, but a test that sleeps to wait for recovery would be flaky AND slow | Medium | Low | No sleep anywhere; the quota window is 60s and the test runs in well under that |
| Blocker | This step runs in parallel with `04-docs-consistency`, which must NOT edit `docs/tdd/mapping.md` | Low | Low | mapping.md is exclusively owned by THIS step; stated in `04-docs-consistency` |
