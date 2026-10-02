//! Homebrew package manager backend implementation (Sprint 26 Phase 1).
//!
//! Caches the Homebrew fetch surface via brew's env-var mirror domains
//! (`~/.homebrew/brew.env`, brew's native env file — user-level, no shell
//! profile clobbering):
//! - `HOMEBREW_API_DOMAIN` → formula/cask/JWS-bundle JSON from
//!   `formulae.brew.sh/api/...` (mutable → `LangMetadata`, TTL revalidation
//!   rides free on etag).
//! - `HOMEBREW_BOTTLE_DOMAIN` → ghcr manifests (immutable per
//!   name+version+rebuild → `LangArtifact`; avoids the revalidation-auth gap).
//!
//! Bottle blob URLs are constructed by the proxy's bespoke handler, NOT via
//! URL patterns (digest URLs live inside the formula JSON), so this backend
//! exposes exactly two patterns. On a shim 404 brew silently falls back to
//! origin — mirror-list semantics, never breaks installs.
//!
//! Wire facts bench-verified 2026-10-01 (brew 7.0.7, macOS 26.3.1); see
//! docs/plans/sprint26-phase1-homebrew/context-pack-common.md.

use crate::{PackageManagerBackend, PackageManagerKind, PackageRef, ProxyConfigAction, UrlPattern};
use async_trait::async_trait;
use std::path::PathBuf;

/// Path of brew's user-level env file that Monopsony owns outright.
const BREW_ENV_PATH: &str = "~/.homebrew/brew.env";

/// Homebrew package manager backend.
pub struct HomebrewBackend {
    #[allow(dead_code)] // Populated by `proxy_config` when a client is configured.
    config_path: Option<PathBuf>,
}

impl HomebrewBackend {
    /// Create a new Homebrew backend instance.
    pub fn new() -> Self {
        HomebrewBackend { config_path: None }
    }
}

impl Default for HomebrewBackend {
    fn default() -> Self {
        Self::new()
    }
}

/// Parse `brew list --formula/--cask --versions` output: plain `name version`
/// lines (version may contain multiple space-separated tokens for
/// multiply-installed versions — keep the latest/last token as brew prints
/// them in ascending order). Tap prefixes (`homebrew/cask/`) are stripped if
/// present. Exposed as a plain fn so the parser is testable without brew.
fn parse_installed_output(output: &str) -> Vec<PackageRef> {
    output
        .lines()
        .map(|line| line.trim())
        .filter(|line| !line.is_empty())
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let raw_name = parts.next()?;
            // Strip tap prefixes down to the bare name (brew normally emits
            // bare names, but `homebrew/cask/<name>` shapes exist).
            let name = raw_name.rsplit('/').next().unwrap_or(raw_name);
            // `--versions` may print several versions ascending; take the last.
            let version = parts.last().map(String::from);
            Some(PackageRef {
                name: name.to_string(),
                version,
                architecture: None,
                manager: PackageManagerKind::Homebrew,
                installed_at: 0,
            })
        })
        .collect()
}

#[async_trait]
impl PackageManagerBackend for HomebrewBackend {
    fn kind(&self) -> PackageManagerKind {
        PackageManagerKind::Homebrew
    }

    fn is_available(&self) -> bool {
        which::which("brew").is_ok()
    }

    async fn list_installed(&self) -> crate::Result<Vec<PackageRef>> {
        // Absolute path first: non-interactive contexts (launchd, ssh without
        // login shell) don't have /opt/homebrew/bin on PATH.
        let brew = if std::path::Path::new("/opt/homebrew/bin/brew").exists() {
            "/opt/homebrew/bin/brew"
        } else {
            "brew"
        };

        let mut packages = Vec::new();
        // Formulae and casks are separate invocations; a cask-less install
        // must not fail the whole inventory.
        for args in [
            ["list", "--formula", "--versions"],
            ["list", "--cask", "--versions"],
        ] {
            let output = tokio::process::Command::new(brew).args(args).output().await;
            // Both invocations failing (no brew / bare install) → empty Vec,
            // not an error — matches sibling backends' container behavior.
            if let Ok(out) = output {
                if out.status.success() {
                    let stdout = String::from_utf8_lossy(&out.stdout);
                    packages.extend(parse_installed_output(&stdout));
                }
            }
        }
        Ok(packages)
    }

    fn upstream_url_patterns(&self) -> Vec<UrlPattern> {
        vec![
            // ghcr bottle manifests: /v2/homebrew/core/<name>/manifests/<ver>-<rebuild>.
            // Anchored to homebrew/core so generic ghcr.io repos (GitLab etc.)
            // never false-positive; docker.rs's ghcr handling is a separate
            // bespoke path. Priority 110 wins if a URL ever matches both.
            UrlPattern::with_class(
                r"https?://ghcr\.io/v2/homebrew/core/[^/]+/manifests/[^/]+$",
                110,
                "Homebrew bottle manifests",
                crate::config::CacheClass::LangArtifact,
            )
            .expect("valid regex"),
            // formulae.brew.sh API JSON: formula/<name>.json,
            // cask/<name>.json, internal/packages.<platform>.jws.json.
            UrlPattern::with_class(
                r"https?://[^/]*formulae\.brew\.sh/api/.*",
                100,
                "Homebrew API metadata",
                crate::config::CacheClass::LangMetadata,
            )
            .expect("valid regex"),
        ]
    }

    fn url_to_cache_path(&self, url: &str) -> Option<String> {
        let url = url::Url::parse(url).ok()?;
        // Both pattern shapes share the flat mapping: brew/api/formula/jq.json,
        // brew/v2/homebrew/core/jq/manifests/1.8.2-1. Bottle blob keys are
        // constructed by the proxy's bespoke handler instead.
        Some(format!("brew{}", url.path()))
    }

    fn proxy_config(&self, proxy_addr: &str) -> ProxyConfigAction {
        // brew's native env file: plain KEY=value lines, no export, no shell
        // expansion. Only API+BOTTLE domains (ARTIFACT_DOMAIN cask prefixing
        // is unverified — deliberately out of scope). Plain http on purpose:
        // the proxy's plain-http shim is the whole point.
        let config_content = format!(
            "HOMEBREW_API_DOMAIN=http://{}/brew/api\nHOMEBREW_BOTTLE_DOMAIN=http://{}/brew/bottles\n",
            proxy_addr, proxy_addr
        );
        ProxyConfigAction::WriteFile {
            path: PathBuf::from(BREW_ENV_PATH),
            content: config_content,
        }
    }

    fn remove_proxy_config(&self) -> ProxyConfigAction {
        ProxyConfigAction::RemoveFile {
            path: PathBuf::from(BREW_ENV_PATH),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_kind_and_availability() {
        let backend = HomebrewBackend::new();
        assert_eq!(backend.kind(), PackageManagerKind::Homebrew);
        // Must not panic; result depends on the host.
        let _ = backend.is_available();
    }

    #[test]
    fn test_parse_kind_round_trip() {
        assert_eq!(
            PackageManagerKind::parse_kind("brew"),
            Some(PackageManagerKind::Homebrew)
        );
        assert_eq!(PackageManagerKind::Homebrew.as_str(), "brew");
    }

    #[test]
    fn test_patterns_match_formula_cask_and_jws() {
        let backend = HomebrewBackend::new();
        let patterns = backend.upstream_url_patterns();
        // Formula JSON (live-verified shape).
        assert!(patterns
            .iter()
            .any(|p| p.matches("https://formulae.brew.sh/api/formula/jq.json")));
        // Cask JSON.
        assert!(patterns
            .iter()
            .any(|p| p.matches("https://formulae.brew.sh/api/cask/visual-studio-code.json")));
        // Aggregate JWS bundle with platform tag.
        assert!(patterns
            .iter()
            .any(|p| p
                .matches("https://formulae.brew.sh/api/internal/packages.arm64_tahoe.jws.json")));
    }

    #[test]
    fn test_patterns_classify_metadata_vs_manifest() {
        let backend = HomebrewBackend::new();
        let patterns = backend.upstream_url_patterns();

        let meta = "https://formulae.brew.sh/api/formula/jq.json";
        let meta_class = patterns
            .iter()
            .find(|p| p.matches(meta))
            .unwrap()
            .cache_class;
        assert_eq!(meta_class, crate::config::CacheClass::LangMetadata);

        let manifest = "https://ghcr.io/v2/homebrew/core/jq/manifests/1.8.2-1";
        assert!(patterns.iter().any(|p| p.matches(manifest)));
        let art_class = patterns
            .iter()
            .find(|p| p.matches(manifest))
            .unwrap()
            .cache_class;
        assert_eq!(art_class, crate::config::CacheClass::LangArtifact);
    }

    #[test]
    fn test_patterns_reject_non_brew_urls() {
        let backend = HomebrewBackend::new();
        let patterns = backend.upstream_url_patterns();
        // Generic ghcr repo (GitLab/other) must NOT match the homebrew/core
        // anchor.
        assert!(!patterns
            .iter()
            .any(|p| p.matches("https://ghcr.io/v2/some/other/repo/manifests/x")));
        // ghcr blob (bespoke-handler territory, not a pattern shape).
        assert!(!patterns
            .iter()
            .any(|p| p.matches("https://ghcr.io/v2/homebrew/core/jq/blobs/sha256:abc123")));
        // Unrelated hosts.
        assert!(!patterns
            .iter()
            .any(|p| p.matches("https://registry.npmjs.org/left-pad")));
        assert!(!patterns
            .iter()
            .any(|p| p.matches("https://example.com/api/formula/jq.json")));
    }

    #[test]
    fn test_url_to_cache_path() {
        let backend = HomebrewBackend::new();
        // Formula JSON.
        assert_eq!(
            backend.url_to_cache_path("https://formulae.brew.sh/api/formula/jq.json"),
            Some("brew/api/formula/jq.json".to_string())
        );
        // ghcr manifest.
        assert_eq!(
            backend.url_to_cache_path("https://ghcr.io/v2/homebrew/core/jq/manifests/1.8.2-1"),
            Some("brew/v2/homebrew/core/jq/manifests/1.8.2-1".to_string())
        );
        // Not a URL.
        assert_eq!(backend.url_to_cache_path("not a url"), None);
    }

    #[test]
    fn test_proxy_config_writes_brew_env() {
        let backend = HomebrewBackend::new();
        let action = backend.proxy_config("10.10.20.58:3128");
        match action {
            ProxyConfigAction::WriteFile { path, content } => {
                assert_eq!(path, PathBuf::from("~/.homebrew/brew.env"));
                assert!(content.contains("HOMEBREW_API_DOMAIN=http://10.10.20.58:3128/brew/api"));
                assert!(
                    content.contains("HOMEBREW_BOTTLE_DOMAIN=http://10.10.20.58:3128/brew/bottles")
                );
                // Status-check contract (client lang_pm_config.rs): the file
                // content must contain http:// to be recognized as proxied.
                assert!(content.contains("http://"));
                // Only the two verified domains — no ARTIFACT_DOMAIN.
                assert!(!content.contains("ARTIFACT_DOMAIN"));
            }
            _ => panic!("expected WriteFile action"),
        }
    }

    #[test]
    fn test_remove_proxy_config() {
        let backend = HomebrewBackend::new();
        match backend.remove_proxy_config() {
            ProxyConfigAction::RemoveFile { path } => {
                assert_eq!(path, PathBuf::from("~/.homebrew/brew.env"));
            }
            _ => panic!("expected RemoveFile action"),
        }
    }

    #[test]
    fn test_parse_installed_output() {
        let refs = parse_installed_output("jq 1.8.2\nwget 1.25\n");
        assert_eq!(refs.len(), 2);
        assert_eq!(refs[0].name, "jq");
        assert_eq!(refs[0].version, Some("1.8.2".to_string()));
        assert_eq!(refs[0].manager, PackageManagerKind::Homebrew);
        assert_eq!(refs[0].installed_at, 0);
        assert_eq!(refs[1].name, "wget");
        assert_eq!(refs[1].version, Some("1.25".to_string()));
    }

    #[test]
    fn test_parse_installed_output_strips_tap_prefix() {
        let refs = parse_installed_output("homebrew/cask/visual-studio-code 1.92.0");
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].name, "visual-studio-code");
        assert_eq!(refs[0].version, Some("1.92.0".to_string()));
    }

    #[test]
    fn test_parse_installed_output_multi_version_and_noise() {
        // `--versions` can print several versions ascending; keep the last.
        let refs = parse_installed_output("python@3.11 3.11.9_1 3.11.10\n\n  \n");
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].name, "python@3.11");
        assert_eq!(refs[0].version, Some("3.11.10".to_string()));
        // Empty output → empty vec.
        assert!(parse_installed_output("").is_empty());
    }
}
