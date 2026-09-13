use monopsony_common::shadow_types::*;
use std::path::Path;

#[derive(Debug)]
pub enum RecipeError {
    InvalidToml {
        source: toml::de::Error,
    },
    Io(std::io::Error),
    InvalidRegex {
        field: String,
        pattern: String,
        error: String,
    },
    AssetPatternNoMatch {
        pattern: String,
        available: Vec<String>,
    },
    VersionCommandFailed {
        command: String,
        exit_code: i32,
        output: String,
    },
    PathNotAbsolute {
        path: String,
    },
    PathNotWritable {
        path: String,
    },
    ServiceNotFound {
        name: String,
    },
    MissingField {
        field: String,
    },
}

impl std::fmt::Display for RecipeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidToml { source } => write!(f, "invalid TOML: {source}"),
            Self::Io(e) => write!(f, "IO error: {e}"),
            Self::InvalidRegex {
                field,
                pattern,
                error,
            } => {
                write!(f, "invalid regex in '{field}' pattern '{pattern}': {error}")
            }
            Self::AssetPatternNoMatch { pattern, available } => {
                write!(f, "asset pattern '{pattern}' matches none of {available:?}")
            }
            Self::VersionCommandFailed {
                command,
                exit_code,
                output,
            } => {
                write!(
                    f,
                    "version command '{command}' failed (exit {exit_code}): {output}"
                )
            }
            Self::PathNotAbsolute { path } => write!(f, "path not absolute: {path}"),
            Self::PathNotWritable { path } => write!(f, "path not writable: {path}"),
            Self::ServiceNotFound { name } => write!(f, "service not found: {name}"),
            Self::MissingField { field } => write!(f, "missing required field: {field}"),
        }
    }
}

/// Parse a recipe TOML file from disk
pub fn parse_recipe(path: &Path) -> Result<ShadowRecipe, RecipeError> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| RecipeError::Io(std::io::Error::other(e.to_string())))?;
    parse_recipe_str(&content)
}

/// Parse a recipe from a TOML string
pub fn parse_recipe_str(toml: &str) -> Result<ShadowRecipe, RecipeError> {
    toml::from_str(toml).map_err(|source| RecipeError::InvalidToml { source })
}

/// Quick validation (no network calls):
/// - asset patterns are valid regex
/// - version detect_pattern is valid regex
/// - target_dir is an absolute path
/// - tag_pattern is valid regex
pub fn validate_recipe_quick(recipe: &ShadowRecipe) -> Result<(), Vec<RecipeError>> {
    let mut errors = Vec::new();

    // target_dir must be absolute or tilde-relative (`~/...`).
    // Tilde paths are per-user install targets (e.g. ~/.local/lib/<pkg>);
    // they expand against the invoking user's home at execution time.
    if !recipe.install.target_dir.starts_with('/') && !recipe.install.target_dir.starts_with("~/") {
        errors.push(RecipeError::PathNotAbsolute {
            path: recipe.install.target_dir.clone(),
        });
    }

    // version detect_pattern must be valid regex
    if let Err(e) = regex::Regex::new(&recipe.version.detect_pattern) {
        errors.push(RecipeError::InvalidRegex {
            field: "version.detect_pattern".into(),
            pattern: recipe.version.detect_pattern.clone(),
            error: e.to_string(),
        });
    }

    // asset selector validation
    if let Some(sel) = &recipe.asset_selector {
        if let Err(e) = regex::Regex::new(&sel.tag_pattern) {
            errors.push(RecipeError::InvalidRegex {
                field: "asset_selector.tag_pattern".into(),
                pattern: sel.tag_pattern.clone(),
                error: e.to_string(),
            });
        }
        for (i, rule) in sel.rules.iter().enumerate() {
            if let Err(e) = regex::Regex::new(&rule.pattern) {
                errors.push(RecipeError::InvalidRegex {
                    field: format!("asset_selector.rules[{i}].pattern"),
                    pattern: rule.pattern.clone(),
                    error: e.to_string(),
                });
            }
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

/// Generate a pre-filled recipe template for interactive registration
pub fn generate_registration_template(
    name: &str,
    source_url: &str,
    source_type: ShadowSource,
    detected_arch: &str,
    detected_version: Option<&str>,
    available_assets: &[String],
) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "[package]\n\
         name = \"{name}\"\n\
         source_type = \"{source_type_snake}\"\n\
         source_url = \"{source_url}\"\n\n",
        source_type_snake = match source_type {
            ShadowSource::GithubRelease => "github_release",
            ShadowSource::GitBuild => "git_build",
            ShadowSource::DirectUrl => "direct_url",
        }
    ));

    out.push_str(&format!("# Detected architecture: {detected_arch}\n"));
    if let Some(v) = detected_version {
        out.push_str(&format!("# Detected version: {v}\n"));
    }
    out.push('\n');

    if !available_assets.is_empty() {
        out.push_str("# Available assets:\n");
        for a in available_assets {
            out.push_str(&format!("#   - {a}\n"));
        }
        out.push('\n');
    }

    out.push_str(&format!(
        "[[asset_selector.rules]]\n\
         os = \"linux\"\n\
         arch = \"{detected_arch}\"\n\
         pattern = \"\"  # <-- fill this in\n\n\
         [install]\n\
         method = \"extract\"\n\
         target_dir = \"\"  # <-- fill this in\n\n\
         [version]\n\
         detect_command = \"\"  # <-- fill this in\n\
         detect_pattern = \"\"  # <-- fill this in\n"
    ));

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sonarr_toml() -> String {
        [
            "[package]",
            "name = \"sonarr\"",
            "description = \"Sonarr PVR\"",
            "source_type = \"github_release\"",
            "source_url = \"https://github.com/Sonarr/Sonarr\"",
            "",
            "[install]",
            "method = \"extract\"",
            "target_dir = \"/var/lib/sonarr\"",
            "backup_old = true",
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
            "[version]",
            "detect_command = \"/var/lib/sonarr/Sonarr --version\"",
            "detect_pattern = \"(\\\\d+\\\\.\\\\d+\\\\.\\\\d+\\\\.\\\\d+)\"",
        ]
        .join("\n")
    }

    #[test]
    fn parse_valid_recipe() {
        let recipe = parse_recipe_str(&sonarr_toml()).unwrap();
        assert_eq!(recipe.package.name, "sonarr");
        assert_eq!(recipe.package.source_type, ShadowSource::GithubRelease);
    }

    #[test]
    fn parse_invalid_toml() {
        let result = parse_recipe_str("not valid toml {{{");
        assert!(matches!(result, Err(RecipeError::InvalidToml { .. })));
    }

    #[test]
    fn validate_quick_ok() {
        let recipe = parse_recipe_str(&sonarr_toml()).unwrap();
        assert!(validate_recipe_quick(&recipe).is_ok());
    }

    #[test]
    fn validate_quick_bad_regex() {
        let toml = [
            "[package]",
            "name = \"bad\"",
            "source_type = \"github_release\"",
            "source_url = \"https://github.com/example/tool\"",
            "",
            "[install]",
            "method = \"extract\"",
            "target_dir = \"/usr/local/bin\"",
            "",
            "[version]",
            "detect_command = \"tool --version\"",
            "detect_pattern = \"([invalid\"",
        ]
        .join("\n");
        let recipe = parse_recipe_str(&toml).unwrap();
        let errors = validate_recipe_quick(&recipe).unwrap_err();
        assert!(errors.iter().any(|e| matches!(e, RecipeError::InvalidRegex { field, .. } if field == "version.detect_pattern")));
    }

    #[test]
    fn validate_quick_relative_path() {
        let toml = [
            "[package]",
            "name = \"bad\"",
            "source_type = \"github_release\"",
            "source_url = \"https://github.com/example/tool\"",
            "",
            "[install]",
            "method = \"extract\"",
            "target_dir = \"relative/path\"",
            "",
            "[version]",
            "detect_command = \"tool --version\"",
            "detect_pattern = \"([0-9]+)\"",
        ]
        .join("\n");
        let recipe = parse_recipe_str(&toml).unwrap();
        let errors = validate_recipe_quick(&recipe).unwrap_err();
        assert!(errors
            .iter()
            .any(|e| matches!(e, RecipeError::PathNotAbsolute { .. })));
    }

    #[test]
    fn template_generation() {
        let tmpl = generate_registration_template(
            "sonarr",
            "https://github.com/Sonarr/Sonarr",
            ShadowSource::GithubRelease,
            "x86_64",
            Some("4.0.6.1805"),
            &[
                "Sonarr.main.linux-amd64.tar.gz".into(),
                "Sonarr.main.macos.tar.gz".into(),
            ],
        );
        assert!(tmpl.contains("sonarr"));
        assert!(tmpl.contains("github_release"));
        assert!(tmpl.contains("x86_64"));
        assert!(tmpl.contains("4.0.6.1805"));
        assert!(tmpl.contains("Sonarr.main.linux-amd64.tar.gz"));
        assert!(tmpl.contains("fill this in"));
    }
}
