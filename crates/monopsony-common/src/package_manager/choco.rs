//! Chocolatey package manager backend implementation

use crate::{PackageRef, PackageManagerBackend, PackageManagerKind, ProxyConfigAction, UrlPattern};
use async_trait::async_trait;
use std::path::PathBuf;

pub struct ChocoBackend;

impl ChocoBackend {
    pub fn new() -> Self {
        ChocoBackend
    }
}

impl Default for ChocoBackend {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl PackageManagerBackend for ChocoBackend {
    fn kind(&self) -> PackageManagerKind {
        PackageManagerKind::Choco
    }

    fn is_available(&self) -> bool {
        which::which("choco").is_ok()
    }

    async fn list_installed(&self) -> crate::Result<Vec<PackageRef>> {
        let output = tokio::process::Command::new("choco")
            .args(["list", "--local-only", "--limit-output"])
            .output()
            .await?;

        if !output.status.success() {
            return Err(crate::Error::CommandFailed {
                command: "choco list --local-only".to_string(),
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
            let parts: Vec<&str> = line.splitn(2, '|').collect();
            if parts.len() == 2 {
                packages.push(PackageRef {
                    name: parts[0].to_string(),
                    version: Some(parts[1].to_string()),
                    architecture: None,
                    manager: PackageManagerKind::Choco,
                    installed_at: 0,
                });
            }
        }

        Ok(packages)
    }

    fn upstream_url_patterns(&self) -> Vec<UrlPattern> {
        vec![
            UrlPattern::new(r"https?://[^/]*chocolatey\.org/.*", 100, "Chocolatey packages").expect("valid regex"),
            UrlPattern::new(r"https?://[^/]*community\.chocolatey\.org/.*", 100, "Chocolatey community").expect("valid regex"),
        ]
    }

    fn url_to_cache_path(&self, url: &str) -> Option<String> {
        let url = url::Url::parse(url).ok()?;
        Some(format!("choco{}", url.path()))
    }

    fn proxy_config(&self, proxy_addr: &str) -> ProxyConfigAction {
        ProxyConfigAction::WriteFile {
            path: PathBuf::from("C:/ProgramData/chocolatey/config/monopsony-proxy.config"),
            content: format!(r#"<?xml version="1.0"?>
<configuration>
  <proxy proxyUrl="{}" />
</configuration>
"#, proxy_addr),
        }
    }

    fn remove_proxy_config(&self) -> ProxyConfigAction {
        ProxyConfigAction::RemoveFile {
            path: PathBuf::from("C:/ProgramData/chocolatey/config/monopsony-proxy.config"),
        }
    }
}
