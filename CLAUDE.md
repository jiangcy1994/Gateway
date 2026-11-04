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
   LLM gateway (`src/llm/`); `/qqbot/*` by the embedded QQ Bot gateway
   (`src/qqbot/`); everything else hits the fallback. `/llm` and `/qqbot` are
   **reserved prefixes** — they never reach the route table, so do not mount a
   route there.
2. `RouteTable::match_route` picks the **longest matching prefix** (`/echo` matches
   `/echo` and `/echo/...`; a route at `/` matches everything).
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
(route changes), `persist_providers` (provider changes) or `persist_bots` (bot
changes), which serializes the whole table to TOML and replaces the file atomically
(temp file in the same directory + `persist`). **Rewrites drop comments** — the
managed header line is the only comment that survives. A missing routes,
providers or bots file at startup is not fatal: the gateway starts with none.

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

LLM gateway (`src/llm/`): two inbound forms — `/llm/v1/{endpoint}` infers the
protocol from the endpoint name via a closed table (`chat/completions`... ->
openai, `messages`/`complete` -> anthropic; `models`/`files` are ambiguous and
404 with a hint to use the explicit form) and `/llm/{protocol}/...` is the
unambiguous explicit form (openai/anthropic/systemone). In both cases the request
is forwarded **as-is** (no cross-protocol translation; the v1 inference only
decides the provider pool and credential injection) with per-protocol credentials
(Bearer for openai/systemone, `x-api-key` + default `anthropic-version` for
anthropic; client credentials are always replaced). A provider declares
`protocols: Vec<Protocol>` and appears in every pool it declares, but quota is
counted once per **provider** (all its models and protocols combined). Selection
is weighted by remaining quota — `weight = min(rpm_ratio, tpm_ratio)` over a 60 s
rolling window; all providers exhausted -> 429, none enabled -> 503. Selection
and RPM accounting happen in one critical section with all trackers locked in
table order (no awaits while held), so concurrent requests cannot over-select.
TPM is counted *after the fact* from the response tail: a `UsageTap` stream
copies chunks into a 64 KiB `TailBuffer` while streaming them to the client and
extracts `usage` exactly once on stream end / upstream error / consumer drop;
extraction is best-effort and never fails (missing usage records 0). `systemone`
is reachable only via the explicit segment and its usage extraction is a
deliberate stub (always 0) — routing and forwarding work, accounting does not.
`docs/sdd/llm/interfaces.md` is the implementation authority for this module.

QQ Bot gateway (`src/qqbot/`): three channels over one bot table. OpenAPI
passthrough at `/qqbot/{app_id}/...` — the first path segment is the platform
`app_id` (the routing key), the rest joins `api_base`; the gateway fetches and
caches `access_token` (`POST /app/getAppAccessToken`, success judged by
`access_token` present in the body because the platform always returns HTTP 200 —
ADR-Q3; cached until 5 min before expiry with a 30 s refresh cooldown, and a
refresh returning the same token string does **not** rewrite `expires_at`
—ADR-Q6), then injects `Authorization: QQBot <token>` (client credentials always
replaced) and `x-qqbot-bot: <bot id>` on the response. Webhook callbacks at
`/qqbot/callback/{app_id}`: `op == 13` gets an ed25519 signature reply (sign-only
— the platform's inbound-signature public key is not in the reference docs,
ADR-Q7/R4); every other op is ACK'd with `{"op":12}` **before** the async
delivery to `bot.event_url` (at-most-once, failure counts an error and is never
retried or surfaced to the platform — ADR-Q13). Bots with
`event_source = "websocket"` get a background WS supervisor per bot
(`WsTable`): Identify → READY (stores `session_id`) → periodic heartbeat
(op 1 with the latest seq, first beat null; 2 missed op 11 acks → reconnect) →
events delivered to `event_url` by the same `deliver_event` the webhook uses;
disconnects are dispatched by close code (4008/4009 resume-retry,
4006/4007/4900..=4913 re-identify clearing the session,
4001/4002/4010..=4014/4914/4915 stop without reconnecting, everything else
including 1006 keeps the session), exponential backoff 2 s → 60 s. Disabled bots
return 503 on forwarding but **404** on the callback path (ADR-Q8).
`docs/sdd/qqbot/interfaces.md` is the implementation authority for this module.

## Conventions

- Error responses are JSON: `{"error":{"code":502,"message":"..."}}` via `ApiError`
  (`src/error.rs`), which maps each variant to a status code.
- The TOML routes file and the admin API share one `Route` struct; anything
  settable in the file is settable through the API and vice versa. Same for
  `Provider` / `providers.toml` / `/__admin/llm/providers` and `Bot` /
  `bots.toml` / `/__admin/qqbot/bots`.
- `PUT /__admin/routes/{id}` is **merge** semantics: absent fields keep the
  current value. `id` is never patchable. Same for `PUT /__admin/llm/providers/{id}`,
  where `rpm_limit`/`tpm_limit` are `Option<Option<u64>>` — absent keeps the value,
  explicit `null` sets unlimited (the `deserialize_some` recipe in
  `src/llm/provider.rs` is what separates them; serde's default does not) and
  `protocols` replaces the whole list when given. Same
  for `PUT /__admin/qqbot/bots/{id}`, where `api_base`/`ws_url`/`event_url`/
  `event_secret` are `Option<Option<String>>` — absent keeps the value, explicit
  `null` clears the override. Bot mutations also drive the WS supervisor lifecycle
  (`bot_ws.ensure` / `stop`) after persisting.
- Missing required fields in a JSON body return **422** (axum `Json` extractor
  rejection, same for routes, providers and bots); 400 is reserved for `validate()`
  rule failures and duplicate ids.
