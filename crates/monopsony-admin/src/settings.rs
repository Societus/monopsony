//! Configuration read/write endpoints.

use axum::{extract::State, response::IntoResponse, Json};
use serde::Serialize;

use crate::AdminState;

/// GET /api/v1/config — read the full server configuration.
pub async fn handle_get_config(State(state): State<AdminState>) -> impl IntoResponse {
    match std::fs::read_to_string(&state.config_path) {
        Ok(content) => {
            Json(serde_json::json!({
                "path": state.config_path.display().to_string(),
                "toml": content,
            }))
        }
        Err(e) => Json(serde_json::json!({
            "error": format!("Could not read config: {}", e),
            "path": state.config_path.display().to_string(),
        })),
    }
}

/// POST /api/v1/config — update server configuration.
pub async fn handle_update_config(
    State(state): State<AdminState>,
    body: String,
) -> impl IntoResponse {
    let request: serde_json::Value = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(e) => return Json(serde_json::json!({"success": false, "message": format!("Invalid request: {}", e)})),
    };

    let toml_content = request.get("toml").and_then(|v| v.as_str()).unwrap_or("");

    if let Err(e) = toml::from_str::<toml::Value>(toml_content) {
        return Json(serde_json::json!({"success": false, "message": format!("Invalid TOML: {}", e)}));
    }

    match std::fs::write(&state.config_path, toml_content) {
        Ok(_) => Json(serde_json::json!({"success": true, "message": "Config updated. Restart server to apply."})),
        Err(e) => Json(serde_json::json!({"success": false, "message": format!("Write failed: {}", e)})),
    }
}

#[derive(Debug, Serialize)]
pub struct ScanConfigResponse {
    pub enabled: bool,
    pub clamd_socket: Option<String>,
    pub yara_rules_dir: Option<String>,
    pub entropy_threshold: Option<f64>,
}

/// GET /api/v1/config/scan — read scan configuration.
pub async fn handle_get_scan_config(State(_state): State<AdminState>) -> impl IntoResponse {
    Json(ScanConfigResponse {
        enabled: false,
        clamd_socket: None,
        yara_rules_dir: None,
        entropy_threshold: None,
    })
}

#[derive(Debug, Serialize)]
pub struct NodeConfigResponse {
    pub tier: String,
    pub name: Option<String>,
}

/// GET /api/v1/config/node — read node tier configuration.
pub async fn handle_get_node_config(State(_state): State<AdminState>) -> impl IntoResponse {
    Json(NodeConfigResponse { tier: "private".into(), name: None })
}
