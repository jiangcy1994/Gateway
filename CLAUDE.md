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

1. `/health` and `/__admin/*` are real routes; everything else hits the fallback.
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

Persistence (`src/config.rs`): every admin mutation calls `AppState::persist`,
which serializes the whole table to TOML and replaces the routes file atomically
(temp file in the same directory + `persist`). **Rewrites drop comments** — the
managed header line is the only comment that survives. A missing routes file at
startup is not fatal: the gateway starts with no routes.

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

## Conventions

- Error responses are JSON: `{"error":{"code":502,"message":"..."}}` via `ApiError`
  (`src/error.rs`), which maps each variant to a status code.
- The TOML routes file and the admin API share one `Route` struct; anything
  settable in the file is settable through the API and vice versa.
- `PUT /__admin/routes/{id}` is **merge** semantics: absent fields keep the
  current value. `id` is never patchable.
