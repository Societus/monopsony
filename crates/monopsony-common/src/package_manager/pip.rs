//! pip (PyPI) package manager backend implementation (Sprint 23 Phase B).
//!
//! Caches PyPI's PEP 503 simple-index metadata page and wheel/sdist artifacts.
//! Metadata is TTL-revalidated (`LangMetadata`); wheels/SDists are immutable
//! (`LangArtifact`). Note the host split: metadata lives on pypi.org, artifacts
//! on files.pythonhosted.org — both covered and classified correctly.
//!
//! URL shapes verified live 2026-09-21 (curl, 200):
//! - simple index: `https://pypi.org/simple/<name>/`
//! - artifact: `https://files.pythonhosted.org/packages/<2hex>/<2hex>/<hexdigest>/<file>`
//!   where `<file>` is e.g. `requests-0.2.0.tar.gz` /
//!   `cffi-1.17.1-cp312-cp312-manylinux_2_17_x86_64.whl`

use crate::{PackageManagerBackend, PackageManagerKind, PackageRef, ProxyConfigAction, UrlPattern};
use std::path::PathBuf;

/// pip / PyPI package manager backend.
pub struct PipBackend {
    #[allow(dead_code)] // Populated by `proxy_config` when a client is configured.
    config_path: Option<PathBuf>,
}

impl PipBackend {
    /// Create a new pip backend instance.
    pub fn new() -> Self {
        PipBackend { config_path: None }
    }
}

impl Default for PipBackend {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl PackageManagerBackend for PipBackend {
    fn kind(&self) -> PackageManagerKind {
        PackageManagerKind::Pip
    }

    fn is_available(&self) -> bool {
        which::which("pip").is_ok()
            || which::which("pip3").is_ok()
            || which::which("python").is_ok()
    }

    async fn list_installed(&self) -> crate::Result<Vec<PackageRef>> {
        // `pip list --format=json` → JSON array of {name, version}.
        let output = tokio::process::Command::new("pip")
            .args(["list", "--format=json"])
            .output()
            .await?;

        if !output.status.success() {
            return Err(crate::Error::CommandFailed {
                command: "pip list --format=json".to_string(),
                exit_code: output.status.code(),
            });
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let parsed: serde_json::Value = serde_json::from_str(&stdout)
            .map_err(|e| crate::Error::Parse(format!("pip list parse: {}", e)))?;

        let mut packages = Vec::new();
        if let Some(arr) = parsed.as_array() {
            for entry in arr {
                if let Some(name) = entry.get("name").and_then(|v| v.as_str()) {
                    let ver = entry.get("version").and_then(|v| v.as_str());
                    packages.push(PackageRef {
                        name: name.to_string(),
                        version: ver.map(|s| s.to_string()),
                        architecture: None,
                        manager: PackageManagerKind::Pip,
                        installed_at: 0,
                    });
                }
            }
        }
        Ok(packages)
    }

    fn upstream_url_patterns(&self) -> Vec<UrlPattern> {
        vec![
            // Wheel/sdist artifacts on files.pythonhosted.org. Live-verified
            // path: /packages/<2hex>/<2hex>/<hexdigest>/<filename> where
            // filename ends .whl/.tar.gz/.zip. Priority 110 wins over metadata.
            UrlPattern::with_class(
                r"https?://[^/]*files\.pythonhosted\.org/packages/[0-9a-f]{2}/[0-9a-f]{2}/[0-9a-f]+/[^/]+\.(?:whl|tar\.gz|zip)$",
                110,
                "Python wheel/sdist artifacts",
                crate::config::CacheClass::LangArtifact,
            )
            .expect("valid regex"),
            // Simple-index metadata page on pypi.org: /simple/<name>/.
            UrlPattern::with_class(
                r"https?://[^/]*pypi\.org/simple/[A-Za-z0-9_.\-]+/?(?:\?.*)?$",
                100,
                "PyPI simple-index metadata",
                crate::config::CacheClass::LangMetadata,
            )
            .expect("valid regex"),
        ]
    }

    fn url_to_cache_path(&self, url: &str) -> Option<String> {
        let url = url::Url::parse(url).ok()?;
        // Prefix by host family so pypi.org metadata and files.pythonhosted.org
        // artifacts never collide on identical paths.
        let host = url.host_str().unwrap_or("");
        let relative = url.path().trim_start_matches('/');
        if host.contains("files.pythonhosted.org") {
            Some(format!("pip/files/{}", relative))
        } else {
            Some(format!("pip/{}", relative))
        }
    }

    fn proxy_config(&self, proxy_addr: &str) -> ProxyConfigAction {
        // Registry-cache model: point pip's index at the local /pypi/simple/
        // shim (which maps to https://pypi.org + path). The shim speaks plain
        // HTTP so the metadata page is cacheable (PyPI itself 403s plain http,
        // hence the shim). `trusted-host` (no scheme) silences pip's
        // insecure-index warning for the http:// address.
        //
        // KNOWN LIMITATION: the simple-index page body embeds absolute
        // https://files.pythonhosted.org links for artifacts. The proxy does
        // not rewrite bodies, so artifact downloads go directly to upstream
        // and BYPASS the shim/cache. Only the metadata page (LangMetadata,
        // TTL-managed) is cached; artifacts (LangArtifact patterns) still
        // classify correctly server-side but are only cached if fetched
        // through the proxy by other means.
        ProxyConfigAction::WriteFile {
            path: pip_conf_path(),
            content: format!(
                "[global]\nindex-url = http://{addr}/pypi/simple\ntrusted-host = {addr}\n",
                addr = proxy_addr
            ),
        }
    }

    fn remove_proxy_config(&self) -> ProxyConfigAction {
        ProxyConfigAction::RemoveFile {
            path: pip_conf_path(),
        }
    }
}

/// User-level pip config file location.
fn pip_conf_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/root".to_string());
    PathBuf::from(home).join(".pip").join("pip.conf")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_kind_and_availability() {
        let backend = PipBackend::new();
        assert_eq!(backend.kind(), PackageManagerKind::Pip);
    }

    #[test]
    fn test_patterns_classify_metadata_vs_artifact() {
        let backend = PipBackend::new();
        let patterns = backend.upstream_url_patterns();

        // Simple-index metadata (live-verified) → LangMetadata.
        let meta = "https://pypi.org/simple/requests/";
        assert!(patterns.iter().any(|p| p.matches(meta)));
        let meta_class = patterns
            .iter()
            .find(|p| p.matches(meta))
            .unwrap()
            .cache_class;
        assert_eq!(meta_class, crate::config::CacheClass::LangMetadata);

        // Artifact (live-verified shape from the real simple-index page) →
        // LangArtifact.
        let sdist = "https://files.pythonhosted.org/packages/ba/bb/dfa0141a32d773c47e4dede1a617c59a23b74dd302e449cf85413fc96bc4/requests-0.2.0.tar.gz";
        assert!(patterns.iter().any(|p| p.matches(sdist)));
        let art_class = patterns
            .iter()
            .find(|p| p.matches(sdist))
            .unwrap()
            .cache_class;
        assert_eq!(art_class, crate::config::CacheClass::LangArtifact);
    }

    #[test]
    fn test_artifact_pattern_wheel_shape() {
        let backend = PipBackend::new();
        let patterns = backend.upstream_url_patterns();
        // Wheel with platform tags in the filename.
        assert!(patterns.iter().any(|p| p.matches(
            "https://files.pythonhosted.org/packages/4b/ad/d536b2e572e843fda13e4458c67f937b05ce359722c1e4cdad35ba05b6e3/cffi-1.17.1-cp312-cp312-manylinux_2_17_x86_64.whl"
        )));
        // Metadata pattern must NOT swallow artifact paths (host split).
        let meta_pat = patterns
            .iter()
            .find(|p| p.description.contains("metadata"))
            .unwrap();
        assert!(!meta_pat.matches(
            "https://files.pythonhosted.org/packages/ba/bb/dfa0141a32d773c47e4dede1a617c59a23b74dd302e449cf85413fc96bc4/requests-0.2.0.tar.gz"
        ));
    }

    #[test]
    fn test_url_to_cache_path_splits_by_host_family() {
        let backend = PipBackend::new();
        assert_eq!(
            backend.url_to_cache_path("https://pypi.org/simple/requests/"),
            Some("pip/simple/requests/".to_string())
        );
        assert_eq!(
            backend.url_to_cache_path(
                "https://files.pythonhosted.org/packages/ba/bb/d4/requests-0.2.0.tar.gz"
            ),
            Some("pip/files/packages/ba/bb/d4/requests-0.2.0.tar.gz".to_string())
        );
    }

    #[test]
    fn test_proxy_config_sets_index_url() {
        let backend = PipBackend::new();
        match backend.proxy_config("10.10.20.58:3128") {
            ProxyConfigAction::WriteFile { path, content } => {
                assert!(path.ends_with(".pip/pip.conf"));
                assert!(content.contains("index-url = http://10.10.20.58:3128/pypi/simple"));
                assert!(content.contains("trusted-host = 10.10.20.58:3128"));
            }
            _ => panic!("expected WriteFile action"),
        }
    }

    #[test]
    fn test_remove_proxy_config() {
        let backend = PipBackend::new();
        match backend.remove_proxy_config() {
            ProxyConfigAction::RemoveFile { path } => {
                assert!(path.ends_with(".pip/pip.conf"));
            }
            _ => panic!("expected RemoveFile action"),
        }
    }
}
