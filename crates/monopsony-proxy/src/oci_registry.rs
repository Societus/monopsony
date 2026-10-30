//! Personal OCI registry — Sprint 25 Slice 1: pull-back (read-only).
//!
//! Serves pushed content from CacheStorage over the OCI distribution-spec
//! end-points this slice implements: version check (end-1), blob GET/HEAD
//! (end-2), manifest GET/HEAD (end-3), tags list (end-8a). The write path
//! (upload sessions, manifest push, tagless auto-version) lands in S2/S3.
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
    let app = create_registry_router_with_sessions(state, auth_state, uploads);
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
/// `run_registry` passes the same store its TTL sweeper walks.
pub fn create_registry_router_with_sessions(
    state: Arc<ProxyState>,
    auth_state: Arc<monopsony_control::auth::AuthState>,
    uploads: Arc<UploadSessions>,
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
        .with_state(RegistryState { proxy: state, uploads })
}

/// Router state: shared proxy state plus the registry-owned upload-session
/// store. `FromRef` keeps every S1 handler extracting `State<Arc<ProxyState>>`.
#[derive(Clone)]
struct RegistryState {
    proxy: Arc<ProxyState>,
    uploads: Arc<UploadSessions>,
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
        ("tags", "list") => tags_list(&state, &repo).await,
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
    if method == Method::HEAD {
        return manifest_response(&digest, length, None);
    }
    manifest_response(&digest, length, Some(bytes))
}

fn manifest_response(digest: &str, length: u64, body: Option<Vec<u8>>) -> Response {
    let builder = axum::http::response::Response::builder()
        .status(StatusCode::OK)
        .header(
            "Content-Type",
            // S3 persists the real media type alongside the manifest; until
            // then the OCI manifest media type is the correct default for
            // everything the push path (S2/S3) will store.
            "application/vnd.oci.image.manifest.v1+json",
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

async fn tags_list(state: &ProxyState, repo: &str) -> Response {
    let key = format!("{REGISTRY_PREFIX}/{repo}/tags.json");
    let bytes = match state.storage.read(&key).await {
        Ok(b) => b,
        Err(_) => return oci_error(StatusCode::NOT_FOUND, "NAME_UNKNOWN", "repository not found"),
    };
    let tags: HashMap<String, String> = match serde_json::from_slice(&bytes) {
        Ok(t) => t,
        Err(_) => return oci_error(StatusCode::NOT_FOUND, "NAME_UNKNOWN", "repository not found"),
    };
    let mut names: Vec<String> = tags.into_keys().collect();
    names.sort();
    (
        StatusCode::OK,
        axum::Json(serde_json::json!({ "name": repo, "tags": names })),
    )
        .into_response()
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

    fn sha256_hex(bytes: &[u8]) -> String {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        format!("{:x}", hasher.finalize())
    }

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
}
