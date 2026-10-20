//! Enrollment key management (pairing-v2 P5).
//!
//! Tailscale-auth-key pattern: the admin creates a key with a use-count and
//! optional expiry; the plaintext is returned ONCE at creation and only the
//! SHA-256 hash is stored. Clients present the plaintext at
//! `POST /api/v1/pair/enroll` to pair without interactive admin approval.

use axum::{
    extract::{Path, State},
    response::IntoResponse,
    Json,
};
use rand::Rng;
use sha2::{Digest, Sha256};

use crate::AdminState;

/// Prefix for enrollment keys — recognizable in configs and logs, and
/// grep-able so operators can find where they pasted one.
const ENROLLMENT_KEY_PREFIX: &str = "mopk-";
/// Random byte length of the plaintext key (32 bytes → 43-char base64url).
const ENROLLMENT_KEY_BYTES: usize = 32;

fn hash_key(plaintext: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(plaintext.as_bytes());
    hex::encode(hasher.finalize())
}

#[derive(Debug, serde::Deserialize)]
pub struct CreateEnrollmentKeyRequest {
    pub label: String,
    #[serde(default = "default_max_uses")]
    pub max_uses: i64,
    /// Hours until expiry; None = never expires.
    #[serde(default)]
    pub expires_in_hours: Option<i64>,
}

fn default_max_uses() -> i64 {
    1
}

/// POST /api/v1/enrollment-keys — create a key (auth). The plaintext key is
/// returned exactly once; only its hash is stored.
pub async fn handle_create_enrollment_key(
    State(state): State<AdminState>,
    Json(req): Json<CreateEnrollmentKeyRequest>,
) -> impl IntoResponse {
    let Some(ref db) = state.db else {
        return (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"error": "Database not available"})),
        )
            .into_response();
    };

    if req.label.trim().is_empty() {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "label is required"})),
        )
            .into_response();
    }
    if req.max_uses < 1 {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "max_uses must be >= 1"})),
        )
            .into_response();
    }

    // Generate the plaintext key: prefix + base64url of random bytes.
    let mut random_bytes = [0u8; ENROLLMENT_KEY_BYTES];
    rand::rngs::OsRng.fill(&mut random_bytes);
    use base64::Engine as _;
    let plaintext = format!(
        "{}{}",
        ENROLLMENT_KEY_PREFIX,
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(random_bytes)
    );

    let expires_at = req
        .expires_in_hours
        .map(|h| chrono::Utc::now() + chrono::Duration::hours(h));

    let key_id = match db
        .create_enrollment_key(&hash_key(&plaintext), &req.label, req.max_uses, expires_at)
        .await
    {
        Ok(id) => id,
        Err(e) => {
            return (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": e.to_string()})),
            )
                .into_response();
        }
    };

    (
        axum::http::StatusCode::CREATED,
        Json(serde_json::json!({
            "key_id": key_id,
            "key": plaintext, // shown ONCE — never recoverable
            "label": req.label,
            "max_uses": req.max_uses,
            "expires_at": expires_at.map(|t| t.timestamp()),
        })),
    )
        .into_response()
}

/// GET /api/v1/enrollment-keys — list all keys (auth, dashboard key manager).
pub async fn handle_list_enrollment_keys(State(state): State<AdminState>) -> impl IntoResponse {
    let Some(ref db) = state.db else {
        return (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"error": "Database not available"})),
        )
            .into_response();
    };

    match db.list_enrollment_keys().await {
        Ok(keys) => {
            let result: Vec<serde_json::Value> = keys
                .into_iter()
                .map(|k| {
                    serde_json::json!({
                        "key_id": k.id,
                        "key_hash_prefix": k.key_hash.chars().take(12).collect::<String>(),
                        "label": k.label,
                        "max_uses": k.max_uses,
                        "use_count": k.use_count,
                        "created_at": k.created_at,
                        "expires_at": k.expires_at,
                        "revoked_at": k.revoked_at,
                    })
                })
                .collect();
            Json(serde_json::Value::Array(result)).into_response()
        }
        Err(e) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

/// DELETE /api/v1/enrollment-keys/:id — revoke a key (auth).
pub async fn handle_revoke_enrollment_key(
    State(state): State<AdminState>,
    Path(key_id): Path<String>,
) -> impl IntoResponse {
    let Some(ref db) = state.db else {
        return (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"error": "Database not available"})),
        )
            .into_response();
    };

    let id = match uuid::Uuid::parse_str(&key_id) {
        Ok(id) => id,
        Err(e) => {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": format!("Invalid key id: {e}")})),
            )
                .into_response();
        }
    };

    match db.revoke_enrollment_key(&id).await {
        Ok(true) => (
            axum::http::StatusCode::OK,
            Json(serde_json::json!({"revoked": true, "key_id": id})),
        )
            .into_response(),
        Ok(false) => (
            axum::http::StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "key not found or already revoked"})),
        )
            .into_response(),
        Err(e) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}
