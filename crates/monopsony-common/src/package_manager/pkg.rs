//! FreeBSD pkg package manager backend implementation

use crate::{PackageRef, PackageManagerBackend, PackageManagerKind, ProxyConfigAction, UrlPattern};
use async_trait::async_trait;

use super::bsd_privilege;

/// FreeBSD pkg package manager backend
pub struct PkgBackend;

impl PkgBackend {
    pub fn new() -> Self {
        PkgBackend
    }
}

impl Default for PkgBackend {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl PackageManagerBackend for PkgBackend {
    fn kind(&self) -> PackageManagerKind {
        PackageManagerKind::Pkg
    }

    fn is_available(&self) -> bool {
        which::which("pkg").is_ok()
    }

    async fn list_installed(&self) -> crate::Result<Vec<PackageRef>> {
        let output = bsd_privilege::escalate("pkg", &["query", "%n\t%v\t%q"])
            .output()
            .await?;

        if !output.status.success() {
            return Err(crate::Error::CommandFailed {
                command: "pkg query".to_string(),
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
            let parts: Vec<&str> = line.splitn(3, '\t').collect();
            if parts.len() >= 2 {
                packages.push(PackageRef {
                    name: parts[0].to_string(),
                    version: Some(parts[1].to_string()),
                    architecture: None,
                    manager: PackageManagerKind::Pkg,
                });
            }
        }

        Ok(packages)
    }

    fn upstream_url_patterns(&self) -> Vec<UrlPattern> {
        vec![
            UrlPattern::new(
                r"https?://[^/]*freebsd\.org/.*",
                100,
                "FreeBSD official",
            )
            .expect("valid regex"),
            UrlPattern::new(
                r"https?://[^/]*pkg\.freebsd\.org/.*\.pkg",
                95,
                "FreeBSD package files",
            )
            .expect("valid regex"),
            UrlPattern::new(
                r"https?://[^/]*freebsd\.org/.*\.(txz|tar)",
                80,
                "FreeBSD distribution files",
            )
            .expect("valid regex"),
        ]
    }

    fn url_to_cache_path(&self, url: &str) -> Option<String> {
        let url = url::Url::parse(url).ok()?;
        let path = url.path().trim_start_matches('/');
        Some(format!("pkg/{}", path))
    }

    fn proxy_config(&self, proxy_addr: &str) -> ProxyConfigAction {
        ProxyConfigAction::Multiple(bsd_privilege::proxy_env_actions(proxy_addr))
    }

    fn remove_proxy_config(&self) -> ProxyConfigAction {
        ProxyConfigAction::Multiple(bsd_privilege::remove_proxy_env_actions())
    }
}
