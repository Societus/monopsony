//! DNF package manager backend implementation

use crate::{PackageManagerBackend, PackageManagerKind, PackageRef, ProxyConfigAction, UrlPattern};
use async_trait::async_trait;
use std::path::PathBuf;

pub struct DnfBackend {
    #[allow(dead_code)] // Will be used when reading actual dnf config
    config_path: Option<PathBuf>,
}

impl DnfBackend {
    pub fn new() -> Self {
        DnfBackend { config_path: None }
    }
}

impl Default for DnfBackend {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl PackageManagerBackend for DnfBackend {
    fn kind(&self) -> PackageManagerKind {
        PackageManagerKind::Dnf
    }

    fn is_available(&self) -> bool {
        which::which("dnf").is_ok()
    }

    async fn list_installed(&self) -> crate::Result<Vec<PackageRef>> {
        let use_rpm = which::which("rpm").is_ok();

        let output = if use_rpm {
            tokio::process::Command::new("rpm")
                .args([
                    "-qa",
                    "--queryformat",
                    "%{NAME}\t%{VERSION}-%{RELEASE}\t%{ARCH}\n",
                ])
                .output()
                .await
                .map_err(|_| crate::Error::CommandFailed {
                    command: "rpm -qa".to_string(),
                    exit_code: None,
                })?
        } else {
            tokio::process::Command::new("dnf")
                .args(["list", "installed"])
                .output()
                .await
                .map_err(|_| crate::Error::CommandFailed {
                    command: "dnf list installed".to_string(),
                    exit_code: None,
                })?
        };

        if !output.status.success() {
            let cmd = if use_rpm {
                "rpm -qa"
            } else {
                "dnf list installed"
            };
            return Err(crate::Error::CommandFailed {
                command: cmd.to_string(),
                exit_code: output.status.code(),
            });
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut packages = Vec::new();

        if use_rpm {
            for line in stdout.lines() {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                let parts: Vec<&str> = line.splitn(3, '\t').collect();
                if parts.len() == 3 {
                    packages.push(PackageRef {
                        name: parts[0].to_string(),
                        version: Some(parts[1].to_string()),
                        architecture: Some(parts[2].to_string()),
                        manager: PackageManagerKind::Dnf,
                        installed_at: 0,
                    });
                }
            }
        } else {
            for line in stdout.lines() {
                let line = line.trim();
                if line.is_empty()
                    || line.starts_with("Installed")
                    || line.starts_with("Last metadata")
                {
                    continue;
                }
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() >= 2 {
                    let name_arch = parts[0];
                    let version = parts[1];
                    let (name, arch) = if let Some(pos) = name_arch.rfind('.') {
                        (&name_arch[..pos], Some(name_arch[pos + 1..].to_string()))
                    } else {
                        (name_arch, None)
                    };
                    packages.push(PackageRef {
                        name: name.to_string(),
                        version: Some(version.to_string()),
                        architecture: arch,
                        manager: PackageManagerKind::Dnf,
                        installed_at: 0,
                    });
                }
            }
        }

        Ok(packages)
    }

    fn upstream_url_patterns(&self) -> Vec<UrlPattern> {
        vec![
            UrlPattern::new(r"https?://[^/]*(?:fedoraproject|centos|redhat)\.org/.*\.rpm", 100, "Fedora/CentOS/RHEL packages").expect("valid regex"),
            UrlPattern::new(r"https?://[^/]*rpmfusion\.org/.*\.rpm", 100, "RPM Fusion packages").expect("valid regex"),
            UrlPattern::new(r"https?://[^/]*almalinux\.org/.*\.rpm", 100, "AlmaLinux packages").expect("valid regex"),
            UrlPattern::new(r"https?://[^/]*rockylinux\.org/.*\.rpm", 100, "Rocky Linux packages").expect("valid regex"),
            UrlPattern::new(r"https?://[^/]*(?:fedoraproject|centos|redhat)\.org/.*\.(?:xml|sqlite(?:\.bz2)?|json)", 90, "Fedora/CentOS/RHEL metadata").expect("valid regex"),
            UrlPattern::new(r"https?://[^/]*rpmfusion\.org/.*\.(?:xml|sqlite(?:\.bz2)?|json)", 90, "RPM Fusion metadata").expect("valid regex"),
            UrlPattern::new(r"https?://[^/]*almalinux\.org/.*\.(?:xml|sqlite(?:\.bz2)?|json)", 90, "AlmaLinux metadata").expect("valid regex"),
            UrlPattern::new(r"https?://[^/]*rockylinux\.org/.*\.(?:xml|sqlite(?:\.bz2)?|json)", 90, "Rocky Linux metadata").expect("valid regex"),
        ]
    }

    fn url_to_cache_path(&self, url: &str) -> Option<String> {
        let url = url::Url::parse(url).ok()?;
        let path = url.path().trim_start_matches('/');
        Some(format!("dnf/{}", path))
    }

    fn proxy_config(&self, proxy_addr: &str) -> ProxyConfigAction {
        let config_content = format!(
            r#"proxy={}
"#,
            proxy_addr
        );

        ProxyConfigAction::WriteFile {
            path: PathBuf::from("/etc/dnf/dnf.conf"),
            content: config_content,
        }
    }

    fn remove_proxy_config(&self) -> ProxyConfigAction {
        ProxyConfigAction::RemoveFile {
            path: PathBuf::from("/etc/dnf/dnf.conf"),
        }
    }
}
