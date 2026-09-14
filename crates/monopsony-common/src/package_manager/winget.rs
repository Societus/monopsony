//! Winget package manager backend implementation

use crate::{PackageManagerBackend, PackageManagerKind, PackageRef, ProxyConfigAction, UrlPattern};
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
            UrlPattern::new(
                r"https?://[^/]*githubusercontent\.com/.*",
                100,
                "GitHub CDN (winget)",
            )
            .expect("valid regex"),
            UrlPattern::new(
                r"https?://[^/]*aka\.ms/.*",
                90,
                "Microsoft redirect (winget)",
            )
            .expect("valid regex"),
            UrlPattern::new(r"https?://[^/]*winget\.cdn\..*", 90, "Winget CDN")
                .expect("valid regex"),
        ]
    }

    fn url_to_cache_path(&self, url: &str) -> Option<String> {
        let url = url::Url::parse(url).ok()?;
        Some(format!("winget{}", url.path()))
    }

    fn proxy_config(&self, proxy_addr: &str) -> ProxyConfigAction {
        // Derive the mirror (shim) base from the proxy address: the mirror
        // router listens on :3129 while the caching proxy is on :3128
        // (same derivation as scoop.rs).
        let mirror_addr = if let Some(stripped) = proxy_addr.strip_suffix(":3128") {
            format!("{}:3129", stripped)
        } else {
            proxy_addr.to_string()
        };
        let source_url = format!("http://{}/winget/", mirror_addr);
        // winget warns on plain-http sources; --trust accepts it. Remove any
        // existing monopsony source first (2>$null makes removal-of-absent a
        // no-op) so re-configure is idempotent.
        let add_script = format!(
            "winget source remove monopsony 2>$null; winget source add -n monopsony -a {} -t Microsoft.Rest --trust",
            source_url
        );
        ProxyConfigAction::Multiple(vec![
            ProxyConfigAction::SetEnv {
                key: "HTTP_PROXY".to_string(),
                value: proxy_addr.to_string(),
            },
            ProxyConfigAction::RunCommand {
                program: "powershell".to_string(),
                args: vec![
                    "-NoProfile".to_string(),
                    "-Command".to_string(),
                    add_script,
                ],
            },
        ])
    }

    fn remove_proxy_config(&self) -> ProxyConfigAction {
        // Best-effort source removal: `; exit 0` makes failure a no-op.
        ProxyConfigAction::Multiple(vec![
            ProxyConfigAction::UnsetEnv {
                key: "HTTP_PROXY".to_string(),
            },
            ProxyConfigAction::RunCommand {
                program: "powershell".to_string(),
                args: vec![
                    "-NoProfile".to_string(),
                    "-Command".to_string(),
                    "winget source remove monopsony; exit 0".to_string(),
                ],
            },
        ])
    }
}
