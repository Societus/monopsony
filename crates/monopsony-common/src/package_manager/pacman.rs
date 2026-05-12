//! Pacman package manager backend implementation

use crate::{PackageRef, PackageManagerBackend, PackageManagerKind, ProxyConfigAction, UrlPattern};
use async_trait::async_trait;
use std::path::PathBuf;

pub struct PacmanBackend;

impl PacmanBackend {
    pub fn new() -> Self {
        PacmanBackend
    }
}

impl Default for PacmanBackend {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl PackageManagerBackend for PacmanBackend {
    fn kind(&self) -> PackageManagerKind {
        PackageManagerKind::Pacman
    }

    fn is_available(&self) -> bool {
        which::which("pacman").is_ok()
    }

    async fn list_installed(&self) -> crate::Result<Vec<PackageRef>> {
        let output = tokio::process::Command::new("pacman")
            .arg("-Q")
            .output()
            .await
            .map_err(|_| crate::Error::CommandFailed {
                command: "pacman -Q".to_string(),
                exit_code: None,
            })?;

        if !output.status.success() {
            return Err(crate::Error::CommandFailed {
                command: "pacman -Q".to_string(),
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
            if let Some(pos) = line.find(' ') {
                packages.push(PackageRef {
                    name: line[..pos].to_string(),
                    version: Some(line[pos + 1..].to_string()),
                    architecture: None,
                    manager: PackageManagerKind::Pacman,
                });
            }
        }

        Ok(packages)
    }

    fn upstream_url_patterns(&self) -> Vec<UrlPattern> {
        vec![
            UrlPattern::new(r"https?://[^/]*archlinux\.org/.*\.pkg\.tar\..*", 100, "Arch Linux packages").expect("valid regex"),
            UrlPattern::new(r"https?://[^/]*mirror\.archlinux\.com/.*\.pkg\.tar\..*", 100, "Arch Linux mirror packages").expect("valid regex"),
            UrlPattern::new(r"https?://[^/]*geo\.mirror\.pkgbuild\.com/.*\.pkg\.tar\..*", 100, "pkgbuild.com mirror packages").expect("valid regex"),
            UrlPattern::new(r"https?://[^/]*archlinux\.org/.*\.db", 90, "Arch Linux repo database").expect("valid regex"),
            UrlPattern::new(r"https?://[^/]*archlinux\.org/.*\.files", 90, "Arch Linux repo files").expect("valid regex"),
            UrlPattern::new(r"https?://[^/]*mirror\.archlinux\.com/.*\.db", 90, "Arch mirror repo database").expect("valid regex"),
            UrlPattern::new(r"https?://[^/]*mirror\.archlinux\.com/.*\.files", 90, "Arch mirror repo files").expect("valid regex"),
            UrlPattern::new(r"https?://[^/]*geo\.mirror\.pkgbuild\.com/.*\.db", 90, "pkgbuild mirror repo database").expect("valid regex"),
            UrlPattern::new(r"https?://[^/]*geo\.mirror\.pkgbuild\.com/.*\.files", 90, "pkgbuild mirror repo files").expect("valid regex"),
            UrlPattern::new(r"https?://[^/]*archlinux\.org/.*\.sig", 80, "Arch Linux signatures").expect("valid regex"),
            UrlPattern::new(r"https?://[^/]*mirror\.archlinux\.com/.*\.sig", 80, "Arch mirror signatures").expect("valid regex"),
            UrlPattern::new(r"https?://[^/]*geo\.mirror\.pkgbuild\.com/.*\.sig", 80, "pkgbuild mirror signatures").expect("valid regex"),
        ]
    }

    fn url_to_cache_path(&self, url: &str) -> Option<String> {
        let url = url::Url::parse(url).ok()?;
        Some(format!("pacman{}", url.path()))
    }

    fn proxy_config(&self, proxy_addr: &str) -> ProxyConfigAction {
        ProxyConfigAction::WriteFile {
            path: PathBuf::from("/etc/pacman.d/monopsony-proxy.conf"),
            content: format!("XferCommand = /usr/bin/curl -x {} -f %u -o %o\n", proxy_addr),
        }
    }

    fn remove_proxy_config(&self) -> ProxyConfigAction {
        ProxyConfigAction::RemoveFile {
            path: PathBuf::from("/etc/pacman.d/monopsony-proxy.conf"),
        }
    }
}
