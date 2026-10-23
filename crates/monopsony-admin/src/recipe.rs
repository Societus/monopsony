//! Recipe validation and estimation endpoints.

use axum::{extract::State, response::IntoResponse, Json};
use serde::{Deserialize, Serialize};

use crate::AdminState;

// ─── Validation ───────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct ValidateRequest {
    pub recipe_toml: String,
}

#[derive(Debug, Serialize)]
pub struct ValidateResponse {
    pub valid: bool,
    pub errors: Vec<ValidationError>,
}

#[derive(Debug, Serialize)]
pub struct ValidationError {
    pub field: String,
    pub message: String,
}

/// POST /api/v1/shadow/recipe/validate — validate recipe TOML.
pub async fn handle_validate(Json(req): Json<ValidateRequest>) -> impl IntoResponse {
    let recipe: monopsony_common::shadow_types::ShadowRecipe =
        match toml::from_str(&req.recipe_toml) {
            Ok(r) => r,
            Err(e) => {
                return Json(ValidateResponse {
                    valid: false,
                    errors: vec![ValidationError {
                        field: "toml".into(),
                        message: e.to_string(),
                    }],
                });
            }
        };

    match monopsony_shadow::shadow_recipe::validate_recipe_quick(&recipe) {
        Ok(()) => Json(ValidateResponse {
            valid: true,
            errors: vec![],
        }),
        Err(errors) => {
            let errs: Vec<_> = errors
                .into_iter()
                .map(|e| {
                    let field = match &e {
                        monopsony_shadow::shadow_recipe::RecipeError::InvalidRegex {
                            field,
                            ..
                        } => field.clone(),
                        monopsony_shadow::shadow_recipe::RecipeError::PathNotAbsolute {
                            ..
                        } => "install.target_dir".into(),
                        monopsony_shadow::shadow_recipe::RecipeError::MissingField { field } => {
                            field.clone()
                        }
                        _ => "unknown".into(),
                    };
                    ValidationError {
                        field,
                        message: e.to_string(),
                    }
                })
                .collect();
            Json(ValidateResponse {
                valid: false,
                errors: errs,
            })
        }
    }
}

// ─── Estimation ───────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct EstimateRequest {
    pub binary_path: String,
    pub source_url: Option<String>,
    pub client_id: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct EstimateResponse {
    pub recipe_toml: String,
    pub detected: DetectedInfo,
    pub warnings: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct DetectedInfo {
    pub name: String,
    pub version: Option<String>,
    pub arch: String,
    pub source_type: String,
    pub available_assets: Vec<String>,
}

/// POST /api/v1/shadow/recipe/estimate — estimate a recipe from a binary path.
pub async fn handle_estimate(
    State(_state): State<AdminState>,
    Json(req): Json<EstimateRequest>,
) -> impl IntoResponse {
    let mut warnings = Vec::new();

    let name = std::path::Path::new(&req.binary_path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown")
        .to_string();

    let arch = match std::env::consts::ARCH {
        "x86_64" => "amd64".to_string(),
        "aarch64" => "arm64".to_string(),
        a => a.to_string(),
    };

    let version = match detect_version(&req.binary_path).await {
        Ok(v) => Some(v),
        Err(e) => {
            warnings.push(format!("Could not detect version: {}", e));
            None
        }
    };

    let (source_type, available_assets) = if let Some(ref url) = req.source_url {
        if url.contains("github.com") {
            match monopsony_shadow::shadow_source::GithubReleaseSource::new(url, None) {
                Ok(source) => {
                    // Use the UpdateSource trait to get latest version and assets
                    use monopsony_shadow::shadow_source::UpdateSource;
                    let mut assets = Vec::new();
                    // Try to get "latest" tag assets
                    if let Ok(release_assets) = source.release_assets("latest").await {
                        assets = release_assets.iter().map(|a| a.name.clone()).collect();
                    }
                    (
                        monopsony_common::shadow_types::ShadowSource::GithubRelease,
                        assets,
                    )
                }
                Err(e) => {
                    warnings.push(format!("GitHub parse failed: {}", e));
                    (
                        monopsony_common::shadow_types::ShadowSource::DirectUrl,
                        vec![],
                    )
                }
            }
        } else if url.starts_with("git://") || url.ends_with(".git") {
            (
                monopsony_common::shadow_types::ShadowSource::GitRelease,
                vec![],
            )
        } else {
            (
                monopsony_common::shadow_types::ShadowSource::DirectUrl,
                vec![],
            )
        }
    } else {
        warnings.push("No source URL provided, defaulting to direct_url".into());
        (
            monopsony_common::shadow_types::ShadowSource::DirectUrl,
            vec![],
        )
    };

    let source_type_str = match source_type {
        monopsony_common::shadow_types::ShadowSource::GithubRelease => "github_release",
        monopsony_common::shadow_types::ShadowSource::GitRelease => "git_release",
        monopsony_common::shadow_types::ShadowSource::DirectUrl => "direct_url",
    };

    let source_url = req.source_url.unwrap_or_default();
    let recipe_toml = monopsony_shadow::shadow_recipe::generate_registration_template(
        &name,
        &source_url,
        source_type,
        &arch,
        version.as_deref(),
        &available_assets,
    );

    Json(EstimateResponse {
        recipe_toml,
        detected: DetectedInfo {
            name,
            version,
            arch,
            source_type: source_type_str.into(),
            available_assets,
        },
        warnings,
    })
}

async fn detect_version(binary_path: &str) -> Result<String, String> {
    use tokio::process::Command;

    for flag in &["--version", "-v", "version"] {
        if let Ok(output) = Command::new(binary_path).arg(flag).output().await {
            if output.status.success() {
                let combined = format!(
                    "{} {}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
                if let Some(v) = extract_semver(&combined) {
                    return Ok(v);
                }
            }
        }
    }
    Err("No version output from binary".into())
}

fn extract_semver(text: &str) -> Option<String> {
    regex::Regex::new(r"\d+\.\d+\.\d+")
        .ok()?
        .find(text)
        .map(|m| m.as_str().to_string())
}
