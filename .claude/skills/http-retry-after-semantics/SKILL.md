---
name: http-retry-after-semantics
description: HTTP Retry-After header semantics, client SDK consumption behavior, and the axum recipe for rendering an Option<Duration> into the header
---

# HTTP `Retry-After` Semantics

## Role

Reference for anyone adding a `Retry-After` response header to an HTTP service
in this repo: what the header means on the wire, which of its two forms real
clients actually read, and how to render a `Duration` into it correctly in axum.

## Goal

Make every `Retry-After` decision (which status may carry it, which value form
to emit, how to round/clamp it) traceable to verified references instead of
folklore, so the header is honored by real clients and not just spec-cosmetic.

## Key Concepts

- **Two value forms**: `Retry-After = HTTP-date / delay-seconds` (RFC 9110
  §10.2.3 ABNF); `delay-seconds = 1*DIGIT` — a non-negative decimal integer of
  seconds. Example: `Retry-After: 120`. The ABNF production name in RFC 9110 is
  `delay-seconds`; "delta-seconds" is the older RFC 7231 / MDN wording for the
  same form.
- **Advisory, never a reservation**: RFC 9110 only ever says the user agent
  "ought to wait" / is "expected" / is "asked to" wait. Nothing promises the
  resource will be free at that moment, and no capacity is reserved. Document
  the value as a *minimum backoff*, not a guaranteed retry slot — concurrent
  requests may take the slot the instant it frees.
- **Which status may carry it**: RFC 9110 §10.2.3 itself mentions only 503
  (service *expected* unavailable) and 3xx (minimum wait before the redirected
  request). The authority for **429** is **RFC 6585 §4**: the representation
  SHOULD include details, and MAY include a `Retry-After` indicating how long
  to wait; 429 responses MUST NOT be stored by a cache. Cite RFC 6585 §4 for
  429, not RFC 9110.
- **What real SDKs read**: the OpenAI Python SDK (`openai-python`,
  `src/openai/_base_client.py`) parses `retry-after-ms` first, then
  `retry-after` as a **number**, and only then falls back to HTTP-date
  (`email.utils.parsedate_tz`). `_calculate_retry_timeout` honors the header
  only when `0 < retry_after <= MAX_RETRY_AFTER_DELAY` with
  `MAX_RETRY_AFTER_DELAY = 2 * 60` — **a value of 0 is discarded** and the SDK
  falls back to its own exponential backoff (0.5s → 8.0s, 0–25% jitter).
  Conclusion: emit **integer delay-seconds in (0, 120]**; HTTP-date is a
  compat fallback, not a requirement.
- **Who does NOT read it**: `reqwest` 0.12 exposes `reqwest::retry` but never
  references 429 or `Retry-After` internally — retry classification is
  caller-supplied, so an upstream client never auto-honors the header. The
  WHATWG Fetch spec contains `retry-after` **zero times**: browsers do not
  auto-retry on it, and it is **not** in the CORS-safelisted response-header
  set, so cross-origin JS cannot read it unless the response also sends
  `Access-Control-Expose-Headers: Retry-After`.

## Instructions (axum Rendering Recipe)

Given an `Option<Duration>` (or any `Duration` you have decided to advertise),
render the header on the **already-built** response:

```rust
use axum::http::header::RETRY_AFTER;
use axum::http::HeaderValue;

// `response: Response` built earlier, `retry_after: Option<Duration>`
if let Some(retry_after) = retry_after {
    let secs = retry_after.as_secs_f64().ceil().clamp(1.0, 60.0) as u64;
    if let Ok(value) = HeaderValue::from_str(&secs.to_string()) {
        response.headers_mut().insert(RETRY_AFTER, value);
    }
}
```

Why each step:

- **`ceil`, not `as_secs()`**: `as_secs()` truncates — a 0.9 s wait would
  render `0`, which SDKs (see above) **discard**, silently reverting to blind
  exponential backoff. Ceiling always rounds up to at least 1 s.
- **clamp `[1, 60]`**: lower bound 1 guarantees the value is never 0 (and
  stays inside the SDK's 120 s honor window with margin); upper bound 60 caps
  the advertised wait. `f64::clamp` cannot panic here because
  `1.0 <= 60.0`. The clamp is defensive formatting — clamp even when your
  domain math claims the raw value is always in range.
- **`HeaderValue::from_str` + `if let Ok`**: returns `Result`; a pure decimal
  ASCII integer string is always legal, so the guard is purely defensive.
- **`response.headers_mut()`** (from `http::Response`) returns `&mut HeaderMap`
  directly — **not** an `Option`, unlike `Response::builder().headers_mut()`.
  `HeaderMap::insert` replaces any existing value wholesale.
- **Use the typed constant `RETRY_AFTER`**: axum 0.8 re-exports the `http`
  crate as `axum::http` (so `axum::http::header::RETRY_AFTER` is exactly
  `http::header::RETRY_AFTER`); the constant eliminates spelling-mistake
  classes that a `"retry-after"` string literal invites.

## Constraints

- Send the header **only** on statuses whose semantics license it (429 per
  RFC 6585 §4, 503 / 3xx per RFC 9110 §10.2.3). A `Retry-After` on a 404 or
  502 is a defect, not a hint.
- Never let the value leak into the error **body** as text: the header is
  additive to the response, and clients key retry logic off the status code,
  not the body shape.
- If the service computes the value from a rolling window / ledger, derive it
  from the record that tripped the threshold and treat the result as the
  *earliest possible* recovery — the advisory semantics above still apply.

## Common Pitfalls

| Pitfall | Impact | Fix |
|---|---|---|
| `as_secs()` truncation → value `0` | SDK discards header, blind backoff | `ceil` + clamp lower bound 1 |
| Emitting HTTP-date only | Numeric parsers never reach the fallback | Emit integer delay-seconds |
| Header on every error status | Spec violation, confuses clients | Guard on the specific status |
| Value > 120 | Exceeds SDK honor ceiling | Clamp ≤ 60 |
| Cross-origin SPA can't read it | Header invisible to browser clients | Send `Access-Control-Expose-Headers` if those clients matter |

## Sources & Verification

| Source | How verified | Last verified |
|---|---|---|
| RFC 9110 §10.2.3 (Retry-After), rfc-editor.org full text, lines 4799–4822 | curl + verbatim grep (ABNF, examples, IANA permanent registration) | 2026-11-07 |
| RFC 6585 §4 (429 Too Many Requests), lines 132–165 | same | 2026-11-07 |
| OpenAI Python SDK `src/openai/_base_client.py` (`_parse_retry_after_header`, `_calculate_retry_timeout`) and `_constants.py` (`MAX_RETRY_AFTER_DELAY = 2*60`) | raw.githubusercontent.com `main` branch | 2026-11-07 |
| WHATWG Fetch spec (0 occurrences of `retry-after`; CORS-safelist checked) | full-text curl + grep | 2026-11-07 |
| `reqwest` 0.12 `src/retry.rs` / `src/lib.rs` (no 429 or Retry-After handling) | local cargo registry source | 2026-11-07 |
| axum 0.8 `pub use http` (`src/lib.rs`), `http` 1.5 `RETRY_AFTER` constant | local cargo registry source + docs.rs | 2026-11-07 |
