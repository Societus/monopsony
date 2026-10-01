//! Winget Microsoft.Rest source shim (Wave A slice A3)
//!
//! Serves a winget REST source at `http://<server>:3129/winget/` so
//! `winget source add -n monopsony -a http://...:3129/winget/ -t Microsoft.Rest`
//! works against the local mirror instead of the public CDN + community repo.
//!
//! Endpoints (winget REST source contract, JSON):
//! - `GET /` or `/information` — source information document
//! - `POST /manifestSearch` — search the synced local manifest index
//! - `GET /packageManifests/<packageId>` — merged REST manifest (versions +
//!   installers) for one package
//!
//! Storage: `<mirror_dir>/winget/manifests/<lowercased-pid>.json` — one
//! aggregated REST manifest per package, built at SYNC time from the
//! winget-community repo (microsoft/winget-pkgs) on GitHub. Only a
//! configured package list is synced in v1 (not the whole 8k-package repo).
//!
//! Rewriting at sync: each `InstallerUrl` becomes
//! `<proxy_base>/<original-upstream-url>` (absolute-URI request through the
//! caching proxy — same trick as the scoop shim). `InstallerSha256` is
//! preserved verbatim: winget verifies it client-side.
//!
//! Shape copied from scoop_shim.rs (A1): `pub fn router(MirrorState) ->
//! axum::Router` (self-contained state; the hub nests this inside
//! `create_mirror_router`), plain files under `state.mirror_dir`, https-only
//! upstream fetches via reqwest, SSRF validation through
//! `crate::url_validation::validate_url_host`, canonicalize-based traversal
//! protection.
//!
//! Deviation note: winget community manifests are YAML; no YAML crate is a
//! workspace dependency, so this module carries a minimal YAML-subset parser
//! (`parse_yaml`) sufficient for winget installer manifests (nested maps +
//! `- ` sequences of maps + scalar values). Exotic YAML (anchors, flow
//! collections, multi-line scalars) is not supported in v1; unparseable
//! manifests are skipped with a warning at sync time.

use axum::{
    extract::{Path as AxumPath, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::{json, Value};
use tracing::{info, warn};

use crate::mirror::MirrorState;
use crate::url_validation::validate_url_host;

/// Default base address of the caching proxy (absolute-URI requests).
pub const DEFAULT_PROXY_BASE: &str = "http://127.0.0.1:3128";

/// Default upstream community repo (winget REST metadata source).
pub const DEFAULT_UPSTREAM_REPO: &str = "https://github.com/microsoft/winget-pkgs";

/// Storage root for synced winget manifests under `mirror_dir`.
fn manifests_root(mirror_dir: &std::path::Path) -> std::path::PathBuf {
    mirror_dir.join("winget").join("manifests")
}

/// Create the winget Microsoft.Rest source router.
///
/// Routes (mounted by the hub at `/winget`):
/// - `GET /` and `GET /information` — source information JSON
/// - `POST /manifestSearch` — search synced manifests
/// - `GET /packageManifests/:package_id` — REST manifest for one package
pub fn router(state: MirrorState) -> axum::Router {
    axum::Router::new()
        .route("/", axum::routing::get(information))
        .route("/information", axum::routing::get(information))
        .route("/manifestSearch", axum::routing::post(manifest_search))
        .route(
            "/packageManifests/:package_id",
            axum::routing::get(package_manifest),
        )
        .with_state(state)
}

/// GET / (and /information) — winget REST source information document.
async fn information() -> Json<Value> {
    json!({
        "Data": {
            "SourceIdentifier": "monopsony",
            "ServerSupportedVersions": ["2.0.0"],
        },
        "ContinuationToken": Value::Null,
    })
    .into()
}

/// Extract the search keyword + match type from a manifestSearch POST body.
/// v1: first Inclusion's RequestMatch, falling back to the top-level Query.
fn search_request(body: &Value) -> Option<(String, String)> {
    let inclusion = body
        .get("Inclusions")
        .and_then(|v| v.as_array())
        .and_then(|a| a.first())
        .and_then(|inc| inc.get("RequestMatch"));
    let from_inclusion = inclusion.and_then(|rm| {
        let kw = rm.get("KeyWord").and_then(|v| v.as_str());
        let mt = rm.get("MatchType").and_then(|v| v.as_str());
        match (kw, mt) {
            (Some(kw), mt) => Some((kw.to_string(), mt.unwrap_or("Exact").to_string())),
            (None, _) => None,
        }
    });
    if from_inclusion.is_some() {
        return from_inclusion;
    }
    body.get("Query").and_then(|q| {
        let kw = q.get("Key").and_then(|v| v.as_str())?;
        let mt = q
            .get("MatchType")
            .and_then(|v| v.as_str())
            .unwrap_or("Exact");
        Some((kw.to_string(), mt.to_string()))
    })
}

/// Case-(in)sensitive match of a package identifier against a keyword.
fn id_matches(id: &str, keyword: &str, match_type: &str) -> bool {
    match match_type.to_lowercase().as_str() {
        "casesensitive" => id == keyword,
        "substring" | "startswithsubstring" | "wordstartswith" => {
            id.to_lowercase().contains(&keyword.to_lowercase())
        }
        // "Exact" and anything unrecognized: case-insensitive exact
        _ => id.eq_ignore_ascii_case(keyword),
    }
}

/// POST /manifestSearch — search the synced local manifest index.
async fn manifest_search(State(state): State<MirrorState>, body: Option<Json<Value>>) -> Response {
    let Some(Json(body)) = body else {
        return (StatusCode::BAD_REQUEST, "Missing search body").into_response();
    };
    let Some((keyword, match_type)) = search_request(&body) else {
        return (StatusCode::BAD_REQUEST, "Malformed search body").into_response();
    };

    let root = manifests_root(&state.mirror_dir);
    let mut hits: Vec<String> = Vec::new();
    let mut entries: Vec<std::path::PathBuf> = Vec::new();
    if let Ok(mut rd) = tokio::fs::read_dir(&root).await {
        while let Ok(Some(entry)) = rd.next_entry().await {
            let p = entry.path();
            if p.extension().and_then(|e| e.to_str()) == Some("json") {
                entries.push(p);
            }
        }
    }
    for path in entries {
        let Ok(text) = tokio::fs::read_to_string(&path).await else {
            continue;
        };
        let Ok(doc) = serde_json::from_str::<Value>(&text) else {
            warn!(
                "winget shim: skipping non-JSON index file {}",
                path.display()
            );
            continue;
        };
        if let Some(pid) = doc.get("PackageIdentifier").and_then(|v| v.as_str()) {
            if id_matches(pid, &keyword, &match_type) {
                hits.push(pid.to_string());
            }
        }
    }

    let data: Vec<Value> = hits
        .into_iter()
        .map(|pid| json!({ "PackageIdentifier": pid }))
        .collect();
    Json(json!({ "Data": data, "ContinuationToken": Value::Null })).into_response()
}

/// GET /packageManifests/:package_id — serve one package's merged REST manifest.
async fn package_manifest(
    State(state): State<MirrorState>,
    AxumPath(package_id): AxumPath<String>,
) -> Response {
    // Sanitize: single path component, no traversal, no percent-encoding games
    if package_id.is_empty()
        || package_id.contains('/')
        || package_id.contains('\\')
        || package_id.contains("..")
        || package_id.contains('%')
    {
        return (StatusCode::BAD_REQUEST, "Invalid package identifier").into_response();
    }

    let root = manifests_root(&state.mirror_dir);
    let canonical_root = match root.canonicalize() {
        Ok(p) => p,
        Err(_) => return (StatusCode::NOT_FOUND, "No manifests synced").into_response(),
    };
    let local = root.join(format!("{}.json", package_id.to_lowercase()));
    let canonical_file = match local.canonicalize() {
        Ok(p) => p,
        Err(_) => return (StatusCode::NOT_FOUND, "Package not found").into_response(),
    };
    if !canonical_file.starts_with(&canonical_root) || !canonical_file.is_file() {
        return (StatusCode::FORBIDDEN, "Path traversal blocked").into_response();
    }

    match tokio::fs::read_to_string(&canonical_file).await {
        Ok(text) => {
            // Wrap the stored manifest in the REST envelope
            let inner: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
            let body = json!({ "Data": inner, "ContinuationToken": Value::Null });
            (
                StatusCode::OK,
                [(axum::http::header::CONTENT_TYPE, "application/json")],
                body.to_string(),
            )
                .into_response()
        }
        Err(e) => {
            warn!(
                "winget shim: failed reading {}: {}",
                canonical_file.display(),
                e
            );
            (StatusCode::INTERNAL_SERVER_ERROR, "Read failed").into_response()
        }
    }
}

/// Rewrite one installer URL to an absolute-URI request through the proxy.
fn rewrite_installer_url(url: &str, proxy_base: &str) -> String {
    if url.starts_with("http://") || url.starts_with("https://") {
        format!("{}/{}", proxy_base.trim_end_matches('/'), url)
    } else {
        url.to_string()
    }
}

/// Convert one winget community YAML manifest (installer or merged) into a
/// single-version REST manifest fragment, rewriting `InstallerUrl` and
/// preserving `InstallerSha256` verbatim. Returns None if the YAML does not
/// carry a PackageIdentifier + PackageVersion (e.g. a locale-only manifest).
fn yaml_to_rest(yaml_text: &str, proxy_base: &str) -> Option<Value> {
    let root = parse_yaml(yaml_text);
    let obj = root.as_object()?;
    let pid = obj.get("PackageIdentifier")?.as_str()?;
    let version = obj.get("PackageVersion")?.as_str()?;

    let mut installers: Vec<Value> = Vec::new();
    if let Some(arr) = obj.get("Installers").and_then(|v| v.as_array()) {
        for inst in arr {
            let Some(io) = inst.as_object() else { continue };
            let mut m = serde_json::Map::new();
            if let Some(u) = io.get("InstallerUrl").and_then(|v| v.as_str()) {
                m.insert(
                    "InstallerUrl".to_string(),
                    Value::String(rewrite_installer_url(u, proxy_base)),
                );
            }
            // Preserved verbatim — winget verifies client-side
            for key in ["InstallerSha256", "InstallerType", "Architecture"] {
                if let Some(v) = io.get(key) {
                    m.insert(key.to_string(), v.clone());
                }
            }
            installers.push(Value::Object(m));
        }
    }

    Some(json!({
        "PackageIdentifier": pid,
        "Versions": [ { "PackageVersion": version, "Installers": installers } ],
    }))
}

// ---------------------------------------------------------------------------
// Minimal YAML-subset parser (nested mappings + `- ` sequences of mappings +
// scalar values). No anchors / flow collections / multi-line scalars — the
// winget installer manifests we sync use only this subset.
// ---------------------------------------------------------------------------

fn strip_quotes(s: &str) -> String {
    let s = s.trim();
    if s.len() >= 2
        && ((s.starts_with('"') && s.ends_with('"')) || (s.starts_with('\'') && s.ends_with('\'')))
    {
        s[1..s.len() - 1].to_string()
    } else {
        s.to_string()
    }
}

/// Split `key: value` on the first `: ` (or trailing `:` for nested keys).
fn split_kv(line: &str) -> Option<(String, String)> {
    let bytes = line.as_bytes();
    for (i, b) in bytes.iter().enumerate() {
        if *b == b':' && (i + 1 == bytes.len() || bytes[i + 1] == b' ') {
            let key = line[..i].trim().to_string();
            let value = if i + 1 == bytes.len() {
                String::new()
            } else {
                line[i + 2..].trim().to_string()
            };
            if key.is_empty() {
                return None;
            }
            return Some((key, value));
        }
    }
    None
}

fn parse_yaml(text: &str) -> Value {
    let lines: Vec<(usize, String)> = text
        .lines()
        .map(|l| {
            let indent = l.len() - l.trim_start_matches(' ').len();
            (indent, l.trim().to_string())
        })
        .filter(|(_, t)| !t.is_empty() && !t.starts_with('#'))
        .collect();
    let mut i = 0;
    parse_block(&lines, &mut i, 0)
}

fn parse_block(lines: &[(usize, String)], i: &mut usize, indent: usize) -> Value {
    if *i >= lines.len() {
        return Value::Null;
    }
    let is_item = |l: &str| l == "-" || l.starts_with("- ");
    // Bare scalar block (e.g. a `- exe` sequence item payload with no `:`)
    if !is_item(&lines[*i].1) && split_kv(&lines[*i].1).is_none() {
        let s = strip_quotes(&lines[*i].1);
        *i += 1;
        return Value::String(s);
    }
    if is_item(&lines[*i].1) {
        // Sequence at this indent
        let mut arr: Vec<Value> = Vec::new();
        while *i < lines.len() && lines[*i].0 == indent && is_item(&lines[*i].1) {
            let item_indent = indent + 2;
            let rest = lines[*i]
                .1
                .strip_prefix("- ")
                .unwrap_or("")
                .trim()
                .to_string();
            *i += 1;
            if rest.is_empty() {
                arr.push(parse_block(lines, i, item_indent));
            } else {
                // Inline first key of a mapping item; collect continuation lines
                let mut sub: Vec<(usize, String)> = vec![(item_indent, rest)];
                while *i < lines.len() && lines[*i].0 >= item_indent {
                    sub.push(lines[*i].clone());
                    *i += 1;
                }
                let mut j = 0;
                arr.push(parse_block(&sub, &mut j, item_indent));
            }
        }
        Value::Array(arr)
    } else {
        // Mapping at this indent
        let mut obj = serde_json::Map::new();
        while *i < lines.len() && lines[*i].0 == indent && !is_item(&lines[*i].1) {
            let Some((key, value)) = split_kv(&lines[*i].1) else {
                *i += 1;
                continue;
            };
            *i += 1;
            if !value.is_empty() {
                obj.insert(key, Value::String(strip_quotes(&value)));
            } else if *i < lines.len() && lines[*i].0 > indent {
                let next_indent = lines[*i].0;
                obj.insert(key, parse_block(lines, i, next_indent));
            } else if *i < lines.len() && lines[*i].0 == indent && is_item(&lines[*i].1) {
                // Sequence at the same indent as its key (common style)
                obj.insert(key, parse_block(lines, i, indent));
            } else {
                obj.insert(key, Value::Null);
            }
        }
        Value::Object(obj)
    }
}

// ---------------------------------------------------------------------------
// Sync (GitHub contents/tree-API pattern from scoop_shim)
// ---------------------------------------------------------------------------

fn is_yaml_path(path: &str) -> bool {
    path.ends_with(".yaml") || path.ends_with(".yml")
}

fn valid_package_id(pid: &str) -> bool {
    !pid.is_empty()
        && pid
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_')
}

/// Sync manifests for a configured package list from the default community
/// repo (microsoft/winget-pkgs), rewriting installer URLs through `proxy_base`.
pub async fn sync(
    state: &MirrorState,
    package_ids: &[String],
    proxy_base: &str,
) -> anyhow::Result<()> {
    sync_from(state, package_ids, DEFAULT_UPSTREAM_REPO, proxy_base).await
}

/// Parse `owner/repo` out of a GitHub repo URL
/// (accepts `.../<owner>/<repo>` with optional `/tree/<ref>...` suffix).
fn parse_github_repo(u: &url::Url) -> Option<(String, String)> {
    let host = u.host_str()?;
    if !host.eq_ignore_ascii_case("github.com") && !host.eq_ignore_ascii_case("www.github.com") {
        return None;
    }
    let mut segs = u.path_segments()?.filter(|s| !s.is_empty());
    let owner = segs.next()?.to_string();
    let repo = segs.next()?.trim_end_matches(".git").to_string();
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    Some((owner, repo))
}

/// Sync the given package ids' manifests from an https GitHub repo URL
/// (the winget community repo layout: `manifests/<c>/<Id/Parts>/<Version>/`).
///
/// v1 scope: only the operator-configured package list, not the full repo.
/// Per-package/per-file failures warn and skip; the sync itself only fails
/// on setup errors (bad upstream URL, SSRF, unusable GitHub API response).
pub async fn sync_from(
    state: &MirrorState,
    package_ids: &[String],
    upstream: &str,
    proxy_base: &str,
) -> anyhow::Result<()> {
    let upstream = upstream.trim_end_matches('/');
    let parsed = url::Url::parse(upstream)?;
    if parsed.scheme() != "https" {
        anyhow::bail!("upstream must be https: {}", upstream);
    }
    if let Err(e) = validate_url_host(&parsed) {
        anyhow::bail!("SSRF validation failed for {}: {}", upstream, e);
    }
    let (owner, repo) = parse_github_repo(&parsed)
        .ok_or_else(|| anyhow::anyhow!("not a GitHub repo URL: {}", upstream))?;

    let api_base = format!("https://api.github.com/repos/{}/{}/contents/", owner, repo);
    let api_parsed = url::Url::parse(&api_base)?;
    if let Err(e) = validate_url_host(&api_parsed) {
        anyhow::bail!("SSRF validation failed for {}: {}", api_base, e);
    }
    let raw_base = format!("https://raw.githubusercontent.com/{}/{}/HEAD/", owner, repo);

    let client = reqwest::Client::builder()
        .user_agent("monopsony-mirror-winget-shim")
        .build()?;

    let root = manifests_root(&state.mirror_dir);
    tokio::fs::create_dir_all(&root).await?;

    for pid in package_ids {
        if !valid_package_id(pid) {
            warn!("winget shim sync: invalid package id {:?} — skipping", pid);
            continue;
        }
        // winget-pkgs layout: manifests/<first-char-lower>/<Id/with/dots/as/slashes>
        let first = pid.chars().next().unwrap().to_ascii_lowercase();
        let dir_path = format!("manifests/{}/{}", first, pid.replace('.', "/"));
        let list_url = format!("{}{}", api_base, dir_path);

        let resp = match client.get(&list_url).send().await {
            Ok(r) => r,
            Err(e) => {
                warn!(
                    "winget shim sync: {}: fetch error {} — skipping {}",
                    list_url, e, pid
                );
                continue;
            }
        };
        if !resp.status().is_success() {
            warn!(
                "winget shim sync: {} {} — skipping {}",
                resp.status(),
                list_url,
                pid
            );
            continue;
        }
        let listing: Value = match resp.json().await {
            Ok(v) => v,
            Err(e) => {
                warn!(
                    "winget shim sync: {}: bad JSON ({}) — skipping {}",
                    list_url, e, pid
                );
                continue;
            }
        };
        let Some(entries) = listing.as_array() else {
            warn!(
                "winget shim sync: {} returned no array — skipping {}",
                list_url, pid
            );
            continue;
        };

        // Collect YAML paths: blobs directly in the package dir, plus blobs
        // inside each version subdirectory.
        let mut yaml_paths: Vec<String> = Vec::new();
        for entry in entries {
            let typ = entry.get("type").and_then(|v| v.as_str()).unwrap_or("");
            let path = entry.get("path").and_then(|v| v.as_str()).unwrap_or("");
            if path.is_empty() {
                continue;
            }
            match typ {
                "blob" => {
                    if is_yaml_path(path) {
                        yaml_paths.push(path.to_string());
                    }
                }
                "dir" => {
                    let sub_url = format!("{}{}", api_base, path);
                    let sub_resp = match client.get(&sub_url).send().await {
                        Ok(r) => r,
                        Err(e) => {
                            warn!("winget shim sync: {}: fetch error {}", sub_url, e);
                            continue;
                        }
                    };
                    if !sub_resp.status().is_success() {
                        warn!(
                            "winget shim sync: {} {} — skipping dir",
                            sub_resp.status(),
                            sub_url
                        );
                        continue;
                    }
                    if let Ok(Value::Array(files)) = sub_resp.json::<Value>().await {
                        for f in files {
                            if f.get("type").and_then(|v| v.as_str()) == Some("blob") {
                                if let Some(fp) = f.get("path").and_then(|v| v.as_str()) {
                                    if is_yaml_path(fp) {
                                        yaml_paths.push(fp.to_string());
                                    }
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
        }

        // Fetch + convert each YAML, merging versions into one document.
        let mut versions: Vec<Value> = Vec::new();
        let mut ident: Option<String> = None;
        for path in &yaml_paths {
            let file_url = format!("{}{}", raw_base, path);
            let resp = match client.get(&file_url).send().await {
                Ok(r) => r,
                Err(e) => {
                    warn!("winget shim sync: {}: fetch error {}", path, e);
                    continue;
                }
            };
            if !resp.status().is_success() {
                warn!(
                    "winget shim sync: {} {} — skipping file",
                    resp.status(),
                    path
                );
                continue;
            }
            let text = match resp.text().await {
                Ok(t) => t,
                Err(e) => {
                    warn!("winget shim sync: {}: read error {}", path, e);
                    continue;
                }
            };
            let Some(mut frag) = yaml_to_rest(&text, proxy_base) else {
                warn!(
                    "winget shim sync: {} has no installer manifest fields — skipping",
                    path
                );
                continue;
            };
            if ident.is_none() {
                ident = frag
                    .get("PackageIdentifier")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
            }
            if let Some(vs) = frag.get_mut("Versions").and_then(|v| v.as_array_mut()) {
                versions.append(vs);
            }
        }

        if versions.is_empty() {
            warn!("winget shim sync: no manifests synced for {}", pid);
            continue;
        }
        let doc = json!({
            "PackageIdentifier": ident.unwrap_or_else(|| pid.to_string()),
            "Versions": versions,
        });
        let out = root.join(format!("{}.json", pid.to_lowercase()));
        tokio::fs::write(&out, serde_json::to_vec_pretty(&doc)?).await?;
        info!(
            "winget shim sync: {} from {}/{}: {} version(s), {} yaml file(s)",
            pid,
            owner,
            repo,
            versions.len(),
            yaml_paths.len()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use http_body_util::BodyExt;
    use tower::ServiceExt; // oneshot

    /// Build a MirrorState backed by a temp dir (no DB, no config file).
    fn test_state() -> (MirrorState, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let manager = crate::mirror::MirrorManager::new(dir.path().to_path_buf(), None);
        let state = MirrorState::new(manager, dir.path().to_path_buf());
        (state, dir)
    }

    const YAML_FIXTURE: &str = r#"# Created using wingetcreate
PackageIdentifier: Test.ExampleApp
PackageVersion: 1.2.3
MinimumOSVersion: 10.0.0.0
FileExtensions:
- exe
- msi
Installers:
- Architecture: x64
  InstallerType: exe
  InstallerUrl: https://example.com/downloads/example-1.2.3-x64.exe
  InstallerSha256: ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789
  InstallerSwitches:
    Silent: /quiet
    SilentWithProgress: /passive
- Architecture: x86
  InstallerType: nullsoft
  InstallerUrl: https://example.com/downloads/example-1.2.3-x86.exe
  InstallerSha256: 0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF
ManifestType: installer
ManifestVersion: 1.6.0
"#;

    #[test]
    fn test_yaml_parse_nested_structure() {
        let v = parse_yaml(YAML_FIXTURE);
        let obj = v.as_object().unwrap();
        assert_eq!(obj["PackageIdentifier"], "Test.ExampleApp");
        assert_eq!(obj["PackageVersion"], "1.2.3");
        // top-level sequence
        assert_eq!(obj["FileExtensions"][0], "exe");
        // nested sequence of mappings with nested map values
        let installers = obj["Installers"].as_array().unwrap();
        assert_eq!(installers.len(), 2);
        assert_eq!(installers[0]["Architecture"], "x64");
        assert_eq!(installers[0]["InstallerType"], "exe");
        assert_eq!(
            installers[0]["InstallerUrl"],
            "https://example.com/downloads/example-1.2.3-x64.exe"
        );
        assert_eq!(installers[0]["InstallerSwitches"]["Silent"], "/quiet");
        assert_eq!(installers[1]["Architecture"], "x86");
        // plain `null` scalars stay strings (documented parser subset)
        assert_eq!(installers[1]["InstallerType"], "nullsoft");
    }

    #[test]
    fn test_yaml_to_rest_rewrites_url_preserves_sha256() {
        let rest = yaml_to_rest(YAML_FIXTURE, "http://192.168.1.10:3128").unwrap();
        assert_eq!(rest["PackageIdentifier"], "Test.ExampleApp");
        let versions = rest["Versions"].as_array().unwrap();
        assert_eq!(versions.len(), 1);
        assert_eq!(versions[0]["PackageVersion"], "1.2.3");

        let installers = versions[0]["Installers"].as_array().unwrap();
        assert_eq!(installers.len(), 2);
        // InstallerUrl rewritten to absolute-URI through the proxy
        assert_eq!(
            installers[0]["InstallerUrl"],
            "http://192.168.1.10:3128/https://example.com/downloads/example-1.2.3-x64.exe"
        );
        // InstallerSha256 preserved VERBATIM
        assert_eq!(
            installers[0]["InstallerSha256"],
            "ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789"
        );
        assert_eq!(
            installers[1]["InstallerSha256"],
            "0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF"
        );
        assert_eq!(installers[0]["Architecture"], "x64");
        assert_eq!(installers[0]["InstallerType"], "exe");
        // InstallerSwitches (nested map) not carried into REST installers
        assert!(installers[0].get("InstallerSwitches").is_none());
    }

    #[test]
    fn test_yaml_to_rest_locale_manifest_rejected() {
        // A locale manifest has no Installers but does have id+version → ok but empty
        let locale = "PackageIdentifier: Test.App\nPackageVersion: 1.0\nPackageLocale: en-US\n";
        let rest = yaml_to_rest(locale, "http://p:3128").unwrap();
        assert_eq!(
            rest["Versions"][0]["Installers"].as_array().unwrap().len(),
            0
        );
        // No PackageVersion → None
        let bad = "PackageIdentifier: Test.App\nPackageName: App\n";
        assert!(yaml_to_rest(bad, "http://p:3128").is_none());
    }

    #[test]
    fn test_rewrite_installer_url_non_http_untouched() {
        assert_eq!(
            rewrite_installer_url("ftp://x/y.exe", "http://p:3128"),
            "ftp://x/y.exe"
        );
        assert_eq!(
            rewrite_installer_url("relative.exe", "http://p:3128"),
            "relative.exe"
        );
        assert_eq!(
            rewrite_installer_url("https://a/b.exe", "http://p:3128/"),
            "http://p:3128/https://a/b.exe"
        );
    }

    #[test]
    fn test_id_matches_match_types() {
        assert!(id_matches(
            "Microsoft.PowerShell",
            "microsoft.powershell",
            "Exact"
        ));
        assert!(!id_matches(
            "Microsoft.PowerShell",
            "microsoft.powershell",
            "CaseSensitive"
        ));
        assert!(id_matches(
            "Microsoft.PowerShell",
            "Microsoft.PowerShell",
            "CaseSensitive"
        ));
        assert!(id_matches(
            "Microsoft.PowerShell",
            "powershell",
            "Substring"
        ));
        assert!(!id_matches("Microsoft.PowerShell", "vim", "Substring"));
        // Unknown match type falls back to case-insensitive exact
        assert!(id_matches("Git.Git", "git.git", "Something"));
    }

    #[test]
    fn test_search_request_extraction() {
        let body: Value = serde_json::from_str(
            r#"{"Query":{"Key":"git","MatchType":"Substring"},
                "Inclusions":[{"PackageMatchField":"Id",
                 "RequestMatch":{"KeyWord":"Git.Git","MatchType":"Exact"}}]}"#,
        )
        .unwrap();
        assert_eq!(
            search_request(&body),
            Some(("Git.Git".into(), "Exact".into()))
        );
        let query_only: Value =
            serde_json::from_str(r#"{"Query":{"Key":"git","MatchType":"Substring"}}"#).unwrap();
        assert_eq!(
            search_request(&query_only),
            Some(("git".into(), "Substring".into()))
        );
        assert_eq!(search_request(&json!({})), None);
    }

    #[tokio::test]
    async fn test_router_information_endpoint() {
        let (state, _dir) = test_state();
        for uri in ["/", "/information"] {
            let app = router(state.clone());
            let resp = app
                .oneshot(
                    axum::http::Request::builder()
                        .uri(uri)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
            assert_eq!(
                resp.headers().get("content-type").unwrap(),
                "application/json"
            );
            let body = resp.into_body().collect().await.unwrap().to_bytes();
            let v: Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(v["Data"]["SourceIdentifier"], "monopsony");
            assert_eq!(v["Data"]["ServerSupportedVersions"][0], "2.0.0");
            assert!(v["ContinuationToken"].is_null());
        }
    }

    /// Write a synced-style manifest fixture (as sync_from would produce).
    async fn write_fixture(dir: &std::path::Path) {
        let root = dir.join("winget").join("manifests");
        tokio::fs::create_dir_all(&root).await.unwrap();
        let git_manifest = r#"{
  "PackageIdentifier": "Git.Git",
  "Versions": [
    {
      "PackageVersion": "2.45.1",
      "Installers": [
        {
          "InstallerUrl": "http://p:3128/https://github.com/git-for-windows/git/releases/download/v2.45.1.windows.1/Git-2.45.1-64-bit.exe",
          "InstallerSha256": "AAAA1111BBBB2222CCCC3333DDDD4444EEEE5555FFFF6666AAAA7777BBBB8888",
          "InstallerType": "exe",
          "Architecture": "x64"
        }
      ]
    }
  ]
}"#;
        tokio::fs::write(root.join("git.git.json"), git_manifest)
            .await
            .unwrap();
        let vim_manifest = r#"{"PackageIdentifier":"Vim.Vim","Versions":[{"PackageVersion":"9.1","Installers":[{"InstallerUrl":"http://p:3128/https://github.com/vim/vim-win32-installer/releases/download/v9.1.0/gvim_9.1.0_x64.exe","InstallerSha256":"FFFF0000EEEE1111DDDD2222CCCC3333BBBB4444AAAA55559999888877776666","InstallerType":null,"Architecture":"x64"}]}]}"#;
        tokio::fs::write(root.join("vim.vim.json"), vim_manifest)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_router_manifest_search() {
        let (state, dir) = test_state();
        write_fixture(dir.path()).await;

        // Exact match via Inclusions
        let app = router(state.clone());
        let body = json!({
            "Query": {"Key": "git", "MatchType": "Substring"},
            "Inclusions": [
                {"PackageMatchField": "Id",
                 "RequestMatch": {"KeyWord": "Git.Git", "MatchType": "Exact"}}
            ]
        });
        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/manifestSearch")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["Data"].as_array().unwrap().len(), 1);
        assert_eq!(v["Data"][0]["PackageIdentifier"], "Git.Git");

        // Substring via Query fallback matches both
        let app = router(state);
        let body = json!({"Query": {"Key": "vim", "MatchType": "Substring"}});
        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/manifestSearch")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["Data"].as_array().unwrap().len(), 1);
        assert_eq!(v["Data"][0]["PackageIdentifier"], "Vim.Vim");
    }

    #[tokio::test]
    async fn test_router_package_manifest_serve() {
        let (state, dir) = test_state();
        write_fixture(dir.path()).await;

        // Case-insensitive package id lookup
        let app = router(state);
        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/packageManifests/GIT.GIT")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get("content-type").unwrap(),
            "application/json"
        );
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["Data"]["PackageIdentifier"], "Git.Git");
        let inst = &v["Data"]["Versions"][0]["Installers"][0];
        // Served manifest keeps the rewritten proxy URL and preserved SHA
        assert!(inst["InstallerUrl"]
            .as_str()
            .unwrap()
            .starts_with("http://p:3128/https://github.com/"));
        assert_eq!(
            inst["InstallerSha256"],
            "AAAA1111BBBB2222CCCC3333DDDD4444EEEE5555FFFF6666AAAA7777BBBB8888"
        );
        assert_eq!(inst["Architecture"], "x64");
        assert!(v["ContinuationToken"].is_null());

        // Unknown package → 404
        let (state2, _d) = test_state();
        let app = router(state2);
        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/packageManifests/Nosuch.Package")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_router_package_manifest_traversal_blocked() {
        let (state, dir) = test_state();
        write_fixture(dir.path()).await;
        let secret = dir.path().join("secret.json");
        tokio::fs::write(&secret, "{\"secret\":true}")
            .await
            .unwrap();

        let app = router(state);
        // Encoded traversal: %2F decodes to '/', caught by the sanitizer
        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/packageManifests/..%2F..%2Fsecret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_ne!(resp.status(), StatusCode::OK);

        // Literal traversal segment is normalized by hyper/router — never 200
        let (state2, _d) = test_state();
        let app = router(state2);
        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/packageManifests/../../etc/passwd")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_ne!(resp.status(), StatusCode::OK);
    }

    #[test]
    fn test_valid_package_id() {
        assert!(valid_package_id("Git.Git"));
        assert!(valid_package_id("Microsoft.PowerShell-Preview_2"));
        assert!(!valid_package_id(""));
        assert!(!valid_package_id("../etc/passwd"));
        assert!(!valid_package_id("a/b"));
        assert!(!valid_package_id("a b"));
    }

    #[test]
    fn test_parse_github_repo() {
        let url = url::Url::parse("https://github.com/microsoft/winget-pkgs").unwrap();
        assert_eq!(
            parse_github_repo(&url),
            Some(("microsoft".to_string(), "winget-pkgs".to_string()))
        );
        let url = url::Url::parse("https://example.com/foo/bar").unwrap();
        assert_eq!(parse_github_repo(&url), None);
    }

    // sync()/sync_from() end-to-end tests skipped: they require network
    // (GitHub contents API + raw.githubusercontent.com). The YAML→REST
    // conversion (URL rewrite + SHA preservation), search filtering, router,
    // and SSRF-relevant parsing paths are covered above; hermetic
    // interception of validate_url_host needs a DNS stub per slice guidance.
}
