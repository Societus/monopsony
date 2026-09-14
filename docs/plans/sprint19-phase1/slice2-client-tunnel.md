# Slice 2 spec — Client-side CONNECT tunnel

FIRST read `docs/plans/sprint19-phase1/context-pack-common.md`, then this.

## Task
Add CONNECT tunneling to the CLIENT proxy (`crates/monopsony-client/src/proxy.rs`).
The client proxy is itself an axum server (PMs point at :3129); when a PM sends
CONNECT (https URLs), the client must relay a tunnel THROUGH the server proxy
(:3128) instead of its current 502-on-no-scheme response.

## Design (follow exactly)
1. In `proxy_handler`: at the TOP, before the deadline/retry-loop setup,
   intercept `method == Method::CONNECT`. CONNECT requests NEVER enter the
   retry loop (a tunnel is one long-lived stream; no rebuilding mid-tunnel).
2. New method on `ClientProxyState` (or free fn taking `&ClientProxyState`),
   `forward_connect_to_server(authority: &str, server_addr: &str) ->
   Result<tokio::net::TcpStream, &'static str>`:
   - `TcpStream::connect(server_addr)` — failure →
     Err("Server proxy connect failed") (matches the existing marker → 503).
   - Write `CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n\r\n` by hand
     (client already imports `TokioIo`; wrap: `let io = TokioIo::new(stream)`),
     read the response head via
     `hyper::client::conn::http1::handshake` is NOT suitable here — instead
     read headers manually up to `\r\n\r\n` with tokio::io::AsyncWriteExt /
     AsyncReadExt, parse the status line: `200` → Ok(stream); anything else →
     Err with a static marker ("Server proxy refused CONNECT" for 4xx, "Server
     proxy CONNECT failed" for 5xx). Respect the server's authority-form
     CONNECT semantics — the server side (context pack) handles auth/deny/SSRF.
   - Enforce a read timeout on the response head (state.config().timeout_ms)
     with tokio::time::timeout — timeout → Err("Server proxy CONNECT timed out").
3. Handler flow after Ok(stream):
   - Respond `StatusCode::OK` with an UPGRADE-capable response so axum/hyper
     performs the h1 upgrade. Follow the ws_tunnel.rs precedent: the file
     `crates/monopsony-admin/src/ws_tunnel.rs` already does 101 upgrades
     through `axum::serve`. For a tunnel: build the 200 response, then in the
     task `hyper::upgrade::on(req)` → `TokioIo::new(upgraded)` →
     `tokio::io::copy_bidirectional(client_io, server_stream)`. Byte counts →
     `monopsony_common::metrics::proxy_connect_bytes(...)`.
   - On `forward_connect_to_server` Err(msg): return `build_503(msg)` — the
     markers match `TRANSIENT_503_MARKERS` so future retry logic stays
     consistent. Do NOT fall back to direct upstream. Never.
4. `axum::serve` already supports upgrades (ws_tunnel.rs precedent) — no serve
   loop changes needed. (If it turns out `axum::serve` does NOT deliver
   upgrades without `.into_make_service_with_connect_info`, add it minimally —
   the client does not currently need ConnectInfo.)

## Do NOT
- Touch the existing GET/HEAD retry path (`forward_to_server`,
  `is_transient_failure`, backoff) — zero regression.
- Add any SSRF/deny logic on the client (server enforces; client adds nothing).
- Buffer tunnel bytes (no Vec<u8> staging).
- Modify config structs — server_addr is already in ProxyConfig.

## Constraints
- Only file touched: `crates/monopsony-client/src/proxy.rs` (packs dir excluded).
- monopsony-client already has hyper-util as a dependency (TokioIo already
  imported in this file). It does NOT need new deps.

## Verify ONCE, from repo root:
cargo check -p monopsony-client 2>&1 | tail -5 && cargo test -p monopsony-client 2>&1 | grep -E "test result|FAILED" | head -5

## Report honestly: files changed, real tail output of both commands, not_done
list, deviations. Do not invent test counts.
