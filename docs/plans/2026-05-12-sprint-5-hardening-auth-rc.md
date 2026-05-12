# Sprint 5: Hardening + Auth Foundation (RC Tag) — Implementation Plan

> **For Hermes:** Use subagent-driven-development skill to implement this plan task-by-task.

**Goal:** Tag v0.1.0-rc1. No known panics, no security holes in network-facing code, auth mode enum scaffolded, GPG verification in place, documentation complete.

**Architecture:** Sprint 5 builds defensive layers around the existing codebase. GPG verification wraps the mirror sync engine. Auth middleware wraps the proxy router. Config validation moves from serde-path-to-error to post-deserialization validate(). The security audit reviews all network-facing code.

**Baseline:** 45 tests pass (1 tracker pre-existing failure), 0 clippy, ~29 warnings (all dead_code/unused).

---

## Pre-flight: Fix tracker test regression

**Objective:** The tracker smoke test `test_tracker_announce_and_discover` returns 500. Fix this before starting Sprint 5 items.

**Files:**
- Investigate: `crates/monopsony-tracker/src/main.rs`, `crates/monopsony-tracker/tests/smoke_test.rs`

**Step 1:** Get the 500 error details

```bash
cd ~/projects/monopsony
PROTOC=/home/societus/.local/bin/python-grpc-tools-protoc \
  RUST_BACKTRACE=1 cargo test -p monopsony-tracker --test smoke_test test_tracker_announce_and_discover 2>&1 | tail -40
```

**Step 2:** Debug the tracker's announce handler. Check if it's a DB schema issue, missing route, or deserialization problem. Fix the root cause.

**Step 3:** Run full test suite to verify no regressions.

```bash
PROTOC=/home/societus/.local/bin/python-grpc-tools-protoc cargo test 2>&1 | grep 'test result'
```

**Expected:** All 46 tests pass (45 existing + 1 previously failing tracker test).

---

## Item 1: GPG Signature Verification for Repo Metadata

**Objective:** Verify `Release`/`InRelease` files against distro GPG keys during mirror sync. Fail-closed by default. Configurable per-mirror.

### Task 1.1: Add `gpg_verify` field to MirrorConfig

**Files:**
- Modify: `crates/monopsony-server/src/mirror.rs:42-84` (MirrorConfig struct)
- Modify: `dist/mirrors.toml.example`

**Step 1:** Add field to MirrorConfig:

```rust
// In MirrorConfig struct, after hash_policy field (line ~83):
/// Enable GPG verification of repository metadata (Release/InRelease files)
#[serde(default = "default_true")]
pub gpg_verify: bool,
/// Path to GPG keyring directory or keyfile for this mirror's upstream distro
pub gpg_keyring: Option<PathBuf>,
```

**Step 2:** Add `gpg_verify` and `gpg_keyring` entries to `dist/mirrors.toml.example` with comments.

**Step 3:** `cargo check` to verify compilation.

### Task 1.2: Create GPG verification module

**Files:**
- Create: `crates/monopsony-server/src/gpg.rs`
- Modify: `crates/monopsony-server/src/main.rs` (add `mod gpg;`)
- Modify: `crates/monopsony-server/Cargo.toml` (add `gpgme` dependency or use `tokio::process::Command` for `gpgv`)

**Step 1:** Create `gpg.rs` with a `verify_release_file` function.

Design decision: Use `gpgv` subprocess (simpler, no extra crate, available on all distros Monopsony targets). Fall back to a clear error if `gpgv` is not in PATH.

```rust
//! GPG signature verification for repository metadata

use std::path::Path;
use tracing::{debug, info, warn};

/// Verify a Release/InRelease file against a GPG keyring.
/// Returns Ok(()) if signature is valid, Err with description otherwise.
pub async fn verify_release_signature(
    release_file: &Path,
    signature_file: Option<&Path>,  // None for InRelease (inline signature)
    keyring: &Path,
) -> Result<(), String> {
    // Use gpgv for verification
    // For InRelease files: gpgv --keyring <keyring> <file>
    // For detached signatures: gpgv --keyring <keyring> <sig> <file>
    ...
}
```

**Step 2:** Implement using `tokio::process::Command` to invoke `gpgv`. Parse exit code and stderr for error details.

**Step 3:** Write tests that use test fixtures (generate a throwaway GPG key, sign a file, verify).

### Task 1.3: Wire GPG verification into mirror sync

**Files:**
- Modify: `crates/monopsony-server/src/mirror.rs` (in the sync logic that downloads Release/InRelease files)

**Step 1:** Find where the mirror sync downloads `Release`, `InRelease`, or `repomd.xml` files. After download, if `gpg_verify` is true and a keyring is configured, call `verify_release_signature`.

**Step 2:** If verification fails and policy is "fail-closed": log error, skip this sync batch, don't update local mirror. If "fail-open": log warning, proceed.

**Step 3:** Test with a manual sync run (can use a test keyring against a real Arch mirror).

### Task 1.4: Default keyring paths for common distros

**Files:**
- Modify: `crates/monopsony-server/src/gpg.rs`

**Step 1:** Add a function that resolves the default system keyring path for known distros:

```rust
/// Return the default GPG keyring path for a distribution.
/// Arch: /usr/share/keyrings/archlinux-keyring
/// Debian/Ubuntu: /usr/share/keyrings/debian-archive-keyring.gpg
/// Fedora/RHEL: /etc/pki/rpm-gpg/
/// Alpine: /usr/share/apk/keys/
pub fn default_keyring_path(distro: &str) -> Option<PathBuf> { ... }
```

**Step 2:** Wire this into the sync logic so if `gpg_keyring` is None but `gpg_verify` is true, we use the default.

**Step 3:** Write unit tests for each distro path.

---

## Item 2: Auth Mode Enum + Config + Middleware Scaffold

**Objective:** Add `auth_mode` to ServerConfig, create axum middleware that branches on the mode.

### Task 2.1: Add AuthMode enum and config field

**Files:**
- Modify: `crates/monopsony-common/src/config.rs`

**Step 1:** Add AuthMode enum:

```rust
/// HTTP proxy authentication mode
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AuthMode {
    /// No authentication (LAN trust model)
    Open,
    /// API key via X-Monopsony-Key header
    Token,
    /// Mutual TLS client certificate auth (Sprint 6)
    Mtls,
}

impl Default for AuthMode {
    fn default() -> Self {
        Self::Open
    }
}
```

**Step 2:** Add `auth_mode` field to `HttpServerConfig`:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpServerConfig {
    pub bind: String,
    pub hostname: Option<String>,
    /// Authentication mode: "open" (default), "token", "mtls"
    #[serde(default)]
    pub auth_mode: AuthMode,
}
```

**Step 3:** Run `cargo check`. Fix any serde default issues. Existing configs without `auth_mode` will default to `Open` — no breaking change.

### Task 2.2: Create auth middleware module

**Files:**
- Create: `crates/monopsony-server/src/auth.rs`
- Modify: `crates/monopsony-server/src/main.rs` (add `mod auth;`)

**Step 1:** Create `auth.rs` with an axum middleware:

```rust
//! HTTP proxy authentication middleware

use axum::{
    body::Body,
    extract::State,
    http::{Request, Response, StatusCode},
    middleware::Next,
    response::IntoResponse,
};
use monopsony_common::config::AuthMode;
use std::sync::Arc;

use crate::db::Database;

pub async fn auth_middleware(
    State(state): State<Arc<AuthState>>,
    req: Request<Body>,
    next: Next,
) -> impl IntoResponse {
    match state.auth_mode {
        AuthMode::Open => next.run(req).await,
        AuthMode::Token => token_auth(state, req, next).await,
        AuthMode::Mtls => {
            // Sprint 6 placeholder
            Response::builder()
                .status(StatusCode::NOT_IMPLEMENTED)
                .body(Body::from("mTLS auth mode not yet implemented"))
                .unwrap_or_else(|_| Response::new(Body::from("501 Not Implemented")))
        }
    }
}

async fn token_auth(
    state: Arc<AuthState>,
    req: Request<Body>,
    next: Next,
) -> Response<Body> {
    let key = req.headers()
        .get("X-Monopsony-Key")
        .and_then(|v| v.to_str().ok());

    match key {
        Some(key) => {
            // Hash the key and look it up
            let hash = sha256_hex(key);
            if let Some(db) = &state.db {
                match db.validate_api_key(&hash).await {
                    Ok(Some(_id)) => next.run(req).await,
                    _ => unauthorized_response(),
                }
            } else {
                unauthorized_response()
            }
        }
        None => unauthorized_response(),
    }
}

fn unauthorized_response() -> Response<Body> {
    Response::builder()
        .status(StatusCode::UNAUTHORIZED)
        .header("WWW-Authenticate", "X-Monopsony-Key")
        .body(Body::from("Unauthorized: missing or invalid API key"))
        .unwrap_or_else(|_| Response::new(Body::from("401 Unauthorized")))
}

fn sha256_hex(input: &str) -> String {
    use sha2::{Sha256, Digest};
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    hex::encode(hasher.finalize())
}

pub struct AuthState {
    pub auth_mode: AuthMode,
    pub db: Option<Arc<Database>>,
}
```

**Step 2:** `cargo check`.

### Task 2.3: Wire auth middleware into proxy router

**Files:**
- Modify: `crates/monopsony-server/src/proxy.rs:142` (create_proxy_router)
- Modify: `crates/monopsony-server/src/main.rs` (pass auth_mode to proxy setup)

**Step 1:** In `create_proxy_router`, add the auth middleware layer:

```rust
use crate::auth::{AuthState, auth_middleware};

pub fn create_proxy_router(
    state: Arc<ProxyState>,
    rate_limiter: Option<Arc<RateLimiter>>,
    auth_state: Arc<AuthState>,
) -> axum::Router {
    axum::Router::new()
        .route("/health", axum::routing::get(health_check))
        .route("/*path", axum::routing::any(handle_proxy_request))
        .route("/", axum::routing::any(handle_proxy_request))
        .layer(axum::middleware::from_fn_with_state(auth_state, auth_middleware))
        .with_state((state, rate_limiter))
}
```

Note: The `/health` endpoint should bypass auth. Use `axum::middleware::from_fn_with_state` applied after health route, or add a path check in the middleware to skip `/health`.

**Step 2:** In `main.rs`, create `AuthState` from config and pass to `create_proxy_router`.

**Step 3:** Run `cargo check`, then `cargo test`.

### Task 2.4: Tests for auth middleware

**Files:**
- Create: `crates/monopsony-server/tests/auth_test.rs`

**Step 1:** Write tests:
- Open mode: any request passes through
- Token mode: request without header → 401
- Token mode: request with valid key → passes through (mock DB)
- Token mode: request with invalid key → 401
- Mtls mode: any request → 501

**Step 2:** `cargo test -p monopsony-server --test auth_test`

---

## Item 3: v1.0 Config Validation (Option D)

**Objective:** Migrate from serde_path_to_error to post-deserialization `validate()` with human-friendly messages.

### Task 3.1: Add validate() method to ServerConfig

**Files:**
- Modify: `crates/monopsony-common/src/config.rs`

**Step 1:** Add a validation function:

```rust
impl ServerConfig {
    pub fn validate(&self) -> Result<(), Vec<String>> {
        let mut errors = Vec::new();

        // Server section
        if self.server.bind.is_empty() {
            errors.push("missing required field 'bind' in [server] — add something like bind = \"0.0.0.0:3128\"".to_string());
        } else if !self.server.bind.contains(':') {
            errors.push(format!(
                "[server].bind \"{}\" doesn't look like host:port — expected something like \"0.0.0.0:3128\"",
                self.server.bind
            ));
        }

        // Database URL
        if self.database.url.is_empty() {
            errors.push("missing [database].url — add something like url = \"postgres://monopsony:password@localhost/monopsony\"".to_string());
        } else if !self.database.url.starts_with("postgres://") {
            errors.push(format!(
                "[database].url should start with postgres:// — got \"{}\"",
                &self.database.url[..self.database.url.len().min(30)]
            ));
        }

        // Cache path
        if self.cache.path.as_os_str().is_empty() {
            errors.push("missing [cache].path — add something like path = \"/var/cache/monopsony\"".to_string());
        }

        // Cross-field: mdns_enabled without hostname
        if self.discovery.mdns_enabled && self.server.hostname.is_none() {
            // This is a warning, not an error — mDNS works without explicit hostname
            // but the user should know the auto-detected name will be used
        }

        // TLS paths
        if !self.tls.ca_path.exists() {
            errors.push(format!(
                "[tls].ca_path \"{}\" does not exist — run the installer to generate certificates, or point to your CA cert",
                self.tls.ca_path.display()
            ));
        }

        // Auth mode validation
        if self.server.auth_mode == AuthMode::Mtls {
            errors.push("auth_mode = \"mtls\" is not yet implemented — use \"open\" or \"token\" for now".to_string());
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }
}
```

**Step 2:** Keep `serde_path_to_error` for deserialization error path reporting (it's still useful for "which TOML key is wrong"). Add `validate()` as a post-deserialization step. Call it in `from_file` after deserialization.

**Step 3:** Update `ServerConfig::from_file` to call `validate()`:

```rust
pub fn from_file(path: &std::path::Path) -> crate::Result<Self> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| crate::Error::Io { path: path.to_path_buf(), source: e })?;
    let config: Self = Self::from_str_with_path(&content, path)?;
    config.validate().map_err(|errors| {
        crate::Error::Config(format!(
            "Configuration validation errors in {}:\n  - {}",
            path.display(),
            errors.join("\n  - ")
        ))
    })?;
    Ok(config)
}
```

**Step 4:** Write tests for validate():
- Valid config → Ok
- Missing bind → error with suggestion
- Bad URL format → error with hint
- mtls mode → "not yet implemented" error
- Port range validation

### Task 3.2: Add validate() to ClientConfig

**Files:**
- Modify: `crates/monopsony-common/src/config.rs`

**Step 1:** Similar validate() for ClientConfig — check bind format, TLS paths, etc.

**Step 2:** Wire into `ClientConfig::from_file`.

**Step 3:** Tests.

---

## Item 4: sqlx 0.9 Upgrade (or Pin Workaround)

**Objective:** Check sqlx 0.9 status. If still alpha, document the workaround.

### Task 4.1: Check and document

**Step 1:** sqlx 0.9.0-alpha.1 is current (checked 2026-05-12). Not suitable for production. The current sqlx 0.8 + rusqlite 0.31 conflict is a future-incompat warning only, not a build failure.

**Step 2:** Add a note in the workspace Cargo.toml:

```toml
# sqlx 0.8 has a future-incompat warning with libsqlite3-sys (via rusqlite 0.31).
# Tracked upstream: https://github.com/launchbadge/sqlx/issues/XXXX
# Will resolve when sqlx 0.9 is stable. No action needed for RC.
```

**Step 3:** Verify `cargo clippy` still passes with 0 warnings.

This item is a documentation-only change. No code changes needed.

---

## Item 5: Security Audit Pass

**Objective:** Review all network-facing code for security holes.

### Task 5.1: Audit proxy.rs

**Files:**
- Review: `crates/monopsony-server/src/proxy.rs`

Checklist:
- [ ] SSRF: `validate_url_host` called on all upstream fetches? Search for all `client.request()` calls.
- [ ] Path traversal: `validate_cache_path` called before all file writes? Search for `tokio::fs::write` and `create_dir_all`.
- [ ] Response body size: `MAX_RESPONSE_BODY_SIZE` enforced on all upstream responses?
- [ ] No unsanitized data in log messages that could inject false entries
- [ ] Rate limiting actually enforced (already wired in Sprint 2, verify it's working)
- [ ] No information leakage in error responses (no internal paths, no stack traces)

### Task 5.2: Audit grpc.rs

**Files:**
- Review: `crates/monopsony-server/src/grpc.rs`

Checklist:
- [ ] mTLS required on all gRPC endpoints (verify TLS config is not optional)
- [ ] No unauthenticated gRPC access possible
- [ ] Pairing code not logged in plaintext
- [ ] Input validation on all RPC parameters
- [ ] No DoS vectors: large messages, many concurrent streams

### Task 5.3: Audit mirror.rs

**Files:**
- Review: `crates/monopsony-server/src/mirror.rs`

Checklist:
- [ ] SSRF: upstream URL validated before fetching
- [ ] Path traversal: local_path doesn't escape mirror_dir
- [ ] Sync loop has timeout for individual file downloads
- [ ] No symlink following during file writes
- [ ] Bandwidth limits actually enforced

### Task 5.4: Fix findings from audit

**Files:**
- Whatever files have issues found in Tasks 5.1-5.3

For each finding, write a test that demonstrates the vulnerability, then fix it.

---

## Item 6: End-to-End Test Matrix

**Objective:** Test that clients on Arch, Debian, and Fedora can use Monopsony as a proxy.

### Task 6.1: Create test matrix document

**Files:**
- Create: `docs/test-matrix.md`

**Step 1:** Create the document with the test structure:

```markdown
# Monopsony Test Matrix

## Test Environment
- Server: [describe test server]
- Method: LXC containers with clean installs

## Test Cases

### Arch Linux (pacman)
| Step | Command | Expected | Result |
|------|---------|----------|--------|
| Install client | `curl ... \| bash` | Binary installed | |
| Pair | `monopsony-client pair ...` | Certs saved | |
| Configure proxy | `monopsony-client configure` | pacman.conf updated | |
| Run update | `pacman -Syu --downloadonly` | Packages cached | |
| Verify cache hit | Check server logs | Cache hit on second run | |

### Debian (apt)
[same structure]

### Fedora (dnf)
[same structure]
```

### Task 6.2: Execute tests (manual)

This requires actual LXC CTs. Document results in the matrix. If we can't spin up CTs right now, mark as "pending manual testing" and fill in the procedure.

---

## Item 7: Release Signing

**Objective:** GPG-sign release binaries with SHA256 checksums.

### Task 7.1: Create release signing script

**Files:**
- Create: `dist/sign-release.sh`

```bash
#!/bin/bash
# Sign a Monopsony release
# Usage: ./dist/sign-release.sh <version> <gpg-key-id>
set -euo pipefail

VERSION="${1:?Usage: sign-release.sh <version> <gpg-key-id>}"
GPG_KEY="${2:?Usage: sign-release.sh <version> <gpg-key-id>}"
RELEASE_DIR="release-${VERSION}"

mkdir -p "${RELEASE_DIR}"

# Generate SHA256SUMS
sha256sum "${RELEASE_DIR}"/* > "${RELEASE_DIR}/SHA256SUMS"

# Sign SHA256SUMS
gpg --default-key "${GPG_KEY}" --detach-sign "${RELEASE_DIR}/SHA256SUMS"
gpg --default-key "${GPG_KEY}" --armor --detach-sign "${RELEASE_DIR}/SHA256SUMS"

# Sign individual binaries
for f in "${RELEASE_DIR}"/*; do
    [[ -f "$f" && "$f" != *.sig && "$f" != *.asc ]] || continue
    gpg --default-key "${GPG_KEY}" --detach-sign "$f"
done

echo "Signed release ${VERSION} with key ${GPG_KEY}"
```

### Task 7.2: Document signing key in README

**Files:**
- Modify: `README.md`

Add a section:

```markdown
## Verifying Releases

Releases are signed with GPG. Import the signing key:
```bash
gpg --recv-keys <FINGERPRINT>
```

Verify a binary:
```bash
gpg --verify monopsony-server-0.1.0-rc1-x86_64-unknown-linux-musl.tar.gz.sig
```
```

---

## Item 8: Documentation — Config Reference

**Objective:** Document every config field for server, client, and mirror configs.

### Task 8.1: Create docs/config-reference.md

**Files:**
- Create: `docs/config-reference.md`

**Step 1:** Document all fields:

```markdown
# Monopsony Configuration Reference

## Server Config (server.toml)

### [server]
| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `bind` | string | required | HTTP proxy bind address (host:port) |
| `hostname` | string | auto | Hostname for TLS certificate SAN |
| `auth_mode` | enum | `"open"` | Authentication mode: open, token, mtls |

### [grpc]
| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `bind` | string | required | gRPC control plane bind address |

[... for every section: database, cache, tls, discovery, metrics, logging]
```

**Step 2:** Add client config reference section.
**Step 3:** Add mirror config (mirrors.toml) reference section.
**Step 4:** Add example scenarios:
- SMB office: basic LAN setup
- Homelab: single server, few clients
- Public mirror: auth enabled, public mirror mode

---

## Item 9: Tag v0.1.0-rc1

**Objective:** Update version, tag, prepare for release.

### Task 9.1: Update version in all Cargo.toml files

**Files:**
- Modify: `Cargo.toml` (workspace.package.version)
- Modify: `crates/monopsony-common/Cargo.toml`
- Modify: `crates/monopsony-server/Cargo.toml`
- Modify: `crates/monopsony-client/Cargo.toml`
- Modify: `crates/monopsony-tracker/Cargo.toml`

Change `version = "0.1.0"` to `version = "0.1.0-rc.1"`.

### Task 9.2: Update AGENTS.md

**Files:**
- Modify: `AGENTS.md`

Add Sprint 5 completion status. Update compilation status table.

### Task 9.3: Create git tag

```bash
git tag -a v0.1.0-rc.1 -m "Release candidate 1: Hardening + Auth Foundation"
```

Do NOT push the tag until user approves.

---

## Execution Order

1. **Pre-flight:** Fix tracker test (prerequisite for clean baseline)
2. **Item 2:** Auth mode enum + middleware (foundational, other items depend on it)
3. **Item 3:** Config validation (auth_mode needs validation too)
4. **Item 1:** GPG verification (independent, can run in parallel with auth)
5. **Item 5:** Security audit (reviews all code including new auth middleware)
6. **Item 4:** sqlx documentation (quick, do alongside audit)
7. **Item 7:** Release signing script (independent)
8. **Item 6:** Test matrix (requires items 1-5 complete)
9. **Item 8:** Config reference docs (requires items 1-3 complete for accurate field lists)
10. **Item 9:** Tag rc1 (last, after everything else passes)

**Suggested parallel execution:**
- Phase A: Pre-flight fix + Items 2 + 3 (auth + config validation)
- Phase B: Items 1 + 4 (GPG + sqlx doc)
- Phase C: Items 5 + 7 (audit + signing)
- Phase D: Items 6 + 8 (test matrix + docs)
- Phase E: Item 9 (tag)
