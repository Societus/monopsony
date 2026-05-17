//! Tracker authentication middleware.
//!
//! Three modes:
//! - **Open**: no auth (default, for testing/LAN)
//! - **Token**: `X-Monopsony-Key` header checked against configured keys
//! - **Jwt**: Bearer JWT validated against trusted issuer keys

use std::sync::Arc;

use axum::{
    body::Body,
    extract::State,
    http::{Request, StatusCode},
    middleware::Next,
    response::IntoResponse,
};
use serde::{Deserialize, Serialize};

use crate::TrackerState;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrackerAuthMode {
    Open,
    Token,
    Jwt,
}

impl std::fmt::Display for TrackerAuthMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Open => write!(f, "open"),
            Self::Token => write!(f, "token"),
            Self::Jwt => write!(f, "jwt"),
        }
    }
}

impl std::str::FromStr for TrackerAuthMode {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "open" => Ok(Self::Open),
            "token" => Ok(Self::Token),
            "jwt" => Ok(Self::Jwt),
            other => Err(format!(
                "invalid auth_mode '{}'. Expected: open, token, jwt",
                other
            )),
        }
    }
}

#[derive(Debug, Clone)]
pub struct JwtConfig {
    /// Trust broker public keys (npub hex) whose JWTs we accept
    pub trusted_issuers: Vec<String>,
    /// Minimum remaining TTL on JWT (seconds)
    pub min_ttl_secs: i64,
}

#[derive(Debug, Clone)]
pub struct AuthState {
    pub auth_mode: TrackerAuthMode,
    pub valid_keys: Vec<String>,
    pub jwt_config: Option<JwtConfig>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct JwtClaims {
    /// Operator npub (hex)
    pub sub: String,
    /// Trust broker npub (hex)
    pub iss: String,
    pub exp: i64,
    pub iat: Option<i64>,
    pub tier: Option<String>,
    pub entitlements: Option<Vec<String>>,
}

/// Extension carrying the authenticated operator npub (set by JWT/token middleware)
#[derive(Clone, Debug)]
pub struct OperatorNpub(pub String);

// ---------------------------------------------------------------------------
// Middleware
// ---------------------------------------------------------------------------

pub async fn auth_middleware(
    State(state): State<Arc<TrackerState>>,
    mut req: Request<Body>,
    next: Next,
) -> impl IntoResponse {
    let auth = &state.auth;

    match auth.auth_mode {
        TrackerAuthMode::Open => next.run(req).await,

        TrackerAuthMode::Token => {
            let key = req
                .headers()
                .get("X-Monopsony-Key")
                .and_then(|v| v.to_str().ok());
            match key {
                Some(k) if auth.valid_keys.iter().any(|vk| vk == k) => next.run(req).await,
                _ => (StatusCode::UNAUTHORIZED, "Unauthorized").into_response(),
            }
        }

        TrackerAuthMode::Jwt => {
            let token = req
                .headers()
                .get("Authorization")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.strip_prefix("Bearer "));
            match token {
                Some(t) => match validate_jwt(t, &auth.jwt_config) {
                    Ok(claims) => {
                        req.extensions_mut().insert(OperatorNpub(claims.sub));
                        next.run(req).await
                    }
                    Err(e) => {
                        tracing::warn!("JWT validation failed: {}", e);
                        (StatusCode::UNAUTHORIZED, "Invalid token").into_response()
                    }
                },
                None => (StatusCode::UNAUTHORIZED, "Missing token").into_response(),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// JWT validation
// ---------------------------------------------------------------------------

fn validate_jwt(token: &str, config: &Option<JwtConfig>) -> Result<JwtClaims, String> {
    let cfg = config
        .as_ref()
        .ok_or("JWT config not set — cannot validate JWTs")?;

    let mut validation = jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::HS256);
    validation.validate_exp = true;
    validation.leeway = 60;

    // Decode with shared secret for now.
    // Production Ed25519 JWT verification will be wired when the trust broker exists.
    let token_data = jsonwebtoken::decode::<JwtClaims>(
        token,
        &jsonwebtoken::DecodingKey::from_secret(b"tracker-dev-secret"),
        &validation,
    )
    .map_err(|e| format!("JWT decode error: {}", e))?;

    if !cfg.trusted_issuers.is_empty()
        && !cfg.trusted_issuers.contains(&token_data.claims.iss)
    {
        return Err("Untrusted issuer".to_string());
    }

    Ok(token_data.claims)
}

// ---------------------------------------------------------------------------
// Helpers for building test JWTs
// ---------------------------------------------------------------------------

/// Encode a test JWT with the given claims (HS256, dev secret).
#[cfg(test)]
pub fn encode_test_jwt(claims: &JwtClaims) -> Result<String, String> {
    let header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256);
    jsonwebtoken::encode(
        &header,
        claims,
        &jsonwebtoken::EncodingKey::from_secret(b"tracker-dev-secret"),
    )
    .map_err(|e| format!("JWT encode error: {}", e))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn test_auth_mode_from_str() {
        assert_eq!(TrackerAuthMode::from_str("open").unwrap(), TrackerAuthMode::Open);
        assert_eq!(TrackerAuthMode::from_str("token").unwrap(), TrackerAuthMode::Token);
        assert_eq!(TrackerAuthMode::from_str("jwt").unwrap(), TrackerAuthMode::Jwt);
        assert!(TrackerAuthMode::from_str("bogus").is_err());
    }

    #[test]
    fn test_validate_jwt_valid() {
        let claims = JwtClaims {
            sub: "abcdef01".to_string(),
            iss: "trustbroker".to_string(),
            exp: chrono::Utc::now().timestamp() + 3600,
            iat: None,
            tier: None,
            entitlements: None,
        };
        let token = encode_test_jwt(&claims).unwrap();
        let config = Some(JwtConfig {
            trusted_issuers: vec!["trustbroker".to_string()],
            min_ttl_secs: 0,
        });
        let result = validate_jwt(&token, &config);
        assert!(result.is_ok(), "valid JWT should decode: {:?}", result);
        assert_eq!(result.unwrap().sub, "abcdef01");
    }

    #[test]
    fn test_validate_jwt_expired() {
        let claims = JwtClaims {
            sub: "abcdef01".to_string(),
            iss: "trustbroker".to_string(),
            exp: chrono::Utc::now().timestamp() - 120,
            iat: None,
            tier: None,
            entitlements: None,
        };
        let token = encode_test_jwt(&claims).unwrap();
        let config = Some(JwtConfig {
            trusted_issuers: vec![],
            min_ttl_secs: 0,
        });
        let result = validate_jwt(&token, &config);
        assert!(result.is_err(), "expired JWT should fail");
    }

    #[test]
    fn test_validate_jwt_untrusted_issuer() {
        let claims = JwtClaims {
            sub: "abcdef01".to_string(),
            iss: "badactor".to_string(),
            exp: chrono::Utc::now().timestamp() + 3600,
            iat: None,
            tier: None,
            entitlements: None,
        };
        let token = encode_test_jwt(&claims).unwrap();
        let config = Some(JwtConfig {
            trusted_issuers: vec!["trustbroker".to_string()],
            min_ttl_secs: 0,
        });
        let result = validate_jwt(&token, &config);
        assert!(result.is_err(), "untrusted issuer should fail");
    }
}
