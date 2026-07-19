# Implementation Plan: WebSocket Tunnel Transport for gRPC mTLS

## Problem

Monopsony's gRPC control plane uses mTLS with a private CA. Cloudflare Tunnels
terminate TLS at the edge, stripping client certificates. This makes the gRPC
control plane unreachable through CF Tunnels, preventing public deployments
without VPNs (Tailscale/WireGuard) or Cloudflare Zero Trust (WARP).

## Solution

Add a WebSocket tunnel that carries the existing gRPC mTLS as opaque byte
frames. CF (or any reverse proxy) sees a standard WSS connection — the inner
TLS handshake, client certs, and gRPC stream all pass through unchanged.

```
monopsony-client                        Cloudflare                    monopsony-server
     │                                      │                                │
     │── WSS upgrade (CF terminates TLS) ──▶│                                │
     │══ WebSocket binary frames ═══════════╪══════════════════════════════▶│
     │   ┌─ inner TLS ClientHello ─────┐    │   cloudflared forwards to      │
     │   │   (Monopsony CA, client     │    │   axum :3130/ws/tunnel          │
     │   │    cert, full mTLS)         │    │                                │
     │   └─────────────────────────────┘    │   axum proxies bytes to        │
     │                                      │   localhost:50051 (gRPC)       │
     │══ gRPC over inner TLS ═══════════════╪══════════════════════════════▶│
     │   (heartbeats, inventory, etc.)      │                                │
```

Key properties:
- **mTLS fully preserved** — CF never sees client certs or the inner TLS
- **Zero host dependencies** — everything compiles into existing binaries
- **Transport-agnostic** — works through CF, nginx, Caddy, any WSS-capable proxy
- **Graceful fallback** — direct TCP gRPC still works for LAN/Tailscale

## Architecture

### Server side: WebSocket → TCP relay

An axum route on the existing admin router (port 3130) that accepts WebSocket
upgrades at `GET /ws/tunnel`. For each connection, it opens a TCP connection to
`localhost:50051` (the gRPC listener) and bidirectionally proxies bytes:

```
WebSocket binary frame → tokio::io::AsyncWrite → TCP socket to :50051
TCP socket from :50051 → tokio::io::AsyncRead → WebSocket binary frame
```

The gRPC server's tonic + rustls stack handles the inner TLS handshake exactly
as if it came from a raw TCP connection. No changes to the gRPC server code.

**New file:** `crates/monopsony-admin/src/ws_tunnel.rs` (~100 lines)
**Route:** `.route("/ws/tunnel", get(ws_tunnel::handle_ws_tunnel))` in lib.rs

### Client side: gRPC-over-WebSocket transport

When the server address starts with `wss://` or `ws://`, the client creates a
WebSocket connection instead of a raw TCP connection, then wraps it as a
`tonic::transport::Channel` using a custom connector.

Tonic's `Channel::from_stream()` accepts any `Stream<Item = Result<Bytes>>`.
But the simpler path: use `tokio-tungstenite` to establish the WebSocket, then
bridge it to a `tokio::io::AsyncRead/AsyncWrite` using
`ws_stream_tungstenite` or a manual bridge (~40 lines of `AsyncRead`/`AsyncWrite`
impl over WebSocket frames).

With an `AsyncRead + AsyncWrite` impl, we create a tonic channel via
`Channel::from_io(ws_stream)` — tonic 0.12 supports this. The existing TLS
config (Monopsony CA, client cert) runs on top as the inner TLS layer.

**Modified files:**
- `crates/monopsony-client/src/grpc_client.rs` — add `connect_ws()` method
- `crates/monopsony-client/src/main.rs` — detect `wss://` prefix, branch to `connect_ws()`

**New file:**
- `crates/monopsony-client/src/ws_transport.rs` — WebSocket ↔ AsyncRead/Write bridge (~80 lines)

### Pairing over WebSocket

Currently pairing uses a raw TCP connection to port 50052 with protobuf framing.
For tunneled clients, pairing needs to work through WSS too.

Two options:
1. **Pair through the WebSocket tunnel** — add a pairing endpoint at `/ws/pair`
   that relays to localhost:50052 (same TCP-to-WS pattern as the gRPC tunnel)
2. **Pair through the gRPC stream** — send pairing as a gRPC RPC instead of a
   separate TCP port (cleaner, but changes the pairing protocol)

**Recommendation:** Option 1 first (simpler, no protocol change).

### Address resolution changes

Currently the client stores and connects to `host:50051`. The new address
scheme adds WebSocket URLs:

| Address format | Transport | Use case |
|---|---|---|
| `host:50051` | Direct TCP + mTLS | LAN, Tailscale, WireGuard |
| `wss://hostname.example.com/ws/tunnel` | WebSocket + inner mTLS | CF Tunnel, reverse proxy |

The client's `resolve_server_addr()` fallback chain becomes:
1. `cert_dir/server.json` → check for `ws_url` field (new) vs `grpc_addr` field
2. mDNS discovery → always returns direct TCP address
3. `--server` CLI flag → accept either format

**`server.json` format addition:**
```json
{
  "grpc_addr": "10.0.0.5:50051",
  "ws_url": "wss://monopsony.example.com/ws/tunnel"
}
```

Client tries `ws_url` first (if present), falls back to `grpc_addr` if WSS fails.

## Implementation Phases

### Phase 1: Server-side WebSocket relay (no client changes)

Add the WebSocket → TCP relay endpoint to the admin router.

**Files:**
- New: `crates/monopsony-admin/src/ws_tunnel.rs`
- Modified: `crates/monopsony-admin/src/lib.rs` (add module + route)
- Modified: `crates/monopsony-admin/Cargo.toml` (add `tokio-tungstenite`)

**Verification:** Use `websocat` or a quick test script to connect to
`ws://localhost:3130/ws/tunnel` and pipe bytes — verify a gRPC client can
connect through the relay.

### Phase 2: Client-side WebSocket transport

Add the WebSocket-to-AsyncRead/Write bridge and wire it into the connection logic.

**Files:**
- New: `crates/monopsony-client/src/ws_transport.rs`
- Modified: `crates/monopsony-client/src/grpc_client.rs` (add `connect_ws()`)
- Modified: `crates/monopsony-client/src/main.rs` (detect `wss://`, branch)
- Modified: `crates/monopsony-client/Cargo.toml` (add `tokio-tungstenite`)

**Verification:** `monopsony-client daemon --server wss://test.example.com/ws/tunnel`
connects, heartbeats succeed, inventory reports accepted.

### Phase 3: Pairing over WebSocket

Add `/ws/pair` relay endpoint and update client pairing to support WSS.

**Files:**
- Modified: `crates/monopsony-admin/src/ws_tunnel.rs` (add pair relay route)
- Modified: `crates/monopsony-admin/src/lib.rs` (add route)
- Modified: `crates/monopsony-client/src/grpc_client.rs` (add `pair_ws()`)
- Modified: `crates/monopsony-client/src/main.rs` (pair command detects WSS)

**Verification:** Full pairing flow through WSS: code exchange → cert issuance →
client reconnects via WSS → heartbeats stable.

### Phase 4: server.json + address resolution

Update the address resolution to store and prefer WSS URLs.

**Files:**
- Modified: `crates/monopsony-client/src/main.rs` (`resolve_server_addr()`)
- Modified: `crates/monopsony-client/src/grpc_client.rs` (pair stores `ws_url`)

**Verification:** `monopsony-client pair` against a WSS server stores the URL;
subsequent `daemon` invocations auto-connect through WSS.

## Key Design Decisions

### Why not use `tonic-web`?

`tonic-web` makes gRPC work through HTTP/1.1 proxies by encoding protobuf in
base64 over HTTP POST. It loses HTTP/2 multiplexing and adds base64 overhead.
Our approach preserves full HTTP/2 gRPC inside the WebSocket, which is faster
and requires no protocol change.

### Why relay to `localhost:50051` instead of embedding the gRPC service?

The gRPC server's tonic + rustls stack already handles mTLS, streaming,
backpressure, and all 15+ RPCs. Reimplementing that inside a WebSocket handler
would be a massive duplication. The relay is ~100 lines and delegates everything
to the existing gRPC server.

### Why not use `hyper` upgrade directly?

Tonic 0.12 uses hyper internally. We could intercept the HTTP/2 connection
before tonic processes it, but that would mean intercepting at the hyper level
inside tonic's server — fragile and version-dependent. The axum WebSocket route
is clean, isolated, and doesn't touch the gRPC server code at all.

### Inner TLS SNI / domain_name issue

The client sets `.domain_name("monopsony-server")` on the TLS config. This is
the certificate CN/SAN the client expects. The WSS outer TLS connects to
`monopsony.example.com` (CF's cert), but the inner TLS still expects
`monopsony-server` — which is what the gRPC server's cert has. These are
independent TLS layers with independent SNI. No conflict.

## Dependencies

| Crate | Where | Purpose | Size impact |
|---|---|---|---|
| `tokio-tungstenite` | server + client | WebSocket accept/connect | ~50KB compiled |
| (nothing else new) | | | |

Both crates already have tokio, hyper, rustls, and axum. tokio-tungstenite
reuses the existing rustls for WSS on the client side.

## Risks and Mitigations

| Risk | Likelihood | Mitigation |
|---|---|---|
| CF WebSocket idle timeout (100s default) | High | Client heartbeat every 60s keeps it alive. Add ping/pong at 30s interval. |
| CF 100MB message limit | N/A | We stream in small frames (gRPC keeps messages small on the control plane) |
| tonic `Channel::from_io` API instability | Medium | Pin tonic version. If API changes, the bridge is ~40 lines to adapt. |
| Pairing port relay adds complexity | Low | Phase 3 is optional — can defer if pairing-over-LAN is acceptable initially. |

## Out of Scope

- Token-based proxy auth for data plane (separate feature, discussed separately)
- Cloudflare-specific configuration (tunnel setup, cloudflared install)
- Fallback from WSS to direct TCP on failure (Phase 4 enhancement)
