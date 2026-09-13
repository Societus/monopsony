//! MidnightBSD mport package manager backend implementation

use crate::{PackageManagerBackend, PackageManagerKind, PackageRef, ProxyConfigAction, UrlPattern};
use async_trait::async_trait;

use super::bsd_privilege;

/// MidnightBSD mport package manager backend
pub struct MportBackend;

impl MportBackend {
    pub fn new() -> Self {
        MportBackend
    }
}

impl Default for MportBackend {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl PackageManagerBackend for MportBackend {
    fn kind(&self) -> PackageManagerKind {
        PackageManagerKind::Mport
    }

    fn is_available(&self) -> bool {
        which::which("mport").is_ok()
    }

    async fn list_installed(&self) -> crate::Result<Vec<PackageRef>> {
        let output = bsd_privilege::escalate("mport", &["list", "-q"])
            .output()
            .await?;

        if !output.status.success() {
            return Err(crate::Error::CommandFailed {
                command: "mport list -q".to_string(),
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
            if let Some((name, version)) = line.rsplit_once('-') {
                if !name.is_empty() && !version.is_empty() {
                    packages.push(PackageRef {
                        name: name.to_string(),
                        version: Some(version.to_string()),
                        architecture: None,
                        manager: PackageManagerKind::Mport,
                        installed_at: 0,
                    });
                }
            } else {
                packages.push(PackageRef {
                    name: line.to_string(),
                    version: None,
                    architecture: None,
                    manager: PackageManagerKind::Mport,
                    installed_at: 0,
                });
            }
        }

        Ok(packages)
    }

    fn upstream_url_patterns(&self) -> Vec<UrlPattern> {
        vec![
            UrlPattern::new(
                r"https?://[^/]*mport\.midnightbsd\.org/.*",
                100,
                "MidnightBSD mport repository",
            )
            .expect("valid regex"),
            UrlPattern::new(
                r"https?://[^/]*midnightbsd\.org/.*\.mport",
                90,
                "MidnightBSD package files",
            )
            .expect("valid regex"),
            UrlPattern::new(
                r"https?://[^/]*midnightbsd\.org/.*\.txz",
                80,
                "MidnightBSD distribution files",
            )
            .expect("valid regex"),
        ]
    }

    fn url_to_cache_path(&self, url: &str) -> Option<String> {
        let url = url::Url::parse(url).ok()?;
        let path = url.path().trim_start_matches('/');
        Some(format!("mport/{}", path))
    }

    fn proxy_config(&self, proxy_addr: &str) -> ProxyConfigAction {
        ProxyConfigAction::Multiple(bsd_privilege::proxy_env_actions(proxy_addr))
    }

    fn remove_proxy_config(&self) -> ProxyConfigAction {
        ProxyConfigAction::Multiple(bsd_privilege::remove_proxy_env_actions())
    }
}
