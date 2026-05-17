//! CLI subcommands for shadow package management.

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
    /// Register a new shadow package
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
    Edit {
        name: String,
    },
    /// Validate a recipe file
    Validate {
        path: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn package_command_clap_parse() {
        // Verify the command structure is valid clap
        let _ = PackageCommand::augment_subcommands(
            clap::Command::new("test"),
        );
    }

    #[test]
    fn recipe_command_clap_parse() {
        let _ = RecipeCommand::augment_subcommands(
            clap::Command::new("test"),
        );
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
