//! npm package manager backend implementation (Sprint 23 Phase B).
//!
//! Caches the dev-machine surface of npm: registry metadata (packuments) and
//! tarball artifacts. Metadata is TTL-revalidated (`LangMetadata`); tarballs are
//! immutable and cached forever subject to retention floors (`LangArtifact`).
//!
//! URL shapes verified live 2026-09-21 (curl HEAD, 200):
//! - packument: `https://registry.npmjs.org/<name>` (single segment; scoped
//!   names may arrive percent-encoded as one segment, `/@scope%2fpkg`)
//! - tarball: `https://registry.npmjs.org/<name>/-/<file>-<ver>.tgz` (scoped:
//!   `/@scope/<name>/-/<file>-<ver>.tgz`)
//!
//! Patterns are anchored to registry.npmjs.org only. Private registries are a
//! config-driven extension (deliberately not host-agnostic: GitLab paths use
//! `/-/` segments and would false-positive).

use crate::{PackageManagerBackend, PackageManagerKind, PackageRef, ProxyConfigAction, UrlPattern};
use async_trait::async_trait;
use std::collections::BTreeMap;
use std::path::PathBuf;

/// npm package manager backend.
pub struct NpmBackend {
    #[allow(dead_code)] // Populated by `proxy_config` when a client is configured.
    config_path: Option<PathBuf>,
}

impl NpmBackend {
    /// Create a new npm backend instance.
    pub fn new() -> Self {
        NpmBackend { config_path: None }
    }
}

impl Default for NpmBackend {
    fn default() -> Self {
        Self::new()
    }
}

/// Recursively walk the `dependencies` maps of `npm ls --json` output,
/// collecting every (name, version) pair. `npm ls --json` emits a nested tree
/// (object of objects), not an array; `--all` includes transitive deps in
/// nested `dependencies` keys, so a flat list needs the walk.
fn collect_npm_deps(node: &serde_json::Value, seen: &mut BTreeMap<String, Option<String>>) {
    let Some(deps) = node.get("dependencies").and_then(|v| v.as_object()) else {
        return;
    };
    for (name, meta) in deps {
        let ver = meta
            .get("version")
            .and_then(|v| v.as_str())
            .map(String::from);
        seen.entry(name.clone()).or_insert(ver);
        // Recurse into children (transitive deps under --all).
        collect_npm_deps(meta, seen);
    }
}

#[async_trait]
impl PackageManagerBackend for NpmBackend {
    fn kind(&self) -> PackageManagerKind {
        PackageManagerKind::Npm
    }

    fn is_available(&self) -> bool {
        which::which("npm").is_ok()
    }

    async fn list_installed(&self) -> crate::Result<Vec<PackageRef>> {
        // `npm ls --json --all`: object with a `dependencies` map (nested tree).
        let output = tokio::process::Command::new("npm")
            .args(["ls", "--json", "--all"])
            .output()
            .await?;

        if !output.status.success() {
            return Err(crate::Error::CommandFailed {
                command: "npm ls --json --all".to_string(),
                exit_code: output.status.code(),
            });
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let trimmed = stdout.trim();
        if trimmed.is_empty() {
            return Ok(Vec::new());
        }
        let parsed: serde_json::Value = serde_json::from_str(trimmed)
            .map_err(|e| crate::Error::Parse(format!("npm ls --json parse: {}", e)))?;

        let mut seen = BTreeMap::new();
        collect_npm_deps(&parsed, &mut seen);
        Ok(seen
            .into_iter()
            .map(|(name, version)| PackageRef {
                name,
                version,
                architecture: None,
                manager: PackageManagerKind::Npm,
                installed_at: 0,
            })
            .collect())
    }

    fn upstream_url_patterns(&self) -> Vec<UrlPattern> {
        vec![
            // Tarball artifacts: /<name>/-/<file>-<ver>.tgz, scoped adds one
            // segment: /@scope/<name>/-/<file>-<ver>.tgz. Priority 110 wins
            // over metadata if a URL ever matches both.
            UrlPattern::with_class(
                r"https?://[^/]*registry\.npmjs\.org/[^/]+(?:/[^/]+)?/-/[^/]+-\d+\.\d+\.\d+[-0-9A-Za-z.]*\.(?:tgz|tar\.gz)$",
                110,
                "npm tarball artifacts",
                crate::config::CacheClass::LangArtifact,
            )
            .expect("valid regex"),
            // Packument metadata: /<name> or /@scope/<name> — single logical
            // package path with no `/-/` artifact segment and no deeper path.
            // Scoped names may arrive percent-encoded in one segment.
            UrlPattern::with_class(
                r"https?://[^/]*registry\.npmjs\.org/(?:@[^/]+(?:/[^/]+)?|[^/@][^/]*)/?(?:\?.*)?$",
                100,
                "npm registry metadata",
                crate::config::CacheClass::LangMetadata,
            )
            .expect("valid regex"),
        ]
    }

    fn url_to_cache_path(&self, url: &str) -> Option<String> {
        let url = url::Url::parse(url).ok()?;
        // Artifact segment (`/-/`) namespaced under `artifact/` — packuments
        // map to `npm/<path>` and tarballs to `npm/artifact/<path>`, so a
        // packument file `npm/left-pad` can never collide with a tarball
        // directory `npm/left-pad/-/...` (os error 20 without this split).
        if url.path().contains("/-/") {
            Some(format!("npm/artifact{}", url.path()))
        } else {
            Some(format!("npm{}", url.path()))
        }
    }

    fn proxy_config(&self, proxy_addr: &str) -> ProxyConfigAction {
        // Registry-shim model: point npm at the proxy's /npm/ origin-form shim
        // (plain http). The transparent forward-proxy model failed for npm —
        // registry.npmjs.org 301s plain http to https (verified live on the
        // bench) — so the registry host is rewritten instead of proxied.
        let config_content = format!("registry=http://{}/npm\n", proxy_addr);
        ProxyConfigAction::WriteFile {
            path: PathBuf::from("~/.npmrc"),
            content: config_content,
        }
    }

    fn remove_proxy_config(&self) -> ProxyConfigAction {
        ProxyConfigAction::RemoveFile {
            path: PathBuf::from("~/.npmrc"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_kind_and_availability() {
        let backend = NpmBackend::new();
        assert_eq!(backend.kind(), PackageManagerKind::Npm);
    }

    #[test]
    fn test_patterns_classify_metadata_vs_tarball() {
        let backend = NpmBackend::new();
        let patterns = backend.upstream_url_patterns();

        // Metadata packument (live-verified: single segment, no trailing slash).
        let meta = "https://registry.npmjs.org/left-pad";
        assert!(patterns.iter().any(|p| p.matches(meta)));
        let meta_class = patterns
            .iter()
            .find(|p| p.matches(meta))
            .unwrap()
            .cache_class;
        assert_eq!(meta_class, crate::config::CacheClass::LangMetadata);

        // Tarball (live-verified) → LangArtifact.
        let tar = "https://registry.npmjs.org/left-pad/-/left-pad-1.3.0.tgz";
        assert!(patterns.iter().any(|p| p.matches(tar)));
        let art_class = patterns
            .iter()
            .find(|p| p.matches(tar))
            .unwrap()
            .cache_class;
        assert_eq!(art_class, crate::config::CacheClass::LangArtifact);
    }

    #[test]
    fn test_tarball_pattern_scope_and_prerelease() {
        let backend = NpmBackend::new();
        let patterns = backend.upstream_url_patterns();
        // Scoped package tarball (live shape: /@scope/<name>/-/...).
        assert!(patterns
            .iter()
            .any(|p| p.matches("https://registry.npmjs.org/@scope/pkg/-/pkg-2.1.0.tgz")));
        // Prerelease tarball.
        assert!(patterns
            .iter()
            .any(|p| p.matches("https://registry.npmjs.org/left-pad/-/left-pad-1.3.0-beta.1.tgz")));
        // Metadata pattern must NOT swallow artifact paths.
        let meta_pat = patterns
            .iter()
            .find(|p| p.description.contains("metadata"))
            .unwrap();
        assert!(!meta_pat.matches("https://registry.npmjs.org/left-pad/-/left-pad-1.3.0.tgz"));
    }

    #[test]
    fn test_url_to_cache_path_namespaces_artifacts() {
        let backend = NpmBackend::new();
        // Packument: flat under npm/.
        assert_eq!(
            backend.url_to_cache_path("https://registry.npmjs.org/left-pad"),
            Some("npm/left-pad".to_string())
        );
        // Tarball: namespaced under npm/artifact/ so the packument file
        // `npm/left-pad` never collides with the tarball directory beneath it.
        assert_eq!(
            backend.url_to_cache_path("https://registry.npmjs.org/left-pad/-/left-pad-1.3.0.tgz"),
            Some("npm/artifact/left-pad/-/left-pad-1.3.0.tgz".to_string())
        );
    }

    #[test]
    fn test_npm_ls_tree_parse() {
        // `npm ls --json --all` shape: nested `dependencies` objects.
        let tree: serde_json::Value = serde_json::from_str(
            r#"{"dependencies": {
                "left-pad": {"version": "1.3.0", "dependencies": {
                    "dup": {"version": "2.0.0"}
                }},
                "chalk": {"version": "5.3.0"}
            }}"#,
        )
        .unwrap();
        let mut seen = BTreeMap::new();
        collect_npm_deps(&tree, &mut seen);
        assert_eq!(seen.get("left-pad"), Some(&Some("1.3.0".to_string())));
        assert_eq!(seen.get("dup"), Some(&Some("2.0.0".to_string())));
        assert_eq!(seen.get("chalk"), Some(&Some("5.3.0".to_string())));
        assert_eq!(seen.len(), 3);
    }

    #[test]
    fn test_proxy_config_sets_registry_shim() {
        let backend = NpmBackend::new();
        let action = backend.proxy_config("10.10.20.58:3128");
        match action {
            ProxyConfigAction::WriteFile { path, content } => {
                assert_eq!(path, PathBuf::from("~/.npmrc"));
                assert!(content.contains("registry=http://10.10.20.58:3128/npm"));
                assert!(!content.contains("https-proxy"));
            }
            _ => panic!("expected WriteFile action"),
        }
    }

    #[test]
    fn test_remove_proxy_config() {
        let backend = NpmBackend::new();
        match backend.remove_proxy_config() {
            ProxyConfigAction::RemoveFile { path } => {
                assert_eq!(path, PathBuf::from("~/.npmrc"));
            }
            _ => panic!("expected RemoveFile action"),
        }
    }
}
