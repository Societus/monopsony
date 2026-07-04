//! Winget package manager backend implementation

use crate::{PackageRef, PackageManagerBackend, PackageManagerKind, ProxyConfigAction, UrlPattern};
use async_trait::async_trait;

pub struct WingetBackend;

impl WingetBackend {
    pub fn new() -> Self {
        WingetBackend
    }
}

impl Default for WingetBackend {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl PackageManagerBackend for WingetBackend {
    fn kind(&self) -> PackageManagerKind {
        PackageManagerKind::Winget
    }

    fn is_available(&self) -> bool {
        which::which("winget").is_ok()
    }

    async fn list_installed(&self) -> crate::Result<Vec<PackageRef>> {
        let output = tokio::process::Command::new("winget")
            .args(["list", "--accept-source-agreements"])
            .output()
            .await?;

        if !output.status.success() {
            return Err(crate::Error::CommandFailed {
                command: "winget list".to_string(),
                exit_code: output.status.code(),
            });
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut packages = Vec::new();

        for line in stdout.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with("Name") || line.starts_with('-') {
                continue;
            }
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 3 {
                let name = parts[0].to_string();
                let version = if parts.len() >= 4 {
                    Some(parts[parts.len() - 3].to_string())
                } else {
                    Some(parts[parts.len() - 1].to_string())
                };
                packages.push(PackageRef {
                    name,
                    version,
                    architecture: None,
                    manager: PackageManagerKind::Winget,
                    installed_at: 0,
                });
            }
        }

        Ok(packages)
    }

    fn upstream_url_patterns(&self) -> Vec<UrlPattern> {
        vec![
            UrlPattern::new(r"https?://[^/]*githubusercontent\.com/.*", 100, "GitHub CDN (winget)").expect("valid regex"),
            UrlPattern::new(r"https?://[^/]*aka\.ms/.*", 90, "Microsoft redirect (winget)").expect("valid regex"),
            UrlPattern::new(r"https?://[^/]*winget\.cdn\..*", 90, "Winget CDN").expect("valid regex"),
        ]
    }

    fn url_to_cache_path(&self, url: &str) -> Option<String> {
        let url = url::Url::parse(url).ok()?;
        Some(format!("winget{}", url.path()))
    }

    fn proxy_config(&self, _proxy_addr: &str) -> ProxyConfigAction {
        ProxyConfigAction::SetEnv {
            key: "HTTP_PROXY".to_string(),
            value: _proxy_addr.to_string(),
        }
    }

    fn remove_proxy_config(&self) -> ProxyConfigAction {
        ProxyConfigAction::UnsetEnv {
            key: "HTTP_PROXY".to_string(),
        }
    }
}
