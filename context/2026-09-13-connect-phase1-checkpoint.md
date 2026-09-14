# Sprint 19 Phase 1 — CONNECT pass-through: implementation checkpoint

**Date:** 2026-09-13
**Status:** Code-complete, all suites green. Pending: live fleet verification (Phase 1 exit criteria).
**Method:** hub-and-spoke (main session coordinates, subagents implement via
context packs in `docs/plans/sprint19-phase1/`, parent finishes slices that hit
the 600s cap). This file is the handoff for the stronger-model code-review pass.

## What landed (5 files, +863/-25, uncommitted)
- `crates/monopsony-common/src/config.rs` — `ProxyServerConfig`
  (`connect_enabled`, `allow_private_upstreams`, `connect_deny`), serde(default),
  at `ServerConfig.proxy`.
- `crates/monopsony-common/src/metrics.rs` — `proxy_connect_bytes` counter.
- `crates/monopsony-proxy/src/proxy.rs` — server CONNECT:
  `handle_connect` (405 disabled / 400 bad authority / 403 deny+SSRF / dial-
  before-200 / TokioIo bridge / copy_bidirectional / per-tunnel slot guard),
  `handle_connect_request` (rate-limit + REAL auth_middleware via FromFnLayer
  wrapping a passthrough service), `ProxyHyperService` (CONNECT intercepted
  before the axum Router; non-CONNECT through Router with explicit
  `ConnectInfo` extension insertion), `serve_proxy_router` (manual hyper-util
  accept loop; used by `run_proxy`).
- `crates/monopsony-client/src/proxy.rs` — client tunnel:
  `forward_connect_to_server` (manual CONNECT request + head parse + over-read
  payload prepend), client `handle_connect` (upgrade + relay, 503 markers
  compatible with TRANSIENT_503_MARKERS, no fallback), `ClientHyperService` +
  manual serve loop (same bypass).
- `crates/monopsony-proxy/tests/streaming_tests.rs` — 4 CONNECT integration
  tests (405/403+no-200/200+echo/502) through the REAL serve path
  (`spawn_proxy_router` now calls `serve_proxy_router`).

## The root cause worth knowing (review context)
axum 0.7 `RouteFuture` injects Content-Length into every top-level response,
outside any middleware → hyper rejects 2xx-to-CONNECT with Content-Length (RFC
7231) → tunnel dies with "operation was canceled". Full chain + fix rationale
documented in the skill `monopsony-development` (axum CONNECT trap section) and
in code comments at `ProxyHyperService`.

## Verification (real cargo output, 2026-09-13)
- `cargo test -p monopsony-client`: 30+101+5+2+5 = **143 passed, 0 failed**
- `cargo test -p monopsony-proxy`: 114+7(**incl. all 4 CONNECT**)+0 = **121 passed, 0 failed**
- `cargo check --workspace`: clean (pre-existing sqlx future-incompat note only)
- Repro harnesses deleted; zero DBG eprintlns remain.

## Not done / next
- **Live fleet verification** (the actual Phase 1 exit criteria): `curl -px
  http://<client>:3129 https://api.github.com` through both hops on the test
  bench; server down → clean 503; SSRF reject e2e. Needs the .58/.20 bench.
- Windows To Go winget source-update over tunnel (Phase 2 entry test).
- Config docs: `[proxy]` section in `dist/server.toml.example` still to document.
- code-review pass (stronger model) before commit; commit per user's signing
  workflow (`.git-commit-message.tmp` + `git add -A -- ':!.git-commit-message.tmp'`).

## SOTA review pass (2026-09-13, after code-complete)
Findings F1-F9 + fixes in `docs/plans/sprint19-phase1/review-fix-pack.md`.
Applied by 3 parallel fix agents (S1 server / S2 client / S3 tests), then
hub-verified:
- F1 slot leak (auth-reject path) — acquire+guard moved inside auth-wrapped svc ✓
- F2 DNS-rebinding TOCTOU — resolve-once via lookup_host, dial validated IPs only ✓
- F3 dial timeout (10s) + tunnel lifetime cap (3600s) both proxies, config knobs ✓
- F4 TCP_NODELAY restored on all tunnel sockets ✓
- F5 client bounded server-connect + authority-form 400 ✓
- F6 dead .fallback removed both routers ✓
- F7 unreachable Router CONNECT branch + unused params/clones removed ✓
- F8 deny-list suffix + trailing-dot matching ✓
- F9 chain e2e test + deny-suffix contract test added (streaming now 9) ✓
- **F10 (found during hub verify, S1-introduced): auth middleware layer had
  been dropped from create_proxy_router — Token-mode GET/HEAD path was
  unauthenticated. RESTORED by hub; tests green (Open-mode tests can't catch
  this class — a token-mode router test is worth adding later).**
Final: workspace **509 passed, 0 failed** (was 507 pre-review). Backlog noted
in pack: GET-path DNS-rebinding (pre-existing), token-auth-CONNECT note,
private-range port-scan doc warning.
