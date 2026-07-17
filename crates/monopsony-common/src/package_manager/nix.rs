//! Nix package manager backend implementation (NixOS / multi-install Nix)
//!
//! Nix has a unique content-addressed model. Packages are stored in the Nix
//! store as NAR (Nix Archive) files. The binary cache serves `.narinfo`
//! metadata files and `.nar.xz` / `.nar` data files. Both are highly
//! cacheable because they are content-addressed (immutable by hash).

use crate::{PackageRef, PackageManagerBackend, PackageManagerKind, ProxyConfigAction, UrlPattern};
use async_trait::async_trait;
use std::path::PathBuf;

pub struct NixBackend;

impl NixBackend {
    pub fn new() -> Self {
        NixBackend
    }
}

impl Default for NixBackend {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl PackageManagerBackend for NixBackend {
    fn kind(&self) -> PackageManagerKind {
        PackageManagerKind::Nix
    }

    fn is_available(&self) -> bool {
        which::which("nix-store").is_ok() || which::which("nix").is_ok()
    }

    async fn list_installed(&self) -> crate::Result<Vec<PackageRef>> {
        // Use `nix-env --query` for user profile packages, or
        // `nix-store --query --installed` for system-wide.
        // On NixOS, `nix-store -q --roots /run/current-system` gives system packages.
        // We try nix-store first, then nix-env as fallback.
        let use_nix_store = which::which("nix-store").is_ok();

        let output = if use_nix_store {
            tokio::process::Command::new("nix-store")
                .args(["--query", "--requisites", "/run/current-system"])
                .output()
                .await
                .map_err(|_| crate::Error::CommandFailed {
                    command: "nix-store --query".to_string(),
                    exit_code: None,
                })?
        } else {
            tokio::process::Command::new("nix")
                .args(["profile", "list"])
                .output()
                .await
                .map_err(|_| crate::Error::CommandFailed {
                    command: "nix profile list".to_string(),
                    exit_code: None,
                })?
        };

        if !output.status.success() {
            let cmd = if use_nix_store {
                "nix-store --query"
            } else {
                "nix profile list"
            };
            return Err(crate::Error::CommandFailed {
                command: cmd.to_string(),
                exit_code: output.status.code(),
            });
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut packages = Vec::new();

        if use_nix_store {
            // nix-store outputs full store paths, one per line:
            //   /nix/store/abc123...-bash-5.2-p15
            for line in stdout.lines() {
                let line = line.trim();
                if line.is_empty() || !line.starts_with("/nix/store/") {
                    continue;
                }
                if let Some((name, version)) = parse_nix_store_path(line) {
                    packages.push(PackageRef {
                        name,
                        version: Some(version),
                        architecture: None,
                        manager: PackageManagerKind::Nix,
                        installed_at: 0,
                    });
                }
            }
        } else {
            // nix profile list outputs structured info; we parse package names
            // from the flake ref or legacy path lines.
            for line in stdout.lines() {
                let line = line.trim();
                // Lines with "Name:" or URI containing store paths
                if let Some(name) = line.strip_prefix("Name: ") {
                    packages.push(PackageRef {
                        name: name.to_string(),
                        version: None,
                        architecture: None,
                        manager: PackageManagerKind::Nix,
                        installed_at: 0,
                    });
                }
            }
        }

        Ok(packages)
    }

    fn upstream_url_patterns(&self) -> Vec<UrlPattern> {
        vec![
            // NAR info metadata — one per store path
            UrlPattern::new(
                r"https?://[^/]*cache\.nixos\.org/.*\.narinfo",
                100,
                "NixOS binary cache narinfo",
            )
            .expect("valid regex"),
            // NAR data files (.nar.xz, .nar)
            UrlPattern::new(
                r"https?://[^/]*cache\.nixos\.org/.*\.nar(?:\.xz|\.zst)?",
                100,
                "NixOS binary cache NAR files",
            )
            .expect("valid regex"),
            // Custom substituters (common pattern: cache.<domain>.org)
            UrlPattern::new(
                r"https?://[^/]*cache\.[a-z0-9.-]+\.(?:org|io|com)/.*\.narinfo",
                90,
                "Custom Nix substituter narinfo",
            )
            .expect("valid regex"),
            UrlPattern::new(
                r"https?://[^/]*cache\.[a-z0-9.-]+\.(?:org|io|com)/.*\.nar(?:\.xz|\.zst)?",
                90,
                "Custom Nix substituter NAR files",
            )
            .expect("valid regex"),
            // Cachix (popular hosted Nix cache service)
            UrlPattern::new(
                r"https?://[^/]*cachix\.org/.*\.narinfo",
                90,
                "Cachix narinfo",
            )
            .expect("valid regex"),
            UrlPattern::new(
                r"https?://[^/]*cachix\.org/.*\.nar(?:\.xz|\.zst)?",
                90,
                "Cachix NAR files",
            )
            .expect("valid regex"),
        ]
    }

    fn url_to_cache_path(&self, url: &str) -> Option<String> {
        let url = url::Url::parse(url).ok()?;
        Some(format!("nix{}", url.path()))
    }

    fn proxy_config(&self, proxy_addr: &str) -> ProxyConfigAction {
        // Nix daemon (nix-daemon) does not respect env vars at runtime because
        // it runs as a systemd service. Proxy must be set via:
        // 1. systemd override for nix-daemon, OR
        // 2. substituter in nix.conf
        // We generate a systemd drop-in override that sets http_proxy.
        ProxyConfigAction::WriteFile {
            path: PathBuf::from("/etc/systemd/system/nix-daemon.service.d/monopsony-proxy.conf"),
            content: format!(
                r#"[Service]
Environment="http_proxy=http://{}"
Environment="https_proxy=http://{}"
"#,
                proxy_addr, proxy_addr
            ),
        }
    }

    fn remove_proxy_config(&self) -> ProxyConfigAction {
        ProxyConfigAction::RemoveFile {
            path: PathBuf::from("/etc/systemd/system/nix-daemon.service.d/monopsony-proxy.conf"),
        }
    }
}

/// Parse a Nix store path to extract (name, version).
///
/// Nix store paths look like:
///   /nix/store/0a2b3c4d...-package-name-1.2.3
/// The hash is 32 chars, then a hyphen, then name-version.
fn parse_nix_store_path(path: &str) -> Option<(String, String)> {
    // Strip /nix/store/ prefix
    let basename = path.strip_prefix("/nix/store/")?;
    // Find first hyphen — everything before it is the hash
    let first_hyphen = basename.find('-')?;
    let name_version = &basename[first_hyphen + 1..];

    // Find the version: last hyphen before a digit
    let bytes = name_version.as_bytes();
    for i in (0..name_version.len().saturating_sub(1)).rev() {
        if bytes[i] == b'-' && bytes[i + 1].is_ascii_digit() {
            return Some((
                name_version[..i].to_string(),
                name_version[i + 1..].to_string(),
            ));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_nix_store_path_simple() {
        let (name, ver) = parse_nix_store_path(
            "/nix/store/0a2b3c4d5e6f7a8b9c0d1e2f3a4b5c6d-bash-5.2-p15",
        )
        .unwrap();
        assert_eq!(name, "bash");
        assert_eq!(ver, "5.2-p15");
    }

    #[test]
    fn test_parse_nix_store_path_hyphenated_name() {
        let (name, ver) = parse_nix_store_path(
            "/nix/store/abc123def456abc123def456abc123de-haskell-platform-9999",
        )
        .unwrap();
        assert_eq!(name, "haskell-platform");
        assert_eq!(ver, "9999");
    }

    #[test]
    fn test_parse_nix_store_path_not_found() {
        assert!(parse_nix_store_path("/usr/bin/bash").is_none());
        assert!(parse_nix_store_path("/nix/store/0a2b3c4d-nohyphenhere").is_none());
    }

    #[test]
    fn test_url_patterns_match() {
        let backend = NixBackend::new();
        let patterns = backend.upstream_url_patterns();

        // narinfo
        assert!(patterns.iter().any(|p| p.matches(
            "https://cache.nixos.org/0a2b3c4d5e6f7a8b9c0d1e2f3a4b5c6d.narinfo"
        )));
        // NAR file
        assert!(patterns.iter().any(|p| p.matches(
            "https://cache.nixos.org/nar/0a2b3c4d5e6f7a8b9c0d1e2f3a4b5c6d.nar.xz"
        )));
        // Cachix
        assert!(patterns.iter().any(|p| p.matches(
            "https://mycache.cachix.org/0a2b3c4d5e6f7a8b9c0d1e2f3a4b5c6d.narinfo"
        )));
        // Non-matching
        assert!(!patterns.iter().any(|p| p.matches(
            "https://archlinux.org/packages/x86_64/bash.tar.zst"
        )));
    }

    #[test]
    fn test_url_to_cache_path() {
        let backend = NixBackend::new();
        let path = backend.url_to_cache_path(
            "https://cache.nixos.org/0a2b3c4d5e6f7a8b9c0d1e2f3a4b5c6d.narinfo",
        );
        assert_eq!(
            path.as_deref(),
            Some("nix/0a2b3c4d5e6f7a8b9c0d1e2f3a4b5c6d.narinfo")
        );
    }

    #[test]
    fn test_url_to_cache_path_invalid() {
        let backend = NixBackend::new();
        assert!(backend.url_to_cache_path("not a url").is_none());
    }

    #[test]
    fn test_proxy_config() {
        let backend = NixBackend::new();
        let action = backend.proxy_config("10.10.20.58:3128");
        match action {
            ProxyConfigAction::WriteFile { path, content } => {
                assert_eq!(
                    path,
                    PathBuf::from(
                        "/etc/systemd/system/nix-daemon.service.d/monopsony-proxy.conf"
                    )
                );
                assert!(content.contains("http_proxy=http://10.10.20.58:3128"));
                assert!(content.contains("https_proxy=http://10.10.20.58:3128"));
            }
            _ => panic!("expected WriteFile action"),
        }
    }

    #[test]
    fn test_kind_and_availability() {
        let backend = NixBackend::new();
        assert_eq!(backend.kind(), PackageManagerKind::Nix);
    }
}
