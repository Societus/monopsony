//! Alpine apk package manager backend implementation

use super::upgradable::{parse_apk_version_l, UpgradableReport};
use crate::{PackageManagerBackend, PackageManagerKind, PackageRef, ProxyConfigAction, UrlPattern};
use async_trait::async_trait;
use std::path::PathBuf;

pub struct ApkBackend;

impl ApkBackend {
    pub fn new() -> Self {
        ApkBackend
    }
}

impl Default for ApkBackend {
    fn default() -> Self {
        Self::new()
    }
}

/// Split an Alpine package string `name-version-rN` into (name, version).
///
/// The version always starts with digits followed by a dot (e.g. `1.37.0`).
/// The build suffix is `-r<digits>` at the end. Package names can contain
/// hyphens and digits (e.g. `gcc-x86-64`, `libcrypto3`), so we must look for
/// the version pattern specifically, not just any hyphen-digit boundary.
fn split_apk_package(pkg: &str) -> Option<(&str, &str)> {
    // Find and strip the -rN build suffix from the right
    let build_pos = pkg.rfind("-r")?;
    let after_r = &pkg[build_pos + 2..];
    if after_r.is_empty() || !after_r.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let rest = &pkg[..build_pos]; // name-version (without build suffix)

    // Version starts at the first hyphen followed by digits then a dot.
    // This distinguishes names like "gcc-x86-64" (digits not followed by dot)
    // from versions like "13.2.1" (digits followed by dot).
    let bytes = rest.as_bytes();
    for i in 0..rest.len().saturating_sub(2) {
        if bytes[i] == b'-' && bytes[i + 1].is_ascii_digit() {
            let after = &rest[i + 1..];
            let digits_end = after.bytes().take_while(|b| b.is_ascii_digit()).count();
            if digits_end > 0 && after.as_bytes().get(digits_end) == Some(&b'.') {
                // Return version including the -rN suffix from the original string
                return Some((&rest[..i], &pkg[i + 1..]));
            }
        }
    }
    None
}

#[async_trait]
impl PackageManagerBackend for ApkBackend {
    fn kind(&self) -> PackageManagerKind {
        PackageManagerKind::Apk
    }

    fn is_available(&self) -> bool {
        which::which("apk").is_ok()
    }

    async fn list_installed(&self) -> crate::Result<Vec<PackageRef>> {
        // `apk list --installed` output format:
        //   busybox-1.37.0-r20 x86_64 :BusyBox standard utilities
        let output = tokio::process::Command::new("apk")
            .args(["list", "--installed"])
            .output()
            .await
            .map_err(|_| crate::Error::CommandFailed {
                command: "apk list --installed".to_string(),
                exit_code: None,
            })?;

        if !output.status.success() {
            return Err(crate::Error::CommandFailed {
                command: "apk list --installed".to_string(),
                exit_code: output.status.code(),
            });
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut packages = Vec::new();

        for line in stdout.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            // First token is `name-version-rN`, second token is arch
            let mut parts = line.split_whitespace();
            let pkg_str = match parts.next() {
                Some(s) => s,
                None => continue,
            };
            let arch = parts.next().map(|s| s.to_string());

            if let Some((name, version)) = split_apk_package(pkg_str) {
                packages.push(PackageRef {
                    name: name.to_string(),
                    version: Some(version.to_string()),
                    architecture: arch,
                    manager: PackageManagerKind::Apk,
                    installed_at: 0,
                });
            }
        }

        Ok(packages)
    }

    /// Fast (local metadata only), but keep it at inventory cadence for
    /// parity with the other backends (grounding D3).
    async fn list_upgradable(&self) -> crate::Result<UpgradableReport> {
        let output = tokio::process::Command::new("apk")
            .args(["version", "-l", "<"])
            .output()
            .await
            .map_err(|_| crate::Error::CommandFailed {
                command: "apk version -l <".to_string(),
                exit_code: None,
            })?;

        if !output.status.success() {
            return Err(crate::Error::CommandFailed {
                command: "apk version -l <".to_string(),
                exit_code: output.status.code(),
            });
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        // Alpine tags no security channel in this output — everything lands
        // in "other" with a note (grounding D4).
        let other = parse_apk_version_l(&stdout);
        Ok(UpgradableReport {
            security: 0,
            other,
            supported: true,
            note: "apk exposes no security channel; all counts are other".to_string(),
        })
    }

    fn upstream_url_patterns(&self) -> Vec<UrlPattern> {
        vec![
            // Package files (.apk extension)
            UrlPattern::new(
                r"https?://[^/]*alpinelinux\.org/.*\.apk",
                100,
                "Alpine packages",
            )
            .expect("valid regex"),
            // Index metadata: APKINDEX.tar.gz
            UrlPattern::new(
                r"https?://[^/]*alpinelinux\.org/.*APKINDEX.*\.tar\.gz",
                90,
                "Alpine repo index",
            )
            .expect("valid regex"),
            // Signature files
            UrlPattern::new(
                r"https?://[^/]*alpinelinux\.org/.*\.SIGN\.RSA.*",
                80,
                "Alpine signatures",
            )
            .expect("valid regex"),
        ]
    }

    fn url_to_cache_path(&self, url: &str) -> Option<String> {
        let url = url::Url::parse(url).ok()?;
        Some(format!("apk{}", url.path()))
    }

    fn proxy_config(&self, proxy_addr: &str) -> ProxyConfigAction {
        // apk respects http_proxy/https_proxy environment variables.
        // Write a profile.d script for system-wide effect.
        ProxyConfigAction::WriteFile {
            path: PathBuf::from("/etc/profile.d/monopsony-proxy.sh"),
            content: format!(
                "export http_proxy=http://{}\nexport https_proxy=http://{}\n",
                proxy_addr, proxy_addr
            ),
        }
    }

    fn remove_proxy_config(&self) -> ProxyConfigAction {
        ProxyConfigAction::RemoveFile {
            path: PathBuf::from("/etc/profile.d/monopsony-proxy.sh"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_split_apk_package_simple() {
        let (name, ver) = split_apk_package("busybox-1.37.0-r20").unwrap();
        assert_eq!(name, "busybox");
        assert_eq!(ver, "1.37.0-r20");
    }

    #[test]
    fn test_split_apk_package_hyphenated_name() {
        let (name, ver) = split_apk_package("gcc-x86-64-13.2.1_git20231014-r0").unwrap();
        assert_eq!(name, "gcc-x86-64");
        assert_eq!(ver, "13.2.1_git20231014-r0");
    }

    #[test]
    fn test_split_apk_package_no_build_suffix() {
        // Without -rN suffix, this is not a valid apk package string
        assert!(split_apk_package("musl-1.2.5").is_none());
    }

    #[test]
    fn test_split_apk_package_name_with_digits() {
        let (name, ver) = split_apk_package("libcrypto3-3.3.1-r3").unwrap();
        assert_eq!(name, "libcrypto3");
        assert_eq!(ver, "3.3.1-r3");
    }

    #[test]
    fn test_url_patterns_match() {
        let backend = ApkBackend::new();
        let patterns = backend.upstream_url_patterns();

        // Package file
        assert!(patterns.iter().any(|p| p.matches(
            "https://dl-cdn.alpinelinux.org/alpine/v3.22/main/x86_64/busybox-1.37.0-r20.apk"
        )));
        // APKINDEX
        assert!(patterns.iter().any(|p| p
            .matches("https://dl-cdn.alpinelinux.org/alpine/v3.22/main/x86_64/APKINDEX.tar.gz")));
        // Signature
        assert!(patterns.iter().any(|p| p.matches("https://dl-cdn.alpinelinux.org/alpine/v3.22/main/x86_64/APKINDEX.tar.gz.SIGN.RSA.saunders.rsa.pub")));
        // Non-matching URL
        assert!(!patterns
            .iter()
            .any(|p| p.matches("https://archlinux.org/packages/x86_64/bash.tar.zst")));
    }

    #[test]
    fn test_url_to_cache_path() {
        let backend = ApkBackend::new();
        let path = backend.url_to_cache_path(
            "https://dl-cdn.alpinelinux.org/alpine/v3.22/main/x86_64/busybox-1.37.0-r20.apk",
        );
        assert_eq!(
            path.as_deref(),
            Some("apk/alpine/v3.22/main/x86_64/busybox-1.37.0-r20.apk")
        );
    }

    #[test]
    fn test_url_to_cache_path_invalid() {
        let backend = ApkBackend::new();
        assert!(backend.url_to_cache_path("not a url").is_none());
    }

    #[test]
    fn test_proxy_config() {
        let backend = ApkBackend::new();
        let action = backend.proxy_config("10.10.20.58:3128");
        match action {
            ProxyConfigAction::WriteFile { path, content } => {
                assert_eq!(path, PathBuf::from("/etc/profile.d/monopsony-proxy.sh"));
                assert!(content.contains("http_proxy=http://10.10.20.58:3128"));
                assert!(content.contains("https_proxy=http://10.10.20.58:3128"));
            }
            _ => panic!("expected WriteFile action"),
        }
    }

    #[test]
    fn test_kind_and_availability() {
        let backend = ApkBackend::new();
        assert_eq!(backend.kind(), PackageManagerKind::Apk);
        // is_available depends on system having apk installed
        // (won't be true on the dev machine, will be on Alpine)
    }
}
