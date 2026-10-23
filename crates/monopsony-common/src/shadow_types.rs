use regex::Regex;
use serde::{Deserialize, Serialize};

/// Source type for a shadow package
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShadowSource {
    GithubRelease,
    GitRelease,
    DirectUrl,
}

/// Default self-update recipe baked into every Monopsony install.
/// This allows monopsony-server and monopsony-client to update themselves
/// via the same shadow package system used for third-party software.
pub const DEFAULT_MONOPSONY_RECIPE: &str = r#"
[package]
name = "monopsony"
description = "Monopsony self-update recipe"
source_type = "github_release"
source_url = "https://github.com/societus/monopsony"

[install]
method = "extract"
target_dir = "/usr/local/bin"
chmod = ["monopsony-server", "monopsony-client"]
backup_old = true
max_backups = 3
config_paths = ["/etc/monopsony/server.toml", "/etc/monopsony/client.toml"]

# Stop the service before swapping the binary
pre_update = "systemctl stop monopsony-server 2>/dev/null; true"

# Start the service after successful swap
post_update = "systemctl start monopsony-server 2>/dev/null; true"

# On failure: ensure service is running with old binary
on_failure = "systemctl start monopsony-server 2>/dev/null; true"

# Health check: verify the server responds after update
[install.health_check]
type = "port"
port = 50051
timeout_secs = 30

[version]
detect_command = "/usr/local/bin/monopsony-server --version 2>&1 || /usr/local/bin/monopsony-client --version 2>&1"
detect_pattern = "(\\d+\\.\\d+\\.\\d+(?:-rc\\d+)?)"

[asset_selector]
tag_pattern = "v(\\d+\\.\\d+\\.\\d+(?:-rc\\d+)?)$"
[[asset_selector.rules]]
os = "linux"
arch = "x86_64"
libc = "glibc"
pattern = "monopsony-x86_64-unknown-linux-gnu.tar.gz$"

[[asset_selector.rules]]
os = "linux"
arch = "aarch64"
libc = "glibc"
pattern = "monopsony-aarch64-unknown-linux-gnu.tar.gz$"

[[asset_selector.rules]]
os = "linux"
arch = "x86_64"
libc = "musl"
pattern = "monopsony-x86_64-unknown-linux-musl.tar.gz$"

[[asset_selector.rules]]
os = "linux"
arch = "aarch64"
libc = "musl"
pattern = "monopsony-aarch64-unknown-linux-musl.tar.gz$"

# Windows builds (PE32+)
[[asset_selector.rules]]
os = "windows"
arch = "x86_64"
pattern = "monopsony-x86_64-pc-windows-gnu.zip$"

# FreeBSD (for MidnightBSD/mport)
[[asset_selector.rules]]
os = "freebsd"
arch = "x86_64"
pattern = "monopsony-x86_64-unknown-freebsd.tar.gz$"
"#;

/// Install method defined in a recipe
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstallMethod {
    Extract,
    Build,
    Script,
}

/// Method used to detect the currently installed version of a shadow package.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VersionDetectMethod {
    /// Run a shell command and regex-match stdout (default, existing behavior)
    Command,
    /// Read a file and regex-match its contents
    File,
    /// HTTP GET/HEAD to a URL, regex-match a response header
    Header,
}

/// Version detection configuration. Extends the existing `detect_command` +
/// `detect_pattern` pair with alternative detection methods.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VersionDetect {
    /// Detection method (default: "command")
    #[serde(default = "default_version_detect_method")]
    pub method: VersionDetectMethod,
    /// Source: command string (method=command), file path (method=file), or URL (method=header)
    pub source: String,
    /// Regex pattern with one capture group for the version
    pub pattern: String,
    /// Header name to inspect (only used with method=header)
    #[serde(default)]
    pub header_name: Option<String>,
}

fn default_version_detect_method() -> VersionDetectMethod {
    VersionDetectMethod::Command
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthCheckType {
    Http,
    Port,
    Process,
}

/// Update status for history entries
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpdateStatus {
    Pending,
    Applied,
    RolledBack,
    Failed,
}

fn default_health_timeout_secs() -> u64 {
    30
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecipePackage {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub homepage: Option<String>,
    pub source_type: ShadowSource,
    pub source_url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecipeInstall {
    pub method: InstallMethod,
    pub target_dir: String,
    #[serde(default)]
    pub chmod: Option<Vec<String>>,
    #[serde(default)]
    pub service: Option<String>,
    #[serde(default)]
    pub config_paths: Option<Vec<String>>,
    #[serde(default)]
    pub backup_old: bool,
    #[serde(default)]
    pub health_check: Option<HealthCheck>,
    /// Failure policy: "rollback" (restore backup on failure) or "hold"
    /// (keep the broken state and await operator intervention). Default: "rollback".
    #[serde(default = "default_failure_policy")]
    pub failure_policy: String,
    /// Optional provisional success-monitor window (seconds). During the
    /// window the update is provisional; failure inside it triggers
    /// `failure_policy`, expiry records the update as final.
    #[serde(default)]
    pub success_monitor: Option<SuccessMonitor>,
    /// Command to run before update starts
    #[serde(default)]
    pub pre_update: Option<String>,
    /// Command to run after successful update
    #[serde(default)]
    pub post_update: Option<String>,
    /// Command to run if update fails (before rollback)
    #[serde(default)]
    pub on_failure: Option<String>,
    /// Maximum number of backup versions to keep (default: 3, min: 1)
    #[serde(default = "default_max_backups")]
    pub max_backups: usize,
    /// Multi-binary install map. When set, individual binaries are extracted
    /// from the archive and installed to their own target paths instead of
    /// extracting the whole archive to `target_dir`.
    #[serde(default)]
    pub binaries: Vec<BinaryEntry>,
}

fn default_max_backups() -> usize {
    3
}

fn default_failure_policy() -> String {
    "rollback".to_string()
}

/// Describes a single binary to extract from a release archive.
/// When `binaries` is set on `RecipeInstall`, each binary is installed
/// to its own target path instead of extracting the whole archive to `target_dir`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BinaryEntry {
    /// Filename within the archive (e.g., "bin/myapp" or "myapp")
    pub source: String,
    /// Absolute install path on the target system (e.g., "/usr/local/bin/myapp")
    pub target: String,
    /// Optional chmod string (e.g., "0755"). Defaults to "0755" if not set.
    #[serde(default)]
    pub mode: Option<String>,
}

/// Direct URL source configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecipeDirectUrl {
    /// How to discover versions: "scrape" or "url_pattern"
    pub version_discovery: String,
    /// URL for version scraping (if discovery = "scrape")
    #[serde(default)]
    pub scrape_url: Option<String>,
    /// Regex to extract version from scraped page
    #[serde(default)]
    pub scrape_pattern: Option<String>,
    /// Asset URL template with {version}, {arch}, {os} placeholders
    pub asset_url_template: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthCheck {
    #[serde(rename = "type")]
    pub check_type: HealthCheckType,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub port: Option<u16>,
    #[serde(default)]
    pub process: Option<String>,
    #[serde(default = "default_health_timeout_secs")]
    pub timeout_secs: u64,
    /// Post-update stability criterion. When present the unit must not only
    /// reach `active` but stay stable per `mode`. Absent = no extra stability
    /// requirement beyond reaching active within `timeout_secs`.
    #[serde(default)]
    pub stable: Option<StableCriterion>,
}

/// How the unit must remain stable after reaching `active`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StableMode {
    /// Stay `active` for `secs` (default 30s).
    Time,
    /// `NRestarts` must not exceed baseline + `count` within the window.
    Restarts,
    /// `journalctl -u <unit>` shows no new errors.
    LogQuiet,
}

/// Stability criterion for `[install.health_check.stable]`.
/// Default (when the table is present but fields omitted): `time:30`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StableCriterion {
    #[serde(default = "default_stable_mode")]
    pub mode: StableMode,
    /// Seconds to wait (mode=time/log_quiet). Default 30.
    #[serde(default = "default_stable_secs")]
    pub secs: Option<u64>,
    /// Max restarts allowed above baseline (mode=restarts).
    #[serde(default)]
    pub count: Option<u32>,
}

fn default_stable_mode() -> StableMode {
    StableMode::Time
}

fn default_stable_secs() -> Option<u64> {
    Some(30)
}

/// Optional provisional success-monitor window for `[install.success_monitor]`.
/// During `window_secs` the update is provisional; failure inside the window
/// triggers `failure_policy`, expiry records the update as final.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SuccessMonitor {
    pub window_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssetSelector {
    pub tag_pattern: String,
    #[serde(default)]
    pub rules: Vec<AssetRule>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssetRule {
    pub os: String,
    pub arch: String,
    #[serde(default)]
    pub libc: Option<String>,
    pub pattern: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecipeBuild {
    pub steps: Vec<String>,
    #[serde(default)]
    pub requires: Vec<String>,
    /// Allow network access during build step execution. Default (false)
    /// DENIES network: steps run under `unshare -n` where available, with
    /// proxy env-poisoning as fallback. Opt in explicitly per recipe with
    /// `allow_network = true` in the `[build]` table.
    #[serde(default)]
    pub allow_network: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecipeScript {
    pub update: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecipeVersion {
    pub detect_command: String,
    pub detect_pattern: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecipeNotifications {
    #[serde(default)]
    pub changelog_url: Option<String>,
    #[serde(default)]
    pub notes: String,
}

/// A validated build recipe (parsed from TOML)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShadowRecipe {
    pub package: RecipePackage,
    pub install: RecipeInstall,
    #[serde(default)]
    pub asset_selector: Option<AssetSelector>,
    #[serde(default)]
    pub build: Option<RecipeBuild>,
    #[serde(default)]
    pub script: Option<RecipeScript>,
    pub version: RecipeVersion,
    #[serde(default)]
    pub notifications: Option<RecipeNotifications>,
    #[serde(default)]
    pub direct_url: Option<RecipeDirectUrl>,
    /// Set when `failure_policy = "hold"`: catalog/adopt surfaces warn before
    /// offering updates because downgrade is unsafe.
    #[serde(default)]
    pub downgrade_unsafe: bool,
}

/// A release asset from upstream
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReleaseAsset {
    pub name: String,
    pub url: String,
    pub size: u64,
    #[serde(default)]
    pub sha256: Option<String>,
}

/// A signature over a recipe's canonical TOML representation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecipeSignature {
    pub key_id: String,
    pub signature: String,
    pub signed_at: chrono::DateTime<chrono::Utc>,
}

/// Severity of an update annotation
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnnotationSeverity {
    Info,
    Warning,
    Breaking,
}

/// An update annotation published by an operator to warn about package transitions.
///
/// Operators sign annotations with their Ed25519 operator key so consumers
/// can verify provenance before acting on the information.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateAnnotation {
    pub package: String,
    pub from_version: String,
    pub to_version: String,
    pub severity: AnnotationSeverity,
    pub message: String,
    #[serde(default)]
    pub url: Option<String>,
    pub author: String,
    pub timestamp: i64,
    pub signature: String,
}

/// A recipe with its signature(s)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignedRecipe {
    pub recipe_toml: String,
    pub recipe: ShadowRecipe,
    pub operator_signature: RecipeSignature,
    #[serde(default)]
    pub author_signature: Option<RecipeSignature>,
}

impl AssetSelector {
    /// Find the first rule matching (os, arch, libc) whose pattern matches an asset name.
    pub fn match_asset(
        &self,
        os: &str,
        arch: &str,
        libc: Option<&str>,
        asset_names: &[String],
    ) -> Option<String> {
        for rule in &self.rules {
            if rule.os != os || rule.arch != arch {
                continue;
            }
            if let Some(rule_libc) = &rule.libc {
                if libc != Some(rule_libc.as_str()) {
                    continue;
                }
            }
            let pat = Regex::new(&rule.pattern).ok()?;
            for name in asset_names {
                if pat.is_match(name) {
                    return Some(name.clone());
                }
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sonarr_toml() -> String {
        [
            "[package]",
            "name = \"sonarr\"",
            "description = \"Sonarr PVR\"",
            "homepage = \"https://sonarr.tv\"",
            "source_type = \"github_release\"",
            "source_url = \"https://github.com/Sonarr/Sonarr\"",
            "",
            "[install]",
            "method = \"extract\"",
            "target_dir = \"/var/lib/sonarr\"",
            "chmod = [\"Sonarr\", \"Sonarr.Update\"]",
            "service = \"sonarr\"",
            "config_paths = [\"/var/lib/sonarr/config.xml\"]",
            "backup_old = true",
            "",
            "[install.health_check]",
            "type = \"http\"",
            "url = \"http://localhost:8989/sonarr/api/v3/system/status\"",
            "timeout_secs = 60",
            "",
            "[asset_selector]",
            "tag_pattern = \"^v?(\\\\d+\\\\.\\\\d+\\\\.\\\\d+\\\\.\\\\d+)$\"",
            "",
            "[[asset_selector.rules]]",
            "os = \"linux\"",
            "arch = \"x86_64\"",
            "libc = \"glibc\"",
            "pattern = \"Sonarr\\\\.main\\\\.linux-amd64\\\\.tar\\\\.gz$\"",
            "",
            "[[asset_selector.rules]]",
            "os = \"linux\"",
            "arch = \"aarch64\"",
            "libc = \"musl\"",
            "pattern = \"Sonarr\\\\.main\\\\.linux-musl-arm64\\\\.tar\\\\.gz$\"",
            "",
            "[version]",
            "detect_command = \"/var/lib/sonarr/Sonarr --version\"",
            "detect_pattern = \"(\\\\d+\\\\.\\\\d+\\\\.\\\\d+\\\\.\\\\d+)\"",
            "",
            "[notifications]",
            "changelog_url = \"https://github.com/Sonarr/Sonarr/compare/{old}...{new}\"",
            "notes = \"\"",
        ]
        .join("\n")
    }

    #[test]
    fn deserialize_full_recipe() {
        let recipe: ShadowRecipe = toml::from_str(&sonarr_toml()).unwrap();
        assert_eq!(recipe.package.name, "sonarr");
        assert_eq!(recipe.package.source_type, ShadowSource::GithubRelease);
        assert_eq!(recipe.install.method, InstallMethod::Extract);
        assert_eq!(recipe.install.target_dir, "/var/lib/sonarr");
        assert!(recipe.install.backup_old);
        let sel = recipe.asset_selector.unwrap();
        assert_eq!(sel.rules.len(), 2);
        assert_eq!(recipe.install.health_check.unwrap().timeout_secs, 60);
        assert_eq!(recipe.install.chmod.unwrap().len(), 2);
    }

    #[test]
    fn deserialize_minimal_recipe() {
        let toml = [
            "[package]",
            "name = \"my-tool\"",
            "source_type = \"github_release\"",
            "source_url = \"https://github.com/example/tool\"",
            "",
            "[install]",
            "method = \"extract\"",
            "target_dir = \"/usr/local/bin\"",
            "",
            "[version]",
            "detect_command = \"my-tool --version\"",
            "detect_pattern = \"([0-9]+\\\\.[0-9]+\\\\.[0-9]+)\"",
        ]
        .join("\n");
        let recipe: ShadowRecipe = toml::from_str(&toml).unwrap();
        assert_eq!(recipe.package.name, "my-tool");
        assert!(recipe.package.description.is_none());
        assert!(!recipe.install.backup_old);
        assert!(recipe.install.chmod.is_none());
        assert!(recipe.asset_selector.is_none());
        assert!(recipe.notifications.is_none());
    }

    #[test]
    fn serialize_round_trip() {
        let recipe: ShadowRecipe = toml::from_str(&sonarr_toml()).unwrap();
        let reserialized = toml::to_string_pretty(&recipe).unwrap();
        let recipe2: ShadowRecipe = toml::from_str(&reserialized).unwrap();
        assert_eq!(recipe.package.name, recipe2.package.name);
        assert_eq!(recipe.package.source_type, recipe2.package.source_type);
        assert_eq!(recipe.install.method, recipe2.install.method);
    }

    #[test]
    fn asset_selector_match_linux_glibc() {
        let recipe: ShadowRecipe = toml::from_str(&sonarr_toml()).unwrap();
        let sel = recipe.asset_selector.unwrap();
        let assets: Vec<String> = vec![
            "Sonarr.main.linux-amd64.tar.gz".into(),
            "Sonarr.main.linux-arm64.tar.gz".into(),
            "Sonarr.main.macos.tar.gz".into(),
        ];
        let matched = sel.match_asset("linux", "x86_64", Some("glibc"), &assets);
        assert_eq!(matched, Some("Sonarr.main.linux-amd64.tar.gz".to_string()));
    }

    #[test]
    fn asset_selector_match_linux_musl_aarch64() {
        let recipe: ShadowRecipe = toml::from_str(&sonarr_toml()).unwrap();
        let sel = recipe.asset_selector.unwrap();
        let assets: Vec<String> = vec![
            "Sonarr.main.linux-amd64.tar.gz".into(),
            "Sonarr.main.linux-musl-arm64.tar.gz".into(),
        ];
        let matched = sel.match_asset("linux", "aarch64", Some("musl"), &assets);
        assert_eq!(
            matched,
            Some("Sonarr.main.linux-musl-arm64.tar.gz".to_string())
        );
    }

    #[test]
    fn asset_selector_no_match() {
        let recipe: ShadowRecipe = toml::from_str(&sonarr_toml()).unwrap();
        let sel = recipe.asset_selector.unwrap();
        let assets: Vec<String> = vec!["Sonarr.main.macos.tar.gz".into()];
        let matched = sel.match_asset("linux", "x86_64", Some("glibc"), &assets);
        assert!(matched.is_none());
    }

    #[test]
    fn health_check_default_timeout() {
        let hc = HealthCheck {
            check_type: HealthCheckType::Port,
            url: None,
            port: Some(8080),
            process: None,
            timeout_secs: default_health_timeout_secs(),
            stable: None,
        };
        assert_eq!(hc.timeout_secs, 30);
    }

    #[test]
    fn update_status_round_trip() {
        for status in [
            UpdateStatus::Pending,
            UpdateStatus::Applied,
            UpdateStatus::RolledBack,
            UpdateStatus::Failed,
        ] {
            let json = serde_json::to_string(&status).unwrap();
            let back: UpdateStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(status, back);
        }
    }

    #[test]
    fn annotation_severity_round_trip() {
        for sev in [
            AnnotationSeverity::Info,
            AnnotationSeverity::Warning,
            AnnotationSeverity::Breaking,
        ] {
            let json = serde_json::to_string(&sev).unwrap();
            let back: AnnotationSeverity = serde_json::from_str(&json).unwrap();
            assert_eq!(sev, back);
        }
    }

    #[test]
    fn update_annotation_serialize_round_trip() {
        let ann = UpdateAnnotation {
            package: "sonarr".to_string(),
            from_version: "4.0.6".to_string(),
            to_version: "4.0.7".to_string(),
            severity: AnnotationSeverity::Warning,
            message: "Database migration required".to_string(),
            url: Some("https://example.com/changelog".to_string()),
            author: "op123".to_string(),
            timestamp: 1700000000,
            signature: "base64sig".to_string(),
        };
        let json = serde_json::to_string(&ann).unwrap();
        let back: UpdateAnnotation = serde_json::from_str(&json).unwrap();
        assert_eq!(ann.package, back.package);
        assert_eq!(ann.severity, back.severity);
        assert_eq!(ann.url, back.url);
    }

    #[test]
    fn update_annotation_minimal() {
        let ann = UpdateAnnotation {
            package: "mytool".to_string(),
            from_version: "1.0".to_string(),
            to_version: "2.0".to_string(),
            severity: AnnotationSeverity::Info,
            message: "Minor update".to_string(),
            url: None,
            author: "op456".to_string(),
            timestamp: 1700000000,
            signature: "sig".to_string(),
        };
        let json = serde_json::to_string(&ann).unwrap();
        let back: UpdateAnnotation = serde_json::from_str(&json).unwrap();
        assert!(back.url.is_none());
    }

    #[test]
    fn binary_entry_deserialize() {
        let toml = r#"
            source = "bin/myapp"
            target = "/usr/local/bin/myapp"
            mode = "0755"
        "#;
        let entry: BinaryEntry = toml::from_str(toml).unwrap();
        assert_eq!(entry.source, "bin/myapp");
        assert_eq!(entry.target, "/usr/local/bin/myapp");
        assert_eq!(entry.mode.as_deref(), Some("0755"));
    }

    #[test]
    fn binary_entry_minimal() {
        let toml = r#"
            source = "myapp"
            target = "/usr/local/bin/myapp"
        "#;
        let entry: BinaryEntry = toml::from_str(toml).unwrap();
        assert_eq!(entry.source, "myapp");
        assert!(entry.mode.is_none());
    }

    #[test]
    fn recipe_with_binaries() {
        let toml = [
            "[package]",
            "name = \"mytool\"",
            "source_type = \"github_release\"",
            "source_url = \"https://github.com/example/tool\"",
            "",
            "[install]",
            "method = \"extract\"",
            "target_dir = \"/opt/mytool\"",
            "",
            "[[install.binaries]]",
            "source = \"myapp\"",
            "target = \"/usr/local/bin/myapp\"",
            "",
            "[[install.binaries]]",
            "source = \"myapp-helper\"",
            "target = \"/usr/local/bin/myapp-helper\"",
            "mode = \"0700\"",
            "",
            "[version]",
            "detect_command = \"myapp --version\"",
            r#"detect_pattern = "([0-9]+\\.[0-9]+\\.[0-9]+)""#,
        ]
        .join("\n");
        let recipe: ShadowRecipe = toml::from_str(&toml).unwrap();
        assert_eq!(recipe.install.binaries.len(), 2);
        assert_eq!(recipe.install.binaries[0].source, "myapp");
        assert_eq!(recipe.install.binaries[0].target, "/usr/local/bin/myapp");
        assert!(recipe.install.binaries[0].mode.is_none());
        assert_eq!(recipe.install.binaries[1].mode.as_deref(), Some("0700"));
    }

    #[test]
    fn recipe_without_binaries_is_empty() {
        let toml = [
            "[package]",
            "name = \"simple\"",
            "source_type = \"direct_url\"",
            "source_url = \"https://example.com/simple.tar.gz\"",
            "",
            "[install]",
            "method = \"extract\"",
            "target_dir = \"/opt/simple\"",
            "",
            "[version]",
            "detect_command = \"simple --version\"",
            "detect_pattern = \"([0-9]+)\"",
        ]
        .join("\n");
        let recipe: ShadowRecipe = toml::from_str(&toml).unwrap();
        assert!(recipe.install.binaries.is_empty());
    }

    #[test]
    fn failure_policy_defaults_to_rollback() {
        let toml = "method = \"extract\"\ntarget_dir = \"/x\"\n";
        let inst: RecipeInstall = toml::from_str(toml).unwrap();
        assert_eq!(inst.failure_policy, "rollback");
        assert!(inst.success_monitor.is_none());
    }

    #[test]
    fn failure_policy_hold_and_success_monitor_parse() {
        let toml = "method = \"extract\"\ntarget_dir = \"/x\"\nfailure_policy = \"hold\"\n\n[success_monitor]\nwindow_secs = 60\n";
        let inst: RecipeInstall = toml::from_str(toml).unwrap();
        assert_eq!(inst.failure_policy, "hold");
        assert_eq!(inst.success_monitor.unwrap().window_secs, 60);
    }

    #[test]
    fn stable_criterion_defaults_to_time_30() {
        let toml = "method = \"extract\"\ntarget_dir = \"/x\"\n\n[health_check]\ntype = \"port\"\nport = 8080\n\n[health_check.stable]\n";
        let inst: RecipeInstall = toml::from_str(toml).unwrap();
        let st = inst.health_check.unwrap().stable.unwrap();
        assert_eq!(st.mode, StableMode::Time);
        assert_eq!(st.secs, Some(30));
        assert_eq!(st.count, None);
    }

    #[test]
    fn stable_criterion_restarts_mode() {
        let toml = "method = \"extract\"\ntarget_dir = \"/x\"\n\n[health_check]\ntype = \"port\"\nport = 8080\n\n[health_check.stable]\nmode = \"restarts\"\ncount = 3\n";
        let inst: RecipeInstall = toml::from_str(toml).unwrap();
        let st = inst.health_check.unwrap().stable.unwrap();
        assert_eq!(st.mode, StableMode::Restarts);
        assert_eq!(st.count, Some(3));
        assert_eq!(st.secs, Some(30)); // default secs still populated
    }

    #[test]
    fn downgrade_unsafe_defaults_false() {
        let toml = "[package]\nname = \"x\"\nsource_type = \"direct_url\"\nsource_url = \"u\"\n\n[install]\nmethod = \"extract\"\ntarget_dir = \"/x\"\n\n[version]\ndetect_command = \"x\"\ndetect_pattern = \"([0-9]+)\"\n";
        let recipe: ShadowRecipe = toml::from_str(toml).unwrap();
        assert!(!recipe.downgrade_unsafe);
    }
}
