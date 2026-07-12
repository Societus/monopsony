# Client Toggle Commands + Docker Mirroring — Implementation Plan

Created: 2026-07-05
Status: Plan. For next session.

## Overview

Two related features:
1. **Toggle commands**: `monopsony-client proxy on/off/status` for each subsystem
2. **Docker mirroring**: Server-side Docker Registry V2 API on the proxy port

The toggle system gives the user an at-a-glance view of what's being proxied and
the ability to selectively enable/disable proxy interception per subsystem.

## Part 1: Toggle Commands

### New CLI Structure

```
monopsony-client proxy status           # Show what's proxied, what's not
monopsony-client proxy on [--pm] [--docker] [--shadow] [--all]
monopsony-client proxy off [--pm] [--docker] [--shadow] [--all]
```

Default (no flags): acts on all detected subsystems. Flags allow targeting one.

### `proxy status` Output

```
Monopsony Proxy Status
═══════════════════════════════════════════════
Server:           10.10.20.58:50051 (connected)
Daemon:           running (PID 12345)
Local proxy:      127.0.0.1:3129

Package Managers:
  pacman          ON   ✓ /etc/pacman.d/monopsony-proxy.conf exists
  apt             OFF  ✗ no proxy config found
  apk             ON   ✓ /etc/profile.d/monopsony-proxy.sh exists

Docker:
  registry mirror ON   ✓ /etc/docker/daemon.json has registry-mirrors entry
  upstream        http://10.10.20.58:3128

Shadow Packages:
  enabled         OFF  ✗ no shadow trust configured
```

### How Toggle Works Per Subsystem

#### Package Managers (pacman/apt/dnf/apk/etc.)

**ON**: Calls `backend.proxy_config(addr)` → writes config files / env vars.
**OFF**: Calls `backend.remove_proxy_config()` → removes config files.
**STATUS**: Checks if the config files exist (per-backend).

This reuses the existing `ProxyConfigAction` infrastructure. Each backend already
has `proxy_config()` and `remove_proxy_config()` methods that produce the right
files per OS:

| Backend | ON writes | OFF removes |
|---------|-----------|-------------|
| pacman | `/etc/pacman.d/monopsony-proxy.conf` (XferCommand) | same file |
| apt | `/etc/apt/apt.conf.d/99monopsony` (Acquire::http::Proxy) | same file |
| dnf | `/etc/dnf/dnf.conf` proxy= line (or `/etc/yum.repos.d/`) | reverts line |
| apk | `/etc/profile.d/monopsony-proxy.sh` (http_proxy env) | same file |
| winget | Sets `HTTP_PROXY`/`HTTPS_PROXY` env vars (registry on Windows) | unsets |
| choco | Same env var approach | unsets |
| scoop | Same env var approach | unsets |

For **status checking**, add a new trait method:

```rust
// In PackageManagerBackend trait
fn proxy_status(&self) -> ProxyStatus;

pub enum ProxyStatus {
    Enabled { details: String },   // Config file exists, env var set, etc.
    Disabled,                      // Not configured
    NotApplicable,                 // PM not installed on this system
}
```

Default implementation in the trait: checks if the file from `proxy_config()`
would exist by inspecting the action:

```rust
fn proxy_status(&self) -> ProxyStatus {
    if !self.is_available() {
        return ProxyStatus::NotApplicable;
    }
    // Try to detect by running proxy_config() and checking if files exist
    let action = self.proxy_config("dummy");
    match action {
        ProxyConfigAction::WriteFile { path, .. } => {
            if path.exists() { ProxyStatus::Enabled { details: path.display().to_string() } }
            else { ProxyStatus::Disabled }
        }
        ProxyConfigAction::SetEnv { key, .. } => {
            if std::env::var(&key).is_ok() { ProxyStatus::Enabled { details: format!("${} set", key) } }
            else { ProxyStatus::Disabled }
        }
        _ => ProxyStatus::Disabled,
    }
}
```

#### Docker

**ON**: Writes `/etc/docker/daemon.json` (Linux) or
`%ProgramData%\docker\config\daemon.json` (Windows) with:

```json
{
  "registry-mirrors": ["http://<monopsony-server-ip>:3128"]
}
```

Then restarts Docker daemon:
- Linux: `systemctl restart docker` (requires sudo/root)
- Windows: `Restart-Service docker` (requires admin)

**OFF**: Removes the `registry-mirrors` key from daemon.json, restarts Docker.

**STATUS**: Reads daemon.json, checks if `registry-mirrors` contains the Monopsony
server. Checks if Docker daemon is running.

```rust
fn docker_proxy_status() -> ProxyStatus {
    let daemon_json = if cfg!(windows) {
        PathBuf::from(std::env::var("ProgramData").unwrap_or_default())
            .join("docker/config/daemon.json")
    } else {
        PathBuf::from("/etc/docker/daemon.json")
    };

    if !daemon_json.exists() {
        return ProxyStatus::Disabled;
    }

    let content = std::fs::read_to_string(&daemon_json).ok()?;
    let config: serde_json::Value = serde_json::from_str(&content).ok()?;
    let mirrors = config["registry-mirrors"].as_array()?;

    if mirrors.iter().any(|m| m.as_str().map_or(false, |s| s.contains("monopsony") || s.contains(&server_ip))) {
        ProxyStatus::Enabled { details: format!("daemon.json registry-mirrors: {:?}", mirrors) }
    } else {
        ProxyStatus::Disabled
    }
}
```

#### Shadow Packages

**ON**: Enables shadow package trust enforcement (client trusts the server's signed
recipes). Writes `[shadow_trust]` config with `enabled = true`.
**OFF**: Sets `enabled = false`. Shadow packages are ignored but not uninstalled.
**STATUS**: Reads trust config, reports enabled/disabled + count of installed shadow
packages.

### Implementation Files

| File | Change |
|------|--------|
| `monopsony-client/src/main.rs` | Add `Proxy` subcommand with `on`/`off`/`status` subcommands |
| `monopsony-common/src/package_manager/mod.rs` | Add `proxy_status()` to trait, `ProxyStatus` enum |
| Each backend file (apk.rs, apt.rs, etc.) | Implement `proxy_status()` (or use default) |
| `monopsony-client/src/proxy_toggle.rs` | New module: toggle logic for PMs, Docker, shadow |

### Docker daemon.json Merge Logic

Writing daemon.json requires care — it may have existing config. Must merge, not
overwrite:

```rust
fn set_docker_mirror(server_addr: &str, enable: bool) -> anyhow::Result<()> {
    let path = docker_daemon_json_path()?;
    let mut config: serde_json::Value = if path.exists() {
        serde_json::from_str(&std::fs::read_to_string(&path)?)?
    } else {
        serde_json::json!({})
    };

    let mirror_url = format!("http://{}", server_addr);

    let mirrors = config
        .get_mut("registry-mirrors")
        .and_then(|v| v.as_array_mut());

    if enable {
        if mirrors.is_none() {
            config["registry-mirrors"] = serde_json::json!([mirror_url]);
        } else if !mirrors.unwrap().iter().any(|m| m == &serde_json::json!(mirror_url)) {
            mirrors.unwrap().push(serde_json::json!(mirror_url));
        }
    } else {
        if let Some(arr) = mirrors {
            arr.retain(|m| m.as_str().map_or(true, |s| !s.contains(server_addr)));
            if arr.is_empty() {
                // Remove empty array
                config.as_object_mut().unwrap().remove("registry-mirrors");
            }
        }
    }

    std::fs::write(&path, serde_json::to_string_pretty(&config)?)?;
    restart_docker_daemon()?;
    Ok(())
}
```

### Privilege Handling

Writing to `/etc/docker/daemon.json`, `/etc/pacman.d/`, `/etc/apt/apt.conf.d/`
requires root. The client should:

1. Detect if running as root (or check `sudo`/`doas` availability)
2. If not root: print the exact commands for the user to run with sudo
3. If root: execute directly

```rust
fn ensure_root_or_guide(action: &str) -> anyhow::Result<()> {
    if std::env::var("EUID").map_or(false, |euid| euid == "0") || !std::process::Command::new("id").arg("-u").output()?.stdout.trim().ends_with("0") {
        // Not root — print sudo commands
        eprintln!("This action requires root. Run with sudo:");
        eprintln!("  sudo monopsony-client proxy {}", action);
        anyhow::bail!("Permission denied — rerun with sudo");
    }
    Ok(())
}
```

On Windows: check for admin token via `net session` or `whoami /groups`.

---

## Part 2: Docker Registry V2 API (Server-Side)

### Endpoints to Implement

Three routes added to the Monopsony server's proxy (port 3128):

| Route | Method | Upstream | Cache? |
|-------|--------|----------|--------|
| `/v2/` | GET | (none — instant 200 `{}`) | No |
| `/v2/<name>/manifests/<ref>` | GET | `registry-1.docker.io` | Yes (TTL: 1h) |
| `/v2/<name>/blobs/<digest>` | GET | `registry-1.docker.io` | Yes (forever) |

Also need:
| `/v2/<name>/manifests/<ref>` | HEAD | Upstream | No (existence check) |
| `/v2/_catalog` | GET | Upstream | No (optional) |

### Auth Token Passthrough

Docker's auth flow with a mirror:

```
Docker → Mirror: GET /v2/library/nginx/manifests/latest
Mirror → Docker: 401 WWW-Authenticate: Bearer realm="https://auth.docker.io/token",service="registry.docker.io",scope="repository:library/nginx:pull"
Docker → auth.docker.io: GET token (directly, not through mirror)
Docker → Mirror: GET /v2/library/nginx/manifests/latest Authorization: Bearer <token>
Mirror → registry-1.docker.io: GET /v2/library/nginx/manifests/latest Authorization: Bearer <token>
Mirror → Docker: 200 {manifest JSON}
```

The mirror (Monopsony) **passes through** the Authorization header to the upstream
registry. It doesn't need to obtain tokens itself.

### Multi-Registry Routing

Docker image names encode the registry:

| Image name | Registry | Notes |
|------------|----------|-------|
| `nginx` / `library/nginx` | `registry-1.docker.io` | Docker Hub default |
| `ghcr.io/owner/repo` | `ghcr.io` | GitHub Container Registry |
| `quay.io/owner/repo` | `quay.io` | Red Hat Quay |
| `gcr.io/project/image` | `gcr.io` | Google Container Registry |
| `registry.gitlab.com/...` | `registry.gitlab.com` | GitLab Registry |
| `myregistry.com:5000/image` | `myregistry.com:5000` | Custom |

Routing logic:
```rust
fn resolve_upstream_registry(image_name: &str) -> &str {
    // If name contains a '.' or ':' in the first path segment, it's a custom registry
    let first_segment = image_name.split('/').next().unwrap_or("");
    if first_segment.contains('.') || first_segment.contains(':') {
        return first_segment;  // Custom registry host
    }
    "registry-1.docker.io"  // Default: Docker Hub
}
```

### Cache Key Strategy

| Artifact | Cache Key | TTL |
|----------|-----------|-----|
| Manifest by tag (`latest`, `3.15`) | `docker/v2/<name>/manifests/<tag>` | 1 hour (tags are mutable) |
| Manifest by digest (`sha256:...`) | `docker/v2/<name>/manifests/<digest>` | Forever (immutable) |
| Layer blob | `docker/v2/<name>/blobs/<digest>` | Forever (immutable) |

Blobs are the big win — 10-500MB each, sha256-addressed, shared across all images
that use the same base layer.

### Implementation Files (Server-Side)

| File | Change |
|------|--------|
| `monopsony-proxy/src/proxy.rs` | Add `/v2/` routes before catch-all |
| `monopsony-proxy/src/docker.rs` | New module: V2 API handlers, registry routing, auth passthrough |
| `monopsony-common/src/package_manager/mod.rs` | Add `DockerBackend` (or keep in docker.rs — not a PM per se) |
| `monopsony-mirror/src/mirror.rs` | Manifest TTL config in MirrorConfig |

### Estimated Effort

| Task | Hours |
|------|-------|
| `/v2/` API check handler | 0.5 |
| Manifest proxy + cache with TTL | 2 |
| Blob proxy + cache (reuses existing cache path) | 1.5 |
| Auth header passthrough | 1 |
| Multi-registry routing | 1.5 |
| Client toggle (daemon.json merge + restart) | 2 |
| Client `proxy status` command | 2 |
| Client `proxy on/off` for PMs (reuse existing) | 1 |
| Testing (live Docker pull through proxy) | 2 |
| **Total** | **~13.5 hours (2-3 sessions)** |

### Suggested Implementation Order

1. **Session 1**: Client toggle commands for existing PMs (proxy status/on/off)
2. **Session 2**: Server-side Docker V2 API (3 endpoints + cache)
3. **Session 3**: Client Docker toggle (daemon.json) + live integration test

---

## CLI Examples (Final Design)

```bash
# See everything at a glance
monopsony-client proxy status

# Turn on all detected subsystems
sudo monopsony-client proxy on --all

# Turn on just Docker
sudo monopsony-client proxy on --docker

# Turn off just pacman (keep Docker and apt proxied)
sudo monopsony-client proxy off --pm pacman

# Turn on only shadow packages
monopsony-client proxy on --shadow

# Docker-specific: also set HTTP_PROXY for docker CLI (not just daemon)
sudo monopsony-client proxy on --docker --docker-cli-proxy
```

### Windows Examples

```cmd
:: Run as Administrator
monopsony-client.exe proxy status
monopsony-client.exe proxy on --all
monopsony-client.exe proxy on --docker
monopsony-client.exe proxy off --docker
```

## Edge Cases

- **Docker not installed**: `proxy status` shows "Docker: not detected". `proxy on --docker` prints instructions.
- **Multiple PMs**: `proxy on --all` configures all detected PMs. Individual PMs via `--pm <name>`.
- **Daemon.json has existing mirrors**: Merge, don't overwrite. Preserve non-Monopsony mirrors.
- **Docker Desktop vs Docker Engine**: Docker Desktop on Windows/Mac stores config differently (`~/.docker/daemon.json` vs `/etc/docker/daemon.json`). Detect which is installed.
- **Rootless Docker**: Config path is `~/.config/docker/daemon.json`. Check `DOCKER_HOST` env var.
- **Containerd**: Some systems use containerd directly (no Docker daemon). Containerd has its own mirror config (`/etc/containerd/config.toml`). Detect and support if present.
