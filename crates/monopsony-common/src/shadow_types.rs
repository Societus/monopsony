use regex::Regex;
use serde::{Deserialize, Serialize};

/// Source type for a shadow package
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShadowSource {
    GithubRelease,
    GitBuild,
    DirectUrl,
}

/// Install method defined in a recipe
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstallMethod {
    Extract,
    Build,
    Script,
}

/// Health check type
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
    /// Command to run before update starts
    #[serde(default)]
    pub pre_update: Option<String>,
    /// Command to run after successful update
    #[serde(default)]
    pub post_update: Option<String>,
    /// Command to run if update fails (before rollback)
    #[serde(default)]
    pub on_failure: Option<String>,
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
        assert_eq!(matched, Some("Sonarr.main.linux-musl-arm64.tar.gz".to_string()));
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
}
