# Step 01: Recovery Math in QuotaGuard (`retry_after`)

**Task File:** `.specs/tasks/todo/add-retry-after-llm-429.feature.md`

> The task file moves between `.specs/tasks/{draft,todo,in-progress,done}/` as work progresses; if it is not at this path, resolve it by its filename under `.specs/tasks/`.

**Phase:** Phase 1
**Model:** sonnet
**Agent:** sdd:developer
**Depends on:** None
**Parallel with:** None
**Note:** None

**Goal:** Give `QuotaTracker` exactly ONE place where a 429 recovery duration is derived —
`QuotaGuard::retry_after(rpm_limit, tpm_limit) -> Option<Duration>` — pinned by 3 deterministic
in-file unit tests that inject backdated `Instant`s, so the recovery math is proven before any
caller exists and nothing downstream is allowed to re-derive it.

Build this in `src/llm/quota.rs` ONLY. Do not touch `src/error.rs`, `src/llm/balancer.rs`,
`src/llm/forward.rs` or any test file in this step — a later step wires the callers, and until
then `retry_after` is intentionally inert (dead-code warnings from an unused `pub` method are
fine; if `cargo` complains, keep the method `pub` — it is part of a `pub` `impl` on a `pub`
struct in a `lib` crate, so it stays reachable regardless).

The ADR-L24 formulas (from the task file `## Architecture Overview`, non-negotiable):

- **RPM recovery**: gate on `rpm_limit.is_some() && requests.len() >= limit`; recovery instant =
  `requests[len - limit] + WINDOW` (when `len == limit` this is `requests.front()`; evicting that
  entry leaves `limit - 1 < limit` in window, i.e. the dimension is no longer exhausted).
- **TPM recovery**: gate on `tpm_limit.is_some() && tpm_used() >= limit`; walk the `tokens` deque
  from the front, accumulating the evicted token amounts, and find the FIRST entry whose
  cumulative- inclusive amount makes the remaining sum `< limit`; recovery instant = that entry
  `+ WINDOW`. `limit > 0` is guaranteed by validation, so the walk always terminates (evicting
  everything leaves `0 < limit`).
- **Provider level**: `max` of the two present dimension recoveries — the dual of
  `weight = min`: a provider is schedulable again only when BOTH its limits have recovered. A
  dimension that is unlimited (no limit) or not exhausted contributes `None` (gate fails) and the
  `max` skips it.
- **Return value**: the remaining `Duration` from now, i.e. recovery instant
  `.saturating_duration_since(Instant::now())` — always `<= WINDOW`.

Design requirements so the math is testable deterministically and never re-evicts:

- The RPM/TPM recovery helpers are **module-private** and take the deque, the limit and
  `now: Instant` as parameters (so tests inject controlled instants and assert EXACT instants,
  never wall-clock arithmetic).
- `retry_after` is a `&self` method on `QuotaGuard` — the guard is already the held lock and
  `lock()` has already evicted window-expired entries before the guard was handed out
  (`src/llm/quota.rs:78-84`), so `retry_after` must NOT lock anything, must NOT await, and must
  NOT evict again. It reads the post-eviction deques only.
- Keep the existing module layout: `WINDOW` stays `Duration::from_secs(60)`; helpers sit next to
  `evict` / `evict_pairs` (file bottom), and the test module follows the in-file
  `#[cfg(test)] mod tests` pattern used in `src/llm/usage.rs` (~line 319).

The 3 unit tests (names are fixed by the task file's Test Cases to Cover — do not rename):

- `quota_retry_after_none_when_unlimited_or_not_exhausted` — both dimensions unlimited → `None`;
  one dimension unlimited and the other with a limit but under-used → `None` (pins CK-10).
- `quota_retry_after_rpm_tracks_evicting_entry` — `requests.len() == limit` and
  `requests.len() > limit` both assert recovery == `requests[len - limit] + WINDOW`; add a case
  with BOTH dimensions exhausted (TPM with a mid-deque landing point) asserting the provider
  level is the `max` of the two recoveries (pins CK-7 / CK-9 / CK-12).
- `quota_retry_after_tpm_accumulates_from_front` — the accumulate landing point sits on a
  MIDDLE entry (not the front, not the back) — e.g. a deque where evicting only the front entry
  still leaves `remaining >= limit` but evicting the first two drops below — and assert recovery
  == that middle entry `+ WINDOW` (pins CK-8 / CK-12).

Injection technique for the tests: from the in-file `#[cfg(test)]` child module,
`tracker.inner.lock().unwrap().requests.push_back(Instant::now() - Duration::from_secs(N))`
and `.tokens.push_back((Instant::now() - Duration::from_secs(N), tokens))` reach the private
fields directly; backdate by `< 60s` so the entries survive `lock()`'s eviction, then call
`tracker.lock().retry_after(...)` and compare against the known injected instants
(`t_injected + WINDOW`, or the remaining duration derived from it). No `sleep`, no
`tokio::time::pause` — plain synchronous `#[test]`s.

#### Expected Output

- `src/llm/quota.rs` — `QuotaGuard::retry_after` + module-private RPM/TPM helpers + `#[cfg(test)] mod tests` with the 3 named tests
- No other file changes

#### Success Criteria

- [X] `QuotaGuard::retry_after(&self, rpm_limit: Option<u64>, tpm_limit: Option<u64>) -> Option<Duration>` exists and is `pub` on the existing guard impl
- [X] Unlimited or not-yet-exhausted dimensions yield `None`; only a genuinely exhausted (limit set, used >= limit) dimension yields `Some`
- [X] RPM recovery equals `requests[len - limit] + WINDOW` for both `len == limit` and `len > limit`, asserted with injected (non-wall-clock) instants
- [X] TPM recovery equals the first middle/front entry whose inclusive cumulative eviction drops the remaining sum below `limit`, `+ WINDOW`, demonstrated with a mid-deque landing point
- [X] Provider-level result is the `max` of the two exhausted dimensions' recoveries
- [X] `retry_after` takes `&self`, performs no lock, no eviction, no await, and reads only post-eviction deques
- [X] `cargo test --lib quota` runs the 3 tests green with zero `sleep`
- [X] `cargo fmt` and `cargo clippy --all-targets` stay clean; existing 148+ tests stay green (the module is otherwise unchanged)

#### Subtasks

- [X] Add `retry_after` to `impl<'a> QuotaGuard<'a>` in `src/llm/quota.rs`, computing the remaining `Duration` from the provider-level recovery instant
- [X] Implement the module-private RPM recovery helper (`requests[len - limit] + WINDOW`, gated on limit set and `len >= limit`, explicit `now` parameter)
- [X] Implement the module-private TPM recovery helper (front-accumulate until remaining `< limit`, that entry `+ WINDOW`, explicit `now` parameter)
- [X] Write `#[cfg(test)] mod tests` in `src/llm/quota.rs` with the 3 named tests using backdated-`Instant` injection (no sleep)
- [X] Run `cargo test`, `cargo fmt`, `cargo clippy --all-targets` and confirm all green with no regressions

#### Blockers & Risks

| Type | Item | Impact | Likelihood | Mitigation / Resolution |
|------|------|--------|------------|-------------------------|
| Risk | Index underflow on `requests[len - limit]` if the exhaustion gate is inverted or the deque is read pre-eviction | High | Low | Gate inside the helper on `limit.is_some() && requests.len() >= limit`; the `None_when_unlimited_or_not_exhausted` test pins the gate; only ever read deques via a guard from `lock()` |
| Risk | TPM accumulator walk compared against the wrong total (e.g. current `tpm_used()` re-counted mid-walk) or off-by-one on the inclusive cumulative comparison | High | Medium | Compute the total once before the walk (`tpm_used()`), accumulate over the entries as you advance, compare `total.saturating_sub(cumulative_inclusive) < limit`; the mid-deque landing test pins the off-by-one |
| Risk | Tests become wall-clock dependent (asserting an exact remaining duration that drifts with test runtime) instead of exact recovery instants | Medium | Medium | Parameterize helpers with an explicit `now: Instant` and assert on the recovery instant (`t + WINDOW`); derive the remaining-duration check from injected instants only |
| Blocker | Called code may expect `retry_after` to be callable without holding the tracker — it is not; a later step calls it through `QuotaGuard` inside `select_and_record` | Low | Low | This is by design (guard == lock, rubric "Critical-Section Discipline"); no action needed here |

**Implementation notes (no deviations from spec):**

- `retry_after` sits at `src/llm/quota.rs:53`; helpers `rpm_recovery` (line 172) and `tpm_recovery` (line 188) sit immediately after `evict`/`evict_pairs`; `#[cfg(test)] mod tests` at the file bottom mirrors `src/llm/usage.rs:319`.
- `Option::max` implements the provider-level max and inherently skips the `None` dimension (unlimited / not exhausted); no dead-code warning — the `pub` method on a `pub` struct in the `lib` crate stays reachable as predicted.
- Tests inject instants via `secs_ago(Instant::now(), n)` (`checked_sub`, never wall-clock subtraction) and assert exact `Duration`s at helper level plus a `(secs-1, secs]` band at guard level; no `sleep`, no `tokio::time::pause`.
- Cargo verdicts: `cargo test --lib quota` 3/3 green in 0.00s; full `cargo test` 151 green (57 lib = 54 pre-existing + 3 new; 18+61+15 integration); `cargo fmt --check` clean; `cargo clippy --all-targets` exit 0 with zero warnings; `git diff --stat` shows only `src/llm/quota.rs` (207 insertions, 0 deletions) — an mtime audit confirmed the workspace-wide `cargo fmt` touched no other file.
