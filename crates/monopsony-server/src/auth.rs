//! HTTP proxy authentication middleware
//!
//! Supports three modes:
//! - Open: no auth (default, LAN trust)
//! - Token: API key via X-Monopsony-Key header
//! - mTLS: mutual TLS (placeholder for future sprint)

use axum::{
    body::Body,
    http::{Request, Response, StatusCode},
    middleware::Next,
    response::IntoResponse,
};
use monopsony_common::config::AuthMode;
use std::sync::Arc;
use tracing::{debug, warn};

use crate::db::Database;

/// Shared state for auth middleware
pub struct AuthState {
    pub auth_mode: AuthMode,
    pub db: Option<Arc<Database>>,
}

/// Auth middleware — branches on auth_mode
pub async fn auth_middleware(
    axum::extract::State(state): axum::extract::State<Arc<AuthState>>,
    req: Request<Body>,
    next: Next,
) -> impl IntoResponse {
    // Health check bypasses auth
    if req.uri().path() == "/health" {
        return next.run(req).await;
    }

    match state.auth_mode {
        AuthMode::Open => next.run(req).await,
        AuthMode::Token => token_auth(state, req, next).await,
        AuthMode::Mtls => {
            // Sprint 6 placeholder
            Response::builder()
                .status(StatusCode::NOT_IMPLEMENTED)
                .body(Body::from("mTLS auth mode not yet implemented"))
                .unwrap_or_else(|_| Response::new(Body::from("501 Not Implemented")))
        }
    }
}

/// Token auth: check X-Monopsony-Key header against stored hashes
async fn token_auth(
    state: Arc<AuthState>,
    req: Request<Body>,
    next: Next,
) -> Response<Body> {
    let key = req
        .headers()
        .get("X-Monopsony-Key")
        .and_then(|v| v.to_str().ok());

    match key {
        Some(key_value) => {
            let hash = sha256_hex(key_value);
            if let Some(db) = &state.db {
                match db.validate_api_key(&hash).await {
                    Ok(Some(key_id)) => {
                        debug!(key_id = %key_id, "API key authenticated");
                        next.run(req).await
                    }
                    Ok(None) => {
                        warn!("API key not found or invalid");
                        unauthorized_response()
                    }
                    Err(e) => {
                        warn!("API key validation error: {}", e);
                        unauthorized_response()
                    }
                }
            } else {
                warn!("Token auth enabled but no database configured");
                unauthorized_response()
            }
        }
        None => {
            debug!("Missing X-Monopsony-Key header");
            unauthorized_response()
        }
    }
}

/// Build a 401 Unauthorized response
fn unauthorized_response() -> Response<Body> {
    Response::builder()
        .status(StatusCode::UNAUTHORIZED)
        .header("WWW-Authenticate", "X-Monopsony-Key")
        .body(Body::from("Unauthorized: missing or invalid API key"))
        .unwrap_or_else(|_| Response::new(Body::from("401 Unauthorized")))
}

/// SHA-256 hex digest
fn sha256_hex(input: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    hex::encode(hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sha256_hex_produces_consistent_hash() {
        let h1 = sha256_hex("test-key");
        let h2 = sha256_hex("test-key");
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 64); // SHA-256 hex = 64 chars
    }

    #[test]
    fn test_sha256_hex_different_inputs() {
        let h1 = sha256_hex("key-one");
        let h2 = sha256_hex("key-two");
        assert_ne!(h1, h2);
    }

    #[test]
    fn test_auth_mode_default_is_open() {
        assert_eq!(AuthMode::default(), AuthMode::Open);
    }

    #[test]
    fn test_auth_mode_serde() {
        // Test via a TOML table with auth_mode field
        #[derive(serde::Deserialize)]
        struct TestConfig {
            #[serde(default)]
            auth_mode: AuthMode,
        }

        let open: TestConfig = toml::from_str("auth_mode = \"open\"").unwrap();
        assert_eq!(open.auth_mode, AuthMode::Open);

        let token: TestConfig = toml::from_str("auth_mode = \"token\"").unwrap();
        assert_eq!(token.auth_mode, AuthMode::Token);

        let mtls: TestConfig = toml::from_str("auth_mode = \"mtls\"").unwrap();
        assert_eq!(mtls.auth_mode, AuthMode::Mtls);

        // Missing field defaults to Open
        let default: TestConfig = toml::from_str("").unwrap();
        assert_eq!(default.auth_mode, AuthMode::Open);
    }

    #[test]
    fn test_auth_mode_display() {
        assert_eq!(format!("{}", AuthMode::Open), "open");
        assert_eq!(format!("{}", AuthMode::Token), "token");
        assert_eq!(format!("{}", AuthMode::Mtls), "mtls");
    }
}
