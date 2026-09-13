//! OSV.dev CVE lookup client.
//!
//! Queries the OSV API for known vulnerabilities in package versions.
//! Results are used by the policy engine to warn or block packages with
//! known CVEs.

use serde::{Deserialize, Serialize};
use std::time::Duration;
use tracing::debug;

/// A single CVE match from OSV.dev.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CveMatch {
    /// OSV ID (e.g., "GHSA-xxxx-xxxx-xxxx" or "CVE-2024-1234").
    pub id: String,
    /// Summary of the vulnerability.
    pub summary: String,
    /// Severity level (if available): "critical", "high", "medium", "low".
    pub severity: Option<String>,
    /// Whether the vulnerability has a known fix.
    pub fixed: bool,
}

/// Result of an OSV query.
#[derive(Debug, Clone)]
pub struct OsvResult {
    /// CVE matches found.
    pub matches: Vec<CveMatch>,
    /// Error message if query failed.
    pub error: Option<String>,
}

/// OSV.dev API request body.
#[derive(Debug, Serialize)]
struct OsvQuery {
    version: String,
    package: OsvPackage,
}

#[derive(Debug, Serialize)]
struct OsvPackage {
    name: String,
    ecosystem: String,
}

/// OSV.dev API response (partial — we only need id, summary, severity).
#[derive(Debug, Deserialize)]
struct OsvResponse {
    vulns: Option<Vec<OsvVuln>>,
}

#[derive(Debug, Deserialize)]
struct OsvVuln {
    id: String,
    summary: Option<String>,
    severity: Option<Vec<OsvSeverity>>,
}

#[derive(Debug, Deserialize)]
struct OsvSeverity {
    #[serde(rename = "type")]
    _sev_type: Option<String>,
    score: Option<String>,
}

/// Query OSV.dev for vulnerabilities in a package version.
///
/// `endpoint` is the base URL (e.g., "https://api.osv.dev").
/// `package_name` is the package name (e.g., "lodash").
/// `package_version` is the version string (e.g., "4.17.20").
/// `ecosystem` is the package ecosystem (e.g., "npm", "PyPI", "crates.io").
pub async fn lookup_cve(
    endpoint: &str,
    package_name: &str,
    package_version: &str,
    ecosystem: &str,
    timeout: Duration,
) -> OsvResult {
    // Skip lookup if ecosystem is empty (can't query without it)
    if ecosystem.is_empty() {
        debug!(
            "Skipping CVE lookup: no ecosystem for {}@{}",
            package_name, package_version
        );
        return OsvResult {
            matches: vec![],
            error: None,
        };
    }

    let client = match reqwest::Client::builder().timeout(timeout).build() {
        Ok(c) => c,
        Err(e) => {
            return OsvResult {
                matches: vec![],
                error: Some(format!("failed to build HTTP client: {}", e)),
            }
        }
    };

    let url = format!("{}/v1/query", endpoint.trim_end_matches('/'));
    let body = OsvQuery {
        version: package_version.to_string(),
        package: OsvPackage {
            name: package_name.to_string(),
            ecosystem: ecosystem.to_string(),
        },
    };

    debug!(
        "OSV query: {}@{} ecosystem={}",
        package_name, package_version, ecosystem
    );

    match client.post(&url).json(&body).send().await {
        Ok(resp) => match resp.json::<OsvResponse>().await {
            Ok(osv_resp) => {
                let matches: Vec<CveMatch> = osv_resp
                    .vulns
                    .unwrap_or_default()
                    .into_iter()
                    .map(|v| {
                        let severity = v
                            .severity
                            .as_ref()
                            .and_then(|s| s.first())
                            .and_then(|s| s.score.clone())
                            .map(normalize_severity);
                        CveMatch {
                            id: v.id,
                            summary: v.summary.unwrap_or_default(),
                            severity,
                            fixed: false,
                        }
                    })
                    .collect();
                debug!(
                    "OSV returned {} vulns for {}@{}",
                    matches.len(),
                    package_name,
                    package_version
                );
                OsvResult {
                    matches,
                    error: None,
                }
            }
            Err(e) => OsvResult {
                matches: vec![],
                error: Some(format!("failed to parse OSV response: {}", e)),
            },
        },
        Err(e) => OsvResult {
            matches: vec![],
            error: Some(format!("OSV query failed: {}", e)),
        },
    }
}

/// Normalize severity strings to lowercase canonical form.
fn normalize_severity(raw: String) -> String {
    let lower = raw.to_lowercase();
    if lower.contains("critical") {
        "critical".into()
    } else if lower.contains("high") {
        "high".into()
    } else if lower.contains("medium") {
        "medium".into()
    } else if lower.contains("low") {
        "low".into()
    } else {
        lower
    }
}

/// Infer the package ecosystem from a file path for OSV lookup.
pub fn infer_ecosystem(path: &std::path::Path) -> &'static str {
    let path_str = path.to_string_lossy();
    if path_str.contains("npm") || path_str.ends_with(".tgz") {
        "npm"
    } else if path_str.contains("pypi") || path_str.ends_with(".whl") {
        "PyPI"
    } else if path_str.contains("crates.io") || path_str.ends_with(".crate") {
        "crates.io"
    } else if path_str.contains("apt") || path_str.ends_with(".deb") {
        "Debian"
    } else if path_str.contains("dnf") || path_str.ends_with(".rpm") {
        "Fedora"
    } else {
        "" // OSV requires ecosystem — empty means skip lookup
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_severity_levels() {
        assert_eq!(normalize_severity("HIGH".into()), "high");
        assert_eq!(normalize_severity("Critical".into()), "critical");
        assert_eq!(normalize_severity("MEDIUM".into()), "medium");
        assert_eq!(normalize_severity("low".into()), "low");
    }

    #[test]
    fn test_normalize_severity_cvss_string() {
        // CVSS vector strings don't contain severity keywords
        assert_eq!(normalize_severity("CVSS:3.1/AV:N".into()), "cvss:3.1/av:n");
    }

    #[test]
    fn test_osv_result_default() {
        let result = OsvResult {
            matches: vec![],
            error: None,
        };
        assert!(result.matches.is_empty());
        assert!(result.error.is_none());
    }

    #[test]
    fn test_parse_osv_response_json() {
        let json = r#"{"vulns":[{"id":"GHSA-xxxx","summary":"test vuln","severity":[{"type":"CVSS","score":"HIGH"}]}]}"#;
        let resp: OsvResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.vulns.unwrap().len(), 1);
    }

    #[test]
    fn test_parse_empty_osv_response() {
        let json = r#"{}"#;
        let resp: OsvResponse = serde_json::from_str(json).unwrap();
        assert!(resp.vulns.is_none());
    }

    #[test]
    fn test_cve_match_serialization() {
        let m = CveMatch {
            id: "CVE-2024-1234".into(),
            summary: "A bad thing".into(),
            severity: Some("high".into()),
            fixed: false,
        };
        let json = serde_json::to_string(&m).unwrap();
        assert!(json.contains("CVE-2024-1234"));
    }

    #[test]
    fn test_infer_ecosystem_npm() {
        assert_eq!(
            infer_ecosystem(std::path::Path::new("/cache/npm/lodash-4.17.20.tgz")),
            "npm"
        );
    }

    #[test]
    fn test_infer_ecosystem_pypi() {
        assert_eq!(
            infer_ecosystem(std::path::Path::new("/cache/pypi/requests-2.28.0.whl")),
            "PyPI"
        );
    }

    #[test]
    fn test_infer_ecosystem_crates() {
        assert_eq!(
            infer_ecosystem(std::path::Path::new("/cache/crates.io/serde-1.0.crate")),
            "crates.io"
        );
    }

    #[test]
    fn test_infer_ecosystem_unknown() {
        assert_eq!(
            infer_ecosystem(std::path::Path::new("/cache/random/file.bin")),
            ""
        );
    }

    #[tokio::test]
    async fn test_lookup_cve_empty_ecosystem_skips() {
        let result = lookup_cve(
            "https://api.osv.dev",
            "test",
            "1.0.0",
            "", // empty ecosystem
            Duration::from_secs(5),
        )
        .await;
        assert!(result.matches.is_empty());
        assert!(result.error.is_none());
    }

    // NOTE: Live OSV.dev test behind #[ignore] to avoid CI failures.
    #[tokio::test]
    #[ignore] // Run with `cargo test -- --ignored` for live test
    async fn test_live_osv_lookup() {
        let result = lookup_cve(
            "https://api.osv.dev",
            "lodash",
            "4.17.20",
            "npm",
            Duration::from_secs(10),
        )
        .await;
        assert!(result.error.is_none(), "Error: {:?}", result.error);
        // lodash 4.17.20 has known prototype pollution vulns
        assert!(
            !result.matches.is_empty(),
            "Expected CVE matches for lodash 4.17.20"
        );
    }
}
