//! Chocolatey package manager backend implementation

use crate::{PackageManagerBackend, PackageManagerKind, PackageRef, ProxyConfigAction, UrlPattern};
use async_trait::async_trait;

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
            UrlPattern::new(
                r"https?://[^/]*chocolatey\.org/.*",
                100,
                "Chocolatey packages",
            )
            .expect("valid regex"),
            UrlPattern::new(
                r"https?://[^/]*community\.chocolatey\.org/.*",
                100,
                "Chocolatey community",
            )
            .expect("valid regex"),
        ]
    }

    fn url_to_cache_path(&self, url: &str) -> Option<String> {
        let url = url::Url::parse(url).ok()?;
        Some(format!("choco{}", url.path()))
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
        let feed_url = format!("http://{}/choco/api/v2/", mirror_addr);
        // Point choco at our NuGet v2 shim feed; remove any stale source of
        // the same name first so re-configuration is idempotent.
        let add_script = format!(
            "if (choco source list -r | Select-String '^monopsony') {{ choco source remove -n monopsony }}; \
             choco source add -n monopsony -s {}",
            feed_url
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
        // Best-effort source removal: `; exit 0` makes failure a no-op
        // (e.g. source already absent or choco not installed).
        ProxyConfigAction::Multiple(vec![
            ProxyConfigAction::UnsetEnv {
                key: "HTTP_PROXY".to_string(),
            },
            ProxyConfigAction::RunCommand {
                program: "powershell".to_string(),
                args: vec![
                    "-NoProfile".to_string(),
                    "-Command".to_string(),
                    "choco source remove -n monopsony; exit 0".to_string(),
                ],
            },
        ])
    }
}
