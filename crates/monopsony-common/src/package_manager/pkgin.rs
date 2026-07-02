//! NetBSD pkgin package manager backend implementation

use crate::{PackageRef, PackageManagerBackend, PackageManagerKind, ProxyConfigAction, UrlPattern};
use async_trait::async_trait;

use super::bsd_privilege;

/// NetBSD pkgin package manager backend
pub struct PkginBackend;

impl PkginBackend {
    pub fn new() -> Self {
        PkginBackend
    }
}

impl Default for PkginBackend {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl PackageManagerBackend for PkginBackend {
    fn kind(&self) -> PackageManagerKind {
        PackageManagerKind::Pkgin
    }

    fn is_available(&self) -> bool {
        which::which("pkgin").is_ok()
    }

    async fn list_installed(&self) -> crate::Result<Vec<PackageRef>> {
        let output = bsd_privilege::escalate("pkgin", &["list"])
            .output()
            .await?;

        if !output.status.success() {
            return Err(crate::Error::CommandFailed {
                command: "pkgin list".to_string(),
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
                        manager: PackageManagerKind::Pkgin,
                    });
                }
            }
        }

        Ok(packages)
    }

    fn upstream_url_patterns(&self) -> Vec<UrlPattern> {
        vec![
            UrlPattern::new(
                r"https?://[^/]*netbsd\.org/.*",
                100,
                "NetBSD official",
            )
            .expect("valid regex"),
            UrlPattern::new(
                r"https?://cdn\.netbsd\.org/pub/pkgsrc/.*",
                95,
                "NetBSD pkgsrc CDN",
            )
            .expect("valid regex"),
            UrlPattern::new(
                r"https?://[^/]*pkgsrc\.org/.*",
                80,
                "pkgsrc repository",
            )
            .expect("valid regex"),
        ]
    }

    fn url_to_cache_path(&self, url: &str) -> Option<String> {
        let url = url::Url::parse(url).ok()?;
        let path = url.path().trim_start_matches('/');
        Some(format!("pkgin/{}", path))
    }

    fn proxy_config(&self, proxy_addr: &str) -> ProxyConfigAction {
        ProxyConfigAction::Multiple(bsd_privilege::proxy_env_actions(proxy_addr))
    }

    fn remove_proxy_config(&self) -> ProxyConfigAction {
        ProxyConfigAction::Multiple(bsd_privilege::remove_proxy_env_actions())
    }
}
