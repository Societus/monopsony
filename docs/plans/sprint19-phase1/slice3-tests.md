# Slice 3 spec — CONNECT integration tests

FIRST read `docs/plans/sprint19-phase1/context-pack-common.md`, then this.

## Task
Write integration tests for the CONNECT pass-through (server side, already
implemented) in the EXISTING `crates/monopsony-proxy/tests/streaming_tests.rs`
(read its first ~80 lines first — reuse its harness helpers, particularly how
it spawns `run_proxy` on a random port and how it injects a test backend /
allow_private_upstreams).

## Tests to write (loopback-only, hermetic — NO public-network dials)
1. `test_connect_disabled_returns_405` — run_proxy with default config
   (connect_enabled=false). Send raw `CONNECT host:443 HTTP/1.1\r\nHost: host:443\r\n\r\n`
   over a raw tokio TcpStream to the proxy port. Expect HTTP/1.1 405.
2. `test_connect_private_target_blocked` — construct ProxyState/node_config
   with `connect_enabled=true`, `allow_private_upstreams=false`. CONNECT to
   127.0.0.1:<port> (loopback = private → SSRF block). Expect 403, and NO
   bytes after headers (no 200 was ever sent).
3. `test_connect_end_to_end_loopback` — connect_enabled=true,
   allow_private_upstreams=true (loopback test upstream allowed), spawn a tiny
   tokio echo TcpListener as the "upstream" on a random port. Send CONNECT
   upstream_host:port → expect 200 → send arbitrary bytes → read them back
   from the echo server → close cleanly. Assert bidirectional flow works.
4. `test_connect_refused_upstream_502` — connect_enabled=true,
   allow_private_upstreams=true, CONNECT to a closed port on loopback.
   Expect 502.

## How to send raw requests
Use a raw `tokio::net::TcpStream` to the proxy port, write the CONNECT
request line by hand (AsyncWriteExt), then read the response head
(AsyncReadExt). DO NOT use reqwest/Client::request — it dials the target host
directly and bypasses the proxy (documented pitfall from the streaming tests).

## Do NOT
- Touch `crates/monopsony-proxy/src/proxy.rs` or the client crate. If a test
  cannot compile against the current API, STOP and report the exact error
  under not_done — do not modify production code to make tests pass.
- Dial public hosts (github.com etc.) — loopback only, CI must stay hermetic.

## Constraints
- Only file touched: `crates/monopsony-proxy/tests/streaming_tests.rs`
  (append; reuse harness; keep tests isolated with distinct ports).
- If two tests conflict on ports, bind :0 (random) everywhere.

## Verify ONCE, from repo root:
cargo test -p monopsony-proxy --test streaming_tests 2>&1 | tail -8

## Report honestly: tests added (names), real tail output, pass/fail counts,
not_done list. Do not invent results.
