//! cargo package manager backend implementation (Sprint 23 Phase B).
//!
//! Caches crates.io's sparse-index metadata and crate artifacts. The sparse
//! index is newline-sensitive (JSON lines), so its cache key must be stable;
//! artifacts are immutable (`LangArtifact`).
//!
//! URL shapes verified live 2026-09-21 (curl, 200/302):
//! - sparse index: `https://index.crates.io/<a>/<ab>/<crate>` — NO `/index/`
//!   prefix; the path IS the repo layout (e.g. `/ca/rg/cargo`, `/2/in`)
//! - new download: `https://crates.io/api/v1/crates/<name>/<ver>/download`
//!   (302 → static) and `https://static.crates.io/crates/<name>/<ver>/download`
//! - legacy static: `https://static.crates.io/crates/<name>/<name>-<ver>.crate`
//!   (still 200; kept for older cargo clients)

use crate::{PackageManagerBackend, PackageManagerKind, PackageRef, ProxyConfigAction, UrlPattern};
use std::path::PathBuf;

/// cargo package manager backend.
pub struct CargoBackend {
    #[allow(dead_code)] // Populated by `proxy_config` when a client is configured.
    config_path: Option<PathBuf>,
}

impl CargoBackend {
    /// Create a new cargo backend instance.
    pub fn new() -> Self {
        CargoBackend { config_path: None }
    }
}

impl Default for CargoBackend {
    fn default() -> Self {
        Self::new()
    }
}

/// Parse `cargo install --list` output: `name v1.2.3:` headers with indented
/// binaries beneath. Returns one entry per installed crate version.
fn parse_cargo_install_list(stdout: &str) -> Vec<PackageRef> {
    let mut packages = Vec::new();
    for line in stdout.lines() {
        let line = line.trim_start();
        // Header lines are not indented in the raw output and end with ':'.
        if line.starts_with(' ') {
            continue;
        }
        let header = line.strip_suffix(':').unwrap_or(line);
        let mut parts = header.split_whitespace();
        if let (Some(name), Some(ver_tok)) = (parts.next(), parts.next()) {
            if let Some(ver) = ver_tok.strip_prefix('v') {
                packages.push(PackageRef {
                    name: name.to_string(),
                    version: Some(ver.to_string()),
                    architecture: None,
                    manager: PackageManagerKind::Cargo,
                    installed_at: 0,
                });
            }
        }
    }
    packages
}

#[async_trait::async_trait]
impl PackageManagerBackend for CargoBackend {
    fn kind(&self) -> PackageManagerKind {
        PackageManagerKind::Cargo
    }

    fn is_available(&self) -> bool {
        which::which("cargo").is_ok()
    }

    async fn list_installed(&self) -> crate::Result<Vec<PackageRef>> {
        // There is no `cargo list`; `cargo install --list` is the
        // installed-crate surface (plain text: `name vX.Y.Z:`).
        let output = tokio::process::Command::new("cargo")
            .args(["install", "--list"])
            .output()
            .await?;

        if !output.status.success() {
            return Err(crate::Error::CommandFailed {
                command: "cargo install --list".to_string(),
                exit_code: output.status.code(),
            });
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        Ok(parse_cargo_install_list(&stdout))
    }

    fn upstream_url_patterns(&self) -> Vec<UrlPattern> {
        vec![
            // Legacy static `.crate` blob (200 live-verified; older clients).
            UrlPattern::with_class(
                r"https?://[^/]*static\.crates\.io/crates/[A-Za-z0-9_.\-]+/[A-Za-z0-9_.\-]+-\d+\.\d+\.\d+[-0-9A-Za-z.]*\.crate$",
                120,
                "cargo .crate artifacts (legacy)",
                crate::config::CacheClass::LangArtifact,
            )
            .expect("valid regex"),
            // New download endpoints: /crates/<name>/<ver>/download on both
            // hosts. crates.io 302s to static.crates.io, which serves 200.
            UrlPattern::with_class(
                r"https?://[^/]*(?:static\.crates\.io/crates|crates\.io/api/v1/crates)/[A-Za-z0-9_.\-]+/\d+\.\d+\.\d+[-0-9A-Za-z.]*/download$",
                110,
                "cargo crate download",
                crate::config::CacheClass::LangArtifact,
            )
            .expect("valid regex"),
            // Sparse-index metadata: repo layout keyed by name length, no
            // /index/ prefix (all four shapes live-verified 200):
            //   /1/<c>  /2/<cc>  /3/<c>/<ccc>  /<cc>/<cc>/<name>
            // Class = LangMetadata (TTL-revalidated, JSON lines).
            UrlPattern::with_class(
                r"https?://index\.crates\.io/(?:1/[a-z0-9_]|2/[a-z0-9_]{2}|3/[a-z0-9_]/[a-z0-9_-]{3}|[a-z0-9_]{2}/[a-z0-9_]{2}/[A-Za-z0-9_.\-]+)/?$",
                100,
                "cargo sparse-index metadata",
                crate::config::CacheClass::LangMetadata,
            )
            .expect("valid regex"),
        ]
    }

    fn url_to_cache_path(&self, url: &str) -> Option<String> {
        let url = url::Url::parse(url).ok()?;
        // Host namespaced: the sparse index (index.crates.io) and artifact
        // blobs (static.crates.io, crates.io) must never collide on identical
        // paths, and index entries are mutable metadata while downloads are
        // immutable artifacts.
        let host = url.host_str().unwrap_or("");
        let relative = url.path().trim_start_matches('/');
        if host == "index.crates.io" {
            Some(format!("cargo/index/{}", relative))
        } else {
            Some(format!("cargo/blob/{}", relative))
        }
    }

    fn proxy_config(&self, proxy_addr: &str) -> ProxyConfigAction {
        // Sparse-registry replacement model: point cargo's sparse registry at
        // the Monopsony `/cargo-index/` shim. With `registry =
        // sparse+http://ADDR/cargo-index/` cargo requests
        // `http://ADDR/cargo-index/ca/rg/cargo` for crate entries — exactly
        // the shim path — and fetches `config.json` at the registry root,
        // which the shim transparently proxies from
        // `https://index.crates.io/config.json`.
        //
        // KNOWN LIMITATION (dl-template bypass): the proxied config.json's
        // `dl` template points at the upstream `https://crates.io/api/v1/...`
        // URL, so cargo artifact *downloads* go straight to upstream and
        // bypass the shim/cache. Only the sparse index (high-traffic,
        // TTL-managed LangMetadata) is cached through this configuration.
        // Body rewriting of config.json is deliberately NOT attempted
        // (checksum/consistency risk outweighs the artifact-cache gain).
        //
        // The `/crates/` shim for static.crates.io artifacts remains useful
        // for the prefetch/purge path and for clients configured manually;
        // static.crates.io 308-redirects plain http, so the shim is the only
        // cacheable route for downloads.
        let content = format!(
            "[source.crates-io]\n\
             replace-with = \"monopsony-sparse\"\n\
             \n\
             [source.monopsony-sparse]\n\
             registry = \"sparse+http://{}/cargo-index/\"\n",
            proxy_addr
        );
        ProxyConfigAction::WriteFile {
            path: cargo_config_path(),
            content,
        }
    }

    fn remove_proxy_config(&self) -> ProxyConfigAction {
        ProxyConfigAction::RemoveFile {
            path: cargo_config_path(),
        }
    }
}

/// User-level cargo config location (`~/.cargo/config.toml`).
fn cargo_config_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/root".to_string());
    PathBuf::from(home).join(".cargo").join("config.toml")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_kind_and_availability() {
        let backend = CargoBackend::new();
        assert_eq!(backend.kind(), PackageManagerKind::Cargo);
    }

    #[test]
    fn test_sparse_index_pattern_live_shape() {
        let backend = CargoBackend::new();
        let patterns = backend.upstream_url_patterns();
        // Live-verified (200): all four name-length layouts, no /index/ prefix.
        for url in [
            "https://index.crates.io/ca/rg/cargo",
            "https://index.crates.io/2/in",
            "https://index.crates.io/1/g",
            "https://index.crates.io/3/g/gfx",
        ] {
            assert!(
                patterns.iter().any(|p| p.matches(url)),
                "no pattern matched {}",
                url
            );
        }
        let meta = patterns
            .iter()
            .find(|p| p.matches("https://index.crates.io/ca/rg/cargo"))
            .unwrap()
            .cache_class;
        assert_eq!(meta, crate::config::CacheClass::LangMetadata);
    }

    #[test]
    fn test_download_patterns_live_shapes() {
        let backend = CargoBackend::new();
        let patterns = backend.upstream_url_patterns();
        // Live-verified: crates.io 302 + static 200, versioned path + /download.
        for url in [
            "https://crates.io/api/v1/crates/serde/1.0.210/download",
            "https://static.crates.io/crates/serde/1.0.210/download",
        ] {
            assert!(
                patterns.iter().any(|p| p.matches(url)),
                "no pattern matched {}",
                url
            );
            let class = patterns
                .iter()
                .find(|p| p.matches(url))
                .unwrap()
                .cache_class;
            assert_eq!(class, crate::config::CacheClass::LangArtifact);
        }
        // Legacy .crate shape (live-verified 200).
        let legacy = "https://static.crates.io/crates/serde/serde-1.0.210.crate";
        assert!(patterns.iter().any(|p| p.matches(legacy)));
    }

    #[test]
    fn test_index_pattern_not_swallow_downloads() {
        let backend = CargoBackend::new();
        let patterns = backend.upstream_url_patterns();
        let meta_pat = patterns
            .iter()
            .find(|p| p.description.contains("sparse-index"))
            .unwrap();
        assert!(!meta_pat.matches("https://static.crates.io/crates/serde/serde-1.0.210.crate"));
        assert!(!meta_pat.matches("https://crates.io/api/v1/crates/serde/1.0.210/download"));
    }

    #[test]
    fn test_parse_cargo_install_list() {
        let text = "\
cargo v0.13.0:
    cargo
ripgrep v14.1.0:
    rg
fd-find v10.2.0:
    fd
";
        let pkgs = parse_cargo_install_list(text);
        assert_eq!(pkgs.len(), 3);
        assert_eq!(pkgs[0].name, "cargo");
        assert_eq!(pkgs[0].version.as_deref(), Some("0.13.0"));
        assert_eq!(pkgs[1].name, "ripgrep");
        assert_eq!(pkgs[2].version.as_deref(), Some("10.2.0"));
    }

    #[test]
    fn test_url_to_cache_path_namespaces_hosts() {
        let backend = CargoBackend::new();
        assert_eq!(
            backend.url_to_cache_path("https://index.crates.io/ca/rg/cargo"),
            Some("cargo/index/ca/rg/cargo".to_string())
        );
        assert_eq!(
            backend.url_to_cache_path("https://static.crates.io/crates/serde/1.0.210/download"),
            Some("cargo/blob/crates/serde/1.0.210/download".to_string())
        );
    }

    #[test]
    fn test_proxy_config_sets_sparse_registry() {
        let backend = CargoBackend::new();
        match backend.proxy_config("10.10.20.58:3128") {
            ProxyConfigAction::WriteFile { path, content } => {
                assert!(path.ends_with(".cargo/config.toml"));
                assert!(content.contains("[source.crates-io]"));
                assert!(content.contains("replace-with = \"monopsony-sparse\""));
                assert!(content.contains("[source.monopsony-sparse]"));
                assert!(
                    content.contains("registry = \"sparse+http://10.10.20.58:3128/cargo-index/\"")
                );
            }
            _ => panic!("expected WriteFile action"),
        }
    }

    #[test]
    fn test_remove_proxy_config() {
        let backend = CargoBackend::new();
        match backend.remove_proxy_config() {
            ProxyConfigAction::RemoveFile { path } => {
                assert!(path.ends_with(".cargo/config.toml"));
            }
            _ => panic!("expected RemoveFile action"),
        }
    }
}
