//! Apt package manager backend implementation

use crate::{PackageManagerBackend, PackageManagerKind, PackageRef, ProxyConfigAction, UrlPattern};
use async_trait::async_trait;
use std::path::PathBuf;

/// Apt package manager backend
pub struct AptBackend {
    #[allow(dead_code)] // Will be used when reading actual apt config
    config_path: Option<PathBuf>,
}

impl AptBackend {
    /// Create a new Apt backend instance
    pub fn new() -> Self {
        AptBackend { config_path: None }
    }
}

impl Default for AptBackend {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl PackageManagerBackend for AptBackend {
    fn kind(&self) -> PackageManagerKind {
        PackageManagerKind::Apt
    }

    fn is_available(&self) -> bool {
        which::which("apt-get").is_ok() || which::which("dpkg").is_ok()
    }

    async fn list_installed(&self) -> crate::Result<Vec<PackageRef>> {
        let output = tokio::process::Command::new("dpkg")
            .args(["-l", "--no-pager"])
            .output()
            .await?;

        if !output.status.success() {
            return Err(crate::Error::CommandFailed {
                command: "dpkg -l".to_string(),
                exit_code: output.status.code(),
            });
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut packages = Vec::new();

        for line in stdout.lines().skip(5) {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 3 {
                let name = parts[1].to_string();
                let version = parts[2].to_string();
                let arch = parts.get(3).map(|s| s.to_string());

                packages.push(PackageRef {
                    name,
                    version: Some(version),
                    architecture: arch,
                    manager: PackageManagerKind::Apt,
                    installed_at: 0,
                });
            }
        }

        Ok(packages)
    }

    fn upstream_url_patterns(&self) -> Vec<UrlPattern> {
        vec![
            // Debian/Ubuntu mirrors
            UrlPattern::new(
                r"https?://[^/]*(?:debian|ubuntu)\.org/.*\.deb",
                100,
                "Debian/Ubuntu official",
            )
            .expect("valid regex"),
            UrlPattern::new(
                r"https?://[^/]*(?:archive|security)\.(?:debian|ubuntu)\.org/.*",
                90,
                "Debian/Ubuntu archives",
            )
            .expect("valid regex"),
            UrlPattern::new(r"https?://[^/]*/ubuntu/.*\.deb", 80, "Ubuntu mirrors")
                .expect("valid regex"),
        ]
    }

    fn url_to_cache_path(&self, url: &str) -> Option<String> {
        let url = url::Url::parse(url).ok()?;
        let path = url.path().trim_start_matches('/');
        Some(format!("apt/{}", path))
    }

    fn proxy_config(&self, proxy_addr: &str) -> ProxyConfigAction {
        let config_content = format!(
            r#"Acquire::http::Proxy "{}";
Acquire::https::Proxy "{}";
"#,
            proxy_addr, proxy_addr
        );

        ProxyConfigAction::WriteFile {
            path: PathBuf::from("/etc/apt/apt.conf.d/99monopsony"),
            content: config_content,
        }
    }

    fn remove_proxy_config(&self) -> ProxyConfigAction {
        ProxyConfigAction::RemoveFile {
            path: PathBuf::from("/etc/apt/apt.conf.d/99monopsony"),
        }
    }
}
