# Monopsony

Centralized package caching proxy for LAN environments. One server pulls packages from upstream repos; every other machine on the network gets instant cache hits.

**Scale:** offices, labs, homelabs (20-200 machines, single server).

## Features

- **HTTP caching proxy**: transparent cache for apt, dnf, pacman, and 7 other package managers
- **mTLS security**: built-in private CA, automatic client certificate generation
- **Distribution mirrors**: sync and serve Arch, Debian, Alpine, Fedora repos locally
- **Tracker service**: discover mirrors by geo-proximity with operator reputation and blacklisting
- **Shadow packages**: install third-party software via signed TOML recipes with rollback chains
- **Scheduled auto-updates**: cron-based update windows with severity filtering
- **Federated recipes**: share recipes via tracker with dual-signature verification and voting

## Install

### Client (default)

Route this machine's package traffic through a Monopsony server:

```bash
curl -fsSL https://monopsony.dev/install.sh | bash
```

The installer detects your distro and configures the package manager proxy:
- apt: `/etc/apt/apt.conf.d/99monopsony-proxy`
- dnf: `proxy=` in `/etc/dnf/dnf.conf`
- nix-daemon: systemd override with proxy env
- pacman, apk, xbps, emerge, zypper: environment variables via profile.d

### Server

```bash
curl -fsSL https://monopsony.dev/install.sh | bash -s -- --server
```

Sets up PostgreSQL, TLS, systemd/OpenRC service, health check.

## Supported Platforms

**Client** (proxy config only, no local database):

| Distro | Package Manager | Method |
|--------|----------------|--------|
| Debian 12+ / Ubuntu 22.04+ | apt | apt.conf.d |
| Rocky / Alma / RHEL / CentOS 9+ | dnf | dnf.conf |
| Arch | pacman | env vars |
| Alpine 3.x | apk | env vars |
| NixOS | nix-daemon | systemd override |
| openSUSE Tumbleweed/Leap, SLES | zypper | env vars |
| Void Linux | xbps | env vars |
| Gentoo | emerge | env vars |
| Clear Linux | swupd | env vars |
| Solus | eopkg | env vars |

**Server** (full install with PostgreSQL):

Tested on Debian 12, Ubuntu 25.04, Arch, Rocky 9.4, Alpine 3.22.

Code paths exist for openSUSE, Void (runit), Gentoo, Clear Linux, Solus.

Arches: x86_64, aarch64 (musl-static binary).

## Architecture

```
              Upstream Repos
                    |
         monopsony-server
         :3128 (HTTP proxy)
         :3129 (Mirror HTTP)
         :50051 (gRPC mTLS)
         :9090 (Prometheus)
                |
           PostgreSQL
                |
         Clients (pacman,
          apt, dnf, nix, ...)

       monopsony-tracker
       :3130 (HTTP tracker)
       SQLite
```

First client request: cache miss, proxy fetches from upstream, caches, serves.
Second request for same package: instant cache hit from local storage.

### Shadow Packages

Shadow packages install third-party software outside the system package manager:

```toml
# recipe.toml — signed with Ed25519
[package]
name = "tailscale"
version = "1.72.0"

[source]
type = "github_release"
repo = "tailscale/tailscale"
asset = "tailscale_{{version}}_{{arch}}.tar.gz"

[install]
method = "extract"
binaries = [
  { source = "tailscale", target = "/usr/local/bin/tailscale" },
  { source = "tailscaled", target = "/usr/local/bin/tailscaled" },
]

[health_check]
type = "command"
command = "tailscale version"
```

Features: rollback chains (last N backups), cron-based auto-updates, version detection via command/file/HTTP header, pre/post hooks.

### Tracker

The tracker enables multi-site mirror discovery:

- Operators register mirrors with Nostr-style Ed25519 identity
- Clients discover nearest mirrors by geo-proximity scoring
- Reputation system with anti-malice dispute mechanism
- Client reports (hash mismatch, stale mirror) trigger auto-blacklisting
- Server metrics aggregation (installs, OS/arch distribution)

## Other Flags

```bash
curl -fsSL ... | bash -s -- --server        # Install server
curl -fsSL ... | bash -s -- --client        # Install client (same as default)
curl -fsSL ... | bash -s -- --uninstall     # Remove everything
curl -fsSL ... | bash -s -- --upgrade       # Download newer binary, restart
curl -fsSL ... | bash -s -- --reconfigure   # Re-run setup prompts
```

## Configuration

Server: `/etc/monopsony/server.toml`
Mirrors: `/etc/monopsony/mirrors.toml`

See `dist/server.toml.example` and `dist/mirrors.toml.example`.

## Development

```bash
cargo check
cargo build
cargo test
cargo clippy
```

238 tests, 0 failures, 4 crates.

## License

MIT
