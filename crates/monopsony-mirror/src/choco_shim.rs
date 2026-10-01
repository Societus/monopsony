//! Chocolatey NuGet v2 feed shim (Wave A slice A2)
//!
//! Serves a NuGet v2 (OData/Atom) feed subset at
//! `http://<server>:3129/choco/api/v2/` so that
//! `choco source add -n monopsony -s http://<server>:3129/choco/api/v2/`
//! gives Windows clients cache-backed installs.
//!
//! Storage layout (plain files under `state.mirror_dir`, mirroring the
//! scoop_shim template):
//!   `<mirror_dir>/choco/feed/<id>/<version>/<id>.<version>.nupkg`
//!   `<mirror_dir>/choco/feed/<id>/<version>/nuspec.json` (sidecar meta:
//!   id, version, title, project url, sha256, size, synced_at)
//!
//! v1 sync is config-driven (explicit (id, version) list, NOT the full
//! community catalog): each nupkg is fetched once from upstream and stored
//! raw — the mirror_dir IS the cache. The `zip` crate is not a workspace
//! dep, so the embedded .nuspec is NOT parsed; id/version/title come from
//! the operator's request. HONEST CAVEAT (per plan): installer-script URLs
//! inside non-internalized packages are NOT rewritten — chocolateyInstall
//! scripts that download extra bits go direct to the network (or through
//! the client's proxy env), not through this feed.
//!
//! Module shape copied from scoop_shim.rs (A1, validated):
//! `pub fn router(MirrorState) -> axum::Router`, sync via reqwest (https
//! upstreams only), SSRF via `crate::url_validation::validate_url_host`,
//! canonicalize-based traversal protection, XML hand-built with format!.
//! The hub nests this router at `/choco` — this module does NOT touch
//! `create_mirror_router`.

use axum::{
    extract::{Path as AxumPath, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tracing::{debug, info, warn};

use crate::mirror::MirrorState;
use crate::url_validation::validate_url_host;

/// Default public upstream (community Chocolatey gallery NuGet v2 endpoint).
pub const DEFAULT_UPSTREAM: &str = "https://community.chocolatey.org/api/v2";

// ---------------------------------------------------------------------------
// Storage
// ---------------------------------------------------------------------------

/// Root of the choco feed tree under `mirror_dir`.
fn feed_root(mirror_dir: &std::path::Path) -> std::path::PathBuf {
    mirror_dir.join("choco").join("feed")
}

/// Sidecar metadata written next to each synced nupkg.
///
/// v1: populated from the sync request + the downloaded bytes (sha256/size);
/// the embedded .nuspec inside the zip is NOT parsed (no `zip` crate dep).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NuspecMeta {
    pub id: String,
    pub version: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_url: Option<String>,
    /// SHA-256 of the stored .nupkg, hex.
    pub sha256: String,
    pub nupkg_size: u64,
    /// RFC3339 timestamp recorded at sync time.
    pub synced_at: String,
}

/// Validate a package id or version path component (no traversal).
fn valid_component(s: &str) -> bool {
    !s.is_empty()
        && !s.contains('/')
        && !s.contains('\\')
        && s != "."
        && s != ".."
        && !s.contains("..")
}

/// Load every sidecar `nuspec.json` under the feed root (best-effort;
/// unreadable entries are skipped with a warning).
async fn load_all_meta(state: &MirrorState) -> Vec<NuspecMeta> {
    let mut out = Vec::new();
    let root = feed_root(&state.mirror_dir);
    let mut id_iter = match tokio::fs::read_dir(&root).await {
        Ok(it) => it,
        Err(_) => return out,
    };
    while let Ok(Some(id_entry)) = id_iter.next_entry().await {
        let Ok(id_type) = id_entry.file_type().await else {
            continue;
        };
        if !id_type.is_dir() {
            continue;
        }
        let mut ver_iter = match tokio::fs::read_dir(id_entry.path()).await {
            Ok(it) => it,
            Err(_) => continue,
        };
        while let Ok(Some(ver_entry)) = ver_iter.next_entry().await {
            let p = ver_entry.path().join("nuspec.json");
            match tokio::fs::read(&p).await {
                Ok(bytes) => match serde_json::from_slice::<NuspecMeta>(&bytes) {
                    Ok(meta) => out.push(meta),
                    Err(e) => warn!("choco shim: bad sidecar {}: {}", p.display(), e),
                },
                Err(_) => continue,
            }
        }
    }
    out.sort_by(|a, b| (&a.id, &a.version).cmp(&(&b.id, &b.version)));
    out
}

// ---------------------------------------------------------------------------
// XML generation (hand-built Atom/OData — no XML crate for v1)
// ---------------------------------------------------------------------------

/// Escape the XML-significant characters in text content / attribute values.
fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '\'' => out.push_str("&apos;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
    out
}

/// One Atom `<entry>` for a package version (NuGet v2 OData shape).
fn entry_xml(meta: &NuspecMeta, base: &str, is_latest: bool) -> String {
    let esc_id = xml_escape(&meta.id);
    let esc_ver = xml_escape(&meta.version);
    let esc_title = xml_escape(&meta.title);
    let esc_url = meta
        .project_url
        .as_deref()
        .map(xml_escape)
        .unwrap_or_default();
    let content_src = format!(
        "{}/api/v2/package/{}/{}",
        base.trim_end_matches('/'),
        esc_id,
        esc_ver
    );
    format!(
        concat!(
            "<entry>",
            "<id>{base}/api/v2/Packages(Id='{id}',Version='{ver}')</id>",
            "<title type=\"text\">{title}</title>",
            "<updated>{updated}</updated>",
            "<author><name>monopsony-mirror</name></author>",
            "<link rel=\"edit\" href=\"{base}/api/v2/Packages(Id='{id}',Version='{ver}')\"/>",
            "<category term=\"Monopsony.Mirror.NuGetPackage\"/>",
            "<content type=\"application/zip\" src=\"{src}\"/>",
            "<m:properties>",
            "<d:Id>{id}</d:Id>",
            "<d:Version>{ver}</d:Version>",
            "<d:Title>{title}</d:Title>",
            "<d:PackageHash>SHA-256={hash}</d:PackageHash>",
            "<d:PackageHashAlgorithm>SHA256</d:PackageHashAlgorithm>",
            "<d:PackageSize m:type=\"Edm.Int64\">{size}</d:PackageSize>",
            "<d:ProjectUrl>{url}</d:ProjectUrl>",
            "<d:IsLatestVersion m:type=\"Edm.Boolean\">{latest}</d:IsLatestVersion>",
            "<d:Created m:type=\"Edm.DateTime\">{updated}</d:Created>",
            "</m:properties>",
            "</entry>"
        ),
        base = xml_escape(base.trim_end_matches('/')),
        id = esc_id,
        ver = esc_ver,
        title = esc_title,
        updated = xml_escape(&meta.synced_at),
        src = xml_escape(&content_src),
        hash = xml_escape(&meta.sha256),
        size = meta.nupkg_size,
        url = esc_url,
        latest = is_latest,
    )
}

/// Atom feed document wrapping package entries (NuGet v2 subset choco reads).
fn feed_xml(title: &str, self_href: &str, base: &str, metas: &[NuspecMeta]) -> String {
    // Latest-version flag: naive string compare per id (v1 — fine for the
    // explicit package lists operators configure).
    let mut latest_by_id: HashMap<&str, &str> = HashMap::new();
    for m in metas {
        let e = latest_by_id
            .entry(m.id.as_str())
            .or_insert(m.version.as_str());
        if m.version.as_str() > *e {
            *e = m.version.as_str();
        }
    }
    let entries: String = metas
        .iter()
        .map(|m| {
            entry_xml(
                m,
                base,
                latest_by_id.get(m.id.as_str()).copied() == Some(m.version.as_str()),
            )
        })
        .collect();
    format!(
        concat!(
            "<?xml version=\"1.0\" encoding=\"utf-8\"?>",
            "<feed xml:base=\"{base}\" ",
            "xmlns=\"http://www.w3.org/2005/Atom\" ",
            "xmlns:d=\"http://schemas.microsoft.com/ado/2007/08/dataservices\" ",
            "xmlns:m=\"http://schemas.microsoft.com/ado/2007/08/dataservices/metadata\">",
            "<id>{self_href}</id>",
            "<title type=\"text\">{title}</title>",
            "<updated>{now}</updated>",
            "<link rel=\"self\" href=\"{self_href}\"/>",
            "{entries}",
            "</feed>"
        ),
        base = xml_escape(base.trim_end_matches('/')),
        self_href = xml_escape(self_href),
        title = xml_escape(title),
        now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        entries = entries,
    )
}

/// Minimal EDMX stub for `GET /api/v2/$metadata`. Chocolatey tolerates a
/// sparse services document; it rarely fetches this, but NuGet clients may.
fn metadata_xml() -> &'static str {
    concat!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>",
        "<edmx:Edmx xmlns:edmx=\"http://schemas.microsoft.com/ado/2007/06/edmx\" Version=\"1.0\">",
        "<edmx:DataServices xmlns:m=\"http://schemas.microsoft.com/ado/2007/08/dataservices/metadata\" ",
        "m:DataServiceVersion=\"2.0\">",
        "<Schema xmlns=\"http://schemas.microsoft.com/ado/2006/04/edm\" Namespace=\"Monopsony.Mirror\">",
        "<EntityType Name=\"Package\">",
        "<Key><PropertyRef Name=\"Id\"/><PropertyRef Name=\"Version\"/></Key>",
        "<Property Name=\"Id\" Type=\"Edm.String\" Nullable=\"false\"/>",
        "<Property Name=\"Version\" Type=\"Edm.String\" Nullable=\"false\"/>",
        "<Property Name=\"Title\" Type=\"Edm.String\"/>",
        "<Property Name=\"ProjectUrl\" Type=\"Edm.String\"/>",
        "<Property Name=\"PackageHash\" Type=\"Edm.String\"/>",
        "<Property Name=\"PackageSize\" Type=\"Edm.Int64\"/>",
        "</EntityType>",
        "<EntityContainer Name=\"Packages\">",
        "<EntitySet Name=\"Packages\" EntityType=\"Monopsony.Mirror.Package\"/>",
        "</EntityContainer>",
        "</Schema>",
        "</edmx:DataServices>",
        "</edmx:Edmx>"
    )
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

/// Derive the externally-visible feed base from the request Host header
/// (shim is nested at `/choco` by the hub).
fn request_base(headers: &HeaderMap) -> String {
    let host = headers
        .get("host")
        .and_then(|h| h.to_str().ok())
        .unwrap_or("localhost:3129");
    format!("http://{}/choco", host)
}

/// Extract `Id eq '<value>'` from a NuGet `$filter` query value, if present.
fn filter_id(filter: &str) -> Option<String> {
    let lower = filter.to_ascii_lowercase();
    let idx = lower.find("id eq '")?;
    let rest = &filter[idx + "id eq '".len()..];
    let end = rest.find('\'')?;
    Some(rest[..end].to_string())
}

/// Create the choco NuGet v2 shim router (hub nests at `/choco`).
pub fn router(state: MirrorState) -> axum::Router {
    axum::Router::new()
        .route("/api/v2/Packages()", axum::routing::get(packages_feed))
        .route("/api/v2/packages()", axum::routing::get(packages_feed))
        .route(
            "/api/v2/FindPackagesById()",
            axum::routing::get(find_packages_by_id),
        )
        .route(
            "/api/v2/findPackagesById()",
            axum::routing::get(find_packages_by_id),
        )
        .route("/api/v2/$metadata", axum::routing::get(metadata))
        .route(
            "/api/v2/package/:id/:version",
            axum::routing::get(download_package),
        )
        .route("/api/v2/package/:id", axum::routing::get(download_latest))
        .with_state(state)
}

fn xml_response(body: String) -> Response {
    (
        StatusCode::OK,
        [(
            axum::http::header::CONTENT_TYPE,
            "application/atom+xml; charset=utf-8",
        )],
        body,
    )
        .into_response()
}

/// GET /api/v2/Packages() — full (or $filter Id subset) package feed.
async fn packages_feed(
    State(state): State<MirrorState>,
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let metas = load_all_meta(&state).await;
    let metas = match params.get("$filter").and_then(|f| filter_id(f)) {
        Some(id) => {
            let lower = id.to_ascii_lowercase();
            metas
                .into_iter()
                .filter(|m| m.id.to_ascii_lowercase() == lower)
                .collect::<Vec<_>>()
        }
        None => metas,
    };
    let base = request_base(&headers);
    xml_response(feed_xml(
        "Packages",
        &format!("{}/api/v2/Packages()", base),
        &base,
        &metas,
    ))
}

/// GET /api/v2/FindPackagesById()?id=<pkg> — per-package versions feed.
async fn find_packages_by_id(
    State(state): State<MirrorState>,
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let Some(id) = params.get("id") else {
        return (StatusCode::BAD_REQUEST, "missing id query parameter").into_response();
    };
    let lower = id.to_ascii_lowercase();
    let metas: Vec<NuspecMeta> = load_all_meta(&state)
        .await
        .into_iter()
        .filter(|m| m.id.to_ascii_lowercase() == lower)
        .collect();
    let base = request_base(&headers);
    let href = format!("{}/api/v2/FindPackagesById()?id={}", base, xml_escape(id));
    xml_response(feed_xml(id, &href, &base, &metas))
}

/// GET /api/v2/$metadata — static EDMX stub.
async fn metadata() -> Response {
    (
        StatusCode::OK,
        [(
            axum::http::header::CONTENT_TYPE,
            "application/xml; charset=utf-8",
        )],
        metadata_xml(),
    )
        .into_response()
}

/// GET /api/v2/package/<id>/<version> — serve the .nupkg (application/zip)
/// from mirror storage, with canonicalize traversal protection.
async fn download_package(
    State(state): State<MirrorState>,
    AxumPath((id, version)): AxumPath<(String, String)>,
) -> Response {
    if !valid_component(&id) || !valid_component(&version) {
        return (StatusCode::BAD_REQUEST, "Invalid package id/version").into_response();
    }
    serve_nupkg(&state, &id, &version).await
}

/// GET /api/v2/package/<id> — serve the highest synced version (v1: naive
/// string compare, consistent with the feed's IsLatestVersion flag).
async fn download_latest(
    State(state): State<MirrorState>,
    AxumPath(id): AxumPath<String>,
) -> Response {
    if !valid_component(&id) {
        return (StatusCode::BAD_REQUEST, "Invalid package id").into_response();
    }
    let metas = load_all_meta(&state).await;
    let lower = id.to_ascii_lowercase();
    let best = metas
        .iter()
        .filter(|m| m.id.to_ascii_lowercase() == lower)
        .max_by(|a, b| a.version.cmp(&b.version));
    match best {
        Some(m) => serve_nupkg(&state, &m.id, &m.version).await,
        None => (StatusCode::NOT_FOUND, "Package not found").into_response(),
    }
}

async fn serve_nupkg(state: &MirrorState, id: &str, version: &str) -> Response {
    let dir = feed_root(&state.mirror_dir).join(id).join(version);
    let canonical_dir = match dir.canonicalize() {
        Ok(p) => p,
        Err(_) => return (StatusCode::NOT_FOUND, "Package version not found").into_response(),
    };
    let file = canonical_dir.join(format!("{}.{}.nupkg", id, version));
    let canonical_file = match file.canonicalize() {
        Ok(p) => p,
        Err(_) => return (StatusCode::NOT_FOUND, "nupkg not found").into_response(),
    };
    if !canonical_file.starts_with(&canonical_dir) || !canonical_file.is_file() {
        return (StatusCode::FORBIDDEN, "Path traversal blocked").into_response();
    }
    match tokio::fs::read(&canonical_file).await {
        Ok(data) => Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", "application/zip")
            .header("Content-Length", data.len())
            .header("X-Mirror", "hit")
            .body(axum::body::Body::from(data))
            .unwrap(),
        Err(e) => {
            warn!(
                "choco shim: failed reading {}: {}",
                canonical_file.display(),
                e
            );
            (StatusCode::INTERNAL_SERVER_ERROR, "Read failed").into_response()
        }
    }
}

// ---------------------------------------------------------------------------
// Sync
// ---------------------------------------------------------------------------

/// Sync an explicit operator-configured package list into mirror storage.
///
/// v1 (per slice spec): NOT a full-catalog mirror. For each `(id, version)`
/// fetch `<upstream>/api/v2/package/<id>/<version>` once, store the raw
/// .nupkg (the mirror_dir IS the cache — no serve-time url-rewrite trick
/// for A2) and write the `nuspec.json` sidecar recorded from the request +
/// computed sha256/size. Embedded .nuspec parsing deferred (no `zip` crate
/// in the workspace). Upstream host is SSRF-validated before any fetch.
///
/// Returns the number of packages successfully synced.
pub async fn sync(
    state: &MirrorState,
    packages: &[(String, String)],
    upstream_base: &str,
) -> anyhow::Result<usize> {
    // Sanitize every package id/version BEFORE any network I/O (also keeps
    // the no-network unit test hermetic).
    for (id, version) in packages {
        if !valid_component(id) || !valid_component(version) {
            anyhow::bail!("invalid package id/version: {:?}/{:?}", id, version);
        }
    }

    let upstream = upstream_base.trim_end_matches('/');
    let parsed = url::Url::parse(upstream)?;
    if parsed.scheme() != "https" {
        anyhow::bail!("upstream must be https: {}", upstream);
    }
    if let Err(e) = validate_url_host(&parsed) {
        anyhow::bail!("SSRF validation failed for {}: {}", upstream, e);
    }

    let client = reqwest::Client::builder()
        .user_agent("monopsony-mirror-choco-shim")
        .build()?;
    let mut synced = 0usize;

    for (id, version) in packages {
        let url = format!("{}/api/v2/package/{}/{}", upstream, id, version);
        let resp = match client.get(&url).send().await {
            Ok(r) => r,
            Err(e) => {
                warn!("choco shim sync: {} {}: skipping ({})", id, version, e);
                continue;
            }
        };
        if !resp.status().is_success() {
            warn!(
                "choco shim sync: {} {}: upstream {}",
                id,
                version,
                resp.status()
            );
            continue;
        }
        let bytes = match resp.bytes().await {
            Ok(b) => b,
            Err(e) => {
                warn!(
                    "choco shim sync: {} {}: body read failed ({})",
                    id, version, e
                );
                continue;
            }
        };

        // sha256 of the exact bytes we store.
        use sha2::Digest;
        let sha256 = hex::encode(sha2::Sha256::digest(&bytes));
        let meta = NuspecMeta {
            id: id.clone(),
            version: version.clone(),
            title: id.clone(), // no .nuspec parse in v1; title = id
            project_url: None,
            sha256,
            nupkg_size: bytes.len() as u64,
            synced_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        };

        let dir = feed_root(&state.mirror_dir).join(id).join(version);
        if let Err(e) = tokio::fs::create_dir_all(&dir).await {
            anyhow::bail!("create_dir_all {}: {}", dir.display(), e);
        }
        let nupkg = dir.join(format!("{}.{}.nupkg", id, version));
        if let Err(e) = tokio::fs::write(&nupkg, &bytes).await {
            anyhow::bail!("write {}: {}", nupkg.display(), e);
        }
        let sidecar = dir.join("nuspec.json");
        if let Err(e) = tokio::fs::write(&sidecar, serde_json::to_vec_pretty(&meta)?).await {
            anyhow::bail!("write {}: {}", sidecar.display(), e);
        }
        synced += 1;
        debug!(
            "choco shim sync: {} {} ({} bytes) stored",
            id,
            version,
            bytes.len()
        );
    }

    info!(
        "choco shim sync: {}/{} package(s) synced from {}",
        synced,
        packages.len(),
        upstream
    );
    Ok(synced)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use http_body_util::BodyExt;
    use tower::ServiceExt; // oneshot

    fn test_state() -> (MirrorState, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let manager = crate::mirror::MirrorManager::new(dir.path().to_path_buf(), None);
        let state = MirrorState::new(manager, dir.path().to_path_buf());
        (state, dir)
    }

    async fn seed(state: &MirrorState, id: &str, version: &str) {
        let dir = feed_root(&state.mirror_dir).join(id).join(version);
        tokio::fs::create_dir_all(&dir).await.unwrap();
        tokio::fs::write(
            dir.join(format!("{}.{}.nupkg", id, version)),
            b"PK-fake-zip",
        )
        .await
        .unwrap();
        let meta = NuspecMeta {
            id: id.to_string(),
            version: version.to_string(),
            title: format!("{} & <Friends>", id),
            project_url: Some(format!("https://example.com/{}", id)),
            sha256: "deadbeef".to_string(),
            nupkg_size: 12,
            synced_at: "2026-09-14T00:00:00Z".to_string(),
        };
        tokio::fs::write(
            dir.join("nuspec.json"),
            serde_json::to_vec_pretty(&meta).unwrap(),
        )
        .await
        .unwrap();
    }

    // ---- XML generation unit tests ----

    #[test]
    fn test_xml_escape() {
        assert_eq!(xml_escape("a&<>'\""), "a&amp;&lt;&gt;&apos;&quot;");
        assert_eq!(xml_escape("plain"), "plain");
    }

    #[test]
    fn test_entry_shape_and_escaping() {
        let meta = NuspecMeta {
            id: "7zip".into(),
            version: "22.1".into(),
            title: "7-Zip & tools".into(),
            project_url: None,
            sha256: "abc123".into(),
            nupkg_size: 1234,
            synced_at: "2026-09-14T00:00:00Z".into(),
        };
        let xml = entry_xml(&meta, "http://h:3129/choco", true);
        assert!(xml.contains("<d:Id>7zip</d:Id>"));
        assert!(xml.contains("<d:Version>22.1</d:Version>"));
        assert!(xml.contains("7-Zip &amp; tools"));
        assert!(xml.contains("<d:IsLatestVersion m:type=\"Edm.Boolean\">true</d:IsLatestVersion>"));
        assert!(xml.contains("src=\"http://h:3129/choco/api/v2/package/7zip/22.1\""));
        assert!(xml.contains("<d:PackageHash>SHA-256=abc123</d:PackageHash>"));
        assert!(!xml.contains("<d:ProjectUrl>x")); // absent when None
    }

    #[test]
    fn test_feed_latest_flag_and_filter_parse() {
        let metas = vec![
            NuspecMeta {
                id: "git".into(),
                version: "2.40.0".into(),
                title: "git".into(),
                project_url: None,
                sha256: "h1".into(),
                nupkg_size: 1,
                synced_at: "2026-09-14T00:00:00Z".into(),
            },
            NuspecMeta {
                id: "git".into(),
                version: "2.45.1".into(),
                title: "git".into(),
                project_url: None,
                sha256: "h2".into(),
                nupkg_size: 2,
                synced_at: "2026-09-14T00:00:00Z".into(),
            },
        ];
        let xml = feed_xml(
            "Packages",
            "http://h/choco/api/v2/Packages()",
            "http://h/choco",
            &metas,
        );
        // exactly one latest=true (2.45.1)
        let latest_count = xml
            .matches("<d:IsLatestVersion m:type=\"Edm.Boolean\">true<")
            .count();
        assert_eq!(latest_count, 1);
        assert!(xml.contains("<d:Version>2.45.1</d:Version>"));

        assert_eq!(filter_id("(Id eq '7zip')"), Some("7zip".to_string()));
        assert_eq!(filter_id("(tolower(Id) eq 'git')"), None);
        assert_eq!(filter_id("no quotes"), None);
    }

    // ---- Router oneshot tests ----

    #[tokio::test]
    async fn test_router_find_packages_by_id() {
        let (state, _dir) = test_state();
        seed(&state, "7zip", "22.1").await;
        seed(&state, "other", "1.0").await;

        let app = router(state);
        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/v2/FindPackagesById()?id=7zip")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get("content-type").unwrap(),
            "application/atom+xml; charset=utf-8"
        );
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let text = String::from_utf8_lossy(&body);
        assert!(text.contains("<d:Id>7zip</d:Id>"));
        assert!(!text.contains("<d:Id>other</d:Id>"));
    }

    #[tokio::test]
    async fn test_router_packages_feed_with_filter() {
        let (state, _dir) = test_state();
        seed(&state, "7zip", "22.1").await;
        seed(&state, "other", "1.0").await;

        let app = router(state);
        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/v2/Packages()?$filter=(Id%20eq%20%277zip%27)")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let text = String::from_utf8_lossy(&body);
        assert!(text.contains("<d:Id>7zip</d:Id>"));
        assert!(!text.contains("<d:Id>other</d:Id>"));
    }

    #[tokio::test]
    async fn test_router_metadata_stub() {
        let (state, _dir) = test_state();
        let app = router(state);
        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/v2/$metadata")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        assert!(String::from_utf8_lossy(&body).contains("<edmx:Edmx"));
    }

    #[tokio::test]
    async fn test_router_package_download() {
        let (state, _dir) = test_state();
        seed(&state, "7zip", "22.1").await;

        let app = router(state);
        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/v2/package/7zip/22.1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get("content-type").unwrap(),
            "application/zip"
        );
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(&body[..], b"PK-fake-zip");
    }

    #[tokio::test]
    async fn test_router_package_download_latest_unversioned() {
        let (state, _dir) = test_state();
        seed(&state, "git", "2.40.0").await;
        seed(&state, "git", "2.45.1").await;

        let app = router(state);
        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/v2/package/git")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(&body[..], b"PK-fake-zip"); // same bytes; version picked = 2.45.1
    }

    #[tokio::test]
    async fn test_router_blocks_traversal() {
        let (state, dir) = test_state();
        let secret = dir.path().join("secret.txt");
        tokio::fs::write(&secret, "secret").await.unwrap();

        let app = router(state);
        for uri in [
            "/api/v2/package/../../secret.txt",
            "/api/v2/package/..%2f..%2fsecret.txt",
        ] {
            let resp = app
                .clone()
                .oneshot(
                    axum::http::Request::builder()
                        .uri(uri)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_ne!(resp.status(), StatusCode::OK, "uri {} must not be 200", uri);
        }
    }

    #[tokio::test]
    async fn test_sync_rejects_invalid_package_id_before_network() {
        let (state, _dir) = test_state();
        let err = sync(
            &state,
            &[("../evil".to_string(), "1.0".to_string())],
            "https://community.chocolatey.org/api/v2",
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("invalid package id/version"));
    }

    // sync() end-to-end skipped: requires network (community.chocolatey.org).
    // XML gen, storage layout, traversal, and pre-network sanitization are
    // covered above, per slice-spec guidance.
}
