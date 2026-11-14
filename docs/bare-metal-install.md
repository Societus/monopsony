# Monopsony Server Installation Guide

This guide covers installing Monopsony on a bare-metal host or LXC container
using the official installer script. For Docker/Podman, see
[container-deploy.md](container-deploy.md).

## Prerequisites

- **OS:** Linux x86_64 (Arch, Debian/Ubuntu, Alpine, Rocky, Fedora, Gentoo)
- **Root access** (or sudo)
- **PostgreSQL 14+** — install locally or use an external server
- **Ports:** 3128 (proxy), 50051 (gRPC), 3130 (dashboard), 9090 (metrics)

## Quick Install

```bash
curl -fsSL https://base.societus.club/societus/monopsony/raw/branch/master/dist/install.sh | bash
```

The installer is interactive — it will ask for:
1. Whether to install PostgreSQL locally or use an external server
2. Database password (auto-generated if left blank)
3. Admin dashboard password

## What the Installer Does

1. Creates the `monopsony` system user and group
2. Installs the server binary to `/usr/bin/monopsony-server`
3. Creates config directory at `/etc/monopsony/`
4. Sets up PostgreSQL database and user
5. Generates `/etc/monopsony/server.toml` with your settings
6. Bootstraps a TLS CA (generates CA + server certificates)
7. Installs and enables a service for the detected init system (systemd, OpenRC, runit, s6, SysV init.d — launchd on macOS)
8. Runs a health check to verify the server started

The dashboard is embedded in the binary — no separate install step needed.

## Post-Install Setup

### Enable the Dashboard

```bash
# Set dashboard credentials
monopsony-server dashboard-set-password

# Edit config to enable
sudo nano /etc/monopsony/server.toml
```

Uncomment the `[dashboard]` section and set `enabled = true`:

```toml
[dashboard]
enabled = true
# always_require_login = true   # Recommended for public-facing servers
```

Restart the server:
```bash
sudo systemctl restart monopsony-server
```

The dashboard is now at `http://YOUR-SERVER-IP:3130`.

### Pair a Client

On each client machine:

```bash
monopsony-client pair --server YOUR-SERVER-IP:50051
```

Enter the 6-character pairing code shown on the server. The client receives
a certificate and reconnects via mTLS automatically.

### Configure Client Proxy

Point the client's package manager at the Monopsony proxy:

```bash
# On the client — configure package manager to use the proxy
monopsony-client configure --proxy YOUR-SERVER-IP:3128
```

See `monopsony-client configure --help` for package-manager-specific options.

## Reverse Proxy / Tunnel Setup

### Behind nginx, Caddy, or Cloudflare Tunnel

The dashboard and API are plain HTTP — no WebSocket needed. Set up your
reverse proxy to forward to the dashboard port (3130).

**Important:** With a single reverse proxy, forwarding `X-Forwarded-For`
is enough — the proxy appends the client IP and monopsony reads it.
Behind Cloudflare this is NOT enough — the final proxy must *overwrite*
XFF; see the mandatory rewrite below.

For public-facing deployments, enable mandatory login:

```toml
[dashboard]
enabled = true
always_require_login = true
```

### Cloudflare Tunnel Notes

- The proxy (port 3128) and gRPC (port 50051) should NOT be exposed
  through the tunnel. Only tunnel the dashboard port (3130).
- Leave the Cloudflare **Network → gRPC** toggle OFF: the control plane
  requires client certificates at TLS termination, and Cloudflare's edge
  termination strips them — every gRPC handshake fails. Terminate TLS
  on-host or use `tailscale serve`; clients behind a tunnel connect via
  the WebSocket relay instead (`[grpc] ws_url = "wss://…/ws/tunnel"`).
- Keep the OCI registry (port 3131) off Cloudflare too — Cloudflare caps
  request bodies at 100 MB (Free/Pro), breaking image blob pushes.
- The cache proxy (port 3128) should only be accessible on the LAN.

### Mandatory: rewrite X-Forwarded-For behind Cloudflare

If a Cloudflare-proxied hostname reaches monopsony through one of your own
reverse proxies (CF → nginx/Caddy/Traefik → server), the final proxy MUST
overwrite `X-Forwarded-For` with Cloudflare's `CF-Connecting-IP` header.
Cloudflare preserves attacker-supplied `X-Forwarded-For` values and appends
the real client IP, so the origin sees `forged, real-client` — and
monopsony's leftmost extraction picks the forged value, making per-IP rate
limiting, login lockout, and observed-IP display attacker-controllable.
The overwrite must happen at the LAST proxy in the chain:

- nginx: `proxy_set_header X-Forwarded-For $http_cf_connecting_ip;`
- Caddy: `header_up X-Forwarded-For {http.request.header.CF-Connecting-IP}`
- Traefik: headers middleware `customRequestHeaders.X-Forwarded-For` bound
  the same way

Then `[dashboard] trusted_proxies` must list ONLY that final proxy — never
Cloudflare's edge ranges (their hop is not a client source). Behind any
proxy, the mirror router (:3129) and OCI registry (:3131) keep keying their
rate buckets on the direct peer — one bucket per proxy IP — so size those
rate limits as a global budget in this posture.

## Configuration Reference

The full configuration reference is at `/etc/monopsony/server.toml`.
See also `dist/server.toml.example` in the source repo for all options
with inline comments.

Key sections:
- `[server]` — HTTP proxy bind address
- `[grpc]` — gRPC control plane (client pairing, heartbeats)
- `[database]` — PostgreSQL connection
- `[cache]` — Cache directory and size limits
- `[tls]` — Certificate paths (auto-generated by installer)
- `[dashboard]` — Web UI settings
- `[scan]` — Optional malware scanning (ClamAV + YARA)

## Troubleshooting

### Server won't start

```bash
# Check the logs
sudo journalctl -u monopsony-server -n 50 --no-pager

# Verify PostgreSQL is running
sudo systemctl status postgresql

# Test the config file
monopsony-server --config /etc/monopsony/server.toml --dry-run
```

### Dashboard shows 404

The dashboard is embedded in the binary. If you built from source, make
sure you ran `cd dashboard && npm install && npm run build` before
compiling the server with cargo. Pre-built release binaries have the
dashboard baked in.

### Client can't connect

```bash
# Verify the gRPC port is reachable
nc -zv YOUR-SERVER-IP 50051

# Check client logs
journalctl -u monopsony-client -n 50 --no-pager

# Re-pair if certificates are stale
monopsony-client pair --server YOUR-SERVER-IP:50051
```

### Reset Everything

```bash
# Uninstall (keeps database data)
sudo bash /opt/monopsony/install.sh --uninstall

# Full reset including database
sudo bash /opt/monopsony/install.sh --uninstall --purge
```

## Service Management

```bash
sudo systemctl start monopsony-server
sudo systemctl stop monopsony-server
sudo systemctl restart monopsony-server
sudo systemctl status monopsony-server
sudo journalctl -u monopsony-server -f    # Follow logs
```
