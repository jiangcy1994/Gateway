# Step 04: Docs Consistency Check (README / interfaces / decisions / data-model / risks / cycles)

**Task File:** `.specs/tasks/todo/add-retry-after-llm-429.feature.md`

> The task file moves between `.specs/tasks/{draft,todo,in-progress,done}/` as work progresses; if it is not at this path, resolve it by its filename under `.specs/tasks/`.

**Phase:** Phase 2
**Model:** sonnet
**Agent:** sdd:tech-writer
**Depends on:** `02-429-path-wiring`
**Parallel with:** `03-integration-test-and-mapping`
**Note:** None

**Goal:** Make sure every document that mentions LLM 429 behavior states the ADR-L24 semantics
exactly as the implementation now renders them — correction of drift toward the pre-registered
target text only, never a rewrite.

The task file's `## Architecture Overview` pre-registered the target state for these docs
("均已预登记为目标态，实现期只需核对一致性（若有漂移按已登记文本校正，不重写语义）"). So this
step is READ + VERIFY, then surgically correct drift:

- `README.md` — 429 behavior mentions: after this change a 429 carries `Retry-After` (integer
  seconds, delta-seconds), and it is a **minimum backoff, not a reservation** (R19).
- `docs/sdd/llm/interfaces.md` — the LLM module's implementation authority: must carry the
  Retry-After entry stating header presence condition (`QuotaExhausted` with a recovery), the
  rendering (`ceil(d)` seconds clamped to `[1, 60]`), and the advisory semantics.
- `docs/sdd/llm/decisions.md` — ADR-L24 present and consistent with the implemented rendering;
  no competing decision text.
- `docs/sdd/llm/data-model.md` — `ApiError::QuotaExhausted(String, Option<Duration>)` as the
  current shape; the duration is NOT part of the message.
- `docs/sdd/llm/risks.md` — **R6 flipped to 已实现** (was: 429 lacks retry guidance); **R19**
  kept as an advisory risk: a client that retries exactly at the header value without jitter can
  still receive a second 429 — the header is a lower bound, not a slot reservation. Register R19's
  timing reality as-is; do NOT mask it.
- `docs/cycles.md` — the C31 red/green entry for this feature references the fixed integration
  test name `llm_429_response_carries_retry_after`; make sure the name matches exactly what step
  `03-integration-test-and-mapping` wrote.

Hard constraints:

- Verify each claim against the ACTUAL implemented behavior in `src/error.rs` /
  `src/llm/quota.rs` / `src/llm/balancer.rs` (read them; do not trust your prior reading of the
  task file alone) — the source you are verifying against is the code from step
  `02-429-path-wiring`.
- Edits are limited to reconciling wording with the registered target state. Do NOT invent new
  semantics, do NOT restructure sections, do NOT add unregistered ADRs.
- **Do NOT edit `docs/tdd/mapping.md`** — that file is exclusively owned by the parallel step
  `03-integration-test-and-mapping` in this phase.
- Do NOT edit any `src/` file; if you believe the CODE drifted from the docs rather than the docs
  from the code, report it instead of changing code.

#### Expected Output

- Drift corrections (if any) in `README.md`, `docs/sdd/llm/interfaces.md`, `docs/sdd/llm/decisions.md`, `docs/sdd/llm/data-model.md`, `docs/sdd/llm/risks.md`, `docs/cycles.md`
- All six docs consistent with the implemented `Retry-After` semantics: present only on 429 with a
  known recovery, integer delta-seconds = `ceil(d)` clamped to `[1, 60]`, minimum-backoff wording
  per R19, R6 marked 已实现

#### Success Criteria

- [ ] Every 429-related passage in the six docs matches implemented behavior; any drift is corrected TOWARD the registered target text
- [ ] R6 is marked 已实现 in `docs/sdd/llm/risks.md` and R19 remains registered as an advisory risk with the exact-retry-can-still-429 timing reality stated, not masked
- [ ] The `Retry-After` semantics are documented as a minimum backoff lower bound (NOT a reservation) in both `README.md` and `docs/sdd/llm/interfaces.md` (CK-16 / CK-24)
- [ ] Rendering wording states integer seconds, `ceil`, clamp `[1, 60]`, delta-seconds
- [ ] `docs/cycles.md`'s C31 entry references the exact test name `llm_429_response_carries_retry_after`
- [ ] `docs/tdd/mapping.md` untouched; zero `src/` file edits; no section restructures or new ADRs

#### Subtasks

- [ ] Read `src/error.rs`, `src/llm/quota.rs`, `src/llm/balancer.rs` from step 02 to fix the actual rendering semantics in mind
- [ ] Check `README.md` + `docs/sdd/llm/interfaces.md` for the 429 / `Retry-After` entry (presence condition, `ceil` + `[1,60]` integer delta-seconds, minimum-backoff wording) and correct drift
- [ ] Check `docs/sdd/llm/decisions.md` (ADR-L24) + `docs/sdd/llm/data-model.md` (variant shape) and correct drift
- [ ] Flip R6 → 已实现 in `docs/sdd/llm/risks.md` and confirm R19's advisory wording (exact retry can still 429) is stated, not softened
- [ ] Confirm the `docs/cycles.md` C31 red/green entry names `llm_429_response_carries_retry_after` exactly

#### Blockers & Risks

| Type | Item | Impact | Likelihood | Mitigation / Resolution |
|------|------|--------|------------|-------------------------|
| Risk | Doc wording drifts into "reservation"/guaranteed-slot semantics instead of minimum backoff (R19) | High | Medium | Verify against R19's registered wording; Success Criteria explicitly forbid it (CK-16 / CK-24) |
| Risk | Over-editing: restructuring sections or adding unregistered ADRs while "fixing drift" | Medium | Low | Edits limited to reconciling with the registered target state; no new ADRs, no restructures |
| Blocker | Parallel step `03-integration-test-and-mapping` owns `docs/tdd/mapping.md` and `tests/llm_gateway.rs`; this step must not touch either | Low | Low | file exclusivity stated in this step's constraints |
| Risk | Docs disagree with code because the DOC reading was stale, not the code | Medium | Low | Re-read the step-02 sources first, then verify docs against code |
