//! CLI subcommands for shadow package management.
#![allow(dead_code)]

use clap::Subcommand;
use monopsony_common::shadow_types::ShadowSource;

/// Auto-detect source type from URL pattern.
pub fn detect_source_type(url: &str) -> ShadowSource {
    let url_lower = url.to_lowercase();
    if url_lower.contains("github.com") && !url_lower.ends_with(".git") {
        ShadowSource::GithubRelease
    } else if url_lower.ends_with(".git") {
        ShadowSource::GitBuild
    } else {
        ShadowSource::DirectUrl
    }
}

#[derive(Subcommand, Debug)]
pub enum PackageCommand {
    /// Register a new shadow package from a local recipe
    Register {
        /// Path to the installed binary or directory
        path: String,
        /// Source URL (GitHub repo, git repo, or direct URL)
        #[arg(long)]
        source: String,
        /// Override package name
        #[arg(long)]
        name: Option<String>,
        /// Use a pre-written recipe file instead of auto-detection
        #[arg(long)]
        recipe: Option<String>,
        /// Install method: extract, build, or script
        #[arg(long, default_value = "extract")]
        method: String,
        /// Git branch for HEAD tracking (git_build only)
        #[arg(long, default_value = "main")]
        branch: String,
        /// Build dependencies, comma-separated (git_build only)
        #[arg(long)]
        requires: Option<String>,
    },

    /// Search the tracker and register a package from a federated recipe
    Add {
        /// Package name to search for on the tracker
        name: String,
        /// Tracker base URL
        #[arg(long)]
        tracker: Option<String>,
        /// Non-interactive mode: auto-select highest-scored variant
        #[arg(long)]
        non_interactive: bool,
    },

    /// Remove a shadow package registration
    Unregister {
        /// Package name or ID
        name: String,
    },

    /// List all registered shadow packages
    List,

    /// Show detailed info for a shadow package
    Inspect {
        /// Package name or ID
        name: String,
    },

    /// Check for available updates
    Check {
        /// Check a specific package (omit for all)
        name: Option<String>,
    },

    /// Apply an available update
    Update {
        /// Package name or ID
        name: String,
        /// Show what would happen without making changes
        #[arg(long)]
        dry_run: bool,
        /// Download and stage, but don't restart the service
        #[arg(long)]
        no_restart: bool,
    },

    /// Rollback to the previous version
    Rollback {
        /// Package name or ID
        name: String,
    },

    /// Manage build recipes
    Recipe {
        #[command(subcommand)]
        command: RecipeCommand,
    },
}

#[derive(Subcommand, Debug)]
pub enum RecipeCommand {
    /// Open recipe in $EDITOR
    Edit { name: String },
    /// Validate a recipe file
    Validate { path: String },
}

/// Summary card for a recipe variant, used during interactive selection.
#[derive(Debug, Clone)]
pub struct RecipeVariant {
    pub id: String,
    pub name: String,
    pub author_key_id: String,
    pub operator_key_id: Option<String>,
    pub source_type: String,
    pub version: Option<String>,
    pub score: f64,
    pub install_count: i64,
    pub report_count: i64,
    pub quarantined: bool,
}

/// Display a numbered list of recipe variants and return the selected index.
///
/// In non-interactive mode, returns the highest-scored non-quarantined variant.
/// Returns `None` if no valid selection can be made.
pub fn select_recipe_variant(
    variants: &[RecipeVariant],
    non_interactive: bool,
    input: Option<&str>,
) -> Option<usize> {
    let valid: Vec<(usize, &RecipeVariant)> = variants
        .iter()
        .enumerate()
        .filter(|(_, v)| !v.quarantined)
        .collect();

    if valid.is_empty() {
        return None;
    }

    if non_interactive {
        // Select highest-scored variant
        return valid
            .into_iter()
            .max_by(|a, b| {
                a.1.score
                    .partial_cmp(&b.1.score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .map(|(i, _)| i);
    }

    // Parse user input (1-indexed)
    let choice = input?.trim();
    if choice == "q" || choice == "quit" {
        return None;
    }
    let idx: usize = choice.strip_prefix('[').unwrap_or(choice).parse().ok()?;
    if idx == 0 || idx > variants.len() {
        return None;
    }
    let selected = &variants[idx - 1];
    if selected.quarantined {
        return None;
    }
    Some(idx - 1)
}

/// Format a recipe variant for display.
pub fn format_variant(index: usize, v: &RecipeVariant) -> String {
    let quarantine_tag = if v.quarantined { " [QUARANTINED]" } else { "" };
    let op_tag = v.operator_key_id.as_deref().unwrap_or("none");
    format!(
        "  [{}] {}{}{}\n       Author:  {}\n       Operator: {}\n       Version: {}\n       Score:   {:.1} ({} installs, {} reports)",
        index,
        v.name,
        quarantine_tag,
        "",
        v.author_key_id,
        op_tag,
        v.version.as_deref().unwrap_or("unknown"),
        v.score,
        v.install_count,
        v.report_count,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn package_command_clap_parse() {
        // Verify the command structure is valid clap
        let _ = PackageCommand::augment_subcommands(clap::Command::new("test"));
    }

    #[test]
    fn recipe_command_clap_parse() {
        let _ = RecipeCommand::augment_subcommands(clap::Command::new("test"));
    }

    #[test]
    fn detect_github_release() {
        assert_eq!(
            detect_source_type("https://github.com/Sonarr/Sonarr"),
            ShadowSource::GithubRelease
        );
        assert_eq!(
            detect_source_type("https://github.com/Sonarr/Sonarr/releases"),
            ShadowSource::GithubRelease
        );
    }

    #[test]
    fn detect_git_build() {
        assert_eq!(
            detect_source_type("https://github.com/example/tool.git"),
            ShadowSource::GitBuild
        );
        assert_eq!(
            detect_source_type("https://gitlab.com/example/tool.git"),
            ShadowSource::GitBuild
        );
    }

    #[test]
    fn detect_direct_url() {
        assert_eq!(
            detect_source_type("https://pkgs.tailscale.com/stable"),
            ShadowSource::DirectUrl
        );
        assert_eq!(
            detect_source_type("https://example.com/downloads"),
            ShadowSource::DirectUrl
        );
    }
}

#[test]
fn select_variant_interactive() {
    let variants = vec![
        RecipeVariant {
            id: "h1".into(),
            name: "sonarr".into(),
            author_key_id: "auth1".into(),
            operator_key_id: Some("op1".into()),
            source_type: "github_release".into(),
            version: Some("4.0.7".into()),
            score: 47.0,
            install_count: 89,
            report_count: 0,
            quarantined: false,
        },
        RecipeVariant {
            id: "h2".into(),
            name: "sonarr-alpine".into(),
            author_key_id: "auth2".into(),
            operator_key_id: None,
            source_type: "github_release".into(),
            version: Some("4.0.7".into()),
            score: 12.0,
            install_count: 5,
            report_count: 0,
            quarantined: false,
        },
    ];
    // Select variant 2
    assert_eq!(select_recipe_variant(&variants, false, Some("2")), Some(1));
    // Select variant 1
    assert_eq!(select_recipe_variant(&variants, false, Some("1")), Some(0));
    // Quit
    assert_eq!(select_recipe_variant(&variants, false, Some("q")), None);
    // Invalid
    assert_eq!(select_recipe_variant(&variants, false, Some("99")), None);
}

#[test]
fn select_variant_non_interactive() {
    let variants = vec![
        RecipeVariant {
            id: "h1".into(),
            name: "sonarr".into(),
            author_key_id: "auth1".into(),
            operator_key_id: Some("op1".into()),
            source_type: "github_release".into(),
            version: Some("4.0.7".into()),
            score: 12.0,
            install_count: 5,
            report_count: 0,
            quarantined: false,
        },
        RecipeVariant {
            id: "h2".into(),
            name: "sonarr-high".into(),
            author_key_id: "auth2".into(),
            operator_key_id: None,
            source_type: "github_release".into(),
            version: Some("4.0.7".into()),
            score: 47.0,
            install_count: 89,
            report_count: 0,
            quarantined: false,
        },
    ];
    // Should select index 1 (score 47 > 12)
    assert_eq!(select_recipe_variant(&variants, true, None), Some(1));
}

#[test]
fn select_variant_skips_quarantined() {
    let variants = vec![
        RecipeVariant {
            id: "h1".into(),
            name: "bad-sonarr".into(),
            author_key_id: "evil".into(),
            operator_key_id: None,
            source_type: "github_release".into(),
            version: Some("4.0.7".into()),
            score: 100.0,
            install_count: 0,
            report_count: 5,
            quarantined: true,
        },
        RecipeVariant {
            id: "h2".into(),
            name: "good-sonarr".into(),
            author_key_id: "auth1".into(),
            operator_key_id: Some("op1".into()),
            source_type: "github_release".into(),
            version: Some("4.0.7".into()),
            score: 30.0,
            install_count: 50,
            report_count: 0,
            quarantined: false,
        },
    ];
    // Non-interactive should skip quarantined
    assert_eq!(select_recipe_variant(&variants, true, None), Some(1));
    // Interactive selecting quarantined returns None
    assert_eq!(select_recipe_variant(&variants, false, Some("1")), None);
    // Interactive selecting non-quarantined works
    assert_eq!(select_recipe_variant(&variants, false, Some("2")), Some(1));
}

#[test]
fn format_variant_display() {
    let v = RecipeVariant {
        id: "h1".into(),
        name: "sonarr".into(),
        author_key_id: "keypad".into(),
        operator_key_id: Some("societus".into()),
        source_type: "github_release".into(),
        version: Some("4.0.7".into()),
        score: 47.0,
        install_count: 89,
        report_count: 3,
        quarantined: false,
    };
    let out = format_variant(1, &v);
    assert!(out.contains("[1]"));
    assert!(out.contains("sonarr"));
    assert!(out.contains("keypad"));
    assert!(out.contains("47.0"));
}
