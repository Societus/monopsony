# Monopsony Configuration Reference

Complete reference for all configuration files: `server.toml`, `client.toml`, and `mirrors.toml`.

---

## Table of Contents

1. [Server Configuration (server.toml)](#server-configuration)
2. [Client Configuration (client.toml)](#client-configuration)
3. [Mirror Configuration (mirrors.toml)](#mirror-configuration)
4. [Example Configurations](#example-configurations)

---

## Server Configuration

File: `/etc/monopsony/server.toml`
Loaded by: `monopsony-server --config /etc/monopsony/server.toml`

### `[server]` — HTTP Cache Proxy

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `bind` | string | **required** | Bind address for HTTP cache proxy. Example: `"0.0.0.0:3128"` |
| `hostname` | string | `null` | Optional hostname for TLS certificate SAN. Auto-detected if not set. |
| `auth_mode` | string | `"open"` | Authentication mode: `"open"` (no auth, LAN trust), `"token"` (API key), `"mtls"` (client certs, future) |

### `[grpc]` — gRPC Control Plane

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `bind` | string | **required** | Bind address for gRPC control plane. Example: `"0.0.0.0:50051"` |

### `[database]` — PostgreSQL

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `url` | string | **required** | PostgreSQL connection URL. Example: `"postgres://monopsony:secret@localhost:5432/monopsony"` |
| `max_connections` | u32 | `10` | Maximum connections in the connection pool. |

### `[cache]` — Package Cache

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `path` | path | **required** | Path to cache directory. Example: `"/var/cache/monopsony"` |
| `max_size_gb` | u64 | **required** | Maximum cache size in GB. Oldest entries purged first. |
| `purge_after_days` | u32 | `30` | Days before purging unreferenced packages. |

### `[tls]` — TLS Certificates

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `ca_path` | path | **required** | Path to CA certificate (PEM). Auto-generated if missing during pairing. |
| `ca_key_path` | path | **required** | Path to CA private key (PEM). |
| `server_cert_path` | path | **required** | Path to server certificate (PEM). Auto-generated from CA. |
| `server_key_path` | path | **required** | Path to server private key (PEM). |

### `[discovery]` — mDNS Discovery

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `mdns_enabled` | bool | `true` | Enable mDNS broadcast for client auto-discovery. |
| `mdns_service_name` | string | `"monopsony-server"` | mDNS service name. Must match across all clients. |

### `[metrics]` — Prometheus Metrics

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `enabled` | bool | `true` | Enable Prometheus metrics endpoint. |
| `bind` | string | `"0.0.0.0:9090"` | Bind address for metrics HTTP endpoint. |
| `path` | string | `"/metrics"` | URL path for Prometheus scrape endpoint. |
| `stale_threshold_secs` | u64 | `900` | Sprint 27: heartbeat staleness threshold. A client whose last heartbeat is older than this is marked stale (`monopsony_clients_stale_current` gauge, per-client `monopsony_client_last_heartbeat_age_seconds` gauges). |

### `[logging]` — Log Configuration

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `level` | string | `"info"` | Log level: `trace`, `debug`, `info`, `warn`, `error` |
| `format` | string | `"json"` | Log format: `json` (structured, for log aggregators) or `pretty` (human-readable) |

### Minimal server.toml Example

```toml
[server]
bind = "0.0.0.0:3128"

[grpc]
bind = "0.0.0.0:50051"

[database]
url = "postgres://monopsony:secret@localhost/monopsony"

[cache]
path = "/var/cache/monopsony"
max_size_gb = 50

[tls]
ca_path = "/etc/monopsony/ca.pem"
ca_key_path = "/etc/monopsony/ca-key.pem"
server_cert_path = "/etc/monopsony/server.pem"
server_key_path = "/etc/monopsony/server-key.pem"
```

---

## Client Configuration

File: `/etc/monopsony/client.toml`
Loaded by: `monopsony-client daemon`

### `[client]` — Client Identity

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `id` | string | `""` | Client ID. Set automatically during pairing. Do not edit manually. |
| `hostname` | string | auto | Client hostname. Auto-detected from system if not set. |

### `[server]` — Server Connection

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `address` | string | `null` | Server address. If empty, uses mDNS discovery. Example: `"192.168.1.100"` |
| `grpc_port` | u16 | `50051` | gRPC port on the server. |
| `http_port` | u16 | `3142` | HTTP cache proxy port on the server. |

### `[tls]` — Client TLS

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `ca_path` | path | **required** | Path to CA certificate (received during pairing). |
| `client_cert_path` | path | **required** | Path to client certificate (received during pairing). |
| `client_key_path` | path | **required** | Path to client private key (received during pairing). |
| `trust_on_first_use` | bool | `false` | Trust the server's CA on first connection. |
| `expected_fingerprint` | string | `null` | Expected SHA-256 fingerprint of server CA for TOFU verification. |

### `[proxy]` — Local HTTP Proxy

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `bind` | string | `"127.0.0.1:3143"` | Bind address for local HTTP proxy. |
| `fallback_timeout_ms` | u64 | `2000` | Timeout (ms) before falling back to direct upstream when server is unreachable. |

### `[metrics]` — Client Metrics

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `enabled` | bool | `true` | Enable Prometheus metrics endpoint. |
| `bind` | string | `"127.0.0.1:9091"` | Bind address for metrics. Localhost-only by default. |

### `[logging]` — Log Configuration

Same as server: `level` and `format` fields.

### `[ledger]` — Integrity Ledger (downgrade enforcement)

Client-side anti-rewind defense (Sprint 20 Phase 4). The daemon records the
newest version seen per package (parsed from pacman `.db` / Alpine APKINDEX
responses) and flags — or blocks — index/payload responses offering older
versions. Ledger DB lives at `~/.local/state/monopsony/ledger.sqlite3`
(outside the cert dir; survives re-pairing).

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `enabled` | bool | `true` | Master switch. Absent section = enabled (defaults apply). |
| `policy` | string | `"warn"` | Downgrade/index-parse-error verdicts: `"warn"` passes with an `X-Monopsony-Downgrade` header + tamper event; `"reject"` returns 503. |
| `max_index_age_secs` | u64 | `0` | Warn-only staleness threshold for parsed indexes (`X-Monopsony-Index-Stale` header). `0` = disabled. |

Verdicts also spool tamper events to the existing tamper-events JSONL, which
the daemon drains to the server with heartbeats.

### Minimal client.toml Example

```toml
[client]
# ID is set during pairing

[server]
address = "192.168.1.100"

[tls]
ca_path = "/etc/monopsony/ca.pem"
client_cert_path = "/etc/monopsony/client.pem"
client_key_path = "/etc/monopsony/client-key.pem"
```

---

## Mirror Configuration

File: `/etc/monopsony/mirrors.toml`
Loaded by: monopsony-server at startup

Mirrors are defined as an array under `[[mirrors]]` (TOML table array syntax).

### MirrorConfig Fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `distro` | string | **required** | Distribution identifier. Used in URLs: `/mirror/{distro}/...` |
| `upstream_url` | string | **required** | Upstream mirror URL to sync from. Example: `"https://mirror.rackspace.com/archlinux"` |
| `local_path` | path | **required** | Local directory (relative to mirror_dir). Example: `"archlinux"` |
| `sync_enabled` | bool | `true` | Whether periodic sync is active. |
| `sync_interval_secs` | u64 | `86400` | Seconds between syncs. Default: 24 hours. |
| `bandwidth_limit_bps` | u64 | `null` | Bandwidth limit in bytes/sec. `null` = unlimited. |
| `include_patterns` | string[] | `[]` | Glob patterns to include. Empty = all files. |
| `exclude_patterns` | string[] | `[]` | Glob patterns to exclude. Example: `["*.iso", "*/sources/*"]` |
| `public` | bool | `false` | If true, mirror serves files without auth. |
| `per_ip_rps` | u32 | `0` | Per-IP requests per second limit. `0` = unlimited. |
| `max_concurrent_per_ip` | u32 | `0` | Max concurrent connections per IP. `0` = unlimited. |
| `hash_policy` | string | `"fail-closed"` | Hash verification: `"fail-closed"` (reject mismatch) or `"fail-open"` (warn only). |

### mirrors.toml Example

```toml
[[mirrors]]
distro = "archlinux"
upstream_url = "https://mirror.rackspace.com/archlinux"
local_path = "archlinux"
sync_interval_secs = 21600    # 6 hours
exclude_patterns = ["*/iso/*", "*/sources/*"]
per_ip_rps = 50
max_concurrent_per_ip = 10

[[mirrors]]
distro = "ubuntu"
upstream_url = "http://archive.ubuntu.com/ubuntu"
local_path = "ubuntu"
sync_interval_secs = 86400    # 24 hours
exclude_patterns = ["*/dists/*/*/installer-*"]
hash_policy = "fail-open"

[[mirrors]]
distro = "alpine"
upstream_url = "https://dl-cdn.alpinelinux.org/alpine"
local_path = "alpine"
bandwidth_limit_bps = 10485760  # 10 MB/s
```

---

## Example Configurations

### SMB Office (20-200 clients)

```toml
# server.toml — Office deployment
[server]
bind = "0.0.0.0:3128"
auth_mode = "open"       # LAN trust, all clients on same network

[grpc]
bind = "0.0.0.0:50051"

[database]
url = "postgres://monopsony:secret@db.local/monopsony"
max_connections = 20

[cache]
path = "/var/cache/monopsony"
max_size_gb = 200
purge_after_days = 14

[tls]
ca_path = "/etc/monopsony/ca.pem"
ca_key_path = "/etc/monopsony/ca-key.pem"
server_cert_path = "/etc/monopsony/server.pem"
server_key_path = "/etc/monopsony/server-key.pem"

[discovery]
mdns_enabled = true

[metrics]
enabled = true
bind = "0.0.0.0:9090"
```

### Homelab (single server, 1-5 clients)

```toml
# server.toml — Homelab
[server]
bind = "0.0.0.0:3128"

[grpc]
bind = "0.0.0.0:50051"

[database]
url = "postgres://monopsony:monopsony@localhost/monopsony"

[cache]
path = "/var/cache/monopsony"
max_size_gb = 20
purge_after_days = 30

[tls]
ca_path = "/etc/monopsony/ca.pem"
ca_key_path = "/etc/monopsony/ca-key.pem"
server_cert_path = "/etc/monopsony/server.pem"
server_key_path = "/etc/monopsony/server-key.pem"

[logging]
level = "debug"
format = "pretty"
```

### Public Mirror (WAN, token auth)

```toml
# server.toml — Public mirror service
[server]
bind = "0.0.0.0:3128"
auth_mode = "token"      # API key required for proxy access

[grpc]
bind = "0.0.0.0:50051"

[database]
url = "postgres://monopsony:secret@db.internal/monopsony"
max_connections = 50

[cache]
path = "/data/monopsony-cache"
max_size_gb = 500
purge_after_days = 7

[tls]
ca_path = "/etc/monopsony/ca.pem"
ca_key_path = "/etc/monopsony/ca-key.pem"
server_cert_path = "/etc/monopsony/server.pem"
server_key_path = "/etc/monopsony/server-key.pem"

[discovery]
mdns_enabled = false     # No mDNS on WAN

[metrics]
enabled = true
bind = "127.0.0.1:9090"  # Metrics only accessible locally

[logging]
level = "info"
format = "json"
```

---

## CLI Overrides

All config values can be overridden via CLI flags:

```
monopsony-server --config /path/to/server.toml \
  --http-addr 0.0.0.0:8080 \    # Overrides [server].bind
  --grpc-addr 0.0.0.0:50052 \   # Overrides [grpc].bind
  --metrics-addr 127.0.0.1:9091 \ # Overrides [metrics].bind
  --cache-dir /tmp/cache \       # Overrides [cache].path
  --discovery                    # Force-enable mDNS
```

```
monopsony-client daemon          # Start with client.toml defaults
monopsony-client discover        # Find servers via mDNS
monopsony-client pair <ip>       # Pair with a server (exchanges certs)
monopsony-client configure       # Apply proxy config to package managers
monopsony-client status          # Show connection status
```

---

## Release Signing

Release binaries are signed with GPG. Verify with:

```bash
# Import the public key
gpg --import monopsony-release-key.asc

# Verify checksums
gpg --verify SHA256SUMS.asc SHA256SUMS

# Check file integrity
sha256sum -c SHA256SUMS
```

**Signing key fingerprint:** `0E88 068C 4945 E4E2 DF7E  1C1B 8A64 B095 D232 F436`
