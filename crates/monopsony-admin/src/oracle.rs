//! Oracle delta distribution (Sprint 21 fleet wiring, Sep 18 2026).
//!
//! Serves the signed oracle delta to clients at
//! `GET /.well-known/monopsony/oracle-delta.json`. Sits OUTSIDE the auth
//! middleware (same tier as the WS tunnel routes): the payload is
//! Ed25519-signed by the oracle key, the verify pin is delivered at pair
//! time, and the document carries no per-client data. Clients verify the
//! signature against the pinned key before trusting anything inside.

use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};

use crate::AdminState;

/// `GET /.well-known/monopsony/oracle-delta.json` — the signed delta
/// envelope (key_id, sig, snapshot) as ONE JSON document.
///
/// Returns 404 until the first successful oracle refresh (server-side, from
/// the observations table). Clients treat 404/5xx as "oracle unavailable"
/// and keep their on-disk floors (fail-open availability, fail-closed
/// integrity: a delta is only ever applied after signature verification).
pub async fn handle_oracle_delta(State(state): State<AdminState>) -> Response {
    let cached = state
        .oracle_delta
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    match cached {
        Some(body) => {
            let mut resp = (StatusCode::OK, body).into_response();
            resp.headers_mut().insert(
                header::CONTENT_TYPE,
                header::HeaderValue::from_static("application/json"),
            );
            resp
        }
        None => (
            StatusCode::NOT_FOUND,
            "{\"error\":\"oracle delta not yet generated\"}",
        )
            .into_response(),
    }
}
