# Local Access Points — IP Allowlist for Client Connections

Created: 2026-07-05
Status: Safety analysis + design. Extends the pair-over-internet plan.

## The Idea (User's Proposal)

Server admin dashboard configures "accessible local points of access" — IP ranges
from which paired clients (those holding a valid cert + client ID) may connect.

Two modes:
1. **IP allowlist**: Only private IP ranges (RFC 1918 + Tailscale CGNAT) are accepted
   for gRPC connections from paired clients.
2. **"Allow local connections" toggle**: Server switch that saves an auth token;
   client validates it's allowed on the server from its alternate locations.

## Safety Analysis

### Current Security Model (what protects the server today)

```
Layer 0: Pairing port 50052 — PLAINTEXT TCP, 6-char code auth
         (Anyone with the code can pair. No IP restriction.)

Layer 1: gRPC port 50051 — mTLS REQUIRED
         (Client must present a cert signed by the server's CA.
          Enforced by tonic's with_client_cert_verifier.
          Attacker without a cert cannot complete TLS handshake.)

Layer 2: Client ID validation — gRPC handlers check client_id against DB
         (Register/heartbeat/inventory all require a valid UUID in the clients table.)

Layer 3: Dashboard/API port 3130 — session-based auth
         (Auto-trusts tailnet IPs in serve mode, requires login in funnel/LAN mode.)
```

### What the Proposal Adds

```
Layer 1a: gRPC port 50051 — IP allowlist (OPTIONAL, on top of mTLS)
          (If enabled: reject connections from IPs not in the allowlist,
           even if the client has a valid cert.)

Layer 0a: Pairing port 50052 — IP allowlist (OPTIONAL)
          (If enabled: reject pairing attempts from non-allowed IPs.)
```

### Is This Safe?

**Yes, but it's defense-in-depth, not a primary control.**

The mTLS layer (Layer 1) is the real security boundary. A client without a cert
signed by the server's CA cannot connect to gRPC regardless of IP. The IP allowlist
adds:

1. **Damage limitation**: If a client cert is stolen, the attacker can only use it
   from allowed IP ranges.
2. **Pairing hardening**: Reduces the attack surface of the plaintext pairing port.
3. **Operational clarity**: Admin can see and control which networks are authorized.

### What Would Be Unsafe

- **Relying on IP filtering INSTEAD of mTLS** — IPs can be spoofed, VPN'd, or
  NAT'd. mTLS is cryptographic; IP filtering is best-effort.
- **Allowing pairing from any IP when the server is internet-exposed** — the 6-char
  code is only ~36 bits of entropy. An allowlist on the pairing port matters.
- **Trusting X-Forwarded-For or proxy headers** — the gRPC server gets the real TCP
  peer address from tonic's connection info, not headers. This is safe.

## Recommended Design

### Server Config

```toml
# server.toml
[access_control]
# When true, gRPC client connections are restricted to these CIDR ranges.
# mTLS is ALWAYS required regardless of this setting.
enabled = false

# Allowed CIDR ranges for client gRPC connections.
# Default: all RFC 1918 private ranges + Tailscale CGNAT.
allowed_cidrs = [
    "10.0.0.0/8",
    "172.16.0.0/12",
    "192.168.0.0/16",
    "100.64.0.0/10",   # Tailscale / CGNAT
]

# When true, pairing port (50052) is also restricted to allowed_cidrs.
restrict_pairing = true

# When true, log and reject any gRPC connection attempt from outside allowed_cidrs.
log_rejected = true
```

### How It Works

1. **Server startup**: Load allowed CIDRs from config (or dashboard settings).
2. **gRPC connection**: tonic's TLS handshake completes (Layer 1 passes).
   After handshake, check the peer's TCP address against the CIDR list.
   If not in range: drop the connection with a log entry.
3. **Pairing connection**: On TCP accept for port 50052, check peer IP against
   CIDR list. If not in range: close immediately.

### Implementation in tonic

tonic doesn't expose peer_addr directly on the gRPC trait, but we can use a
tower middleware layer that checks `ConnectInfo<SocketAddr>`:

```rust
// In grpc.rs start():
builder
    .add_service(MonopsonyControlServer::new(grpc)
        .with_layer(ip_filter_layer.clone()))
    .add_service(MirrorControlServer::new(mirror_grpc)
        .with_layer(ip_filter_layer))
    .serve(addr)
    ...

// IP filter layer
fn ip_filter_layer(
    allowed_nets: Vec<IpNet>,
    state: Arc<AccessControlState>,
) -> impl tower::Layer<tonic::transport::server::TcpConnectInfo> {
    ...
}
```

Actually, simpler: use `into_make_service_with_connect_info::<SocketAddr>()`
(same pattern the HTTP proxy already uses) and add an interceptor:

```rust
// After TLS handshake, before dispatching the RPC
fn check_client_ip(req: &Request<()>, allowed: &[IpNet]) -> Result<(), Status> {
    let peer = req.remote_addr()
        .ok_or_else(|| Status::permission_denied("No peer address"))?;
    let ip = peer.ip();
    if !allowed.iter().any(|net| net.contains(&ip)) {
        warn!(ip = %ip, "Client rejected by IP allowlist");
        return Err(Status::permission_denied("IP not in allowlist"));
    }
    Ok(())
}
```

This runs AFTER mTLS succeeds (tonic dispatches interceptors after the TLS
handshake), so Layer 1 is always enforced first.

### Dashboard Integration

The admin dashboard gets a new settings page:

```
┌─ Access Control ──────────────────────────────────┐
│                                                    │
│  ☑ Restrict client connections to allowed networks │
│                                                    │
│  Allowed Networks:                                 │
│    10.0.0.0/8         [Remove]                     │
│    172.16.0.0/12      [Remove]                     │
│    192.168.0.0/16     [Remove]                     │
│    100.64.0.0/10      [Remove]    (Tailscale)      │
│                              [+ Add Network]       │
│                                                    │
│  ☑ Restrict pairing to allowed networks            │
│                                                    │
│  Rejected connections (last 24h): 3                │
│    203.0.113.42 → gRPC (2026-07-05 14:23)         │
│    198.51.100.7 → pairing (2026-07-05 09:11)      │
│                                                    │
│                          [Save Changes]            │
└────────────────────────────────────────────────────┘
```

Changes write to `server.toml [access_control]` and signal the server to reload
(same pattern as the telemetry toggle).

### The "Allow Local Connections" Toggle

The user's second proposal: a single switch that generates a client auth token.
This is **redundant with mTLS** — the client cert IS the auth token. A client
that has paired (has a cert) is already "allowed." Adding a second token doesn't
increase security; it adds complexity.

**Instead, the toggle should control the IP allowlist:**

```bash
# Server CLI: enable local-only mode
monopsony-server access-control --enable --default-private

# This sets:
# [access_control]
# enabled = true
# allowed_cidrs = ["10.0.0.0/8", "172.16.0.0/12", "192.168.0.0/16", "100.64.0.0/10"]
# restrict_pairing = true
```

One command, locks the server to private networks only. To open up for pairing
over the internet temporarily:

```bash
monopsony-server access-control --pairing-open
# Temporarily lifts pairing restriction (gRPC stays restricted to private IPs)
# Auto-reverts after 10 minutes
```

## The Complete Workflow (Pair Over Internet, Then Lock Down)

```
1. SERVER STARTS (internet-exposed, pairing open)
   monopsony-server --config server.toml
   # access_control.enabled = false (default)
   # Pairing port 50052 accepts from any IP

2. CLIENT PAIRS (over internet)
   monopsony-client pair --server public.example.com:50052 --code XXXXXX
   # Gets cert + client_id + CA
   # Saves server.json with grpc_addr

3. ADMIN LOCKS DOWN (from dashboard)
   Click "Enable access control"
   # Sets allowed_cidrs to private ranges
   # Restricts both gRPC and pairing
   # Server cert already has SANs for public + private IPs

4. CLIENT MOVED TO PRIVATE NETWORK
   monopsony-client daemon --server 10.10.20.58:50051
   # mTLS handshake: cert validates (CA-signed) ✓
   # IP check: 10.10.20.x is in 10.0.0.0/8 ✓
   # Client ID: in DB ✓
   # Connection succeeds

5. ATTACKER TRIES (from internet, with stolen cert)
   # mTLS handshake: would pass (they have a cert)
   # IP check: their public IP is NOT in allowed_cidrs
   # Connection rejected: "IP not in allowlist"
```

## What NOT to Do

- **Don't replace mTLS with IP filtering.** mTLS is cryptographic proof; IPs are
  network-level hints.
- **Don't add a second client-side auth token.** The client cert already serves
  this purpose. Two tokens = two things to rotate, two things to leak.
- **Don't check IPs at the TCP level only.** Do it after TLS so you can log the
  client cert's CN (UUID) alongside the rejection.
- **Don't forget IPv6.** Add `fd00::/8` (ULA), `fe80::/10` (link-local), and
  Tailscale's `fd7a:115c:a1e0::/48` to the default allowlist.

## Impact on Prior Plans

- **Pair-over-internet plan**: Add `server.json` persistence (already planned),
  add server cert SAN for multiple IPs (already planned), add `[access_control]`
  config section (NEW), add IP filter interceptor in gRPC (NEW).
- **Docker/toggle plan**: No changes. Docker registry mirror is on port 3128
  (HTTP proxy), which is separate from gRPC. IP filtering on gRPC doesn't affect
  the HTTP proxy.
- **Dashboard**: Add access control settings page + rejected connections log.

## Estimated Effort

| Task | Hours |
|------|-------|
| `[access_control]` config struct + parsing | 0.5 |
| CIDR matching utility (use `ipnet` crate) | 0.5 |
| gRPC interceptor checking peer IP | 1.5 |
| Pairing port IP check (TCP accept guard) | 0.5 |
| Dashboard settings page + API | 2 |
| Dashboard rejected-connections log view | 1 |
| CLI: `monopsony-server access-control` command | 0.5 |
| **Total** | **~6.5 hours** |
