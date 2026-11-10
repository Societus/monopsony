//! Personal OCI registry — Sprint 25: pull-back (S1), blob uploads (S2),
//! manifests + tag index + tagless auto-version (S3).
//!
//! Serves pushed content from CacheStorage over the OCI distribution-spec
//! end-points this slice implements: version check (end-1), blob GET/HEAD
//! (end-2), manifest GET/HEAD (end-3), tags list (end-8a/8b), blob upload
//! sessions (end-4a/4b/5/6/13/14), and manifest push (end-7a/7b: byte-exact
//! store, referenced-descriptor validation, tag index, tagless pushes
//! auto-versioned `YYYYMMDD-HHMMSS`).
//!
//! Runs on its OWN listener (`--registry-bind`, default :3131): the `/v2`
//! route namespace collides head-on with `docker.rs` pull-through routes and
//! docker clients cannot be prefix-routed. All routes ride the same
//! `auth_middleware`/`AuthState` as the proxy router (keyed in v1).
//!
//! Plan: `docs/plans/sprint25-oci-registry/slice-1-pullback.md`.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::{FromRef, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::Router;
use base64::Engine as _;
use monopsony_db::Database;
use percent_encoding::percent_decode_str;

use crate::cache_storage::{CacheStorage, SpoolHandle};
use crate::proxy::ProxyState;

/// Storage-key namespace for registry content. Disjoint from the distro /
/// language-PM / `docker/v2` cache namespaces so retention and scrub treat
/// the registry as its own class (S4 wires its budget).
const REGISTRY_PREFIX: &str = "registry";

/// Maximum accepted manifest size (S3). The spec says a registry SHOULD
/// enforce a limit (oversize → `413`) and SHOULD accept pushes of at least
/// 4 MiB, so the floor IS the limit: larger manifests are rejected with
/// `413`, exactly-4-MiB manifests are accepted.
const MAX_MANIFEST_SIZE: usize = 4 * 1024 * 1024;

/// Auto-version tag format for tagless pushes (kickoff decision 2026-10-19:
/// `YYYYMMDD-HHMMSS`, UTC, no shorthash suffix — OCI-only scope).
const AUTO_VERSION_FORMAT: &str = "%Y%m%d-%H%M%S";

/// Manifest-push policy knobs (S3), threaded from the `[registry]` config
/// section through `run_registry`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RegistrySettings {
    /// Accept manifest pushes whose referenced blobs / child manifests are
    /// not present yet (lazy pushes). Default `false`: the push is rejected
    /// with `404 MANIFEST_BLOB_UNKNOWN` — the simplest spec-conformant
    /// default (spec: a registry MAY reject such manifests). `subject`
    /// descriptors are exempt either way (spec: referrers may be pushed in
    /// either order).
    pub allow_missing_blobs: bool,
}

/// Storage keys for the registry namespace.
fn blob_key(repo: &str, digest: &str) -> String {
    format!("{REGISTRY_PREFIX}/{repo}/blobs/{digest}")
}

fn manifest_key(repo: &str, digest: &str) -> String {
    format!("{REGISTRY_PREFIX}/{repo}/manifests/{digest}")
}

fn tags_key(repo: &str) -> String {
    format!("{REGISTRY_PREFIX}/{repo}/tags.json")
}

/// Sidecar marking auto-versioned tags (S4's never-auto-delete contract
/// reads this; written by every tagless push in S3).
fn autotags_key(repo: &str) -> String {
    format!("{REGISTRY_PREFIX}/{repo}/autotags.json")
}

/// SHA-256 of `bytes` as lowercase hex — the manifest digest is computed
/// from the received bytes and doubles as the storage key component.
fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

// ---------------------------------------------------------------------------
// Public entry points (construction site — called from monopsony-server)
// ---------------------------------------------------------------------------

/// Run the registry listener. Self-contained like the mirror router: own
/// state, own auth state, plain `axum::serve` (registry traffic never uses
/// CONNECT — pulls are GET/HEAD, pushes are POST/PATCH/PUT).
pub async fn run_registry(
    addr: SocketAddr,
    storage: Arc<dyn CacheStorage>,
    db: Option<Arc<Database>>,
    auth_mode: monopsony_common::config::AuthMode,
    settings: RegistrySettings,
) -> anyhow::Result<()> {
    let state = Arc::new(ProxyState::new(storage, db.clone()));
    let auth_state = Arc::new(monopsony_control::auth::AuthState { auth_mode, db });
    let uploads = Arc::new(UploadSessions::new());
    // TTL sweep for abandoned upload sessions (spec end-14 SHOULD-timeout).
    // The registry listener has no janitor loop of its own, so it runs a
    // dedicated interval task; sweep aborts outside the session-map lock.
    let sweeper = uploads.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(UPLOAD_SWEEP_INTERVAL);
        loop {
            interval.tick().await;
            sweeper.sweep(UPLOAD_SESSION_TTL).await;
        }
    });
    let app = create_registry_router_with_settings(state, auth_state, uploads, settings);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("OCI registry listening on {}", addr);
    axum::serve(listener, app.into_make_service()).await?;
    Ok(())
}

/// Registry router. Auth is layered with its own state (same pattern as
/// `create_proxy_router`), then the handler state is attached. A fresh
/// in-proc upload-session store backs the S2 push path.
pub fn create_registry_router(
    state: Arc<ProxyState>,
    auth_state: Arc<monopsony_control::auth::AuthState>,
) -> Router {
    create_registry_router_with_sessions(state, auth_state, Arc::new(UploadSessions::new()))
}

/// As `create_registry_router`, but with a caller-owned session store —
/// `run_registry` passes the same store its TTL sweeper walks. Default
/// manifest-push settings.
pub fn create_registry_router_with_sessions(
    state: Arc<ProxyState>,
    auth_state: Arc<monopsony_control::auth::AuthState>,
    uploads: Arc<UploadSessions>,
) -> Router {
    create_registry_router_with_settings(state, auth_state, uploads, RegistrySettings::default())
}

/// As `create_registry_router_with_sessions`, with explicit manifest-push
/// settings (S3 test seam; `run_registry` threads the `[registry]` config).
pub fn create_registry_router_with_settings(
    state: Arc<ProxyState>,
    auth_state: Arc<monopsony_control::auth::AuthState>,
    uploads: Arc<UploadSessions>,
    settings: RegistrySettings,
) -> Router {
    Router::new()
        .route("/v2", axum::routing::any(handle_v2))
        .route("/v2/", axum::routing::any(handle_v2))
        .route("/v2/*path", axum::routing::any(handle_v2))
        .layer(axum::middleware::from_fn_with_state(
            auth_state,
            monopsony_control::auth::auth_middleware,
        ))
        // Outermost: rewrite docker-client Basic credentials to the native
        // key header BEFORE the shared middleware validates them.
        .layer(axum::middleware::from_fn(basic_auth_to_key))
        .with_state(RegistryState {
            proxy: state,
            uploads,
            tags_lock: Arc::new(tokio::sync::Mutex::new(())),
            settings,
        })
}

/// Router state: shared proxy state plus the registry-owned upload-session
/// store. `FromRef` keeps every S1 handler extracting `State<Arc<ProxyState>>`.
#[derive(Clone)]
struct RegistryState {
    proxy: Arc<ProxyState>,
    uploads: Arc<UploadSessions>,
    /// Serializes tag-index read-modify-write cycles across concurrent
    /// manifest pushes (tokio mutex: the guard is held across storage
    /// awaits; a std/parking-lot guard would make the future `!Send`).
    tags_lock: Arc<tokio::sync::Mutex<()>>,
    settings: RegistrySettings,
}

impl FromRef<RegistryState> for Arc<ProxyState> {
    fn from_ref(state: &RegistryState) -> Self {
        state.proxy.clone()
    }
}

// ---------------------------------------------------------------------------
// Blob-upload sessions (S2): end-4a/4b/5/6/13/14
// ---------------------------------------------------------------------------

/// How long an idle upload session survives before its spool is aborted
/// (spec end-14: the server SHOULD eventually timeout unfinished uploads).
const UPLOAD_SESSION_TTL: Duration = Duration::from_secs(24 * 3600);
/// Sweep cadence; `run_registry` spawns the interval task (the registry
/// listener has no janitor loop of its own).
const UPLOAD_SWEEP_INTERVAL: Duration = Duration::from_secs(600);

/// In-proc blob-upload session store. Sessions die with the process —
/// spec-legal: after a restart the client sees `404 BLOB_UPLOAD_UNKNOWN` and
/// restarts the push.
pub struct UploadSessions {
    inner: parking_lot::Mutex<HashMap<String, UploadSession>>,
}

struct UploadSession {
    repo: String,
    /// Open spool for the session. `None` only while a PATCH is streaming the
    /// body into the taken handle (never awaited under the map lock).
    spool: Option<SpoolHandle>,
    /// Bytes accepted so far — the next chunk MUST start at this offset.
    offset: u64,
    last_activity: Instant,
}

impl UploadSessions {
    fn new() -> Self {
        Self { inner: parking_lot::Mutex::new(HashMap::new()) }
    }

    /// Abort every session idle longer than `ttl`. Aborts run OUTSIDE the map
    /// lock (they are async and consume the handle).
    pub async fn sweep(&self, ttl: Duration) {
        let expired: Vec<SpoolHandle> = {
            let mut map = self.inner.lock();
            let stale: Vec<String> = map
                .iter()
                .filter(|(_, s)| s.last_activity.elapsed() > ttl)
                .map(|(id, _)| id.clone())
                .collect();
            stale
                .into_iter()
                .filter_map(|id| map.remove(&id))
                .filter_map(|s| s.spool)
                .collect()
        };
        for spool in expired {
            spool.abort().await;
        }
    }
}

/// Staging key for an in-flight session. Never published: the spool is
/// re-keyed to the content-addressed blob key immediately before commit, and
/// abandoned sessions are aborted (DELETE / TTL sweep), never committed.
fn staging_key(repo: &str, sid: &str) -> String {
    format!("{REGISTRY_PREFIX}/{repo}/.uploads/{sid}")
}

fn upload_location(repo: &str, sid: &str) -> String {
    format!("/v2/{repo}/blobs/uploads/{sid}")
}

fn blob_location(repo: &str, digest: &str) -> String {
    format!("/v2/{repo}/blobs/{digest}")
}

/// Docker-client credential mapping (S1's deferred design item): after
/// `docker login <host> -u <any> -p <monopsony-key>` clients send
/// `Authorization: Basic base64(user:password)`. Map the Basic password onto
/// the native `X-Monopsony-Key` header (when the client did not already
/// present one) and let the shared `auth_middleware` validate as usual —
/// zero new trust code. No Bearer/token flow in v1: a 401 carries the
/// middleware's `WWW-Authenticate: X-Monopsony-Key`; the operator flow is a
/// one-time `docker login` with the key as password.
async fn basic_auth_to_key(
    mut req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    if !req.headers().contains_key("X-Monopsony-Key") {
        if let Some(password) = basic_password(req.headers().get(axum::http::header::AUTHORIZATION)) {
            if let Ok(value) = HeaderValue::from_str(&password) {
                req.headers_mut().insert("X-Monopsony-Key", value);
            }
        }
    }
    next.run(req).await
}

/// Extract the password component of an `Authorization: Basic` header.
/// Username is ignored (token-as-password convention). `None` for any other
/// scheme or malformed input.
fn basic_password(header: Option<&HeaderValue>) -> Option<String> {
    let raw = header?.to_str().ok()?;
    let encoded = raw.strip_prefix("Basic ")?.trim();
    let decoded = base64::engine::general_purpose::STANDARD.decode(encoded).ok()?;
    let decoded = String::from_utf8(decoded).ok()?;
    let (_, password) = decoded.split_once(':')?;
    if password.is_empty() {
        return None;
    }
    Some(password.to_string())
}

// ---------------------------------------------------------------------------
// Request routing
// ---------------------------------------------------------------------------

/// One handler for the whole `/v2` tree: repo names contain slashes, so the
/// request path is parsed manually instead of relying on route params. The
/// wildcard capture is unused — we read the raw URI so percent-decoding stays
/// under our control (decode each segment AFTER splitting, RFC 3986 order).
async fn handle_v2(
    State(registry): State<RegistryState>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: axum::body::Body,
) -> Response {
    let state = registry.proxy.clone();
    let uploads = registry.uploads.clone();
    let raw = uri.path();
    let rest = raw.strip_prefix("/v2").unwrap_or(raw);
    let rest = rest.strip_prefix('/').unwrap_or(rest);
    if rest.is_empty() {
        return version_check();
    }

    let mut segments: Vec<&str> = rest.split('/').collect();
    // One trailing slash (the spec's POST target `/blobs/uploads/`) is not a
    // segment; deeper trailing slashes just fail validation downstream.
    if segments.last() == Some(&"") {
        segments.pop();
    }
    if segments.is_empty() {
        return unsupported();
    }

    // Upload-session shapes, checked BEFORE the generic repo/kind split (repo
    // names may legitimately contain `blobs`/`uploads` segments; the registry
    // grammar resolves the ambiguity in favor of the upload tail, same as
    // upstream distribution implementations).
    let n = segments.len();
    if n >= 3 && segments[n - 2] == "blobs" && segments[n - 1] == "uploads" {
        let Some(repo) = decode_repo(&segments[..n - 2]) else {
            return oci_error(StatusCode::BAD_REQUEST, "NAME_INVALID", "invalid repository name");
        };
        return upload_start(&state, &uploads, &method, &uri, body, &repo).await;
    }
    if n >= 4 && segments[n - 3] == "blobs" && segments[n - 2] == "uploads" {
        let Some(repo) = decode_repo(&segments[..n - 3]) else {
            return oci_error(StatusCode::BAD_REQUEST, "NAME_INVALID", "invalid repository name");
        };
        let sid = segments[n - 1].to_string();
        return upload_session(&registry, &method, &uri, &headers, body, &repo, &sid).await;
    }

    // Minimum shape: <repo-segment> + <blobs|manifests|tags> + <ref|list>
    if segments.len() < 3 {
        return unsupported();
    }
    let (repo_raw, kind_raw) = segments.split_at(segments.len() - 2);
    let (kind, ref_raw) = (kind_raw[0], kind_raw[1]);

    // Decode + validate the repo BEFORE anything else (traversal guard).
    let Some(repo) = decode_repo(repo_raw) else {
        return oci_error(StatusCode::BAD_REQUEST, "NAME_INVALID", "invalid repository name");
    };

    match (kind, ref_raw) {
        ("tags", "list") => tags_list(&state, &repo, &uri).await,
        ("blobs", digest_raw) => {
            let Ok(digest) = percent_decode_str(digest_raw).decode_utf8() else {
                return oci_error(StatusCode::BAD_REQUEST, "DIGEST_INVALID", "malformed digest");
            };
            serve_blob(&state, &method, &headers, &repo, &digest).await
        }
        ("manifests", reference_raw) => {
            let Ok(reference) = percent_decode_str(reference_raw).decode_utf8() else {
                return oci_error(StatusCode::BAD_REQUEST, "MANIFEST_UNKNOWN", "malformed reference");
            };
            if method == Method::PUT {
                return put_manifest(&registry, &headers, &uri, body, &repo, &reference).await;
            }
            if method != Method::GET && method != Method::HEAD {
                // end-9 DELETE is S4; until then the spec sanctions 405.
                return oci_error(
                    StatusCode::METHOD_NOT_ALLOWED,
                    "UNSUPPORTED",
                    "manifests support GET, HEAD, and PUT",
                );
            }
            serve_manifest(&state, &method, &repo, &reference).await
        }
        _ => unsupported(),
    }
}

/// Decode + validate repository path segments (S1 traversal guard). Returns
/// the normalized `a/b` repo name, or `None` when any segment fails the
/// grammar (caller responds `400 NAME_INVALID`).
fn decode_repo(segments: &[&str]) -> Option<String> {
    let mut repo = String::new();
    for seg in segments {
        let decoded = percent_decode_str(seg).decode_utf8().ok()?;
        if !is_valid_repo_segment(&decoded) {
            return None;
        }
        if !repo.is_empty() {
            repo.push('/');
        }
        repo.push_str(&decoded);
    }
    Some(repo)
}

// ---------------------------------------------------------------------------
// Blob upload path (S2) — end-4a/4b (start/monolithic), end-5 (PATCH),
// end-6 (PUT finalize), end-13 (GET status), end-14 (DELETE cancel)
// ---------------------------------------------------------------------------

/// Parse a bare OCI `Content-Range: <start>-<end>` header (inclusive both
/// ends; spec regexp `^[0-9]+-[0-9]+$`). This is NOT the HTTP `bytes=` form.
fn parse_oci_content_range(value: &str) -> Option<(u64, u64)> {
    let (start, end) = value.trim().split_once('-')?;
    if start.is_empty() || end.is_empty() {
        return None;
    }
    match (start.parse::<u64>(), end.parse::<u64>()) {
        (Ok(s), Ok(e)) if e >= s => Some((s, e)),
        _ => None,
    }
}

/// Read a simple query parameter. Values are NOT percent-decoded — the only
/// params consumed (`digest`, later `mount`/`from`) are `[A-Za-z0-9:._-]`,
/// which need no escaping.
fn query_param<'a>(uri: &'a Uri, name: &str) -> Option<&'a str> {
    uri.query().and_then(|q| {
        q.split('&').find_map(|kv| {
            let (k, v) = kv.split_once('=')?;
            (k == name).then_some(v)
        })
    })
}

/// All values of a repeated query parameter (`?tag=a&tag=b`, end-7b), in
/// request order. Values are NOT percent-decoded — tags are validated
/// against the tag grammar, which contains no characters needing escapes.
fn query_params<'a>(uri: &'a Uri, name: &str) -> Vec<&'a str> {
    uri.query()
        .map(|q| {
            q.split('&')
                .filter_map(|kv| {
                    let (k, v) = kv.split_once('=')?;
                    (k == name).then_some(v)
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Validate a `?digest=` param strictly for finalize: `sha256:<64 lowercase
/// hex>` — the only algorithm the spool hasher computes. Returns the bare hex.
fn declared_digest_hex(digest: &str) -> Option<String> {
    let (alg, hex_part) = digest.split_once(':')?;
    if alg != "sha256" {
        return None;
    }
    let lowered = hex_part.to_ascii_lowercase();
    if lowered.len() == 64 && lowered.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        Some(lowered)
    } else {
        None
    }
}

/// Stream request body frames into the spool (never buffered whole). Returns
/// `Some(error response)` on a body read failure; the caller owns the spool
/// and aborts it.
async fn stream_into_spool(spool: &mut SpoolHandle, body: axum::body::Body) -> Option<Response> {
    use futures::StreamExt;
    let mut stream = body.into_data_stream();
    loop {
        match stream.next().await {
            Some(Ok(chunk)) => spool.write(&chunk),
            Some(Err(e)) => {
                tracing::error!("registry: upload body read failed: {e}");
                return Some(oci_error(
                    StatusCode::BAD_GATEWAY,
                    "BLOB_UPLOAD_INVALID",
                    "failed reading request body",
                ));
            }
            None => return None,
        }
    }
}

/// `POST /v2/<name>/blobs/uploads/` — open a session (end-4a), or commit a
/// monolithic push inline when `?digest=` is present (end-4b). `mount`/`from`
/// params (end-11, S5) fall through to a normal session: the spec's
/// sanctioned can't-mount behavior.
async fn upload_start(
    state: &ProxyState,
    uploads: &UploadSessions,
    method: &Method,
    uri: &Uri,
    body: axum::body::Body,
    repo: &str,
) -> Response {
    if *method != Method::POST {
        return oci_error(
            StatusCode::METHOD_NOT_ALLOWED,
            "UNSUPPORTED",
            "use POST to start an upload",
        );
    }

    let sid = uuid::Uuid::new_v4().simple().to_string();
    let mut spool = match state.storage.spool(&staging_key(repo, &sid)).await {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("registry: upload spool open failed for {repo}: {e}");
            return oci_error(StatusCode::INTERNAL_SERVER_ERROR, "UNKNOWN", "storage unavailable");
        }
    };

    if let Some(digest) = query_param(uri, "digest") {
        // Validate the declared digest BEFORE reading the body so a bad
        // request cannot consume bandwidth before failing.
        if declared_digest_hex(digest).is_none() {
            spool.abort().await;
            return oci_error(
                StatusCode::BAD_REQUEST,
                "DIGEST_INVALID",
                "unsupported or malformed digest",
            );
        }
        let digest = digest.to_string();
        if let Some(resp) = stream_into_spool(&mut spool, body).await {
            spool.abort().await;
            return resp;
        }
        return finalize_spool(state, spool, staging_key(repo, &sid), repo, &digest).await;
    }

    uploads.inner.lock().insert(
        sid.clone(),
        UploadSession {
            repo: repo.to_string(),
            spool: Some(spool),
            offset: 0,
            last_activity: Instant::now(),
        },
    );
    let loc = upload_location(repo, &sid);
    finish(
        axum::http::response::Response::builder()
            .status(StatusCode::ACCEPTED)
            .header("Location", loc),
        axum::body::Body::empty(),
    )
}

/// Dispatch a session-scoped operation on
/// `/v2/<repo>/blobs/uploads/<sid>`: end-5 PATCH, end-6 PUT, end-13 GET,
/// end-14 DELETE.
async fn upload_session(
    registry: &RegistryState,
    method: &Method,
    uri: &Uri,
    headers: &HeaderMap,
    body: axum::body::Body,
    repo: &str,
    sid: &str,
) -> Response {
    let state = &registry.proxy;
    let uploads = &registry.uploads;
    match *method {
        Method::GET => session_status(uploads, repo, sid),
        Method::PATCH => session_patch(uploads, repo, sid, headers, body).await,
        Method::PUT => session_finalize(state, uploads, repo, sid, uri, headers, body).await,
        Method::DELETE => session_cancel(uploads, repo, sid).await,
        _ => oci_error(
            StatusCode::METHOD_NOT_ALLOWED,
            "UNSUPPORTED",
            "method not allowed on upload session",
        ),
    }
}

/// end-13: current offset of an active session as `204 No Content` +
/// `Range: 0-<end-of-range>`.
fn session_status(uploads: &UploadSessions, repo: &str, sid: &str) -> Response {
    let map = uploads.inner.lock();
    let Some(session) = map.get(sid) else {
        return oci_error(StatusCode::NOT_FOUND, "BLOB_UPLOAD_UNKNOWN", "upload session not found");
    };
    if session.repo != repo {
        return oci_error(
            StatusCode::BAD_REQUEST,
            "BLOB_UPLOAD_INVALID",
            "session belongs to another repository",
        );
    }
    let end = session.offset.saturating_sub(1);
    let loc = upload_location(repo, sid);
    finish(
        axum::http::response::Response::builder()
            .status(StatusCode::NO_CONTENT)
            .header("Location", loc)
            .header("Range", format!("0-{end}")),
        axum::body::Body::empty(),
    )
}

/// end-14: cancel — discard the session and abort its spool.
async fn session_cancel(uploads: &UploadSessions, repo: &str, sid: &str) -> Response {
    {
        let map = uploads.inner.lock();
        match map.get(sid) {
            None => {
                return oci_error(
                    StatusCode::NOT_FOUND,
                    "BLOB_UPLOAD_UNKNOWN",
                    "upload session not found",
                )
            }
            Some(s) if s.repo != repo => {
                return oci_error(
                    StatusCode::BAD_REQUEST,
                    "BLOB_UPLOAD_INVALID",
                    "session belongs to another repository",
                )
            }
            Some(_) => {}
        }
    }
    let spool = uploads.inner.lock().remove(sid).and_then(|s| s.spool);
    match spool {
        Some(spool) => {
            spool.abort().await;
            finish(
                axum::http::response::Response::builder().status(StatusCode::NO_CONTENT),
                axum::body::Body::empty(),
            )
        }
        // Session existed but its spool is mid-PATCH; the in-flight handler
        // will find the session gone and abort the handle itself.
        None => oci_error(StatusCode::CONFLICT, "BLOB_UPLOAD_INVALID", "chunk already in flight"),
    }
}

/// end-5: append one chunk. In-order only (`start == session.offset`),
/// out-of-order → `416`; response `202` + `Location` + `Range: 0-<end>`.
async fn session_patch(
    uploads: &UploadSessions,
    repo: &str,
    sid: &str,
    headers: &HeaderMap,
    body: axum::body::Body,
) -> Response {
    let range = headers
        .get("content-range")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .and_then(parse_oci_content_range);
    let Some((start, _end)) = range else {
        return oci_error(
            StatusCode::BAD_REQUEST,
            "BLOB_UPLOAD_INVALID",
            "missing or malformed Content-Range",
        );
    };

    // Reserve the session's spool for this request: order-check and take the
    // handle under the lock, stream OUTSIDE it (the guard must never be held
    // across an await), then re-file the handle with the advanced offset.
    let (mut spool, expected) = {
        let mut map = uploads.inner.lock();
        let Some(session) = map.get_mut(sid) else {
            return oci_error(
                StatusCode::NOT_FOUND,
                "BLOB_UPLOAD_UNKNOWN",
                "upload session not found",
            );
        };
        if session.repo != repo {
            return oci_error(
                StatusCode::BAD_REQUEST,
                "BLOB_UPLOAD_INVALID",
                "session belongs to another repository",
            );
        }
        if start != session.offset {
            // Out-of-order chunk: 416 per spec; the client resyncs via GET.
            return oci_error(
                StatusCode::RANGE_NOT_SATISFIABLE,
                "BLOB_UPLOAD_INVALID",
                "chunk out of order",
            );
        }
        match session.spool.take() {
            Some(spool) => (spool, session.offset),
            None => {
                return oci_error(
                    StatusCode::CONFLICT,
                    "BLOB_UPLOAD_INVALID",
                    "chunk already in flight",
                )
            }
        }
    };

    if let Some(resp) = stream_into_spool(&mut spool, body).await {
        spool.abort().await;
        // The session was consumed by this request's failure; nothing to
        // re-file — the client restarts the push.
        return resp;
    }

    let written = spool.size() - expected;
    let new_offset = expected + written;
    // Carry the handle as an Option through the refile: the Some arm moves it
    // into the session, the None arm (DELETE raced the PATCH) keeps ownership
    // so it can be aborted below.
    let mut spool = Some(spool);
    let refiled = {
        let mut map = uploads.inner.lock();
        match map.get_mut(sid) {
            Some(session) => {
                session.spool = spool.take();
                session.offset = new_offset;
                session.last_activity = Instant::now();
                true
            }
            None => false,
        }
    };
    if !refiled {
        if let Some(spool) = spool {
            spool.abort().await;
        }
        return oci_error(StatusCode::NOT_FOUND, "BLOB_UPLOAD_UNKNOWN", "upload session not found");
    }

    let end = new_offset.saturating_sub(1);
    let loc = upload_location(repo, sid);
    finish(
        axum::http::response::Response::builder()
            .status(StatusCode::ACCEPTED)
            .header("Location", loc)
            .header("Range", format!("0-{end}")),
        axum::body::Body::empty(),
    )
}

/// end-6: close the session — `PUT ...?digest=<whole-blob digest>`, optional
/// final chunk on the body. Success: `201 Created` + pullable blob Location.
async fn session_finalize(
    state: &ProxyState,
    uploads: &UploadSessions,
    repo: &str,
    sid: &str,
    uri: &Uri,
    headers: &HeaderMap,
    body: axum::body::Body,
) -> Response {
    let Some(digest) = query_param(uri, "digest") else {
        return oci_error(
            StatusCode::BAD_REQUEST,
            "DIGEST_INVALID",
            "missing digest query parameter",
        );
    };
    // Validate before touching the session so a bad request cannot discard a
    // healthy in-flight upload.
    let Some(hex_part) = declared_digest_hex(digest) else {
        return oci_error(
            StatusCode::BAD_REQUEST,
            "DIGEST_INVALID",
            "unsupported or malformed digest",
        );
    };
    let range = headers
        .get("content-range")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .and_then(parse_oci_content_range);

    let mut spool = {
        let mut map = uploads.inner.lock();
        let Some(session) = map.get_mut(sid) else {
            return oci_error(
                StatusCode::NOT_FOUND,
                "BLOB_UPLOAD_UNKNOWN",
                "upload session not found",
            );
        };
        if session.repo != repo {
            return oci_error(
                StatusCode::BAD_REQUEST,
                "BLOB_UPLOAD_INVALID",
                "session belongs to another repository",
            );
        }
        // The final chunk MAY ride the closing PUT (spec); it must still be
        // in order. No Content-Range closes chunkless — legal even after zero
        // chunks, which is how a zero-byte blob (digest e3b0c442…) is pushed.
        if let Some((start, _end)) = range {
            if start != session.offset {
                return oci_error(
                    StatusCode::RANGE_NOT_SATISFIABLE,
                    "BLOB_UPLOAD_INVALID",
                    "final chunk out of order",
                );
            }
        }
        match session.spool.take() {
            Some(spool) => spool,
            None => {
                return oci_error(
                    StatusCode::CONFLICT,
                    "BLOB_UPLOAD_INVALID",
                    "chunk already in flight",
                )
            }
        }
    };
    // Session consumed either way below: remove it from the map.
    uploads.inner.lock().remove(sid);

    if range.is_some() {
        if let Some(resp) = stream_into_spool(&mut spool, body).await {
            spool.abort().await;
            return resp;
        }
    }
    finalize_spool(
        state,
        spool,
        staging_key(repo, sid),
        repo,
        &format!("sha256:{hex_part}"),
    )
    .await
}

/// Commit an upload spool and publish the blob at its content-addressed key.
///
/// The spool publishes to its STAGING key: the session opened it before the
/// digest was known, and the underlying object-store writer is bound to its
/// location at open (a metadata re-key cannot move the data — caught by the
/// end-to-end tests 2026-10-29). Finalize therefore commits at the staging
/// key, stream-copies to the final key in O(chunk) memory, and deletes the
/// staging object. A future `rename`/`copy` on CacheStorage drops the extra
/// write — deferred as an S4 optimization. Consumes the spool; a digest
/// mismatch publishes nothing.
async fn finalize_spool(
    state: &ProxyState,
    spool: SpoolHandle,
    staging: String,
    repo: &str,
    digest: &str,
) -> Response {
    let Some(hex_part) = declared_digest_hex(digest) else {
        spool.abort().await;
        return oci_error(
            StatusCode::BAD_REQUEST,
            "DIGEST_INVALID",
            "unsupported or malformed digest",
        );
    };
    let full_digest = format!("sha256:{hex_part}");
    let final_key = format!("{REGISTRY_PREFIX}/{repo}/blobs/{full_digest}");

    // Zero-byte blob: object_store multipart uploads reject a zero-part
    // finish(), so publish empty content through the buffered write path
    // instead (the declared digest must still be the empty-string sha256).
    if spool.size() == 0 {
        spool.abort().await;
        let meta = match state.storage.write(&final_key, &[]).await {
            Ok(m) => m,
            Err(e) => {
                tracing::error!("registry: empty-blob write failed for {final_key}: {e}");
                return oci_error(StatusCode::INTERNAL_SERVER_ERROR, "UNKNOWN", "storage commit failed");
            }
        };
        if meta.checksum_sha256 != hex_part {
            let _ = state.storage.delete(&final_key).await;
            return oci_error(
                StatusCode::BAD_REQUEST,
                "DIGEST_INVALID",
                "content does not match declared digest",
            );
        }
        if let Err(e) = state.storage.record_spool_meta(&final_key, &meta).await {
            tracing::warn!("registry: scrub-sidecar write failed for {final_key}: {e}");
        }
        let loc = blob_location(repo, &full_digest);
        return finish(
            axum::http::response::Response::builder()
                .status(StatusCode::CREATED)
                .header("Location", loc)
                .header("Docker-Content-Digest", full_digest),
            axum::body::Body::empty(),
        );
    }

    // Publish the uploaded bytes at the staging key and verify the declared
    // digest against the spool hasher BEFORE copying anything.
    let spool_meta = match spool.commit().await {
        Ok(m) => m,
        Err(e) => {
            tracing::error!("registry: spool commit failed for {staging}: {e}");
            return oci_error(StatusCode::INTERNAL_SERVER_ERROR, "UNKNOWN", "storage commit failed");
        }
    };
    if spool_meta.checksum_sha256 != hex_part {
        // Wrong content for the declared digest: publish nothing.
        let _ = state.storage.delete(&staging).await;
        return oci_error(
            StatusCode::BAD_REQUEST,
            "DIGEST_INVALID",
            "content does not match declared digest",
        );
    }

    // Stream-copy staging → content-addressed key.
    let (mut stream, _total) = match state.storage.read_stream(&staging, 0..u64::MAX).await {
        Ok(v) => v,
        Err(e) => {
            tracing::error!("registry: staging read-back failed for {staging}: {e}");
            let _ = state.storage.delete(&staging).await;
            return oci_error(StatusCode::INTERNAL_SERVER_ERROR, "UNKNOWN", "storage read-back failed");
        }
    };
    let mut final_spool = match state.storage.spool(&final_key).await {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("registry: final spool open failed for {final_key}: {e}");
            let _ = state.storage.delete(&staging).await;
            return oci_error(StatusCode::INTERNAL_SERVER_ERROR, "UNKNOWN", "storage unavailable");
        }
    };
    {
        use futures::StreamExt;
        loop {
            match stream.next().await {
                Some(Ok(chunk)) => final_spool.write(&chunk),
                Some(Err(e)) => {
                    tracing::error!("registry: staging copy read failed for {staging}: {e}");
                    final_spool.abort().await;
                    let _ = state.storage.delete(&staging).await;
                    return oci_error(StatusCode::INTERNAL_SERVER_ERROR, "UNKNOWN", "storage copy failed");
                }
                None => break,
            }
        }
    }
    let meta = match final_spool.commit().await {
        Ok(m) => m,
        Err(e) => {
            tracing::error!("registry: final commit failed for {final_key}: {e}");
            let _ = state.storage.delete(&staging).await;
            return oci_error(StatusCode::INTERNAL_SERVER_ERROR, "UNKNOWN", "storage commit failed");
        }
    };
    if meta.checksum_sha256 != hex_part {
        // Copy-integrity paranoia check: the spool hasher already matched.
        let _ = state.storage.delete(&final_key).await;
        let _ = state.storage.delete(&staging).await;
        return oci_error(
            StatusCode::BAD_REQUEST,
            "DIGEST_INVALID",
            "content does not match declared digest",
        );
    }
    // Scrub/verify sidecar for streamed blobs (same convention as docker.rs).
    if let Err(e) = state.storage.record_spool_meta(&final_key, &meta).await {
        tracing::warn!("registry: scrub-sidecar write failed for {final_key}: {e}");
    }
    let _ = state.storage.delete(&staging).await;

    let loc = blob_location(repo, &full_digest);
    finish(
        axum::http::response::Response::builder()
            .status(StatusCode::CREATED)
            .header("Location", loc)
            .header("Docker-Content-Digest", full_digest),
        axum::body::Body::empty(),
    )
}

// ---------------------------------------------------------------------------
// Manifest push (end-7a/7b) — byte-exact store, tag index, tagless
// auto-version
// ---------------------------------------------------------------------------

/// end-7a: `PUT /v2/<repo>/manifests/<tag-or-digest>`. The manifest is stored
/// byte-exact under its content digest; a tag reference (plus the `?tag=`
/// extras) is recorded in the tag-index sidecar. A digest reference is a
/// TAGLESS push: it additionally receives an auto-version tag
/// `YYYYMMDD-HHMMSS` (UTC) so every push is addressable without tag
/// discipline at build time.
async fn put_manifest(
    registry: &RegistryState,
    headers: &HeaderMap,
    uri: &Uri,
    body: axum::body::Body,
    repo: &str,
    reference: &str,
) -> Response {
    let state = &registry.proxy;

    // Reference form: a digest reference is a tagless push; anything else
    // must be a syntactically valid tag.
    let ref_is_digest = reference.contains(':');
    if ref_is_digest {
        if declared_digest_hex(reference).is_none() {
            return oci_error(
                StatusCode::BAD_REQUEST,
                "DIGEST_INVALID",
                "unsupported or malformed digest",
            );
        }
    } else if !is_valid_tag(reference) {
        return oci_error(StatusCode::BAD_REQUEST, "TAG_INVALID", "invalid tag");
    }

    // Extra tags (end-7b) are validated BEFORE anything is stored so a bad
    // request cannot leave a half-pushed manifest behind.
    let mut extra_tags: Vec<String> = Vec::new();
    for tag in query_params(uri, "tag") {
        if !is_valid_tag(tag) {
            return oci_error(
                StatusCode::BAD_REQUEST,
                "TAG_INVALID",
                "invalid tag query parameter",
            );
        }
        extra_tags.push(tag.to_string());
    }

    // Manifests are small JSON documents: buffered whole under the size cap,
    // never spooled. An over-limit body is rejected before any storage I/O.
    let bytes = match axum::body::to_bytes(body, MAX_MANIFEST_SIZE).await {
        Ok(b) => b,
        Err(_) => {
            return oci_error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "MANIFEST_INVALID",
                "manifest exceeds maximum size",
            );
        }
    };

    let parsed: serde_json::Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(_) => {
            return oci_error(
                StatusCode::BAD_REQUEST,
                "MANIFEST_INVALID",
                "manifest is not valid JSON",
            );
        }
    };
    let media_type = parsed
        .get("mediaType")
        .and_then(|v| v.as_str())
        .map(str::to_owned);

    // The Content-Type header must equal the manifest's declared mediaType
    // when one is present (header parameters ignored per spec / RFC 9110).
    if let Some(mt) = &media_type {
        let declared = headers
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|ct| ct.split(';').next().unwrap_or("").trim())
            .unwrap_or("");
        if !declared.is_empty() && declared != mt {
            return oci_error(
                StatusCode::BAD_REQUEST,
                "MANIFEST_INVALID",
                "Content-Type does not match the manifest mediaType",
            );
        }
    }

    // The manifest's own digest, computed from the exact bytes received.
    let digest_hex = sha256_hex(&bytes);
    let digest = format!("sha256:{digest_hex}");
    if ref_is_digest && reference != digest {
        return oci_error(
            StatusCode::BAD_REQUEST,
            "DIGEST_INVALID",
            "manifest does not match the reference digest",
        );
    }

    // Referenced-descriptor validation (skipped under
    // [registry] allow_missing_blobs — the lazy-push escape hatch).
    if !registry.settings.allow_missing_blobs {
        if let Some(resp) = validate_manifest_references(state, repo, &parsed).await {
            return resp;
        }
    }

    // Byte-exact store, plus a media-type sidecar so GET/HEAD can echo the
    // pushed Content-Type instead of assuming OCI JSON.
    let key = manifest_key(repo, &digest);
    if let Err(e) = state.storage.write(&key, &bytes).await {
        tracing::error!("registry: manifest write failed for {key}: {e}");
        return oci_error(StatusCode::INTERNAL_SERVER_ERROR, "UNKNOWN", "storage unavailable");
    }
    if let Some(mt) = &media_type {
        let sidecar = serde_json::json!({ "media_type": mt });
        if let Err(e) = state.storage.save_meta_json(&key, sidecar.to_string().as_bytes()).await {
            tracing::warn!("registry: manifest meta sidecar write failed for {key}: {e}");
        }
    }

    // Tag index update, serialized against concurrent pushes. Auto-version
    // assignment happens here for tagless pushes and is recorded in the
    // autotags sidecar — S4's GC reads that sidecar for its
    // never-auto-delete exemption.
    let applied_tags: Vec<String> = {
        let _guard = registry.tags_lock.lock().await;
        let mut tags: HashMap<String, String> = match state.storage.read(&tags_key(repo)).await {
            // An unreadable index is treated as empty and rewritten: the
            // next write self-heals it (reads treat corrupt as missing too).
            Ok(b) => serde_json::from_slice(&b).unwrap_or_default(),
            Err(_) => HashMap::new(),
        };
        let mut applied: Vec<String> = Vec::new();
        let mut auto_tag: Option<String> = None;
        if ref_is_digest {
            let tag = chrono::Utc::now().format(AUTO_VERSION_FORMAT).to_string();
            tags.insert(tag.clone(), digest.clone());
            applied.push(tag.clone());
            auto_tag = Some(tag);
        } else {
            tags.insert(reference.to_string(), digest.clone());
            applied.push(reference.to_string());
        }
        for tag in &extra_tags {
            tags.insert(tag.clone(), digest.clone());
            applied.push(tag.clone());
        }
        let Ok(payload) = serde_json::to_vec(&tags) else {
            tracing::error!("registry: tag index serialize failed for {repo}");
            return oci_error(StatusCode::INTERNAL_SERVER_ERROR, "UNKNOWN", "tag index update failed");
        };
        if let Err(e) = state.storage.write(&tags_key(repo), &payload).await {
            tracing::error!("registry: tag index write failed for {}: {e}", tags_key(repo));
            return oci_error(StatusCode::INTERNAL_SERVER_ERROR, "UNKNOWN", "tag index update failed");
        }
        if let Some(tag) = &auto_tag {
            record_auto_tag(state, repo, tag).await;
        }
        applied
    };

    let mut builder = axum::http::response::Response::builder()
        .status(StatusCode::CREATED)
        .header("Location", format!("/v2/{repo}/manifests/{digest}"))
        .header(
            "Docker-Content-Digest",
            HeaderValue::from_str(&digest).unwrap_or(HeaderValue::from_static("")),
        );
    // One OCI-Tag header per accepted tag (spec end-7b: MUST, for each).
    for tag in &applied_tags {
        if let Ok(v) = HeaderValue::from_str(tag) {
            builder = builder.header("OCI-Tag", v);
        }
    }
    finish(builder, axum::body::Body::empty())
}

/// Append an auto-versioned tag to the autotags sidecar. Best-effort: the
/// tag itself is already committed to the index, so a sidecar write failure
/// is logged, never fatal.
async fn record_auto_tag(state: &ProxyState, repo: &str, tag: &str) {
    let mut autos: Vec<String> = match state.storage.read(&autotags_key(repo)).await {
        Ok(b) => serde_json::from_slice(&b).unwrap_or_default(),
        Err(_) => Vec::new(),
    };
    if !autos.iter().any(|t| t == tag) {
        autos.push(tag.to_string());
        autos.sort();
    }
    if let Ok(payload) = serde_json::to_vec(&autos) {
        if let Err(e) = state.storage.write(&autotags_key(repo), &payload).await {
            tracing::warn!(
                "registry: autotags sidecar write failed for {}: {e}",
                autotags_key(repo)
            );
        }
    }
}

/// Validate the descriptors a manifest references against local storage
/// (skipped when `allow_missing_blobs` is set). `config` + `layers[]` must
/// exist in the blob namespace; `manifests[]` children (image index) must
/// exist in the manifest namespace. `subject` is NEVER validated: the spec
/// REQUIRES accepting referrer manifests whose subject does not exist yet
/// (manifest and referrers push in either order). Returns the error
/// response, or `None` when every reference checks out.
async fn validate_manifest_references(
    state: &ProxyState,
    repo: &str,
    manifest: &serde_json::Value,
) -> Option<Response> {
    if let Some(config) = manifest.get("config") {
        if let Some(resp) = ensure_descriptor(state, repo, config, true).await {
            return Some(resp);
        }
    }
    if let Some(layers) = manifest.get("layers").and_then(|v| v.as_array()) {
        for layer in layers {
            if let Some(resp) = ensure_descriptor(state, repo, layer, true).await {
                return Some(resp);
            }
        }
    }
    if let Some(children) = manifest.get("manifests").and_then(|v| v.as_array()) {
        for child in children {
            if let Some(resp) = ensure_descriptor(state, repo, child, false).await {
                return Some(resp);
            }
        }
    }
    None
}

/// Check one descriptor. `blob_namespace` selects the storage namespace the
/// referenced digest must exist in (blobs for image-manifest descriptors,
/// manifests for index children). A descriptor without a digest is skipped.
async fn ensure_descriptor(
    state: &ProxyState,
    repo: &str,
    descriptor: &serde_json::Value,
    blob_namespace: bool,
) -> Option<Response> {
    let digest = descriptor.get("digest").and_then(|v| v.as_str())?;
    if !is_valid_digest(digest) {
        return Some(oci_error(
            StatusCode::BAD_REQUEST,
            "MANIFEST_INVALID",
            "descriptor digest is malformed",
        ));
    }
    let key = if blob_namespace {
        blob_key(repo, digest)
    } else {
        manifest_key(repo, digest)
    };
    match state.storage.exists(&key).await {
        Ok(true) => None,
        Ok(false) => Some(oci_error(
            StatusCode::NOT_FOUND,
            "MANIFEST_BLOB_UNKNOWN",
            &format!("manifest references unknown content: {digest}"),
        )),
        Err(e) => {
            tracing::error!("registry: existence check failed for {key}: {e}");
            Some(oci_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "UNKNOWN",
                "storage unavailable",
            ))
        }
    }
}

fn version_check() -> Response {
    (
        StatusCode::OK,
        [(
            "Docker-Distribution-API-Version",
            HeaderValue::from_static("registry/2.0"),
        )],
        axum::Json(serde_json::json!({})),
    )
        .into_response()
}

fn unsupported() -> Response {
    oci_error(StatusCode::NOT_FOUND, "UNSUPPORTED", "unsupported registry path")
}

fn oci_error(status: StatusCode, code: &str, message: &str) -> Response {
    (
        status,
        axum::Json(serde_json::json!({
            "errors": [{"code": code, "message": message, "detail": ""}]
        })),
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// Blob serving (end-2)
// ---------------------------------------------------------------------------

/// Complete a response from a builder, falling back to 500 if a header value
/// was invalid (cannot happen with our validated digests/numeric ranges, but
/// a handler must not panic).
fn finish(builder: axum::http::response::Builder, body: axum::body::Body) -> Response {
    builder
        .body(body)
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

async fn serve_blob(
    state: &ProxyState,
    method: &Method,
    headers: &HeaderMap,
    repo: &str,
    digest: &str,
) -> Response {
    if !is_valid_digest(digest) {
        return oci_error(StatusCode::BAD_REQUEST, "DIGEST_INVALID", "invalid digest");
    }
    let key = format!("{REGISTRY_PREFIX}/{repo}/blobs/{digest}");
    let (stream, total) = match state.storage.read_stream(&key, 0..u64::MAX).await {
        Ok(pair) => pair,
        Err(_) => return oci_error(StatusCode::NOT_FOUND, "BLOB_UNKNOWN", "blob not found"),
    };

    if method == Method::HEAD {
        drop(stream);
        return finish(
            blob_response_headers(digest, total, None),
            axum::body::Body::empty(),
        );
    }

    // Single range only; malformed or multi-range requests are served in
    // full (RFC 9110 permits ignoring Range).
    let range = headers
        .get("range")
        .and_then(|v| v.to_str().ok())
        .and_then(parse_range_spec)
        .and_then(|spec| resolve_range(spec, total));

    match range {
        None => finish(
            blob_response_headers(digest, total, None),
            axum::body::Body::from_stream(stream),
        ),
        Some((start, end)) => {
            // Range requested but unsatisfiable (start past EOF): 416.
            if start == u64::MAX {
                let mut resp = oci_error(
                    StatusCode::RANGE_NOT_SATISFIABLE,
                    "BLOB_UNKNOWN",
                    "requested range not satisfiable",
                );
                if let Ok(v) = HeaderValue::from_str(&format!("bytes */{total}")) {
                    resp.headers_mut().insert("Content-Range", v);
                }
                return resp;
            }
            let length = end - start + 1;
            let (range_stream, _) = match state.storage.read_stream(&key, start..end + 1).await {
                Ok(pair) => pair,
                Err(_) => {
                    return oci_error(StatusCode::NOT_FOUND, "BLOB_UNKNOWN", "blob not found")
                }
            };
            finish(
                blob_response_headers(digest, total, Some((start, end, length))),
                axum::body::Body::from_stream(range_stream),
            )
        }
    }
}

fn blob_response_headers(
    digest: &str,
    total: u64,
    range: Option<(u64, u64, u64)>,
) -> axum::http::response::Builder {
    let mut builder = axum::http::response::Response::builder()
        .header("Accept-Ranges", "bytes")
        .header("Content-Type", "application/octet-stream")
        .header(
            "Docker-Content-Digest",
            HeaderValue::from_str(digest).unwrap_or(HeaderValue::from_static("")),
        );
    builder = match range {
        None => builder.header("Content-Length", total).status(StatusCode::OK),
        Some((start, end, length)) => builder
            .status(StatusCode::PARTIAL_CONTENT)
            .header("Content-Length", length)
            .header(
                "Content-Range",
                HeaderValue::from_str(&format!("bytes {start}-{end}/{total}"))
                    .unwrap_or(HeaderValue::from_static("bytes */0")),
            ),
    };
    builder
}

/// Parsed `Range` header. `LastN` is the suffix form `bytes=-n`.
enum RangeSpec {
    FromTo(u64, u64),
    From(u64),
    LastN(u64),
}

fn parse_range_spec(spec: &str) -> Option<RangeSpec> {
    let spec = spec.strip_prefix("bytes=")?;
    if spec.contains(',') {
        return None; // multi-range: ignore, serve full
    }
    let (start, end) = spec.split_once('-')?;
    match (start.is_empty(), end.is_empty()) {
        (false, false) => {
            let s: u64 = start.parse().ok()?;
            let e: u64 = end.parse().ok()?;
            if e < s {
                return None;
            }
            Some(RangeSpec::FromTo(s, e))
        }
        (false, true) => Some(RangeSpec::From(start.parse().ok()?)),
        (true, false) => {
            let n: u64 = end.parse().ok()?;
            if n == 0 {
                return None;
            }
            Some(RangeSpec::LastN(n))
        }
        (true, true) => None,
    }
}

/// Resolve a parsed range against the object size. Returns the inclusive
/// `(start, end)` byte window, or `Some((u64::MAX, _))` as the unsatisfiable
/// sentinel (start past EOF), or `None` when no usable range was given
/// (caller serves the full object).
fn resolve_range(spec: RangeSpec, total: u64) -> Option<(u64, u64)> {
    if total == 0 {
        return Some((u64::MAX, 0));
    }
    match spec {
        RangeSpec::FromTo(start, end) => {
            if start >= total {
                Some((u64::MAX, 0))
            } else {
                Some((start, end.min(total - 1)))
            }
        }
        RangeSpec::From(start) => {
            if start >= total {
                Some((u64::MAX, 0))
            } else {
                Some((start, total - 1))
            }
        }
        RangeSpec::LastN(n) => Some((total.saturating_sub(n), total - 1)),
    }
}

// ---------------------------------------------------------------------------
// Manifest serving (end-3) + tags list (end-8a)
// ---------------------------------------------------------------------------

async fn serve_manifest(
    state: &ProxyState,
    method: &Method,
    repo: &str,
    reference: &str,
) -> Response {
    // A reference containing ':' is a digest; anything else is a tag.
    let digest = if reference.contains(':') {
        if !is_valid_digest(reference) {
            return oci_error(StatusCode::BAD_REQUEST, "DIGEST_INVALID", "invalid digest");
        }
        reference.to_string()
    } else {
        if !is_valid_tag(reference) {
            return oci_error(StatusCode::BAD_REQUEST, "TAG_INVALID", "invalid tag");
        }
        match resolve_tag(state, repo, reference).await {
            Some(d) => d,
            None => {
                return oci_error(
                    StatusCode::NOT_FOUND,
                    "MANIFEST_UNKNOWN",
                    "manifest not found for tag",
                )
            }
        }
    };

    let key = format!("{REGISTRY_PREFIX}/{repo}/manifests/{digest}");
    let bytes = match state.storage.read(&key).await {
        Ok(b) => b,
        Err(_) => {
            return oci_error(StatusCode::NOT_FOUND, "MANIFEST_UNKNOWN", "manifest not found")
        }
    };
    let length = bytes.len() as u64;
    // The persisted media type rides the push-time sidecar; manifests
    // written outside the push path (and any unreadable sidecar) fall back
    // to the OCI manifest media type.
    let media_type = match state.storage.load_meta_json(&key).await {
        Some(sidecar) => serde_json::from_slice::<serde_json::Value>(&sidecar)
            .ok()
            .and_then(|v| v.get("media_type").and_then(|m| m.as_str()).map(str::to_owned)),
        None => None,
    };
    if method == Method::HEAD {
        return manifest_response(&digest, media_type.as_deref(), length, None);
    }
    manifest_response(&digest, media_type.as_deref(), length, Some(bytes))
}

fn manifest_response(
    digest: &str,
    media_type: Option<&str>,
    length: u64,
    body: Option<Vec<u8>>,
) -> Response {
    let builder = axum::http::response::Response::builder()
        .status(StatusCode::OK)
        .header(
            "Content-Type",
            media_type.unwrap_or("application/vnd.oci.image.manifest.v1+json"),
        )
        .header("Content-Length", length)
        .header(
            "Docker-Content-Digest",
            HeaderValue::from_str(digest).unwrap_or(HeaderValue::from_static("")),
        );
    match body {
        Some(bytes) => finish(builder, axum::body::Body::from(bytes)),
        None => finish(builder, axum::body::Body::empty()),
    }
}

/// Resolve a tag to its manifest digest via the tag-index sidecar
/// (`registry/<repo>/tags.json`, written by the S3 push path).
async fn resolve_tag(state: &ProxyState, repo: &str, tag: &str) -> Option<String> {
    let key = format!("{REGISTRY_PREFIX}/{repo}/tags.json");
    let bytes = state.storage.read(&key).await.ok()?;
    let tags: HashMap<String, String> = serde_json::from_slice(&bytes).ok()?;
    tags.get(tag).cloned()
}

/// end-8a/8b: tag listing. Tags come from the index sidecar in lexical
/// order; `n=<int>` limits the page size and `last=<tagname>` resumes
/// non-inclusively after that tag. A `Link: <...>; rel="next"` header is
/// returned when a full page was cut off and more tags remain; `n=0` MUST
/// yield an empty list and no Link header.
async fn tags_list(state: &ProxyState, repo: &str, uri: &Uri) -> Response {
    let bytes = match state.storage.read(&tags_key(repo)).await {
        Ok(b) => b,
        Err(_) => return oci_error(StatusCode::NOT_FOUND, "NAME_UNKNOWN", "repository not found"),
    };
    let tags: HashMap<String, String> = match serde_json::from_slice(&bytes) {
        Ok(t) => t,
        Err(_) => return oci_error(StatusCode::NOT_FOUND, "NAME_UNKNOWN", "repository not found"),
    };
    let mut names: Vec<String> = tags.into_keys().collect();
    names.sort();

    // `last` cuts everything at or before the named tag (non-inclusive).
    if let Some(last) = query_param(uri, "last") {
        names.retain(|name| name.as_str() > last);
    }

    // `n` caps the page. Non-numeric n is a bad request; n=0 is a legal
    // empty page (and per spec MUST NOT carry a Link header).
    let page_size = match query_param(uri, "n") {
        Some(raw) => match raw.parse::<usize>() {
            Ok(n) => Some(n),
            Err(_) => {
                return oci_error(StatusCode::BAD_REQUEST, "UNSUPPORTED", "invalid n parameter")
            }
        },
        None => None,
    };
    let mut link: Option<String> = None;
    if let Some(n) = page_size {
        if names.len() > n {
            names.truncate(n);
            if n > 0 {
                let last_returned = names.last().map(String::as_str).unwrap_or("");
                link = Some(format!(
                    "</v2/{repo}/tags/list?n={n}&last={last_returned}>; rel=\"next\""
                ));
            }
        }
    }

    let mut response = (
        StatusCode::OK,
        axum::Json(serde_json::json!({ "name": repo, "tags": names })),
    )
        .into_response();
    if let Some(link_value) = link {
        if let Ok(v) = HeaderValue::from_str(&link_value) {
            response.headers_mut().insert("Link", v);
        }
    }
    response
}

// ---------------------------------------------------------------------------
// Input validation — the traversal guard. Storage keys are built ONLY from
// strings that passed these checks, so `..`, `/`, and `:` can never reach a
// key path (slashes are split away before decode; the grammars reject the
// rest).
// ---------------------------------------------------------------------------

/// One repository path segment: `^[a-z0-9]+([._-][a-z0-9]+)*$` (OCI grammar).
fn is_valid_repo_segment(seg: &str) -> bool {
    if seg.is_empty() || seg.len() > 255 {
        return false;
    }
    let mut on_separator = true; // must start with alnum
    for b in seg.bytes() {
        match b {
            b'a'..=b'z' | b'0'..=b'9' => on_separator = false,
            b'.' | b'_' | b'-' => {
                if on_separator {
                    return false;
                }
                on_separator = true;
            }
            _ => return false,
        }
    }
    !on_separator // must end with alnum
}

/// Digest: `^[a-z0-9]+:[a-f0-9]{32,128}$` (covers sha256/sha512 lowercase hex).
fn is_valid_digest(s: &str) -> bool {
    let Some((alg, encoded)) = s.split_once(':') else {
        return false;
    };
    if alg.is_empty() || !alg.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit()) {
        return false;
    }
    (32..=128).contains(&encoded.len())
        && encoded.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Tag: `^[A-Za-z0-9_][A-Za-z0-9._-]{0,127}$` (docker tag grammar).
fn is_valid_tag(s: &str) -> bool {
    if s.is_empty() || s.len() > 128 {
        return false;
    }
    let bytes = s.as_bytes();
    if !(bytes[0].is_ascii_alphanumeric() || bytes[0] == b'_') {
        return false;
    }
    bytes[1..]
        .iter()
        .all(|&b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object_store_backend::ObjectStoreBackend;
    use axum::body::Body;

    const BLOB_HEX: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const MANIFEST_HEX: &str = "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";
    const BLOB_BODY: &[u8] = b"layer-bytes-here"; // 16 bytes
    const MANIFEST_BODY: &[u8] = b"{\"mediaType\":\"application/vnd.oci.image.manifest.v1+json\"}";

    fn blob_digest() -> String {
        format!("sha256:{BLOB_HEX}")
    }
    fn manifest_digest() -> String {
        format!("sha256:{MANIFEST_HEX}")
    }

    fn open_auth() -> Arc<monopsony_control::auth::AuthState> {
        Arc::new(monopsony_control::auth::AuthState {
            auth_mode: monopsony_common::config::AuthMode::Open,
            db: None,
        })
    }

    /// Fresh empty-registry state (own tempdir so seeds never leak between tests).
    async fn fresh_state() -> (Arc<ProxyState>, tempfile::TempDir) {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let dir = tempfile::tempdir().unwrap();
        let storage: Arc<dyn CacheStorage> = Arc::new(
            ObjectStoreBackend::local(dir.path().to_path_buf(), 0).unwrap(),
        );
        (Arc::new(ProxyState::new(storage, None)), dir)
    }

    /// Router seeded with one blob, one manifest, and a two-entry tag index.
    async fn seeded_router() -> (Router, tempfile::TempDir) {
        let (state, dir) = fresh_state().await;
        state
            .storage
            .write(&format!("registry/test/app/blobs/{}", blob_digest()), BLOB_BODY)
            .await
            .unwrap();
        state
            .storage
            .write(
                &format!("registry/test/app/manifests/{}", manifest_digest()),
                MANIFEST_BODY,
            )
            .await
            .unwrap();
        let tags = serde_json::json!({ "latest": manifest_digest(), "v1": manifest_digest() });
        state
            .storage
            .write(
                "registry/test/app/tags.json",
                &serde_json::to_vec(&tags).unwrap(),
            )
            .await
            .unwrap();
        (create_registry_router(state, open_auth()), dir)
    }

    async fn req(router: &Router, method: &str, uri: &str) -> Response {
        use tower::ServiceExt;
        router
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method(method)
                    .uri(uri)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    async fn body_bytes(resp: Response) -> bytes::Bytes {
        axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn version_check_answers_200_on_empty_registry() {
        let (state, _dir) = fresh_state().await;
        let router = create_registry_router(state, open_auth());
        for uri in ["/v2", "/v2/"] {
            let resp = req(&router, "GET", uri).await;
            assert_eq!(resp.status(), StatusCode::OK, "uri {uri}");
            assert_eq!(
                resp.headers()["docker-distribution-api-version"],
                "registry/2.0"
            );
            assert_eq!(body_bytes(resp).await, bytes::Bytes::from_static(b"{}"));
        }
    }

    #[tokio::test]
    async fn blob_head_and_get_serve_digest_and_length() {
        let (router, _dir) = seeded_router().await;
        let uri = format!("/v2/test/app/blobs/{}", blob_digest());
        let head = req(&router, "HEAD", &uri).await;
        assert_eq!(head.status(), StatusCode::OK);
        assert_eq!(head.headers()["content-length"], "16");
        assert_eq!(head.headers()["docker-content-digest"], blob_digest());
        let get = req(&router, "GET", &uri).await;
        assert_eq!(get.status(), StatusCode::OK);
        assert_eq!(body_bytes(get).await, bytes::Bytes::from_static(BLOB_BODY));
    }

    #[tokio::test]
    async fn missing_blob_404s_with_blob_unknown() {
        let (router, _dir) = seeded_router().await;
        let other = "sha256:ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff";
        let resp = req(&router, "GET", &format!("/v2/test/app/blobs/{other}")).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        assert!(String::from_utf8_lossy(&body_bytes(resp).await).contains("BLOB_UNKNOWN"));
    }

    #[tokio::test]
    async fn blob_range_serves_206_clamped_and_416_unsatisfiable() {
        let (router, _dir) = seeded_router().await;
        let uri = format!("/v2/test/app/blobs/{}", blob_digest());

        // Clamped end range: 6-999 -> 6-15.
        let resp = axum::http::Request::builder()
            .method("GET")
            .uri(&uri)
            .header("range", "bytes=6-999")
            .body(Body::empty())
            .unwrap();
        use tower::ServiceExt;
        let resp = router.clone().oneshot(resp).await.unwrap();
        assert_eq!(resp.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(resp.headers()["content-range"], "bytes 6-15/16");
        assert_eq!(resp.headers()["content-length"], "10");
        assert_eq!(body_bytes(resp).await, bytes::Bytes::from_static(b"bytes-here"));

        // Suffix form: last 5 bytes.
        let resp = axum::http::Request::builder()
            .method("GET")
            .uri(&uri)
            .header("range", "bytes=-5")
            .body(Body::empty())
            .unwrap();
        let resp = router.clone().oneshot(resp).await.unwrap();
        assert_eq!(resp.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(resp.headers()["content-range"], "bytes 11-15/16");

        // Unsatisfiable: start past EOF.
        let resp = axum::http::Request::builder()
            .method("GET")
            .uri(&uri)
            .header("range", "bytes=100-200")
            .body(Body::empty())
            .unwrap();
        let resp = router.oneshot(resp).await.unwrap();
        assert_eq!(resp.status(), StatusCode::RANGE_NOT_SATISFIABLE);
        assert_eq!(resp.headers()["content-range"], "bytes */16");
    }

    #[tokio::test]
    async fn manifest_serves_by_digest_and_by_tag() {
        let (router, _dir) = seeded_router().await;
        for reference in [manifest_digest(), "latest".to_string(), "v1".to_string()] {
            let resp = req(&router, "GET", &format!("/v2/test/app/manifests/{reference}")).await;
            assert_eq!(resp.status(), StatusCode::OK, "ref {reference}");
            assert_eq!(
                resp.headers()["docker-content-digest"],
                manifest_digest()
            );
            assert_eq!(
                body_bytes(resp).await,
                bytes::Bytes::from_static(MANIFEST_BODY)
            );
        }
    }

    #[tokio::test]
    async fn unknown_reference_and_unknown_repo_404() {
        let (router, _dir) = seeded_router().await;
        let resp = req(&router, "GET", "/v2/test/app/manifests/nosuchtag").await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        assert!(String::from_utf8_lossy(&body_bytes(resp).await).contains("MANIFEST_UNKNOWN"));

        let resp = req(&router, "GET", "/v2/other/repo/tags/list").await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        assert!(String::from_utf8_lossy(&body_bytes(resp).await).contains("NAME_UNKNOWN"));
    }

    #[tokio::test]
    async fn tags_list_serves_sorted_names() {
        let (router, _dir) = seeded_router().await;
        let resp = req(&router, "GET", "/v2/test/app/tags/list").await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_bytes(resp).await;
        let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(parsed["name"], "test/app");
        assert_eq!(parsed["tags"], serde_json::json!(["latest", "v1"]));
    }

    #[tokio::test]
    async fn invalid_digest_and_repo_are_rejected_before_storage() {
        let (router, _dir) = seeded_router().await;
        // Traversal attempt: decoded segment contains '/' and '.'.
        let resp = req(&router, "GET", "/v2/test/app/blobs/..%2F..%2Fetc%2Fpasswd").await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert!(String::from_utf8_lossy(&body_bytes(resp).await).contains("DIGEST_INVALID"));
        // Bad digest charset (uppercase hex, wrong length).
        let resp = req(&router, "GET", "/v2/test/app/blobs/sha256:ZZZZ").await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        // Uppercase repo segment violates the OCI repo grammar.
        let resp = req(
            &router,
            "GET",
            &format!("/v2/Test/app/blobs/{}", blob_digest()),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert!(String::from_utf8_lossy(&body_bytes(resp).await).contains("NAME_INVALID"));
        // Invalid tag (leading dot).
        let resp = req(&router, "GET", "/v2/test/app/manifests/.bad").await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert!(String::from_utf8_lossy(&body_bytes(resp).await).contains("TAG_INVALID"));
    }

    #[tokio::test]
    async fn percent_encoded_digest_segment_resolves() {
        let (router, _dir) = seeded_router().await;
        let resp = req(
            &router,
            "GET",
            &format!("/v2/test/app/blobs/sha256%3A{BLOB_HEX}"),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(body_bytes(resp).await, bytes::Bytes::from_static(BLOB_BODY));
    }

    #[tokio::test]
    async fn keyed_auth_rejects_without_credentials() {
        let (state, _dir) = fresh_state().await;
        let auth = Arc::new(monopsony_control::auth::AuthState {
            auth_mode: monopsony_common::config::AuthMode::Token,
            db: None, // fail-closed: no key store configured
        });
        let router = create_registry_router(state, auth);
        let resp = req(&router, "GET", "/v2/").await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    // -----------------------------------------------------------------------
    // S2: blob upload path
    // -----------------------------------------------------------------------

    async fn req_full(
        router: &Router,
        method: &str,
        uri: &str,
        body: Vec<u8>,
        headers: Vec<(&str, String)>,
    ) -> Response {
        use tower::ServiceExt;
        let mut builder = axum::http::Request::builder().method(method).uri(uri);
        for (name, value) in headers {
            builder = builder.header(name, value);
        }
        router
            .clone()
            .oneshot(builder.body(Body::from(body)).unwrap())
            .await
            .unwrap()
    }

    /// POST a fresh upload session open on `test/app`; returns its Location.
    async fn start_upload(router: &Router) -> String {
        let resp = req(router, "POST", "/v2/test/app/blobs/uploads/").await;
        assert_eq!(resp.status(), StatusCode::ACCEPTED);
        let loc = resp.headers()["location"].to_str().unwrap().to_string();
        assert!(loc.starts_with("/v2/test/app/blobs/uploads/"), "loc {loc}");
        loc
    }

    #[tokio::test]
    async fn chunked_upload_flow_end_to_end() {
        let (router, _dir) = seeded_router().await;
        let loc = start_upload(&router).await;
        let sid = loc.rsplit('/').next().unwrap();
        assert_eq!(sid.len(), 32, "uuid-simple session id");

        // PATCH first half.
        let resp = req_full(
            &router,
            "PATCH",
            &loc,
            BLOB_BODY[..8].to_vec(),
            vec![("Content-Range", "0-7".to_string())],
        )
        .await;
        assert_eq!(resp.status(), StatusCode::ACCEPTED);
        assert_eq!(resp.headers()["range"], "0-7");
        assert_eq!(
            resp.headers()["location"],
            loc,
            "Location echoed byte-exact"
        );

        // end-13 status probe.
        let resp = req(&router, "GET", &loc).await;
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
        assert_eq!(resp.headers()["range"], "0-7");

        // Final chunk rides the closing PUT.
        let digest = sha256_hex(BLOB_BODY);
        let resp = req_full(
            &router,
            "PUT",
            &format!("{loc}?digest=sha256:{digest}"),
            BLOB_BODY[8..].to_vec(),
            vec![("Content-Range", "8-15".to_string())],
        )
        .await;
        assert_eq!(resp.status(), StatusCode::CREATED, "finalize failed");
        assert_eq!(resp.headers()["docker-content-digest"], format!("sha256:{digest}"));
        assert_eq!(
            resp.headers()["location"],
            format!("/v2/test/app/blobs/sha256:{digest}")
        );

        // Served through the S1 read path, content-addressed.
        let resp = req(&router, "GET", &format!("/v2/test/app/blobs/sha256:{digest}")).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(body_bytes(resp).await, bytes::Bytes::from_static(BLOB_BODY));

        // Session is closed: another op on it 404s.
        let resp = req(&router, "GET", &loc).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn out_of_order_chunk_416_and_malformed_range_400() {
        let (router, _dir) = seeded_router().await;
        let loc = start_upload(&router).await;

        let resp = req_full(
            &router,
            "PATCH",
            &loc,
            vec![0, 1, 2, 3],
            vec![("Content-Range", "0-3".to_string())],
        )
        .await;
        assert_eq!(resp.status(), StatusCode::ACCEPTED);

        // Out of order: next chunk must start at 4, not 100.
        let resp = req_full(
            &router,
            "PATCH",
            &loc,
            vec![5, 6],
            vec![("Content-Range", "100-101".to_string())],
        )
        .await;
        assert_eq!(resp.status(), StatusCode::RANGE_NOT_SATISFIABLE);

        // Malformed: the OCI range is bare `start-end`, not the HTTP `bytes=` form.
        let resp = req_full(
            &router,
            "PATCH",
            &loc,
            vec![4],
            vec![("Content-Range", "bytes=4-4".to_string())],
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert!(String::from_utf8_lossy(&body_bytes(resp).await).contains("BLOB_UPLOAD_INVALID"));
    }

    #[tokio::test]
    async fn session_is_bound_to_its_repository() {
        let (router, _dir) = seeded_router().await;
        let loc = start_upload(&router).await;
        let sid = loc.rsplit('/').next().unwrap();
        let resp = req_full(
            &router,
            "PATCH",
            &format!("/v2/other/repo/blobs/uploads/{sid}"),
            vec![1],
            vec![("Content-Range", "0-0".to_string())],
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert!(String::from_utf8_lossy(&body_bytes(resp).await).contains("BLOB_UPLOAD_INVALID"));
    }

    #[tokio::test]
    async fn digest_mismatch_publishes_nothing_and_closes_session() {
        let (router, _dir) = seeded_router().await;
        let loc = start_upload(&router).await;
        let resp = req_full(
            &router,
            "PATCH",
            &loc,
            BLOB_BODY.to_vec(),
            vec![("Content-Range", format!("0-{}", BLOB_BODY.len() - 1))],
        )
        .await;
        assert_eq!(resp.status(), StatusCode::ACCEPTED);

        let wrong = format!("sha256:{}", "0".repeat(64));
        let resp = req_full(&router, "PUT", &format!("{loc}?digest={wrong}"), Vec::new(), vec![])
            .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert!(String::from_utf8_lossy(&body_bytes(resp).await).contains("DIGEST_INVALID"));

        // Nothing published under the declared (wrong) digest…
        let resp = req(&router, "GET", &format!("/v2/test/app/blobs/{wrong}")).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        // …and the session is gone (finalize consumes it either way).
        let resp = req(&router, "GET", &loc).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn monolithic_post_uploads_blob_inline() {
        let (router, _dir) = seeded_router().await;
        let digest = sha256_hex(BLOB_BODY);
        let resp = req_full(
            &router,
            "POST",
            &format!("/v2/test/app/blobs/uploads/?digest=sha256:{digest}"),
            BLOB_BODY.to_vec(),
            vec![],
        )
        .await;
        assert_eq!(resp.status(), StatusCode::CREATED);
        let resp = req(&router, "GET", &format!("/v2/test/app/blobs/sha256:{digest}")).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(body_bytes(resp).await, bytes::Bytes::from_static(BLOB_BODY));
    }

    #[tokio::test]
    async fn chunkless_put_pushes_empty_blob() {
        let (router, _dir) = seeded_router().await;
        let loc = start_upload(&router).await;
        // sha256 of the empty string.
        let empty_digest = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        let resp = req_full(
            &router,
            "PUT",
            &format!("{loc}?digest=sha256:{empty_digest}"),
            Vec::new(),
            vec![],
        )
        .await;
        assert_eq!(resp.status(), StatusCode::CREATED);
        let resp = req(&router, "GET", &format!("/v2/test/app/blobs/sha256:{empty_digest}")).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.headers()["content-length"], "0");
    }

    #[tokio::test]
    async fn delete_cancels_session_and_fresh_router_404s() {
        let (state, _dir) = fresh_state().await;
        let auth = open_auth();
        let uploads = Arc::new(UploadSessions::new());
        let router = create_registry_router_with_sessions(state.clone(), auth.clone(), uploads);
        let loc = start_upload(&router).await;

        let resp = req(&router, "DELETE", &loc).await;
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);

        let digest = sha256_hex(BLOB_BODY);
        let resp = req(&router, "PUT", &format!("{loc}?digest=sha256:{digest}")).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        assert!(String::from_utf8_lossy(&body_bytes(resp).await).contains("BLOB_UPLOAD_UNKNOWN"));

        // "Restart": a fresh router with a fresh session store loses the id.
        let fresh = create_registry_router(state, auth);
        let resp = req(&fresh, "GET", &loc).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        // Unknown session ids 404 as BLOB_UPLOAD_UNKNOWN too.
        let resp = req_full(
            &fresh,
            "PATCH",
            "/v2/test/app/blobs/uploads/deadbeefdeadbeefdeadbeefdeadbeef",
            vec![1],
            vec![("Content-Range", "0-0".to_string())],
        )
        .await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn sweep_aborts_idle_sessions() {
        let (state, _dir) = fresh_state().await;
        let uploads = Arc::new(UploadSessions::new());
        let router = create_registry_router_with_sessions(state, open_auth(), uploads.clone());
        let loc = start_upload(&router).await;
        let sid = loc.rsplit('/').next().unwrap().to_string();

        // Backdate past the TTL, then sweep.
        {
            let mut map = uploads.inner.lock();
            map.get_mut(&sid).unwrap().last_activity = Instant::now() - Duration::from_secs(3600);
        }
        uploads.sweep(Duration::from_secs(60)).await;
        assert!(uploads.inner.lock().get(&sid).is_none(), "swept");

        let resp = req_full(
            &router,
            "PATCH",
            &loc,
            vec![1, 2],
            vec![("Content-Range", "0-1".to_string())],
        )
        .await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn basic_password_extracts_the_credential() {
        let engine = base64::engine::general_purpose::STANDARD;
        let cred = engine.encode("monopsony:sekret-key");
        let hv = HeaderValue::from_str(&format!("Basic {cred}")).unwrap();
        assert_eq!(basic_password(Some(&hv)).as_deref(), Some("sekret-key"));

        // Non-Basic scheme untouched.
        let hv = HeaderValue::from_static("Bearer abc");
        assert_eq!(basic_password(Some(&hv)), None);
        // No colon in the decoded credential.
        let cred = engine.encode("nocolon");
        let hv = HeaderValue::from_str(&format!("Basic {cred}")).unwrap();
        assert_eq!(basic_password(Some(&hv)), None);
        // Malformed base64.
        let hv = HeaderValue::from_static("Basic !!!not-base64!!!");
        assert_eq!(basic_password(Some(&hv)), None);
        // Absent header.
        assert_eq!(basic_password(None), None);
    }

    #[tokio::test]
    async fn keyed_mode_rejects_basic_credentials_without_key_store() {
        let (state, _dir) = fresh_state().await;
        let auth = Arc::new(monopsony_control::auth::AuthState {
            auth_mode: monopsony_common::config::AuthMode::Token,
            db: None, // fail-closed: no key store configured
        });
        let router = create_registry_router(state, auth);
        let cred = base64::engine::general_purpose::STANDARD.encode("monopsony:any-key");
        let resp = req_full(
            &router,
            "GET",
            "/v2/",
            Vec::new(),
            vec![("Authorization", format!("Basic {cred}"))],
        )
        .await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    // -----------------------------------------------------------------------
    // S3: manifest push (end-7a/7b), tag index, tagless auto-version
    // -----------------------------------------------------------------------

    /// A manifest body with deliberately irregular spacing — byte-exactness
    /// means the GET must return THESE bytes, not a canonicalized form.
    const S3_MANIFEST: &[u8] = b"{  \"mediaType\" : \"application/vnd.oci.image.manifest.v1+json\" , \"schemaVersion\": 2 , \"config\":{  } }";
    const OCI_MANIFEST_TYPE: &str = "application/vnd.oci.image.manifest.v1+json";
    const DOCKER_MANIFEST_TYPE: &str = "application/vnd.docker.distribution.manifest.v2+json";

    async fn push_manifest(
        router: &Router,
        uri: &str,
        body: &[u8],
        content_type: Option<&str>,
    ) -> Response {
        let headers = match content_type {
            Some(ct) => vec![("Content-Type", ct.to_string())],
            None => Vec::new(),
        };
        req_full(router, "PUT", uri, body.to_vec(), headers).await
    }

    #[tokio::test]
    async fn manifest_put_by_tag_round_trips_byte_exact() {
        let (router, _dir) = seeded_router().await;
        let digest = sha256_hex(S3_MANIFEST);
        let resp = push_manifest(
            &router,
            "/v2/test/app/manifests/v1.5",
            S3_MANIFEST,
            Some(OCI_MANIFEST_TYPE),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::CREATED);
        assert_eq!(
            resp.headers()["location"],
            format!("/v2/test/app/manifests/sha256:{digest}")
        );
        assert_eq!(resp.headers()["docker-content-digest"], format!("sha256:{digest}"));

        // GET by tag AND by digest: exactly the pushed bytes, never
        // re-serialized.
        for reference in ["v1.5", &format!("sha256:{digest}")] {
            let resp = req(&router, "GET", &format!("/v2/test/app/manifests/{reference}")).await;
            assert_eq!(resp.status(), StatusCode::OK, "ref {reference}");
            assert_eq!(body_bytes(resp).await, bytes::Bytes::from_static(S3_MANIFEST));
        }
        // HEAD carries the length of the exact bytes.
        let resp = req(&router, "HEAD", "/v2/test/app/manifests/v1.5").await;
        assert_eq!(resp.headers()["content-length"], S3_MANIFEST.len().to_string());

        // The tag index lists the new tag.
        let resp = req(&router, "GET", "/v2/test/app/tags/list").await;
        let parsed: serde_json::Value =
            serde_json::from_slice(&body_bytes(resp).await).unwrap();
        assert!(parsed["tags"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t == "v1.5"));
    }

    #[tokio::test]
    async fn manifest_put_content_type_must_match_mediatype() {
        let (router, _dir) = seeded_router().await;
        let resp = push_manifest(
            &router,
            "/v2/test/app/manifests/v9",
            S3_MANIFEST,
            Some(DOCKER_MANIFEST_TYPE),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert!(String::from_utf8_lossy(&body_bytes(resp).await).contains("MANIFEST_INVALID"));
        // Nothing was stored or tagged.
        let resp = req(&router, "GET", "/v2/test/app/manifests/v9").await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn manifest_put_oversize_413_and_boundary_400() {
        let (router, _dir) = seeded_router().await;
        // One byte over the cap → 413 (rejected before JSON parsing).
        let oversize = vec![b'x'; MAX_MANIFEST_SIZE + 1];
        let resp = push_manifest(&router, "/v2/test/app/manifests/big", &oversize, None).await;
        assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
        // Exactly the cap is ACCEPTED by the size check — a full-size
        // non-JSON payload fails JSON parsing with 400, proving the size
        // gate passed it through.
        let boundary = vec![b'x'; MAX_MANIFEST_SIZE];
        let resp = push_manifest(&router, "/v2/test/app/manifests/edge", &boundary, None).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert!(String::from_utf8_lossy(&body_bytes(resp).await).contains("MANIFEST_INVALID"));
    }

    #[tokio::test]
    async fn manifest_put_rejects_non_json() {
        let (router, _dir) = seeded_router().await;
        let resp = push_manifest(&router, "/v2/test/app/manifests/v1", b"not json", None).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert!(String::from_utf8_lossy(&body_bytes(resp).await).contains("MANIFEST_INVALID"));
    }

    #[tokio::test]
    async fn manifest_put_by_digest_mismatch_rejected_and_unstored() {
        let (router, _dir) = seeded_router().await;
        let real = sha256_hex(S3_MANIFEST);
        let wrong = format!("sha256:{}", "0".repeat(64));
        let resp = push_manifest(&router, &format!("/v2/test/app/manifests/{wrong}"), S3_MANIFEST, None)
            .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert!(String::from_utf8_lossy(&body_bytes(resp).await).contains("DIGEST_INVALID"));
        // The correct digest was NOT stored — nothing published.
        let resp = req(&router, "GET", &format!("/v2/test/app/manifests/sha256:{real}")).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn manifest_put_unsupported_digest_algorithm_rejected() {
        let (router, _dir) = seeded_router().await;
        let sha512 = format!("sha512:{}", "a".repeat(128));
        let resp =
            push_manifest(&router, &format!("/v2/test/app/manifests/{sha512}"), S3_MANIFEST, None)
                .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert!(String::from_utf8_lossy(&body_bytes(resp).await).contains("DIGEST_INVALID"));
    }

    /// Image-manifest JSON referencing a config and one layer by digest.
    fn image_manifest(config_hex: &str, layer_hex: &str) -> Vec<u8> {
        serde_json::json!({
            "schemaVersion": 2,
            "mediaType": OCI_MANIFEST_TYPE,
            "config": {
                "mediaType": "application/vnd.oci.image.config.v1+json",
                "digest": format!("sha256:{config_hex}"),
                "size": 12
            },
            "layers": [
                { "mediaType": "application/vnd.oci.image.layer.v1.tar",
                  "digest": format!("sha256:{layer_hex}"), "size": 11 }
            ]
        })
        .to_string()
        .into_bytes()
    }

    #[tokio::test]
    async fn manifest_put_tagless_push_auto_versions() {
        let (state, _dir) = fresh_state().await;
        let router = create_registry_router(state.clone(), open_auth());
        let digest = sha256_hex(S3_MANIFEST);
        let resp = push_manifest(
            &router,
            &format!("/v2/test/app/manifests/sha256:{digest}"),
            S3_MANIFEST,
            Some(OCI_MANIFEST_TYPE),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::CREATED);

        // The auto-version tag rides OCI-Tag: `YYYYMMDD-HHMMSS` (UTC).
        let auto_tag = resp.headers()["oci-tag"].to_str().unwrap().to_string();
        assert_eq!(auto_tag.len(), 15, "YYYYMMDD-HHMMSS shape");
        assert_eq!(&auto_tag[8..9], "-", "date/time separator at index 8");
        let (date, time) = auto_tag.split_once('-').unwrap();
        assert_eq!(date.len(), 8, "YYYYMMDD");
        assert_eq!(time.len(), 6, "HHMMSS");
        assert!(date.bytes().all(|b| b.is_ascii_digit()));
        assert!(time.bytes().all(|b| b.is_ascii_digit()));

        // The tag listing has exactly the auto tag; resolving it serves the
        // manifest bytes.
        let resp = req(&router, "GET", "/v2/test/app/tags/list").await;
        let parsed: serde_json::Value = serde_json::from_slice(&body_bytes(resp).await).unwrap();
        assert_eq!(parsed["tags"], serde_json::json!([auto_tag]));
        let resp = req(&router, "GET", &format!("/v2/test/app/manifests/{auto_tag}")).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(body_bytes(resp).await, bytes::Bytes::from_static(S3_MANIFEST));

        // The autotags sidecar records it (S4 never-delete contract).
        let autos: Vec<String> = serde_json::from_slice(
            &state
                .storage
                .read(&autotags_key("test/app"))
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(autos, vec![auto_tag]);
    }

    #[tokio::test]
    async fn manifest_put_extra_tags_apply_and_echo() {
        let (router, _dir) = seeded_router().await;
        let digest = sha256_hex(S3_MANIFEST);
        let uri = format!("/v2/test/app/manifests/sha256:{digest}?tag=rel1&tag=rel2");
        let resp = push_manifest(&router, &uri, S3_MANIFEST, None).await;
        assert_eq!(resp.status(), StatusCode::CREATED);
        // One OCI-Tag header per accepted tag: the auto tag plus the extras.
        let headers: Vec<String> = resp
            .headers()
            .get_all("oci-tag")
            .iter()
            .map(|v| v.to_str().unwrap().to_string())
            .collect();
        assert_eq!(headers.len(), 3);
        assert!(headers.contains(&"rel1".to_string()));
        assert!(headers.contains(&"rel2".to_string()));
        for tag in ["rel1", "rel2"] {
            let resp = req(&router, "GET", &format!("/v2/test/app/manifests/{tag}")).await;
            assert_eq!(resp.status(), StatusCode::OK, "tag {tag}");
            assert_eq!(body_bytes(resp).await, bytes::Bytes::from_static(S3_MANIFEST));
        }
    }

    #[tokio::test]
    async fn manifest_put_supports_at_least_ten_extra_tags() {
        // Spec end-7b: at least 10 tags per request SHOULD be supported.
        let (router, _dir) = seeded_router().await;
        let digest = sha256_hex(S3_MANIFEST);
        let query: Vec<String> = (0..11).map(|i| format!("tag=rel{i:02}")).collect();
        let uri = format!("/v2/test/app/manifests/sha256:{digest}?{}", query.join("&"));
        let resp = push_manifest(&router, &uri, S3_MANIFEST, None).await;
        assert_eq!(resp.status(), StatusCode::CREATED, "11 tags accepted");
        // 11 extras + the auto tag.
        assert_eq!(resp.headers().get_all("oci-tag").iter().count(), 12);
        let resp = req(&router, "GET", "/v2/test/app/tags/list").await;
        let parsed: serde_json::Value = serde_json::from_slice(&body_bytes(resp).await).unwrap();
        // The seeded index's two tags + the 12 just applied.
        assert_eq!(parsed["tags"].as_array().unwrap().len(), 14);
    }

    #[tokio::test]
    async fn manifest_put_rejects_invalid_extra_tag_before_storage() {
        let (router, _dir) = seeded_router().await;
        let digest = sha256_hex(S3_MANIFEST);
        let uri = format!("/v2/test/app/manifests/sha256:{digest}?tag=bad~tag");
        let resp = push_manifest(&router, &uri, S3_MANIFEST, None).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert!(String::from_utf8_lossy(&body_bytes(resp).await).contains("TAG_INVALID"));
        // Validated BEFORE anything is stored.
        let resp = req(&router, "GET", &format!("/v2/test/app/manifests/sha256:{digest}")).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn manifest_put_missing_blob_rejected() {
        let (router, _dir) = seeded_router().await;
        let cfg = sha256_hex(b"config-bytes");
        let layer = sha256_hex(b"layer-bytes");
        let body = image_manifest(&cfg, &layer);
        let resp = push_manifest(
            &router,
            "/v2/test/app/manifests/v1",
            &body,
            Some(OCI_MANIFEST_TYPE),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        let body = body_bytes(resp).await;
        let text = String::from_utf8_lossy(&body);
        assert!(text.contains("MANIFEST_BLOB_UNKNOWN"));
        assert!(text.contains(&cfg), "error names the missing digest");
        // The rejected push stored nothing and tagged nothing new (the
        // seeded router already carries `v1`, so a fresh name is asserted).
        let resp = req(&router, "GET", "/v2/test/app/manifests/fresh").await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        let resp = req(&router, "GET", "/v2/test/app/tags/list").await;
        let parsed: serde_json::Value = serde_json::from_slice(&body_bytes(resp).await).unwrap();
        let listed = parsed["tags"].as_array().unwrap();
        assert!(
            !listed.iter().any(|t| t == "fresh"),
            "rejected push tagged nothing new"
        );
    }

    #[tokio::test]
    async fn manifest_put_succeeds_when_referenced_blobs_present() {
        let (state, _dir) = fresh_state().await;
        let router = create_registry_router(state.clone(), open_auth());
        let cfg = sha256_hex(b"config-bytes");
        let layer = sha256_hex(b"layer-bytes");
        state
            .storage
            .write(&blob_key("test/app", &format!("sha256:{cfg}")), b"config-bytes")
            .await
            .unwrap();
        state
            .storage
            .write(&blob_key("test/app", &format!("sha256:{layer}")), b"layer-bytes")
            .await
            .unwrap();
        let body = image_manifest(&cfg, &layer);
        let resp = push_manifest(
            &router,
            "/v2/test/app/manifests/v1",
            &body,
            Some(OCI_MANIFEST_TYPE),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::CREATED);
    }

    #[tokio::test]
    async fn manifest_put_index_children_validated() {
        let (state, _dir) = fresh_state().await;
        let router = create_registry_router(state.clone(), open_auth());
        // The child manifest's bytes define its digest; the index must
        // reference exactly that digest.
        let child: Vec<u8> = b"{\"schemaVersion\": 2}".to_vec();
        let child_hex = sha256_hex(&child);
        let index_type = "application/vnd.oci.image.index.v1+json";
        let index = serde_json::json!({
            "schemaVersion": 2,
            "mediaType": index_type,
            "manifests": [
                { "mediaType": OCI_MANIFEST_TYPE,
                  "digest": format!("sha256:{child_hex}"), "size": 20 }
            ]
        })
        .to_string()
        .into_bytes();
        // Child absent → rejected.
        let resp =
            push_manifest(&router, "/v2/test/app/manifests/idx", &index, Some(index_type)).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        assert!(String::from_utf8_lossy(&body_bytes(resp).await).contains("MANIFEST_BLOB_UNKNOWN"));
        // Push the child at its own digest, then the index succeeds.
        let resp = push_manifest(
            &router,
            &format!("/v2/test/app/manifests/sha256:{child_hex}"),
            &child,
            None,
        )
        .await;
        assert_eq!(resp.status(), StatusCode::CREATED);
        let resp =
            push_manifest(&router, "/v2/test/app/manifests/idx", &index, Some(index_type)).await;
        assert_eq!(resp.status(), StatusCode::CREATED);
    }

    #[tokio::test]
    async fn manifest_put_subject_may_reference_missing_manifest() {
        // Spec: referrer manifests MUST be accepted even when the subject
        // does not exist yet (manifest and referrers push in either order).
        let (router, _dir) = seeded_router().await;
        let subject_hex = sha256_hex(b"subject-manifest");
        let body = serde_json::json!({
            "schemaVersion": 2,
            "mediaType": OCI_MANIFEST_TYPE,
            "subject": {
                "mediaType": OCI_MANIFEST_TYPE,
                "digest": format!("sha256:{subject_hex}"),
                "size": 9
            }
        })
        .to_string()
        .into_bytes();
        let resp = push_manifest(
            &router,
            "/v2/test/app/manifests/signature",
            &body,
            Some(OCI_MANIFEST_TYPE),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::CREATED);
    }

    #[tokio::test]
    async fn allow_missing_blobs_knob_permits_lazy_push() {
        let (state, _dir) = fresh_state().await;
        let uploads = Arc::new(UploadSessions::new());
        let router = create_registry_router_with_settings(
            state,
            open_auth(),
            uploads,
            RegistrySettings {
                allow_missing_blobs: true,
            },
        );
        let cfg = sha256_hex(b"absent-config");
        let body = image_manifest(&cfg, &cfg);
        let resp = push_manifest(
            &router,
            "/v2/test/app/manifests/lazy",
            &body,
            Some(OCI_MANIFEST_TYPE),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::CREATED, "lazy push accepted");
    }

    #[tokio::test]
    async fn manifest_get_serves_persisted_media_type() {
        let (router, _dir) = seeded_router().await;
        let body = serde_json::json!({
            "schemaVersion": 2,
            "mediaType": DOCKER_MANIFEST_TYPE
        })
        .to_string()
        .into_bytes();
        let resp = push_manifest(
            &router,
            "/v2/test/app/manifests/docker",
            &body,
            Some(DOCKER_MANIFEST_TYPE),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::CREATED);
        for method in ["GET", "HEAD"] {
            let resp = req(&router, method, "/v2/test/app/manifests/docker").await;
            assert_eq!(resp.headers()["content-type"], DOCKER_MANIFEST_TYPE, "{method}");
        }
        // A manifest written outside the push path (no sidecar) keeps the
        // S1 default media type.
        let resp = req(&router, "GET", "/v2/test/app/manifests/v1").await;
        assert_eq!(resp.headers()["content-type"], OCI_MANIFEST_TYPE);
    }

    #[tokio::test]
    async fn tags_list_pagination_n_and_last() {
        let (state, _dir) = fresh_state().await;
        // Lexical order: alpha, beta, delta, epsilon, gamma.
        let tags: HashMap<String, String> = ["alpha", "beta", "gamma", "delta", "epsilon"]
            .iter()
            .map(|t| (t.to_string(), manifest_digest()))
            .collect();
        state
            .storage
            .write(&tags_key("test/app"), &serde_json::to_vec(&tags).unwrap())
            .await
            .unwrap();
        let router = create_registry_router(state, open_auth());

        // n=2: first two lexically + Link rel=next anchored at the last tag.
        let resp = req(&router, "GET", "/v2/test/app/tags/list?n=2").await;
        let link = resp.headers()["link"].to_str().unwrap().to_string();
        let parsed: serde_json::Value = serde_json::from_slice(&body_bytes(resp).await).unwrap();
        assert_eq!(parsed["tags"], serde_json::json!(["alpha", "beta"]));
        assert!(link.contains("rel=\"next\""), "link {link}");
        assert!(link.contains("last=beta"), "link {link}");

        // Resume non-inclusively after beta.
        let resp = req(&router, "GET", "/v2/test/app/tags/list?n=2&last=beta").await;
        let link = resp.headers()["link"].to_str().unwrap().to_string();
        let parsed: serde_json::Value = serde_json::from_slice(&body_bytes(resp).await).unwrap();
        assert_eq!(parsed["tags"], serde_json::json!(["delta", "epsilon"]));
        // gamma still remains → another Link.
        assert!(link.contains("last=epsilon"), "link {link}");

        // Final page: no Link header.
        let resp = req(&router, "GET", "/v2/test/app/tags/list?n=2&last=epsilon").await;
        assert!(resp.headers().get("link").is_none());
        let parsed: serde_json::Value = serde_json::from_slice(&body_bytes(resp).await).unwrap();
        assert_eq!(parsed["tags"], serde_json::json!(["gamma"]));

        // n=0 → empty list, and per spec NO Link header.
        let resp = req(&router, "GET", "/v2/test/app/tags/list?n=0").await;
        assert!(resp.headers().get("link").is_none());
        let parsed: serde_json::Value = serde_json::from_slice(&body_bytes(resp).await).unwrap();
        assert_eq!(parsed["tags"], serde_json::json!([]));

        // n larger than the repo: everything, no Link.
        let resp = req(&router, "GET", "/v2/test/app/tags/list?n=99").await;
        assert!(resp.headers().get("link").is_none());
        let parsed: serde_json::Value = serde_json::from_slice(&body_bytes(resp).await).unwrap();
        assert_eq!(parsed["tags"].as_array().unwrap().len(), 5);

        // Non-numeric n → 400.
        let resp = req(&router, "GET", "/v2/test/app/tags/list?n=abc").await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn manifest_delete_not_allowed_until_s4() {
        let (router, _dir) = seeded_router().await;
        let resp = req(&router, "DELETE", "/v2/test/app/manifests/v1").await;
        assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
    }

    #[tokio::test]
    async fn repush_same_tag_remaps_to_new_digest() {
        let (router, _dir) = seeded_router().await;
        let first: &[u8] = b"{\"v\": 1}";
        let second: &[u8] = b"{\"v\": 2}";
        let resp = push_manifest(&router, "/v2/test/app/manifests/rolling", first, None).await;
        assert_eq!(resp.status(), StatusCode::CREATED);
        let resp = push_manifest(&router, "/v2/test/app/manifests/rolling", second, None).await;
        assert_eq!(resp.status(), StatusCode::CREATED);
        // The tag resolves to the newest push…
        let resp = req(&router, "GET", "/v2/test/app/manifests/rolling").await;
        assert_eq!(body_bytes(resp).await, bytes::Bytes::from_static(second));
        // …while the first manifest stays addressable by digest.
        let first_digest = sha256_hex(first);
        let resp = req(
            &router,
            "GET",
            &format!("/v2/test/app/manifests/sha256:{first_digest}"),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
    }
}
