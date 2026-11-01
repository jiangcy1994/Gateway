# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working in this repository.

## Commands

```bash
cargo build                              # debug binary: target/debug/top-gateway(.exe)
cargo run --release                      # serve on GATEWAY_BIND (default 127.0.0.1:8000)
cargo test                               # Rust integration tests (self-contained, no Python needed)
cargo fmt && cargo clippy --all-targets  # both are kept clean
```

Tests spin up a mock upstream (axum) and a real gateway on ephemeral ports inside
the same binary, so no external services are required. Note for this machine: a
loopback *connection refused* takes ~2s here, so tests that assert a 502 use a
generous `timeout_secs` to avoid racing into a 504.

## Architecture

`top-gateway` is a reverse proxy that mounts sub-gateways under path prefixes and
manages them through an admin API. Request flow (`src/proxy.rs`):

1. `/health` and `/__admin/*` are real routes; `/llm/*` is served by the embedded
   LLM gateway (`src/llm/`); everything else hits the fallback. `/llm` is a
   **reserved prefix** — it never reaches the route table, so do not mount a
   route there.
2. `RouteTable::match_route` picks the **longest matching prefix** (`/qqbot` matches
   `/qqbot` and `/qqbot/...`; a route at `/` matches everything).
3. If `strip_prefix`, the matched prefix is stripped and the remainder is forwarded
   to `upstream` with query string intact.
4. Request and response **bodies stream** (`Body::from_stream` / `wrap_stream`) —
   nothing is buffered, so large or chunked upstreams work.
5. On transport error -> 502, on timeout -> 504, disabled route -> 503, no match -> 404.

Routing state (`src/router.rs`): `RouteTable` is a `Vec<Route>` behind `Arc<RwLock>`.
A linear scan for the longest match is intentional — with tens of mounted gateways
it beats any index. `AppState` (`src/state.rs`) is cheaply `Clone` (all fields are
`Arc`s), so handlers take `State<AppState>` directly.

Admin API (`src/admin.rs`): `Router` under `/__admin`. The prefix is deliberately
**not** `/admin`, because mounted sub-gateways commonly own that path. Auth via
`X-Admin-Key` or `Authorization: Bearer` compared in constant time; an unset
`GATEWAY_ADMIN_KEY` means open endpoints plus a startup warning (same policy as
`qqbot-api-gateway`).

Persistence (`src/config.rs`): every admin mutation calls `AppState::persist`
(route changes) or `persist_providers` (provider changes), which serializes the
whole table to TOML and replaces the file atomically
(temp file in the same directory + `persist`). **Rewrites drop comments** — the
managed header line is the only comment that survives. A missing routes or
providers file at startup is not fatal: the gateway starts with none.

Health (`src/health.rs`): background task probes `upstream + health_path` every
`GATEWAY_HEALTH_INTERVAL_SECS`; the first sweep runs immediately after boot rather
than after a full interval. Health is advisory — the proxy forwards regardless and
reports failure as 502/504. `POST /__admin/probe` runs a synchronous sweep.

Metrics (`src/metrics.rs`): per-route atomics (total, in-flight, errors,
latency sum). `InFlightGuard` does the bookkeeping on drop, so early returns
cannot skip it; avg latency is computed when the snapshot is serialized.

Header filtering (`src/proxy.rs`): hop-by-hop headers and `content-length` are
stripped both ways; `host` is replaced by the upstream's own (the original is
preserved as `x-forwarded-host`). `Authorization` is passed through untouched —
sub-gateways do their own client auth; route-level `headers` are injected last so
they can override anything the client sent.

LLM gateway (`src/llm/`): `/llm/{protocol}/...` selects among providers of that
protocol (openai/anthropic/systemone), forwards the protocol **as-is** (no
cross-protocol translation) and injects per-protocol credentials (Bearer for
openai/systemone, `x-api-key` + default `anthropic-version` for anthropic; client
credentials are always replaced). Selection is weighted by remaining quota —
`weight = min(rpm_ratio, tpm_ratio)` over a 60 s rolling window, quotas counted
per **provider** (all its models combined); all providers exhausted -> 429, none
enabled -> 503. Selection and RPM accounting happen in one critical section with
all trackers locked in table order (no awaits while held), so concurrent requests
cannot over-select. TPM is counted *after the fact* from the response tail: a
`UsageTap` stream copies chunks into a 64 KiB `TailBuffer` while streaming them
to the client and extracts `usage` exactly once on stream end / upstream error /
consumer drop; extraction is best-effort and never fails (missing usage records
0). `systemone` usage extraction is a deliberate stub (always 0) — routing and
forwarding work, accounting does not. `docs/sdd/llm/interfaces.md` is the
implementation authority for this module.

## Conventions

- Error responses are JSON: `{"error":{"code":502,"message":"..."}}` via `ApiError`
  (`src/error.rs`), which maps each variant to a status code.
- The TOML routes file and the admin API share one `Route` struct; anything
  settable in the file is settable through the API and vice versa. Same for
  `Provider` / `providers.toml` / `/__admin/llm/providers`.
- `PUT /__admin/routes/{id}` is **merge** semantics: absent fields keep the
  current value. `id` is never patchable. Same for `PUT /__admin/llm/providers/{id}`,
  where `rpm_limit`/`tpm_limit` are `Option<Option<u64>>` — absent keeps the value,
  explicit `null` sets unlimited (the `deserialize_some` recipe in
  `src/llm/provider.rs` is what separates them; serde's default does not).
- Missing required fields in a JSON body return **422** (axum `Json` extractor
  rejection, same for routes and providers); 400 is reserved for `validate()`
  rule failures and duplicate ids.
