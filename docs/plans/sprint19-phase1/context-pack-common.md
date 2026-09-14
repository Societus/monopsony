# Sprint 19 Phase 1 — Context Pack (shared)

Read this ONCE, then execute your slice spec. Do not open the plan doc or the
monopsony-development skill — everything you need is here.

## Mission
Sprint 19 Phase 1 = HTTP CONNECT pass-through so https URLs work through the
Monopsony proxy chain (client :3129 → server :3128 → upstream). Server side is
DONE and compiling. Your slice finishes the remaining piece.

## Environment rules (hard)
- HARD 600s cap: work linearly, write code immediately, verify ONCE at the end.
- NO COMMITS: leave changes in the working tree. Never git add/commit/stash.
- Reading whitelist: ONLY your slice spec + the named files in it. No plan docs.
- If time runs short, SKIP remaining items, list them under not_done.
- Patch-tool lint errors like "async fn not permitted in Rust 2015" are FALSE
  POSITIVES (standalone linter can't see workspace edition). cargo is the only
  arbiter. A successful `cargo check` = done, ignore the lint noise.
- Keep each write_file/patch under ~8K tokens. cargo check between changes.

## Repo facts
- /home/societus/projects/monopsony — Rust workspace, edition 2021, axum 0.7,
  hyper 1.x, hyper-util 0.1, tokio full. `url` crate available.
- Baseline test counts (current tree, slice-1 landed): monopsony-proxy
  `cargo test -p monopsony-proxy` = 114 passed + 3 passed (integration) + 1
  ignored. Workspace `cargo check --workspace` = clean.
- Background cargo jobs serialize on the target-dir lock; if a job dies with no
  output, wait 30s and retry once.

## CONNECT architecture as implemented (server side, for reference)
`crates/monopsony-proxy/src/proxy.rs`:
- `handle_proxy_request` extracts method/uri FIRST (&Request<Body> is !Send),
  acquires ONE rate-limiter concurrent slot per request (all methods), then
  intercepts `Method::CONNECT` BEFORE the non-GET/HEAD passthrough fall-through
  (line ~276). Calls `handle_connect(state, rate_limiter, addr, needs_release,
  limiter_clone, authority, connect_cfg, request)`.
- `handle_connect` (line ~1290): wraps release in `ConnectSlotGuard` (Drop =
  exactly-once release; guard moves into the tunnel task). Checks in order:
  connect_enabled → 405; authority parse → 400; connect_deny → 403; SSRF via
  `crate::security::validate_url_host(&url::Url)` unless
  connect_cfg.allow_private_upstreams → 403; dial upstream TcpStream BEFORE
  responding (failure → 502, no 200 ever sent). Then spawns the tunnel task:
  `hyper::upgrade::on(request).await` → `hyper_util::rt::TokioIo::new(upgraded)`
  → `tokio::io::copy_bidirectional(&mut client_io, &mut upstream)`, counting
  bytes into `monopsony_common::metrics::proxy_connect_bytes(to+from)`.
  Returns 200 with empty body.
- KEY TRAP (already solved server-side): hyper 1.x `Upgraded` implements
  hyper::rt::Read/Write, NOT tokio's traits. `TokioIo` bridges BOTH directions —
  `TokioIo::new(upgraded)` gives something `copy_bidirectional` accepts. Do NOT
  try `upgraded.downcast::<TcpStream>()` (requires hyper-rt impls that tokio
  types don't have). TokioIo lives at `hyper_util::rt::TokioIo`, not
  `hyper::rt::TokioIo`.
- `ProxyState` (line 67) has `connect_config: monopsony_common::config::ProxyServerConfig`;
  `run_proxy` wires it from `node_config.proxy` (ServerConfig [proxy] section).
- Config (crates/monopsony-common/src/config.rs line 72):
  `ProxyServerConfig { connect_enabled: bool, allow_private_upstreams: bool,
  connect_deny: Option<Vec<String>> }` — all serde(default), lives at
  `ServerConfig.proxy`.
- Metric (crates/monopsony-common/src/metrics.rs): name const
  `PROXY_CONNECT_BYTES = "monopsony_proxy_connect_bytes_total"`, helper
  `pub fn proxy_connect_bytes(bytes: u64)`.

## Client proxy as it stands (crates/monopsony-client/src/proxy.rs, 406 lines)
- `ProxyConfig { server_addr: String ("127.0.0.1:3128" default), timeout_ms:
  u64 (30_000), retry_deadline_ms: u64 (300_000), retry_backoff_initial_ms:
  u64 (2_000) }`, `ClientProxyState::new(ProxyConfig)`, `state.config() ->
  &ProxyConfig`.
- `proxy_handler(State<Arc<ClientProxyState>>, Request<Body>) -> impl IntoResponse`
  (line 207): builds a bounded retry loop with backoff for GET/HEAD, forwards
  absolute-URI requests to the server via raw hyper http1 handshake
  (`forward_to_server`), `is_transient_failure` matches server marker strings,
  failures → `build_503(msg: &'static str)`.
- Router (line 315): `axum::Router` with `/*path` + `/` → proxy_handler,
  `axum::serve(listener, app)` — NO auth middleware. axum::serve supports hyper
  upgrades (proven: monopsony-admin ws_tunnel.rs serves WebSockets this way).
- Semantics that MUST survive: no fallback to direct upstream; TCP-connect
  failure to server → 503 `build_503("Server proxy connect failed")`.

## Review checklist (what the code reviewer will check)
1. CONNECT intercepted before any scheme check (CONNECT URIs are
   authority-form — `uri.scheme()` is None by design).
2. Tunnel opaque: no cache, no tee/spool, no X-Cache, no body inspection.
3. 503-on-establishment-failure preserved (server unreachable / CONNECT denied
   / timeout), NO fallback path.
4. Exactly-once resource release; no unbounded buffering.
5. Existing absolute-URI GET/HEAD path byte-identical (zero regression).
6. SSRF enforcement stays on the SERVER side (client adds no deny-list —
   server already 403s private targets unless allow_private_upstreams).
