//! Scan engine orchestrator — aggregates ClamAV, YARA, and entropy scanners.
//!
//! The `ScanEngine` runs all configured scanners against cached package data
//! and produces a `ScanResultRecord` for persistence. Scanners run sequentially
//! (not in parallel) to limit resource usage.

use crate::scan_clamav::{self, ClamAvResult};
use crate::scan_entropy::{self, EntropyResult};
use crate::scan_yara::{self, YaraResult};
use monopsony_common::config::ScanConfig;
use monopsony_db::db::ScanResultRecord;
use std::path::Path;
use std::time::Duration;
use tracing::{debug, info, warn};

/// Current scan schema version. Bump when scanner logic changes significantly
/// so that old results can be re-evaluated.
const SCAN_VERSION: i64 = 1;

/// Result of a full scan pass across all scanners.
#[derive(Debug, Clone)]
pub struct FullScanResult {
    /// ClamAV result.
    pub clamav: ClamAvResult,
    /// Entropy heuristic result.
    pub entropy: EntropyResult,
    /// YARA result.
    pub yara: YaraResult,
    /// Overall status derived from individual results.
    pub status: ScanStatus,
}

/// Overall scan status.
#[derive(Debug, Clone, PartialEq)]
pub enum ScanStatus {
    /// All scanners passed, no issues found.
    Clean,
    /// At least one scanner flagged suspicious content but nothing confirmed.
    Flagged,
    /// Confirmed malware or critical threat detected.
    Infected,
    /// Scanner error(s) prevented full evaluation.
    Error,
}

impl ScanStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            ScanStatus::Clean => "clean",
            ScanStatus::Flagged => "flagged",
            ScanStatus::Infected => "infected",
            ScanStatus::Error => "error",
        }
    }
}

/// Run a full scan against the given data and file path.
///
/// `data` is the raw bytes (used for ClamAV INSTREAM and entropy).
/// `file_path` is the on-disk path (used for YARA file scanning).
/// `config` controls scanner behavior and thresholds.
///
/// Returns a `FullScanResult` with individual scanner results and an
/// overall status. Also returns a `ScanResultRecord` ready for DB persistence.
pub async fn run_scan(
    data: &[u8],
    file_path: &Path,
    package_name: Option<&str>,
    package_version: Option<&str>,
    config: &ScanConfig,
) -> (FullScanResult, ScanResultRecord) {
    let scan_timeout = Duration::from_secs(300); // generous default

    // 1. ClamAV scan
    let clamav = if config.enabled {
        scan_clamav::scan_buffer(data, &config.clamd_socket, scan_timeout, None).await
    } else {
        ClamAvResult::Clean // skip if disabled
    };

    // 2. Entropy scan
    let entropy = scan_entropy::scan_entropy(data, config.entropy_threshold);

    // 3. YARA scan
    let yara = if config.enabled {
        if let Some(ref rules_dir) = config.yara_rules_dir {
            scan_yara::scan_file(file_path, rules_dir, scan_timeout).await
        } else {
            debug!("No YARA rules directory configured, skipping YARA scan");
            YaraResult {
                available: false,
                matches: vec![],
                error: None,
            }
        }
    } else {
        YaraResult {
            available: false,
            matches: vec![],
            error: None,
        }
    };

    // 4. CVE lookup (OSV.dev)
    let cve_result = if config.enabled && config.warn_on_cve {
        if let (Some(name), Some(version)) = (package_name, package_version) {
            let ecosystem = crate::scan_cve::infer_ecosystem(file_path);
            crate::scan_cve::lookup_cve(
                &config.osv_endpoint,
                name,
                version,
                ecosystem,
                Duration::from_secs(10),
            )
            .await
        } else {
            crate::scan_cve::OsvResult {
                matches: vec![],
                error: None,
            }
        }
    } else {
        crate::scan_cve::OsvResult {
            matches: vec![],
            error: None,
        }
    };

    // Determine overall status
    let status = derive_status(&clamav, &entropy, &yara);

    // Build the DB record
    let clamav_str = match &clamav {
        ClamAvResult::Clean => Some("clean".to_string()),
        ClamAvResult::Infected { signature } => Some(format!("infected: {}", signature)),
        ClamAvResult::Error(e) => Some(format!("error: {}", e)),
    };

    let yara_json = if yara.matches.is_empty() {
        None
    } else {
        let matches: Vec<serde_json::Value> = yara
            .matches
            .iter()
            .map(|m| {
                serde_json::json!({
                    "rule": m.rule,
                    "namespace": m.namespace,
                })
            })
            .collect();
        Some(serde_json::Value::Array(matches))
    };

    let blocked_reason = if status == ScanStatus::Infected && config.block_on_malware {
        match &clamav {
            ClamAvResult::Infected { signature } => {
                Some(format!("ClamAV: {}", signature))
            }
            _ => {
                if !yara.matches.is_empty() {
                    Some(format!(
                        "YARA: {}",
                        yara.matches
                            .iter()
                            .map(|m| m.rule.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ))
                } else {
                    None
                }
            }
        }
    } else {
        None
    };

    let now = chrono::Utc::now();

    let record = ScanResultRecord {
        cache_path: file_path.to_string_lossy().to_string(),
        package_name: package_name.map(|s| s.to_string()),
        package_version: package_version.map(|s| s.to_string()),
        scan_version: SCAN_VERSION,
        clamav_result: clamav_str,
        yara_matches: yara_json,
        max_entropy: if entropy.flagged {
            Some(entropy.max_entropy)
        } else {
            None
        },
        cve_matches: if cve_result.matches.is_empty() {
            None
        } else {
            let cves: Vec<serde_json::Value> = cve_result
                .matches
                .iter()
                .map(|m| {
                    serde_json::json!({
                        "id": m.id,
                        "severity": m.severity,
                        "summary": m.summary,
                    })
                })
                .collect();
            Some(serde_json::Value::Array(cves))
        },
        scan_status: status.as_str().to_string(),
        blocked_reason,
        scanned_at: now,
    };

    let full_result = FullScanResult {
        clamav,
        entropy,
        yara,
        status,
    };

    match &full_result.status {
        ScanStatus::Infected => {
            warn!(
                "Scan INFECTED: path={:?}, clamav={:?}, yara_rules={}, entropy_max={:.2}",
                file_path,
                full_result.clamav,
                full_result.yara.matches.len(),
                full_result.entropy.max_entropy,
            );
            monopsony_common::metrics::scan_outcome("infected");
        }
        ScanStatus::Flagged => {
            info!(
                "Scan flagged: path={:?}, entropy_max={:.2}",
                file_path, full_result.entropy.max_entropy,
            );
            monopsony_common::metrics::scan_outcome("flagged");
        }
        ScanStatus::Error => {
            warn!("Scan error for {:?}", file_path);
            monopsony_common::metrics::scan_outcome("error");
        }
        ScanStatus::Clean => {
            debug!("Scan clean: {:?}", file_path);
            monopsony_common::metrics::scan_outcome("clean");
        }
    }

    if !cve_result.matches.is_empty() {
        monopsony_common::metrics::scan_cve_lookup(cve_result.matches.len());
    }

    (full_result, record)
}

/// Derive the overall scan status from individual scanner results.
fn derive_status(clamav: &ClamAvResult, entropy: &EntropyResult, yara: &YaraResult) -> ScanStatus {
    // ClamAV infection is authoritative — immediate infected
    if matches!(clamav, ClamAvResult::Infected { .. }) {
        return ScanStatus::Infected;
    }

    // YARA matches with high-confidence rules also trigger infected
    if !yara.matches.is_empty() {
        return ScanStatus::Infected;
    }

    // ClamAV error (unreachable clamd) — but not a hard failure
    // if other scanners are clean
    let clamav_error = matches!(clamav, ClamAvResult::Error(_));
    let yara_error = yara.error.is_some();

    // Entropy flagging — just flagged, not infected
    if entropy.flagged {
        if clamav_error || yara_error {
            return ScanStatus::Error;
        }
        return ScanStatus::Flagged;
    }

    // If any scanner had an error, report it
    if clamav_error || yara_error {
        return ScanStatus::Error;
    }

    ScanStatus::Clean
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan_entropy::FlaggedMember;

    #[test]
    fn derive_status_all_clean() {
        let clamav = ClamAvResult::Clean;
        let entropy = EntropyResult {
            flagged: false,
            max_entropy: 3.5,
            flagged_members: vec![],
        };
        let yara = YaraResult {
            available: true,
            matches: vec![],
            error: None,
        };
        assert_eq!(derive_status(&clamav, &entropy, &yara), ScanStatus::Clean);
    }

    #[test]
    fn derive_status_clamav_infected() {
        let clamav = ClamAvResult::Infected {
            signature: "Win.Test.EICAR_HDB-1".into(),
        };
        let entropy = EntropyResult {
            flagged: false,
            max_entropy: 3.0,
            flagged_members: vec![],
        };
        let yara = YaraResult {
            available: true,
            matches: vec![],
            error: None,
        };
        assert_eq!(derive_status(&clamav, &entropy, &yara), ScanStatus::Infected);
    }

    #[test]
    fn derive_status_yara_match() {
        let clamav = ClamAvResult::Clean;
        let entropy = EntropyResult {
            flagged: false,
            max_entropy: 4.0,
            flagged_members: vec![],
        };
        let yara = YaraResult {
            available: true,
            matches: vec![scan_yara::YaraMatch {
                rule: "SuspiciousInstallScript".into(),
                namespace: "test.yar".into(),
            }],
            error: None,
        };
        assert_eq!(derive_status(&clamav, &entropy, &yara), ScanStatus::Infected);
    }

    #[test]
    fn derive_status_entropy_flagged() {
        let clamav = ClamAvResult::Clean;
        let entropy = EntropyResult {
            flagged: true,
            max_entropy: 7.9,
            flagged_members: vec![FlaggedMember {
                name: "payload.bin".into(),
                entropy: 7.9,
            }],
        };
        let yara = YaraResult {
            available: true,
            matches: vec![],
            error: None,
        };
        assert_eq!(derive_status(&clamav, &entropy, &yara), ScanStatus::Flagged);
    }

    #[test]
    fn derive_status_clamav_error() {
        let clamav = ClamAvResult::Error("connection refused".into());
        let entropy = EntropyResult {
            flagged: false,
            max_entropy: 3.0,
            flagged_members: vec![],
        };
        let yara = YaraResult {
            available: true,
            matches: vec![],
            error: None,
        };
        assert_eq!(derive_status(&clamav, &entropy, &yara), ScanStatus::Error);
    }

    #[test]
    fn derive_status_clamav_error_with_entropy_flag() {
        let clamav = ClamAvResult::Error("timeout".into());
        let entropy = EntropyResult {
            flagged: true,
            max_entropy: 7.8,
            flagged_members: vec![],
        };
        let yara = YaraResult {
            available: true,
            matches: vec![],
            error: None,
        };
        // Error + entropy flag → Error (can't confirm if the entropy is a real threat)
        assert_eq!(derive_status(&clamav, &entropy, &yara), ScanStatus::Error);
    }

    #[test]
    fn derive_status_yara_error() {
        let clamav = ClamAvResult::Clean;
        let entropy = EntropyResult {
            flagged: false,
            max_entropy: 3.0,
            flagged_members: vec![],
        };
        let yara = YaraResult {
            available: true,
            matches: vec![],
            error: Some("yara exited with code 1".into()),
        };
        assert_eq!(derive_status(&clamav, &entropy, &yara), ScanStatus::Error);
    }
}
