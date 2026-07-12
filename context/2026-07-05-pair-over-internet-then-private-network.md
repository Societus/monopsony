# Pair Over Internet, Then Connect On Private Network — Analysis & Plan

Created: 2026-07-05
Status: Gap analysis + implementation plan. Requires changes to both plan and code.

## The Scenario

1. Client has internet access → pairs with server using public IP/DNS
2. Client is moved to an isolated network → no internet, can only reach server via private IP
3. Client daemon must reconnect to the server's internal address

## Current Architecture: What Works and What Breaks

### Pairing (works over internet)

```
monopsony-client pair --server public.example.com:50052 --code XXXXXX
```

Pairing uses plaintext TCP to port 50052, exchanges protobuf-framed PairRequest/PairResponse.
Server returns `PairResponse.grpc_addr` (server's gRPC address). Client saves:
- `cert_dir/client.crt` — client certificate (CN = client UUID)
- `cert_dir/client.key` — client private key
- `cert_dir/ca.crt` — server's CA certificate
- `cert_dir/client_id` — assigned client UUID

### Daemon Connection (BREAKS on network change)

After pairing, the daemon connects via:

```
monopsony-client daemon --server <addr>:50051
```

**Problem 1: Server address is not persisted after pairing.**

The pairing response includes `grpc_addr` (field 5 in PairResponse proto), but the
`run_pair()` function **discards it**. It saves certs + client_id but never writes
the server address to config. The user must manually pass `--server` to `daemon`
or set it in `client.toml`.

**Problem 2: TLS certificate SAN only includes one IP.**

The server certificate is generated with:
```rust
san_types = vec![DnsName("monopsony-server")]
// + detected LAN IP
// + bind IP
```

If the server generates its cert while on the public internet (1.2.3.4), the SAN
contains `monopsony-server` + `1.2.3.4`. When the client later connects via
`10.10.20.58`, the TLS handshake checks the SAN and **fails** because `10.10.20.58`
is not in the cert's SAN list.

**Problem 3: TLS domain_name is hardcoded.**

```rust
// main.rs:295
.domain_name("monopsony-server")
```

The client always validates the server cert against the DNS name `monopsony-server`,
not the IP address. This works for the DNS SAN entry but NOT for IP-based connections.
tonic/rustls should also check IP SANs, but this depends on how the TLS config is
constructed.

**Problem 4: No `--server` fallback chain.**

The daemon takes `--server` from CLI or discovers via mDNS. There's no "try the
last known address from pairing, then try config, then mDNS" chain. If the user
forgets to update `--server` after moving networks, the daemon just fails.

## Required Changes

### Change 1: Persist server address during pairing

**File**: `monopsony-client/src/main.rs` (`run_pair` function)

After successful pairing, save the server's gRPC address to a file:

```rust
// After certs.save(cert_dir)?
// After std::fs::write(cert_dir.join("client_id"), &client_id)?

// Save server address for future daemon connections
let server_info = ServerConnectionInfo {
    grpc_addr: resp.grpc_addr.clone(),  // From PairResponse
    paired_at: chrono::Utc::now().to_rfc3339(),
};
std::fs::write(
    cert_dir.join("server.json"),
    serde_json::to_string_pretty(&server_info)?
)?;
```

```json
// cert_dir/server.json
{
  "grpc_addr": "public.example.com:50051",
  "paired_at": "2026-07-05T12:00:00Z"
}
```

### Change 2: Support multiple server addresses

**File**: `monopsony-client/src/main.rs` (`run_daemon` function)

Add fallback chain for server resolution:

```rust
// Resolution order:
// 1. --server CLI flag (explicit)
// 2. [server].address in client.toml
// 3. cert_dir/server.json (from pairing)
// 4. mDNS discovery

let server_addr = if let Some(addr) = server {
    addr  // CLI flag wins
} else if let Some(ref config) = client_config {
    if let Some(ref addr) = config.server.address {
        format!("{}:{}", addr, config.server.grpc_port)
    } else { None }
} else if let Ok(info) = load_server_info(&cert_dir) {
    info.grpc_addr  // From pairing
} else {
    mDNS discovery...
};
```

### Change 3: Support `--server` overrides without losing original

Add a `--alt-server` flag or environment variable for temporary overrides:

```bash
# Paired with public.example.com but now on private network
monopsony-client daemon --server 10.10.20.58:50051
```

This already works (CLI `--server` takes precedence), but the user needs to know
to use it. The `server.json` fallback handles the case where they don't specify.

### Change 4: Server certificate SAN must include all known addresses

**File**: `monopsony-control/src/tls.rs` (`generate_server_cert`)

The server cert needs SANs for ALL addresses the server might be reachable at:

```rust
pub fn generate_server_cert(
    &self,
    bind_addr: Option<std::net::IpAddr>,
    additional_sans: &[String],  // NEW: hostnames/IPs from config
) -> anyhow::Result<(Vec<u8>, Vec<u8>)> {
    let mut san_types = vec![
        rcgen::SanType::DnsName("monopsony-server".try_into()?),
    ];

    // Bind address IP
    if let Some(ip) = bind_addr { ... }

    // Additional SANs from config (public DNS, private IP, etc.)
    for san in additional_sans {
        if let Ok(ip) = san.parse::<std::net::IpAddr>() {
            san_types.push(rcgen::SanType::IpAddress(ip));
        } else {
            san_types.push(rcgen::SanType::DnsName(san.as_str().try_into()?));
        }
    }

    // Also auto-detect all non-loopback IPs
    // (the server may have multiple interfaces: public + private)
    for iface in pnet::datalink::interfaces() {
        for ip in iface.ips {
            if !ip.ip().is_loopback() {
                san_types.push(rcgen::SanType::IpAddress(ip.ip()));
            }
        }
    }
    ...
}
```

### Change 5: Server config for certificate SANs

**File**: `monopsony-common/src/config.rs`

Add to `HttpServerConfig` or create `TlsSanConfig`:

```toml
# server.toml
[tls]
# Additional hostnames/IPs to include in server certificate SAN.
# The server will be reachable at these addresses and the client's
# TLS handshake will succeed regardless of which one it connects to.
san_names = ["public.example.com", "10.10.20.58", "monopsony.lan"]
```

This tells the server at startup: "generate your cert with these SANs." The server
then works whether the client connects via public DNS, private IP, or mDNS name.

### Change 6: Client config for alternate addresses

**File**: `monopsony-common/src/config.rs`

Add to `ServerConnectionConfig`:

```toml
# client.toml
[server]
address = "10.10.20.58"        # Primary address (private network)
grpc_port = 50051
http_port = 3128

# Alternate addresses to try if primary fails
alt_addresses = ["public.example.com", "monopsony.lan"]
```

Daemon connection logic tries each in order until one succeeds.

### Change 7: `pair` subcommand saves `--connect-addr` hint

The pairing server may know its own addresses better than the client. Add the
server's returned `grpc_addr` as the primary entry in `server.json`, but also
save the address used during pairing as a fallback:

```bash
# Pair over internet
monopsony-client pair --server public.example.com:50052 --code XXXXXX
# Saves server.json: { "grpc_addr": "public.example.com:50051", "paired_addr": "public.example.com:50052" }

# Later, on private network — update the address
monopsony-client daemon --server 10.10.20.58:50051
# OR: edit client.toml [server].address = "10.10.20.58"
```

## Impact on Docker/Toggle Plan

The toggle plan (`context/2026-07-05-docker-toggle-plan.md`) does NOT need changes.
The toggle commands (`proxy status/on/off`) are orthogonal to server connectivity.
However, the toggle plan's Docker daemon.json writes should use the same server
address resolution chain as the daemon — if the client config changes addresses,
the Docker mirror config should update too.

**One addition to the toggle plan**: `proxy status` should show the current server
connection state and which address it's using. Add a "Server" section to the status
output:

```
Server:
  connected       ✓ 10.10.20.58:50051 (mTLS)
  paired via      public.example.com (2026-07-05)
  cert SANs       monopsony-server, 10.10.20.58, public.example.com
```

## Recommended Implementation Order

1. **Persist server address in pairing** (Change 1+7) — 30min
2. **Fallback chain in daemon** (Change 2+6) — 1hr
3. **Server cert SAN from config** (Change 4+5) — 1.5hr
4. **Update toggle status display** — 15min (add to existing plan)

**Total: ~3 hours.** Can be done in one session before the Docker work.

## Test Scenario

1. Server starts with `[tls] san_names = ["public.example.com", "10.10.20.58"]`
2. Client pairs via `public.example.com:50052` over internet
3. `server.json` saved with `grpc_addr: "public.example.com:50051"`
4. Client moved to private network — `public.example.com` unreachable
5. User runs `monopsony-client daemon` (no --server flag)
6. Daemon tries `public.example.com:50051` → fails (timeout)
7. Daemon tries `10.10.20.58:50051` from `client.toml [server].address` → succeeds
8. TLS handshake passes because `10.10.20.58` is in server cert SAN
9. mTLS succeeds because client cert + CA are valid regardless of network

## What Already Works (No Changes Needed)

- **mTLS certificates are network-agnostic.** The client cert (CN = client UUID) and
  CA cert work on any network. TLS mutual auth doesn't care about IP addresses.
- **Client cert has no SAN.** The client cert's CN is the client UUID — no IP or
  hostname. The server validates it by CN against the CA, not by network address.
- **Pairing protocol.** The protobuf-framed TCP exchange on port 50052 works over
  any TCP connection (internet or LAN).
- **CA certificate.** The server's CA is self-contained and valid regardless of
  what address the server is reachable at.

The entire gap is: (a) persisting server address, (b) trying multiple addresses,
and (c) server cert SAN covering all reachable addresses.
