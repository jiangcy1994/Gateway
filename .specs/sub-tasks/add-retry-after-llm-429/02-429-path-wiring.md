# Step 02: Wire the 429 Path — Variant Field, Header Rendering, Pool-Min Aggregation, Forward Switch

**Task File:** `.specs/tasks/todo/add-retry-after-llm-429.feature.md`

> The task file moves between `.specs/tasks/{draft,todo,in-progress,done}/` as work progresses; if it is not at this path, resolve it by its filename under `.specs/tasks/`.

**Phase:** Phase 2
**Model:** opus
**Agent:** sdd:developer
**Depends on:** `01-quota-retry-after`
**Parallel with:** None
**Note:** None

**Goal:** Thread the recovery duration from the still-held `QuotaGuard`s in
`balancer::select_and_record`'s 429 branch into `ApiError` and out onto the HTTP response as a
`Retry-After` header — as one atomic, build-green change across `src/error.rs`,
`src/llm/balancer.rs` and `src/llm/forward.rs`, with the 429 response body byte-identical to
before and the existing critical-section invariants untouched.

This step is one change, not three: `ApiError::QuotaExhausted` becomes a two-field tuple and
Rust will not compile while `src/llm/balancer.rs:117` and `:120` still construct the one-field
form. All three files in this step must land together. Do not restructure beyond what is listed
here — this is an additive change to a SETTLED architecture (task file `## Architecture
Overview`), and `src/proxy.rs`, `src/llm/admin.rs`, `src/llm/provider.rs`, `src/llm/ledger.rs`
stay untouched.

**1. `src/error.rs`** (the house render authority):

- Change the variant to `QuotaExhausted(String, Option<Duration>)` and add the
  `std::time::Duration` and `axum::http::HeaderValue` imports. The `#[error("...")]` attribute
  MUST stay reading only `{0}` — the Display output is byte-identical to the current single-field
  form (rubric "429 Body Backward Compatibility" score_4). Never let the duration reach the
  message.
- Update the `status()` match arm to the new arity (still `StatusCode::TOO_MANY_REQUESTS`).
- In `impl IntoResponse for ApiError` (the single house render path, ~lines 138-149): keep the
  existing `(status, Json(json!({ "error": { "code": status.as_u16(), "message":
  self.to_string() } })))` body assembly EXACTLY as-is — same status source, same json! envelope,
  same message bytes. Before/while assembling, match on `self` by reference to detect
  `ApiError::QuotaExhausted(_, Some(d))` and, only then, insert the header after the response is
  built: `Retry-After` via the typed constant (e.g. `axum::http::header::RETRY_AFTER`), value =
  integer delta-seconds `(d.as_secs_f64().ceil().clamp(1.0, 60.0)) as u64` (rubric "Rendering
  Conformance" score_4; `HeaderValue::from(secs)` per the anchor — if the http version's `From`
  impl does not cover the integer, fall back to `HeaderValue::from_str(&secs.to_string())`,
  which is infallible for digits). All other variants — 404 / 503 / 401 / 403 / 413 and
  `QuotaExhausted(_, None)` — MUST NOT carry the header (CK-2 / CK-5). Use the `RETRY_AFTER`
  typed constant; string literals like `"retry-after"` are acceptable only for test-side
  `resp.headers()["retry-after"]` lookups.

**2. `src/llm/balancer.rs`** (the only place aggregation happens):

- Add `use std::time::Duration;` (needed by the new `exhausted` signature).
- In `select_and_record`'s 429 branch (the `weights.iter().all(|&w| w <= 0.0)` arm, ~lines
  76-78): all `guards` are STILL HELD — the function is sync, no `await` anywhere in the critical
  section, trackers locked in `ProviderTable` table order. Compute the POOL-level retry-after
  right there as
  `guards.iter().zip(candidates...).filter_map(|g| g.retry_after(provider.rpm_limit, provider.tpm_limit)).min()`
  (pair each candidate's provider limits with its tracker's guard, matching how the weights
  are computed; rubric "Critical-Section Discipline" + "Single Source of Truth" score_4).
  The `filter_map` is MANDATORY: `Option`'s `Ord` orders `None` before `Some`, so a bare
  `.min()` over `Option<Duration>` items would collapse to `None` whenever ANY candidate is
  unlimited (which happens exactly on model-less/protocol-level requests given a pool of them).
  Guard for that in code where a candidate's recovery is `None`.
- Pass the aggregated `Option<Duration>` into `exhausted(protocol, model, retry_after)` and keep
  the message-STRING GENERATION BYTE-IDENTICAL to the current two construction sites
  (`exhausted()` at ~lines 115-122, `Debug` derive already covers `Option<Duration>` so no derive
  change is needed). No locks, no awaits, no re-locking, no computation moved outside the held
  guards (CK-6). Do NOT re-derive any recovery math here — `QuotaGuard::retry_after` from step
  `01-quota-retry-after` is the single source (CK-15).

**3. `src/llm/forward.rs`** (~line 160-165):

- Switch the `select_and_record` error arm from
  `Err(e) => return error_response(e.status(), &e.to_string())` to `Err(e) => return
  e.into_response()` (`IntoResponse` is already imported at line 8) and refresh the comment so
  it states `ApiError::into_response()` is the house render authority and that `Retry-After`
  rides on `QuotaExhausted` (CK-11). `proxy::error_response` stays imported and unchanged for all
  its other call sites in this file (15 remaining sites after the one switched here, including the
  client-build 500 at ~line 242).

#### Expected Output

- `src/error.rs` — two-field variant, {0}-only Display, status arm, `IntoResponse` header insertion, imports
- `src/llm/balancer.rs` — pool-min aggregation in the 429 branch, `exhausted()` signature, `Duration` import
- `src/llm/forward.rs` — one-line render switch + comment
- Clean `cargo build` / `cargo fmt` / `cargo clippy --all-targets` (existing tests green; two existing 429 tests must pass UNCHANGED — they are the byte-identity regression net for this step)

#### Success Criteria

- [X] `ApiError::QuotaExhausted` is `(String, Option<Duration>)` and its `#[error]` Display output is byte-identical to the pre-change single-field form (no `{1}` / `{1:?}` anywhere)
- [X] `IntoResponse` inserts `Retry-After` ONLY for `QuotaExhausted` with `Some(d)`; no other variant and no `None` case ever carries it
- [X] Header value parses as an integer, equals `ceil(d)` seconds clamped to `[1, 60]`
- [X] `exhausted()` message strings are byte-identical to before; `llm_rpm_exhaustion_returns_429` and `llm_all_tiers_exhausted_returns_429` pass with zero modifications
- [X] Pool-level retry-after is computed with `filter_map(...).min()` inside the already-held-guards 429 branch — no new lock, no `await`, no re-lock, no recovery math re-derived in balancer or error
- [X] `forward`'s `select_and_record` error renders via `ApiError::into_response()`; all other `error_response` call sites in the file are untouched
- [X] `cargo build`, `cargo fmt`, `cargo clippy --all-targets` all clean; full `cargo test` green with no test-file edits

> Note (step 02 closure): the [1, 60] floor is implemented but NOT pinned by any test in this
> step — the mutation check (clamp 1.0 → 0.0) left all 151 tests green; step 03's
> `llm_429_response_carries_retry_after` is the intended integration-level pin.

#### Subtasks

- [X] `src/error.rs`: change `QuotaExhausted` to `(String, Option<Duration>)`, keep the {0}-only `#[error]`, update the `status()` match arm, add `Duration` / `HeaderValue` imports
- [X] `src/error.rs`: insert `Retry-After` (typed constant, `ceil` + clamp `[1,60]` integer) in `IntoResponse` only for `QuotaExhausted(_, Some(d))`, body assembly otherwise unchanged
- [X] `src/llm/balancer.rs`: compute pool-min via `guards` + candidate limits with `filter_map(...).min()` inside the all-weights-zero branch, guards still held
- [X] `src/llm/balancer.rs`: change `exhausted()` to take the aggregated `Option<Duration>` and pass it to `QuotaExhausted`, message strings byte-identical; add the `Duration` import
- [X] `src/llm/forward.rs`: switch the `select_and_record` error arm to `e.into_response()` and update the comment
- [X] Run `cargo build`, `cargo fmt`, `cargo clippy --all-targets`, `cargo test` — all clean, existing 148+ tests green, zero test edits

#### Blockers & Risks

| Type | Item | Impact | Likelihood | Mitigation / Resolution |
|------|------|--------|------------|-------------------------|
| Risk | Pool min written as `.min()` over `Option<Duration>` items (no `filter_map`): `None < Some` in `Option`'s `Ord`, so one unlimited candidate collapses the header to absent | High | Medium | MANDATORY `filter_map(...).min()`; a model-less (protocol-level) request where some pool member has no limits is the trigger case — include it in the reviewer's checklist (CK-9) |
| Risk | Entity changes break the byte-identity of the 429 body (Display leaking `{1}`, or `exhausted()` text rewrite) | High | Low | Keep `#[error]` {0}-only and copy `exhausted()`'s strings verbatim; the two existing 429 tests are the regression net and must pass unmodified |
| Risk | Critical-section discipline broken by moving the aggregation off the held guards (re-lock, new lock, or an `await` sneaking in) | High | Low | Compute inside the `weights.iter().all(...)` arm where `guards` is live; the function is sync — no `await` may appear between `lock()` and the `return Err(...)` |
| Risk | Variant arity change breaks other construction sites than the two known ones | Medium | Low | Verified: `QuotaExhausted` appears at definition (`src/error.rs:42`), the status match (`:116`), and exactly two constructions (`src/llm/balancer.rs:117`, `:120`); `forward` only renders. If new sites surface, they go in this same step |
| Risk | `as u64` cast or match ergonomics trip `cargo clippy --all-targets` (project keeps it zero-warning) | Medium | Low | Clamp before the cast; run clippy in this step and fix before declaring done |
| Blocker | Step must produce a compiling crate — `src/llm/balancer.rs` will not compile against the new arity until both files are updated | High | High (by design) | Do all three files in this step; there is intentionally no intermediate-compiling partial state worth shipping |
