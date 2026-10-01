//! Scoop package manager backend implementation

use crate::{PackageManagerBackend, PackageManagerKind, PackageRef, ProxyConfigAction, UrlPattern};
use async_trait::async_trait;

pub struct ScoopBackend;

impl ScoopBackend {
    pub fn new() -> Self {
        ScoopBackend
    }
}

impl Default for ScoopBackend {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl PackageManagerBackend for ScoopBackend {
    fn kind(&self) -> PackageManagerKind {
        PackageManagerKind::Scoop
    }

    fn is_available(&self) -> bool {
        which::which("scoop").is_ok()
    }

    async fn list_installed(&self) -> crate::Result<Vec<PackageRef>> {
        let output = tokio::process::Command::new("scoop")
            .args(["list"])
            .output()
            .await?;

        if !output.status.success() {
            return Err(crate::Error::CommandFailed {
                command: "scoop list".to_string(),
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
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 2 {
                packages.push(PackageRef {
                    name: parts[0].to_string(),
                    version: Some(parts[1].to_string()),
                    architecture: None,
                    manager: PackageManagerKind::Scoop,
                    installed_at: 0,
                });
            }
        }

        Ok(packages)
    }

    fn upstream_url_patterns(&self) -> Vec<UrlPattern> {
        vec![
            UrlPattern::new(
                r"https?://[^/]*github\.com/ScoopInstaller/.*",
                100,
                "Scoop packages",
            )
            .expect("valid regex"),
            UrlPattern::new(
                r"https?://[^/]*raw\.githubusercontent\.com/.*",
                90,
                "GitHub raw (scoop)",
            )
            .expect("valid regex"),
        ]
    }

    fn url_to_cache_path(&self, url: &str) -> Option<String> {
        let url = url::Url::parse(url).ok()?;
        Some(format!("scoop{}", url.path()))
    }

    fn proxy_config(&self, proxy_addr: &str) -> ProxyConfigAction {
        // Derive the mirror (shim) base from the proxy address: the mirror
        // router listens on :3129 while the caching proxy is on :3128.
        let mirror_addr = if let Some(stripped) = proxy_addr.strip_suffix(":3128") {
            format!("{}:3129", stripped)
        } else {
            proxy_addr.to_string()
        };
        let bucket_url = format!("http://{}/scoop/main", mirror_addr);
        let add_script = format!(
            "if (scoop bucket list | Select-String -Quiet '^monopsony') {{ scoop bucket remove monopsony }}; \
             scoop bucket add monopsony {}",
            bucket_url
        );
        ProxyConfigAction::Multiple(vec![
            ProxyConfigAction::SetEnv {
                key: "HTTP_PROXY".to_string(),
                value: proxy_addr.to_string(),
            },
            ProxyConfigAction::RunCommand {
                program: "powershell".to_string(),
                args: vec!["-NoProfile".to_string(), "-Command".to_string(), add_script],
            },
        ])
    }

    fn remove_proxy_config(&self) -> ProxyConfigAction {
        // Best-effort bucket removal: `; exit 0` makes failure a no-op.
        ProxyConfigAction::Multiple(vec![
            ProxyConfigAction::UnsetEnv {
                key: "HTTP_PROXY".to_string(),
            },
            ProxyConfigAction::RunCommand {
                program: "powershell".to_string(),
                args: vec![
                    "-NoProfile".to_string(),
                    "-Command".to_string(),
                    "scoop bucket remove monopsony; exit 0".to_string(),
                ],
            },
        ])
    }
}
