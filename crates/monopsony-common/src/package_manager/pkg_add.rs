//! OpenBSD pkg_add package manager backend implementation

use crate::{PackageManagerBackend, PackageManagerKind, PackageRef, ProxyConfigAction, UrlPattern};
use async_trait::async_trait;

use super::bsd_privilege;

/// OpenBSD pkg_add package manager backend
pub struct PkgAddBackend;

impl PkgAddBackend {
    pub fn new() -> Self {
        PkgAddBackend
    }
}

impl Default for PkgAddBackend {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl PackageManagerBackend for PkgAddBackend {
    fn kind(&self) -> PackageManagerKind {
        PackageManagerKind::PkgAdd
    }

    fn is_available(&self) -> bool {
        which::which("pkg_info").is_ok()
    }

    async fn list_installed(&self) -> crate::Result<Vec<PackageRef>> {
        let output = bsd_privilege::escalate("pkg_info", &["-q"])
            .output()
            .await?;

        if !output.status.success() {
            return Err(crate::Error::CommandFailed {
                command: "pkg_info -q".to_string(),
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
            if let Some((name, version)) = line.split_once('-') {
                if !name.is_empty() && !version.is_empty() {
                    packages.push(PackageRef {
                        name: name.to_string(),
                        version: Some(version.to_string()),
                        architecture: None,
                        manager: PackageManagerKind::PkgAdd,
                        installed_at: 0,
                    });
                }
            }
        }

        Ok(packages)
    }

    fn upstream_url_patterns(&self) -> Vec<UrlPattern> {
        vec![
            UrlPattern::new(
                r"https?://[^/]*openbsd\.org/pub/OpenBSD/.*",
                100,
                "OpenBSD official",
            )
            .expect("valid regex"),
            UrlPattern::new(
                r"https?://cdn\.openbsd\.org/pub/OpenBSD/.*",
                95,
                "OpenBSD CDN",
            )
            .expect("valid regex"),
        ]
    }

    fn url_to_cache_path(&self, url: &str) -> Option<String> {
        let url = url::Url::parse(url).ok()?;
        let path = url.path().trim_start_matches('/');
        Some(format!("openbsd/{}", path))
    }

    fn proxy_config(&self, proxy_addr: &str) -> ProxyConfigAction {
        ProxyConfigAction::Multiple(bsd_privilege::proxy_env_actions(proxy_addr))
    }

    fn remove_proxy_config(&self) -> ProxyConfigAction {
        ProxyConfigAction::Multiple(bsd_privilege::remove_proxy_env_actions())
    }
}
