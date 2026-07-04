# Alpine (apk) Package Manager Backend — Implementation Plan

Created: 2026-07-04
Status: Not started. Blocked on this file being implemented.

## Problem

Alpine Linux clients can connect to Monopsony and run `apk upgrade` through
the proxy chain, but **no packages are cached**. The proxy passes all Alpine
traffic through `proxy_passthrough` because no `apk` backend exists. Every
`dl-cdn.alpinelinux.org` URL is classified as non-package and fetched fresh
from upstream on every request.

Verified live on 10.10.20.159 (Alpine 3.22.0):
- `apk update` + `apk upgrade --available` works through the chain
- 0 entries in `cache_entries` table for alpinelinux.org URLs
- Server log shows no "Cached:" messages for Alpine traffic

## Implementation

### 1. Add `Apk` to `PackageManagerKind` enum

**File**: `crates/monopsony-common/src/package_manager/mod.rs`

- Add `Apk` variant to `PackageManagerKind` enum (line 36)
- Add `"apk" => Some(PackageManagerKind::Apk)` to `parse_kind()` (line 67)
- Add `PackageManagerKind::Apk => "apk"` to `as_str()` (line 50)
- Add `mod apk;` and `pub use apk::ApkBackend;` to module declarations (lines 6-27)
- Add `Box::new(ApkBackend::new())` to `get_available_backends()` (line 260)
- Add `PackageManagerKind::Apk => Some(Box::new(ApkBackend::new()))` to `get_backend()` (line 278)

### 2. Create `crates/monopsony-common/src/package_manager/apk.rs`

Model after `pacman.rs` (simplest existing backend). Key details:

```rust
pub struct ApkBackend;

#[async_trait]
impl PackageManagerBackend for ApkBackend {
    fn kind(&self) -> PackageManagerKind { PackageManagerKind::Apk }

    fn is_available(&self) -> bool {
        which::which("apk").is_ok()
    }

    async fn list_installed(&self) -> crate::Result<Vec<PackageRef>> {
        // apk info -v output: one package per line, format:
        //   busybox-1.37.0-r20
        // Or use: apk list --installed
        // Parse name-version-rN pattern
    }

    fn upstream_url_patterns(&self) -> Vec<UrlPattern> {
        vec![
            // Package files: .apk extension
            UrlPattern::new(r"https?://[^/]*alpinelinux\.org/.*\.apk", 100, "Alpine packages"),
            // Index metadata: APKINDEX.tar.gz
            UrlPattern::new(r"https?://[^/]*alpinelinux\.org/.*APKINDEX.*\.tar\.gz", 90, "Alpine repo index"),
            // Signature files
            UrlPattern::new(r"https?://[^/]*alpinelinux\.org/.*\.SIGN\.RSA", 80, "Alpine signatures"),
        ]
    }

    fn url_to_cache_path(&self, url: &str) -> Option<String> {
        let url = url::Url::parse(url).ok()?;
        Some(format!("apk{}", url.path()))
    }

    fn proxy_config(&self, proxy_addr: &str) -> ProxyConfigAction {
        // apk uses http_proxy/https_proxy env vars, no config file.
        // Return SetEnv action. The client daemon can export this or
        // write to /etc/profile.d/monopsony-proxy.sh
        ProxyConfigAction::Multiple(vec![
            ProxyConfigAction::WriteFile {
                path: PathBuf::from("/etc/profile.d/monopsony-proxy.sh"),
                content: format!(
                    "export http_proxy=http://{}\nexport https_proxy=http://{}\n",
                    proxy_addr, proxy_addr
                ),
            }
        ])
    }

    fn remove_proxy_config(&self) -> ProxyConfigAction {
        ProxyConfigAction::RemoveFile {
            path: PathBuf::from("/etc/profile.d/monopsony-proxy.sh"),
        }
    }
}
```

### 3. Update proto to include apk in manager string

No proto change needed — `PackageEntry.manager` is a free-text string field.
The client just needs to send `"apk"` as the manager value.

### 4. Client-side: detect apk and report inventory

**File**: `crates/monopsony-client/src/main.rs`

The client's inventory reporting loop (line ~353) calls
`get_available_backends()` which auto-detects installed package managers.
Once `ApkBackend` is registered, Alpine clients will automatically report
their installed packages via `report_inventory()`.

### 5. Repo index parsing for prefetch

**File**: `crates/monopsony-mirror/src/repo_index.rs`

Alpine's APKINDEX.tar.gz format differs from pacman's .db files:
- Contains `APKINDEX` (plaintext package list) and `DESCRIPTIONS` files
- Each package line: `P:<name>\nV:<version>\nA:<arch>\n...` (key-value pairs)
- Need to add `parse_alpine_apkindex()` function
- Add `"alpine"` case to `resolve_download_urls()` — Alpine URLs are already
  full paths (`/alpine/v3.22/main/x86_64/busybox-1.37.0-r20.apk`), so
  download_url = upstream + path, no transformation needed

### 6. Mirror config for Alpine test environment

On server (10.10.20.58), add a mirror config for Alpine:
```toml
[[mirrors]]
distro = "alpine"
upstream_url = "https://dl-cdn.alpinelinux.org/alpine"
local_path = "alpine"
repos = ["main", "community"]
arches = ["x86_64"]
```

## Testing Checklist

1. `cargo build -p monopsony-server --release` compiles
2. `cargo test -p monopsony-common` — new apk backend tests pass
3. Deploy to 10.10.20.58, clear cache
4. On 10.10.20.159: `http_proxy=http://127.0.0.1:3129 apk update`
5. Verify APKINDEX.tar.gz appears in cache_entries with `hit_count` incrementing on second `apk update`
6. On 10.10.20.159: `http_proxy=http://127.0.0.1:3129 apk add <new-package>`
7. Verify .apk file cached, second install of same package = X-Cache: HIT
8. Verify prefetch picks up Alpine packages from inventory reports

## Gotchas

- Alpine URLs use `dl-cdn.alpinelinux.org` — the regex must match this host
- apk versions use `-rN` suffix (e.g., `1.37.0-r20`), different from pacman's `-N`
- apk can also use CDNs with different hostnames — pattern should be permissive
- Alpine version increments (3.22 → 3.23) change the URL path (`/alpine/v3.22/` → `/alpine/v3.23/`),
  so mirror config must match the client's `/etc/apk/repositories` version
