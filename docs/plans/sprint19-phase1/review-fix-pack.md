# Sprint 19 Phase 1 — Code review findings & fix pack (2026-09-13, SOTA pass)

Root-cause analysis is DONE. This pack contains verified findings + exact fix
designs. Apply them; do not re-derive. Baseline: 264 tests green (client 143,
proxy 121), workspace check clean. Do not regress.

## Findings (verified against the tree)

### F1 (P0) Rate-limit slot leak on auth-rejected CONNECT
`handle_connect_request` (crates/monopsony-proxy/src/proxy.rs ~L1301):
check_rate + acquire_concurrent run BEFORE the auth layer. On auth rejection
(401 token-mode / 501 mtls) the function returns early and the
ConnectSlotGuard — created only INSIDE `handle_connect`, which auth gates —
never exists. Every rejected CONNECT permanently leaks one slot; after
DEFAULT_MAX_CONCURRENT_PER_IP (50) rejects, that source IP is locked out.
FIX: move check_rate + acquire INSIDE the auth-wrapped passthrough service
(i.e. after auth passes), create the guard in the same scope as the acquire,
and PASS THE GUARD into `handle_connect`, which moves it into the tunnel task.
Early returns then release via Drop — structurally un-leakable.

### F2 (P0) DNS-rebinding TOCTOU on the CONNECT dial
`handle_connect` validates via `validate_url_host(&parsed)` (DNS resolve #1)
then dials `TcpStream::connect((host, port))` (DNS resolve #2). Rebinding
defeats the guard. `validate_url_host` ALSO returns Ok when DNS fails
("allow, fetch will fail naturally") — a resolver blackhole bypasses too.
FIX: resolve ONCE with `tokio::net::lookup_host((host.as_str(), port))`,
validate EVERY resolved SocketAddr with `crate::security::is_private_ip`
(make it `pub` if it isn't), and connect ONLY to validated addrs (try in
order). Never dial the hostname again. This also fixes the DNS-fail hole.

### F3 (P0) No dial timeout / no tunnel lifetime cap
Unbounded `TcpStream::connect` (OS ~2min on blackhole) + immortal tunnels.
FIX: wrap the dial in `tokio::time::timeout(Duration::from_secs(cfg.connect_timeout_secs), …)`
and wrap the relay `copy_bidirectional` in a total-lifetime timeout
`cfg.tunnel_max_lifetime_secs`. Config additions (F3cfg):
in `crates/monopsony-common/src/config.rs` `ProxyServerConfig` add:
  connect_timeout_secs: u64 (default 10)
  tunnel_max_lifetime_secs: u64 (default 3600)
CRITICAL SERDE PITFALL (known repo bug class): `#[serde(default)]` on u64
yields 0 AND `#[derive(Default)]` yields 0 → zero-second timeouts. Use
`#[serde(default = "default_connect_timeout_secs")]` style fns AND replace
the derive with a manual `impl Default for ProxyServerConfig` calling the
same fns. bools/Option keep derive-compatible values.

### F4 (P1) TCP_NODELAY missing everywhere
Manual serve loops replaced axum::serve, which set nodelay by default —
tunnels now suffer Nagle/delayed-ACK (~40ms/direction-switch).
FIX (server): in `serve_proxy_router` after accept: `let _ = stream.set_nodelay(true);`
and on the dialed upstream in `handle_connect` before the relay.
FIX (client, S2): same two spots (`run_proxy` accept; `forward_connect_to_server` stream).

### F5 (P1, client) Unbounded connect to server + absolute-form CONNECT
Client `forward_connect_to_server`: only the response-head read is timed;
`TcpStream::connect(server_addr)` is unbounded → PM hangs on blackholed
server. FIX: wrap the connect in `tokio::time::timeout(timeout_ms)`, map to
the existing "Server proxy connect failed" marker.
Client `handle_connect` uses `request.uri().to_string()` — absolute-form
CONNECT (`CONNECT http://h:443`) forwards garbage. FIX: take
`req.uri().authority()` only; None → 400 response.
Client relay lifetime: wrap `copy_bidirectional` in a total timeout — add
`tunnel_max_lifetime_ms: u64` (default 3_600_000) to client `ProxyConfig`
(it already has a MANUAL `impl Default` — extend it, no serde here).

### F6 (P2) Dead `.fallback()` on both routers
CONNECT is intercepted in the hyper Service before the Router; the Router
paths `/*path` + `/` already matched everything pre-sprint. The fallback is
dead weight and widens matching (malformed paths that matchit rejects now
reach the handler instead of 404). FIX: delete `.fallback(...)` (+comment)
from BOTH routers (server `create_proxy_router`, client `run_proxy`).

### F7 (P2) Redundant CONNECT plumbing
- Remove the now-unreachable Router-path CONNECT branch in
  `handle_proxy_request` (~L276-290, the `if method == Method::CONNECT`
  block calling handle_connect with the old arg list).
- Collapse `handle_connect` signature to
  `(slot: ConnectSlotGuard, ip: IpAddr, authority: Option<String>, connect_cfg: ProxyServerConfig, request: Request<Body>)`
  — drop `_state`, `rate_limiter`, `needs_release`, `_limiter_clone`
  (guard carries the release; addr only feeds the guard's ip + logs → pass
  `IpAddr` or keep SocketAddr for logs).
- In `handle_connect_request`, drop the double limiter clones
  (`connect_limiter`/`limiter_for_slot`); one clone captured by the closure.

### F8 (P2) connect_deny bypass via subdomain / trailing dot
`eq_ignore_ascii_case(&host)` misses `api.evil.com` and `evil.com.`.
FIX: normalize both sides (`trim_end_matches('.')`, lowercase), match
equality OR `.suffix` at a label boundary:
`h == d || h.ends_with(&format!(".{d}"))`.

### F9 (verification gaps) — S3's job, not yours
Client→server chain e2e test; graceful-shutdown wiring investigation;
live fleet verification (hub).

### Documented, NOT fixed this pass (pre-existing)
- Same rebinding TOCTOU in the GET path (`validate_url_host` ~L310 then
  hostname fetch in fetch_upstream) — predates this sprint; needs the same
  resolve-once treatment in the hyper client connector; backlog.
- CONNECT enables timing-based port scanning of private ranges when
  allow_private_upstreams=true — inherent to any CONNECT proxy; config-doc
  warning when shipping docs.
- Token-mode auth (X-Monopsony-Key) can't work for PM CONNECT tunnels (PMs
  don't send custom headers) — Phase 2 winget `--header` note.

## S1 scope (server): files = crates/monopsony-common/src/config.rs,
## crates/monopsony-proxy/src/security.rs (is_private_ip pub only),
## crates/monopsony-proxy/src/proxy.rs. Verify ONCE:
##   cargo test -p monopsony-proxy 2>&1 | tail -4 && cargo check -p monopsony-server 2>&1 | tail -2
## Expected: 114 + 7 streaming + 0 ignored... 121 pass, 0 fail; server check clean.
## NOTE: test_connect_private_target_blocked now hits the NEW resolve-once path
## (127.0.0.1 is already an IP — lookup_host returns it unchanged; still 403).
## If a test needs the slot/IP semantics updated (make_connect_state /
## spawn_proxy_router signature unchanged), update tests MINIMALLY.

## S2 scope (client): file = crates/monopsony-client/src/proxy.rs only. Verify ONCE:
##   cargo test -p monopsony-client 2>&1 | tail -4
## Expected: 143 pass, 0 fail (30+101+5+2+5+0).

Patch-tool "async fn in Rust 2015" lint noise = false positives; cargo is the
arbiter. NO COMMITS. Report files_changed / real outputs / not_done.
