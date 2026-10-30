//! Package manager abstraction for Monopsony
//!
//! This module provides a trait-based abstraction for different package managers,
//! allowing uniform handling of package installation, inventory, and proxy configuration.

mod apk;
mod apt;
mod brew;
mod bsd_privilege;
mod cargo;
mod choco;
mod dnf;
mod emerge;
mod mport;
mod nix;
mod npm;
mod pacman;
mod pip;
mod pkg;
mod pkg_add;
mod pkgin;
mod scoop;
mod upgradable;
mod winget;
mod xbps;
mod zypper;

pub use apk::ApkBackend;
pub use apt::AptBackend;
pub use brew::HomebrewBackend;
pub use cargo::CargoBackend;
pub use choco::ChocoBackend;
pub use dnf::DnfBackend;
pub use emerge::EmergeBackend;
pub use mport::MportBackend;
pub use nix::NixBackend;
pub use npm::NpmBackend;
pub use pacman::PacmanBackend;
pub use pip::PipBackend;
pub use pkg::PkgBackend;
pub use pkg_add::PkgAddBackend;
pub use pkgin::PkginBackend;
pub use scoop::ScoopBackend;
pub use upgradable::UpgradableReport;
pub use winget::WingetBackend;
pub use xbps::XbpsBackend;
pub use zypper::ZypperBackend;

use async_trait::async_trait;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Supported package manager kinds
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PackageManagerKind {
    Apt,
    Dnf,
    Mport,
    Pacman,
    Pkg,
    PkgAdd,
    Pkgin,
    Apk,
    Winget,
    Choco,
    Scoop,
    Npm,
    Pip,
    Cargo,
    Zypper,
    Xbps,
    Emerge,
    Nix,
    Homebrew,
}

impl PackageManagerKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            PackageManagerKind::Apt => "apt",
            PackageManagerKind::Dnf => "dnf",
            PackageManagerKind::Mport => "mport",
            PackageManagerKind::Pacman => "pacman",
            PackageManagerKind::Pkg => "pkg",
            PackageManagerKind::PkgAdd => "pkg_add",
            PackageManagerKind::Pkgin => "pkgin",
            PackageManagerKind::Apk => "apk",
            PackageManagerKind::Winget => "winget",
            PackageManagerKind::Choco => "choco",
            PackageManagerKind::Scoop => "scoop",
            PackageManagerKind::Npm => "npm",
            PackageManagerKind::Pip => "pip",
            PackageManagerKind::Cargo => "cargo",
            PackageManagerKind::Zypper => "zypper",
            PackageManagerKind::Xbps => "xbps",
            PackageManagerKind::Emerge => "emerge",
            PackageManagerKind::Nix => "nix",
            PackageManagerKind::Homebrew => "brew",
        }
    }

    /// Parse a package manager kind from its string name.
    /// Not named `from_str` to avoid confusion with `std::str::FromStr`.
    pub fn parse_kind(s: &str) -> Option<Self> {
        match s {
            "apt" => Some(PackageManagerKind::Apt),
            "dnf" => Some(PackageManagerKind::Dnf),
            "mport" => Some(PackageManagerKind::Mport),
            "pacman" => Some(PackageManagerKind::Pacman),
            "pkg" => Some(PackageManagerKind::Pkg),
            "pkg_add" => Some(PackageManagerKind::PkgAdd),
            "pkgin" => Some(PackageManagerKind::Pkgin),
            "apk" => Some(PackageManagerKind::Apk),
            "winget" => Some(PackageManagerKind::Winget),
            "choco" => Some(PackageManagerKind::Choco),
            "scoop" => Some(PackageManagerKind::Scoop),
            "npm" => Some(PackageManagerKind::Npm),
            "pip" => Some(PackageManagerKind::Pip),
            "cargo" => Some(PackageManagerKind::Cargo),
            "zypper" => Some(PackageManagerKind::Zypper),
            "xbps" => Some(PackageManagerKind::Xbps),
            "emerge" => Some(PackageManagerKind::Emerge),
            "nix" => Some(PackageManagerKind::Nix),
            "brew" => Some(PackageManagerKind::Homebrew),
            _ => None,
        }
    }
}

impl std::fmt::Display for PackageManagerKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// Reference to an installed package
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackageRef {
    pub name: String,
    pub version: Option<String>,
    #[serde(default)]
    pub architecture: Option<String>,
    pub manager: PackageManagerKind,
    /// Epoch seconds when this version was confirmed installed (0 = unknown).
    /// Populated by client inventory scans; stored by server if telemetry enabled.
    #[serde(default)]
    pub installed_at: i64,
}

/// URL pattern for matching upstream package URLs
#[derive(Debug, Clone)]
pub struct UrlPattern {
    /// Regex pattern for matching URLs
    pub pattern: Regex,
    /// Priority for pattern matching (higher = checked first)
    pub priority: u32,
    /// Description of what this pattern matches
    pub description: String,
    /// Cache class assigned to URLs matching this pattern (Sprint 23).
    /// Classification happens at miss time: the matched pattern's class
    /// becomes the stored object's class. Defaults to `Distro` via
    /// `UrlPattern::new` so existing backends are unaffected.
    pub cache_class: crate::config::CacheClass,
}

impl UrlPattern {
    pub fn new(pattern: &str, priority: u32, description: &str) -> crate::Result<Self> {
        Self::with_class(
            pattern,
            priority,
            description,
            crate::config::CacheClass::Distro,
        )
    }

    /// Construct a pattern with an explicit cache class (Sprint 23). Language
    /// backends use this to distinguish immutable artifacts (LangArtifact)
    /// from TTL-revalidated registry metadata (LangMetadata).
    pub fn with_class(
        pattern: &str,
        priority: u32,
        description: &str,
        cache_class: crate::config::CacheClass,
    ) -> crate::Result<Self> {
        let regex = Regex::new(pattern)
            .map_err(|e| crate::Error::Parse(format!("Invalid URL pattern: {}", e)))?;
        Ok(Self {
            pattern: regex,
            priority,
            description: description.to_string(),
            cache_class,
        })
    }

    pub fn matches(&self, url: &str) -> bool {
        self.pattern.is_match(url)
    }
}

/// Action to configure proxy for a package manager
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ProxyConfigAction {
    /// Write a configuration file
    WriteFile { path: PathBuf, content: String },
    /// Remove a configuration file
    RemoveFile { path: PathBuf },
    /// Set an environment variable
    SetEnv { key: String, value: String },
    /// Unset an environment variable
    UnsetEnv { key: String },
    /// Run a command
    RunCommand { program: String, args: Vec<String> },
    /// Multiple actions needed
    Multiple(Vec<ProxyConfigAction>),
    /// No configuration needed (package manager uses system proxy)
    None,
}

impl ProxyConfigAction {
    /// Execute this proxy config action. Returns a human-readable description of what was done.
    /// For `Multiple`, all inner actions are executed in order.
    /// Errors are logged but do not stop execution of subsequent actions in `Multiple`.
    pub fn execute(&self) -> Vec<anyhow::Result<String>> {
        match self {
            ProxyConfigAction::WriteFile { path, content } => {
                let result = (|| -> anyhow::Result<String> {
                    if let Some(parent) = path.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    std::fs::write(path, content)?;
                    Ok(format!("wrote {}", path.display()))
                })();
                vec![result]
            }
            ProxyConfigAction::RemoveFile { path } => {
                let result = (|| -> anyhow::Result<String> {
                    if path.exists() {
                        std::fs::remove_file(path)?;
                        Ok(format!("removed {}", path.display()))
                    } else {
                        Ok(format!("skipped {} (not found)", path.display()))
                    }
                })();
                vec![result]
            }
            ProxyConfigAction::SetEnv { key, value } => {
                std::env::set_var(key, value);
                vec![Ok(format!("set env {}", key))]
            }
            ProxyConfigAction::UnsetEnv { key } => {
                std::env::remove_var(key);
                vec![Ok(format!("unset env {}", key))]
            }
            ProxyConfigAction::RunCommand { program, args } => {
                let result = (|| -> anyhow::Result<String> {
                    let output = std::process::Command::new(program).args(args).output()?;
                    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
                    if output.status.success() {
                        Ok(format!("ran {} -> {}", program, stdout))
                    } else {
                        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
                        anyhow::bail!(
                            "{} failed (exit {:?}): {}",
                            program,
                            output.status.code(),
                            stderr
                        )
                    }
                })();
                vec![result]
            }
            ProxyConfigAction::Multiple(actions) => {
                actions.iter().flat_map(|a| a.execute()).collect()
            }
            ProxyConfigAction::None => vec![Ok("no action needed".to_string())],
        }
    }

    /// Dry-run: describe what would be done without executing.
    pub fn describe(&self) -> Vec<String> {
        match self {
            ProxyConfigAction::WriteFile { path, .. } => vec![format!("write {}", path.display())],
            ProxyConfigAction::RemoveFile { path } => vec![format!("remove {}", path.display())],
            ProxyConfigAction::SetEnv { key, value } => vec![format!("set {}={}", key, value)],
            ProxyConfigAction::UnsetEnv { key } => vec![format!("unset {}", key)],
            ProxyConfigAction::RunCommand { program, args } => {
                vec![format!("run {} {}", program, args.join(" "))]
            }
            ProxyConfigAction::Multiple(actions) => {
                actions.iter().flat_map(|a| a.describe()).collect()
            }
            ProxyConfigAction::None => vec!["no action needed".to_string()],
        }
    }
}

/// Backend trait for package manager implementations
#[async_trait]
pub trait PackageManagerBackend: Send + Sync {
    /// Returns the kind of this package manager
    fn kind(&self) -> PackageManagerKind;

    /// Check if this package manager is available on the current system
    fn is_available(&self) -> bool;

    /// List all installed packages
    async fn list_installed(&self) -> crate::Result<Vec<PackageRef>>;

    /// List upgradable packages as aggregate counts (Sprint 29, grounding D4).
    ///
    /// Default answer is "unsupported" with a note — only distro PMs with an
    /// upgradable listing override this (apt/dnf/pacman/apk first-class;
    /// zypper/xbps/emerge best-effort). `apt list --upgradable` and friends
    /// are slow: callers must run this at inventory cadence, never per
    /// heartbeat tick (grounding D3).
    async fn list_upgradable(&self) -> crate::Result<UpgradableReport> {
        let _ = self;
        Ok(UpgradableReport::unsupported(self.kind()))
    }

    /// Get URL patterns for matching upstream package URLs
    fn upstream_url_patterns(&self) -> Vec<UrlPattern>;

    /// Convert an upstream URL to a cache path
    fn url_to_cache_path(&self, url: &str) -> Option<String>;

    /// Get the action needed to configure this package manager to use a proxy
    fn proxy_config(&self, proxy_addr: &str) -> ProxyConfigAction;

    /// Get the action needed to remove proxy configuration
    fn remove_proxy_config(&self) -> ProxyConfigAction;
}

/// Get all package manager backends (regardless of local availability).
///
/// Use this on the **server** where we need to match/cache URLs for all client
/// OSes, not just the server's own OS. `get_available_backends()` filters by
/// `is_available()` which checks for the PM binary locally — wrong for a server
/// caching packages for Arch, Alpine, Debian, etc. clients simultaneously.
pub fn get_all_backends() -> Vec<Box<dyn PackageManagerBackend>> {
    vec![
        Box::new(AptBackend::new()),
        Box::new(DnfBackend::new()),
        Box::new(MportBackend::new()),
        Box::new(PacmanBackend::new()),
        Box::new(PkgBackend::new()),
        Box::new(PkgAddBackend::new()),
        Box::new(PkginBackend::new()),
        Box::new(ApkBackend::new()),
        Box::new(WingetBackend::new()),
        Box::new(ChocoBackend::new()),
        Box::new(ScoopBackend::new()),
        Box::new(NpmBackend::new()),
        Box::new(PipBackend::new()),
        Box::new(CargoBackend::new()),
        Box::new(ZypperBackend::new()),
        Box::new(XbpsBackend::new()),
        Box::new(EmergeBackend::new()),
        Box::new(NixBackend::new()),
        Box::new(HomebrewBackend::new()),
    ]
}

/// Get all package manager backends available on the current platform.
///
/// Use this on the **client** for inventory reporting (only report packages
/// from PMs that are actually installed locally).
pub fn get_available_backends() -> Vec<Box<dyn PackageManagerBackend>> {
    get_all_backends()
        .into_iter()
        .filter(|b| b.is_available())
        .collect()
}

/// Get a specific package manager backend by kind
pub fn get_backend(kind: PackageManagerKind) -> Option<Box<dyn PackageManagerBackend>> {
    match kind {
        PackageManagerKind::Apt => Some(Box::new(AptBackend::new())),
        PackageManagerKind::Dnf => Some(Box::new(DnfBackend::new())),
        PackageManagerKind::Mport => Some(Box::new(MportBackend::new())),
        PackageManagerKind::Pacman => Some(Box::new(PacmanBackend::new())),
        PackageManagerKind::Pkg => Some(Box::new(PkgBackend::new())),
        PackageManagerKind::PkgAdd => Some(Box::new(PkgAddBackend::new())),
        PackageManagerKind::Pkgin => Some(Box::new(PkginBackend::new())),
        PackageManagerKind::Apk => Some(Box::new(ApkBackend::new())),
        PackageManagerKind::Winget => Some(Box::new(WingetBackend::new())),
        PackageManagerKind::Choco => Some(Box::new(ChocoBackend::new())),
        PackageManagerKind::Scoop => Some(Box::new(ScoopBackend::new())),
        PackageManagerKind::Npm => Some(Box::new(NpmBackend::new())),
        PackageManagerKind::Pip => Some(Box::new(PipBackend::new())),
        PackageManagerKind::Cargo => Some(Box::new(CargoBackend::new())),
        PackageManagerKind::Zypper => Some(Box::new(ZypperBackend::new())),
        PackageManagerKind::Xbps => Some(Box::new(XbpsBackend::new())),
        PackageManagerKind::Emerge => Some(Box::new(EmergeBackend::new())),
        PackageManagerKind::Nix => Some(Box::new(NixBackend::new())),
        PackageManagerKind::Homebrew => Some(Box::new(HomebrewBackend::new())),
    }
}

/// Check if a URL matches any package manager's upstream patterns.
/// Returns true if the URL should be proxied through the cache server.
/// Uses `get_all_backends()` so it matches URLs for all client OSes,
/// not just PMs installed on this machine.
pub fn matches_package_url(url: &str) -> bool {
    get_all_backends().iter().any(|backend| {
        backend
            .upstream_url_patterns()
            .iter()
            .any(|p| p.matches(url))
    })
}
