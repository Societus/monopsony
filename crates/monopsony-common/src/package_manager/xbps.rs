//! XBPS package manager backend implementation (Void Linux)
//!
//! XBPS (X Binary Package System) is the native package manager for Void Linux.
//! It comes in glibc and musl variants. XBPS respects standard http_proxy /
//! https_proxy environment variables, making proxy configuration straightforward.

use crate::{PackageRef, PackageManagerBackend, PackageManagerKind, ProxyConfigAction, UrlPattern};
use async_trait::async_trait;
use std::path::PathBuf;

pub struct XbpsBackend;

impl XbpsBackend {
    pub fn new() -> Self {
        XbpsBackend
    }
}

impl Default for XbpsBackend {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl PackageManagerBackend for XbpsBackend {
    fn kind(&self) -> PackageManagerKind {
        PackageManagerKind::Xbps
    }

    fn is_available(&self) -> bool {
        which::which("xbps-query").is_ok()
    }

    async fn list_installed(&self) -> crate::Result<Vec<PackageRef>> {
        // `xbps-query -l` output format:
        //   NN foo-1.2.3_1
        // The first two chars are status flags, then package name-version_revision
        let output = tokio::process::Command::new("xbps-query")
            .arg("-l")
            .output()
            .await
            .map_err(|_| crate::Error::CommandFailed {
                command: "xbps-query -l".to_string(),
                exit_code: None,
            })?;

        if !output.status.success() {
            return Err(crate::Error::CommandFailed {
                command: "xbps-query -l".to_string(),
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
            // Skip the 2-char status prefix + space
            // Format: "ii package-name-1.2.3_1"
            if line.len() < 4 {
                continue;
            }
            let pkg_part = &line[3..]; // after "ii " or "rr " etc.
            let pkg_part = pkg_part.trim();

            // Split name from version: find last hyphen before version pattern.
            // XBPS version format: name-version_revision (e.g. bash-5.2.15_1)
            // The version starts at the last hyphen followed by a digit and
            // contains an optional _revision suffix.
            if let Some((name, version)) = split_xbps_package(pkg_part) {
                packages.push(PackageRef {
                    name: name.to_string(),
                    version: Some(version.to_string()),
                    architecture: None, // XBPS doesn't show arch in list output
                    manager: PackageManagerKind::Xbps,
                    installed_at: 0,
                });
            }
        }

        Ok(packages)
    }

    fn upstream_url_patterns(&self) -> Vec<UrlPattern> {
        vec![
            // XBPS binary packages (.xbps and .xbps.sig)
            UrlPattern::new(
                r"https?://[^/]*voidlinux\.org/.*\.xbps(?:\.sig)?",
                100,
                "Void Linux XBPS packages",
            )
            .expect("valid regex"),
            // Repository index files
            UrlPattern::new(
                r"https?://[^/]*voidlinux\.org/.*(?:index|repodata)",
                90,
                "Void Linux repo index",
            )
            .expect("valid regex"),
            // Common mirror domains
            UrlPattern::new(
                r"https?://[^/]*\.repo\.voidlinux\.org/.*\.xbps(?:\.sig)?",
                100,
                "Void Linux mirror packages",
            )
            .expect("valid regex"),
            UrlPattern::new(
                r"https?://[^/]*\.repo\.voidlinux\.org/.*(?:index|repodata)",
                90,
                "Void Linux mirror repo index",
            )
            .expect("valid regex"),
        ]
    }

    fn url_to_cache_path(&self, url: &str) -> Option<String> {
        let url = url::Url::parse(url).ok()?;
        Some(format!("xbps{}", url.path()))
    }

    fn proxy_config(&self, proxy_addr: &str) -> ProxyConfigAction {
        // XBPS respects http_proxy / https_proxy env vars.
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

/// Split an XBPS package string `name-version_revision` into (name, version).
///
/// XBPS versions look like `5.2.15_1` where `_1` is the revision.
/// Package names can contain hyphens (e.g. `gcc-fortran`).
/// The version always starts with a digit.
fn split_xbps_package(pkg: &str) -> Option<(&str, &str)> {
    // Find the last hyphen that is followed by a digit — that's the version boundary.
    let bytes = pkg.as_bytes();
    for i in (0..pkg.len().saturating_sub(1)).rev() {
        if bytes[i] == b'-' && bytes[i + 1].is_ascii_digit() {
            return Some((&pkg[..i], &pkg[i + 1..]));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_split_xbps_package_simple() {
        let (name, ver) = split_xbps_package("bash-5.2.15_1").unwrap();
        assert_eq!(name, "bash");
        assert_eq!(ver, "5.2.15_1");
    }

    #[test]
    fn test_split_xbps_package_hyphenated_name() {
        let (name, ver) = split_xbps_package("gcc-fortran-13.2.0_2").unwrap();
        assert_eq!(name, "gcc-fortran");
        assert_eq!(ver, "13.2.0_2");
    }

    #[test]
    fn test_split_xbps_package_no_revision() {
        let (name, ver) = split_xbps_package("foo-1.0.0").unwrap();
        assert_eq!(name, "foo");
        assert_eq!(ver, "1.0.0");
    }

    #[test]
    fn test_url_patterns_match() {
        let backend = XbpsBackend::new();
        let patterns = backend.upstream_url_patterns();

        assert!(patterns.iter().any(|p| p.matches(
            "https://repo-default.voidlinux.org/current/x86_64/bash-5.2.15_1.x86_64.xbps"
        )));
        assert!(patterns.iter().any(|p| p.matches(
            "https://repo-default.voidlinux.org/current/x86_64/bash-5.2.15_1.x86_64.xbps.sig"
        )));
        assert!(patterns.iter().any(|p| p.matches(
            "https://repo-default.voidlinux.org/current/x86_64/x86_64-repodata"
        )));
        assert!(!patterns.iter().any(|p| p.matches(
            "https://archlinux.org/packages/x86_64/bash.tar.zst"
        )));
    }

    #[test]
    fn test_url_to_cache_path() {
        let backend = XbpsBackend::new();
        let path = backend.url_to_cache_path(
            "https://repo-default.voidlinux.org/current/x86_64/bash-5.2.15_1.x86_64.xbps",
        );
        assert_eq!(
            path.as_deref(),
            Some("xbps/current/x86_64/bash-5.2.15_1.x86_64.xbps")
        );
    }

    #[test]
    fn test_url_to_cache_path_invalid() {
        let backend = XbpsBackend::new();
        assert!(backend.url_to_cache_path("not a url").is_none());
    }

    #[test]
    fn test_proxy_config() {
        let backend = XbpsBackend::new();
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
        let backend = XbpsBackend::new();
        assert_eq!(backend.kind(), PackageManagerKind::Xbps);
    }
}
