//! Zypper package manager backend implementation (openSUSE)
//!
//! Zypper is the package manager for openSUSE (Tumbleweed / Leap).
//! It uses RPM packages and shares the underlying libzypp library with
//! SUSE's enterprise tools. URL patterns match openSUSE download mirrors.

use super::upgradable::{parse_zypper_lu, UpgradableReport};
use crate::{PackageManagerBackend, PackageManagerKind, PackageRef, ProxyConfigAction, UrlPattern};
use async_trait::async_trait;
use std::path::PathBuf;

pub struct ZypperBackend {
    #[allow(dead_code)] // Will be used when reading actual zypp config
    config_path: Option<PathBuf>,
}

impl ZypperBackend {
    pub fn new() -> Self {
        ZypperBackend { config_path: None }
    }
}

impl Default for ZypperBackend {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl PackageManagerBackend for ZypperBackend {
    fn kind(&self) -> PackageManagerKind {
        PackageManagerKind::Zypper
    }

    fn is_available(&self) -> bool {
        which::which("zypper").is_ok()
    }

    async fn list_installed(&self) -> crate::Result<Vec<PackageRef>> {
        // Prefer rpm -qa for structured output (same as DnfBackend)
        let use_rpm = which::which("rpm").is_ok();

        let output = if use_rpm {
            tokio::process::Command::new("rpm")
                .args([
                    "-qa",
                    "--queryformat",
                    "%{NAME}\t%{VERSION}-%{RELEASE}\t%{ARCH}\n",
                ])
                .output()
                .await
                .map_err(|_| crate::Error::CommandFailed {
                    command: "rpm -qa".to_string(),
                    exit_code: None,
                })?
        } else {
            tokio::process::Command::new("zypper")
                .args(["--no-refresh", "se", "--installed-only", "-t", "package"])
                .output()
                .await
                .map_err(|_| crate::Error::CommandFailed {
                    command: "zypper se --installed-only".to_string(),
                    exit_code: None,
                })?
        };

        if !output.status.success() {
            let cmd = if use_rpm {
                "rpm -qa"
            } else {
                "zypper se --installed-only"
            };
            return Err(crate::Error::CommandFailed {
                command: cmd.to_string(),
                exit_code: output.status.code(),
            });
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut packages = Vec::new();

        if use_rpm {
            for line in stdout.lines() {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                let parts: Vec<&str> = line.splitn(3, '\t').collect();
                if parts.len() == 3 {
                    packages.push(PackageRef {
                        name: parts[0].to_string(),
                        version: Some(parts[1].to_string()),
                        architecture: Some(parts[2].to_string()),
                        manager: PackageManagerKind::Zypper,
                        installed_at: 0,
                    });
                }
            }
        } else {
            // zypper se output: `i  | package | name    | version    | arch`
            for line in stdout.lines() {
                let line = line.trim();
                if line.is_empty() || !line.starts_with('i') {
                    continue;
                }
                let fields: Vec<&str> = line.split('|').map(|s| s.trim()).collect();
                if fields.len() >= 5 {
                    packages.push(PackageRef {
                        name: fields[2].to_string(),
                        version: Some(fields[3].to_string()),
                        architecture: Some(fields[4].to_string()),
                        manager: PackageManagerKind::Zypper,
                        installed_at: 0,
                    });
                }
            }
        }

        Ok(packages)
    }

    /// Best-effort (grounding D4): zypper's update table exposes a Category
    /// column, so the security split is parsed when present.
    async fn list_upgradable(&self) -> crate::Result<UpgradableReport> {
        let output = tokio::process::Command::new("zypper")
            .args(["-q", "lu"])
            .output()
            .await
            .map_err(|_| crate::Error::CommandFailed {
                command: "zypper -q lu".to_string(),
                exit_code: None,
            })?;

        if !output.status.success() {
            return Err(crate::Error::CommandFailed {
                command: "zypper -q lu".to_string(),
                exit_code: output.status.code(),
            });
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let (security, other) = parse_zypper_lu(&stdout);
        Ok(UpgradableReport {
            security,
            other,
            supported: true,
            note: "security = zypper update-table Category column (best-effort)".to_string(),
        })
    }

    fn upstream_url_patterns(&self) -> Vec<UrlPattern> {
        vec![
            // RPM packages from openSUSE download mirrors
            UrlPattern::new(
                r"https?://[^/]*download\.opensuse\.org/.*\.rpm",
                100,
                "openSUSE packages",
            )
            .expect("valid regex"),
            UrlPattern::new(
                r"https?://[^/]*download\.suse\.de/.*\.rpm",
                100,
                "SUSE internal packages",
            )
            .expect("valid regex"),
            // Repository metadata (repomd, primary, etc.)
            UrlPattern::new(
                r"https?://[^/]*download\.opensuse\.org/.*repodata/.*",
                90,
                "openSUSE repo metadata",
            )
            .expect("valid regex"),
            UrlPattern::new(
                r"https?://[^/]*download\.suse\.de/.*repodata/.*",
                90,
                "SUSE internal repo metadata",
            )
            .expect("valid regex"),
            // Delta RPMs and source RPMs
            UrlPattern::new(
                r"https?://[^/]*download\.opensuse\.org/.*\.(deltarpm|src\.rpm)",
                80,
                "openSUSE delta/source RPMs",
            )
            .expect("valid regex"),
        ]
    }

    fn url_to_cache_path(&self, url: &str) -> Option<String> {
        let url = url::Url::parse(url).ok()?;
        let path = url.path().trim_start_matches('/');
        Some(format!("zypper/{}", path))
    }

    fn proxy_config(&self, proxy_addr: &str) -> ProxyConfigAction {
        // libzypp reads proxy settings from /etc/zypp/zypp.conf under the [main] section.
        // The `proxy` key sets the HTTP proxy for all zypp operations.
        ProxyConfigAction::WriteFile {
            path: PathBuf::from("/etc/zypp/zypp.conf"),
            content: format!(
                r#"[main]
proxy = http://{}
"#,
                proxy_addr
            ),
        }
    }

    fn remove_proxy_config(&self) -> ProxyConfigAction {
        ProxyConfigAction::RemoveFile {
            path: PathBuf::from("/etc/zypp/zypp.conf"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_url_patterns_match() {
        let backend = ZypperBackend::new();
        let patterns = backend.upstream_url_patterns();

        // RPM package
        assert!(patterns.iter().any(|p| p.matches(
            "https://download.opensuse.org/tumbleweed/repo/oss/x86_64/bash-5.2.15-3.1.x86_64.rpm"
        )));
        // Repo metadata
        assert!(patterns
            .iter()
            .any(|p| p
                .matches("https://download.opensuse.org/tumbleweed/repo/oss/repodata/repomd.xml")));
        // SUSE internal
        assert!(patterns.iter().any(
            |p| p.matches("https://download.suse.de/ibs/SUSE/Downloads/SLE/15/x86_64/test.rpm")
        ));
        // Non-matching
        assert!(!patterns
            .iter()
            .any(|p| p.matches("https://archlinux.org/packages/x86_64/bash.tar.zst")));
    }

    #[test]
    fn test_url_to_cache_path() {
        let backend = ZypperBackend::new();
        let path = backend.url_to_cache_path(
            "https://download.opensuse.org/tumbleweed/repo/oss/x86_64/bash-5.2.15-3.1.x86_64.rpm",
        );
        assert_eq!(
            path.as_deref(),
            Some("zypper/tumbleweed/repo/oss/x86_64/bash-5.2.15-3.1.x86_64.rpm")
        );
    }

    #[test]
    fn test_url_to_cache_path_invalid() {
        let backend = ZypperBackend::new();
        assert!(backend.url_to_cache_path("not a url").is_none());
    }

    #[test]
    fn test_proxy_config() {
        let backend = ZypperBackend::new();
        let action = backend.proxy_config("10.10.20.58:3128");
        match action {
            ProxyConfigAction::WriteFile { path, content } => {
                assert_eq!(path, PathBuf::from("/etc/zypp/zypp.conf"));
                assert!(content.contains("proxy = http://10.10.20.58:3128"));
            }
            _ => panic!("expected WriteFile action"),
        }
    }

    #[test]
    fn test_kind_and_availability() {
        let backend = ZypperBackend::new();
        assert_eq!(backend.kind(), PackageManagerKind::Zypper);
    }
}
