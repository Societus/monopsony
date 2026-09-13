//! Scan policy engine — decides whether to allow, warn, or block a package
//! based on scan results and client context. Pure function, no side effects.
//!
//! Policy matrix (from design doc):
//!
//! | Scan status  | CVE in V only | CVE in both | Malware | Not scanned |
//! |-------------|---------------|-------------|---------|-------------|
//! | clean       | Allow         | Allow       | --      | --          |
//! | flagged(CVE)| Allow(warn)   | Allow(info) | --      | --          |
//! | flagged(ent)| Allow(warn)   | Allow       | --      | --          |
//! | infected    | --            | --          | Block*  | --          |
//! | pending     | --            | --          | --      | Allow(warn) |
//!
//! * Block unless client sends --force

use monopsony_common::config::ScanConfig;

/// Client-provided context for policy evaluation.
#[derive(Debug, Clone)]
pub struct ClientContext {
    /// Currently installed version on the client (if known).
    pub installed_version: Option<String>,
    /// Whether the client sent a force flag (e.g., --force).
    pub force: bool,
}

/// Policy decision for a requested package version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyDecision {
    /// Package is clean or acceptable — serve normally.
    Allow,
    /// Package has warnings — serve with warning headers.
    Warn(PolicyWarning),
    /// Package is blocked — refuse to serve unless forced.
    Block(PolicyBlockReason),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyWarning {
    /// CVEs found in the requested version.
    CveInRequested {
        count: usize,
        worst_severity: String,
    },
    /// CVEs found in both requested and installed versions.
    CveAlreadyExposed { count: usize },
    /// High entropy detected (suspicious but not confirmed malware).
    HighEntropy { max_entropy_scaled: u32 },
    /// Scan not yet complete.
    ScanPending,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyBlockReason {
    /// ClamAV detected malware.
    MalwareDetected { scanner: String, signature: String },
    /// YARA rule match confirmed malware.
    YaraMalware { rules: Vec<String> },
}

/// Simplified scan info for policy evaluation.
#[derive(Debug, Clone)]
pub struct ScanInfo {
    /// Overall scan status: "pending", "clean", "flagged", "infected", "error".
    pub status: String,
    /// ClamAV result string, if any.
    pub clamav_result: Option<String>,
    /// YARA matches (rule names).
    pub yara_rules: Vec<String>,
    /// Max entropy value (if entropy was flagged).
    pub max_entropy: Option<f64>,
    /// CVE matches (list of (id, severity) pairs).
    pub cves: Vec<(String, String)>,
    /// CVEs that also affect the installed version.
    pub cves_in_installed: Vec<String>,
}

/// Evaluate the scan policy for a package request.
///
/// This is a pure function — it takes the scan info, client context,
/// and config, and returns a decision. No I/O, no DB, no network.
pub fn evaluate_policy(
    scan_info: &ScanInfo,
    client_ctx: &ClientContext,
    config: &ScanConfig,
) -> PolicyDecision {
    match scan_info.status.as_str() {
        "pending" | "" => PolicyDecision::Warn(PolicyWarning::ScanPending),

        "error" => {
            // If scan errored, treat as pending (allow with warning)
            PolicyDecision::Warn(PolicyWarning::ScanPending)
        }

        "clean" => PolicyDecision::Allow,

        "flagged" => {
            // CVEs in installed version → "already exposed" info
            let exposed_cves: Vec<_> = scan_info
                .cves
                .iter()
                .filter(|(id, _)| scan_info.cves_in_installed.contains(id))
                .collect();

            // CVEs in requested version only
            let requested_cves: Vec<_> = scan_info
                .cves
                .iter()
                .filter(|(id, _)| !scan_info.cves_in_installed.contains(id))
                .collect();

            // Already-exposed CVEs take priority
            if !exposed_cves.is_empty() && config.warn_on_cve {
                return PolicyDecision::Warn(PolicyWarning::CveAlreadyExposed {
                    count: exposed_cves.len(),
                });
            }

            // New CVEs → warn
            if !requested_cves.is_empty() && config.warn_on_cve {
                let worst = requested_cves
                    .iter()
                    .map(|(_, sev)| severity_rank(sev))
                    .max()
                    .unwrap_or(0);
                return PolicyDecision::Warn(PolicyWarning::CveInRequested {
                    count: requested_cves.len(),
                    worst_severity: severity_from_rank(worst).to_string(),
                });
            }

            // Entropy flagged → warn
            if let Some(ent) = scan_info.max_entropy {
                if ent > config.entropy_threshold {
                    return PolicyDecision::Warn(PolicyWarning::HighEntropy {
                        max_entropy_scaled: (ent * 100.0) as u32,
                    });
                }
            }

            // Flagged for unknown reasons — allow
            PolicyDecision::Allow
        }

        "infected" => {
            if client_ctx.force {
                // Client explicitly forced — allow but warn
                return PolicyDecision::Warn(PolicyWarning::ScanPending);
            }

            // Determine block reason
            if let Some(ref clamav) = scan_info.clamav_result {
                if clamav.starts_with("infected:") {
                    let sig = clamav.trim_start_matches("infected:").trim();
                    return PolicyDecision::Block(PolicyBlockReason::MalwareDetected {
                        scanner: "ClamAV".into(),
                        signature: sig.into(),
                    });
                }
            }

            if !scan_info.yara_rules.is_empty() {
                return PolicyDecision::Block(PolicyBlockReason::YaraMalware {
                    rules: scan_info.yara_rules.clone(),
                });
            }

            // Generic infected block
            PolicyDecision::Block(PolicyBlockReason::MalwareDetected {
                scanner: "unknown".into(),
                signature: "unknown".into(),
            })
        }

        _ => PolicyDecision::Warn(PolicyWarning::ScanPending),
    }
}

fn severity_rank(sev: &str) -> u32 {
    match sev.to_lowercase().as_str() {
        "critical" => 4,
        "high" => 3,
        "medium" => 2,
        "low" => 1,
        _ => 0,
    }
}

fn severity_from_rank(rank: u32) -> &'static str {
    match rank {
        4 => "critical",
        3 => "high",
        2 => "medium",
        1 => "low",
        _ => "unknown",
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn default_config() -> ScanConfig {
        ScanConfig::default()
    }

    fn clean_info() -> ScanInfo {
        ScanInfo {
            status: "clean".into(),
            clamav_result: None,
            yara_rules: vec![],
            max_entropy: None,
            cves: vec![],
            cves_in_installed: vec![],
        }
    }

    fn pending_info() -> ScanInfo {
        ScanInfo {
            status: "pending".into(),
            ..clean_info()
        }
    }

    fn infected_clamav_info() -> ScanInfo {
        ScanInfo {
            status: "infected".into(),
            clamav_result: Some("infected: Win.Test.EICAR_HDB-1".into()),
            yara_rules: vec![],
            max_entropy: None,
            cves: vec![],
            cves_in_installed: vec![],
        }
    }

    fn infected_yara_info() -> ScanInfo {
        ScanInfo {
            status: "infected".into(),
            clamav_result: None,
            yara_rules: vec!["SuspiciousInstallScript".into()],
            max_entropy: None,
            cves: vec![],
            cves_in_installed: vec![],
        }
    }

    fn flagged_entropy_info() -> ScanInfo {
        ScanInfo {
            status: "flagged".into(),
            clamav_result: None,
            yara_rules: vec![],
            max_entropy: Some(7.9),
            cves: vec![],
            cves_in_installed: vec![],
        }
    }

    fn flagged_cve_info() -> ScanInfo {
        ScanInfo {
            status: "flagged".into(),
            clamav_result: None,
            yara_rules: vec![],
            max_entropy: None,
            cves: vec![
                ("CVE-2024-1234".into(), "high".into()),
                ("CVE-2024-5678".into(), "medium".into()),
            ],
            cves_in_installed: vec![],
        }
    }

    fn flagged_cve_already_exposed_info() -> ScanInfo {
        ScanInfo {
            status: "flagged".into(),
            clamav_result: None,
            yara_rules: vec![],
            max_entropy: None,
            cves: vec![("CVE-2024-1234".into(), "high".into())],
            cves_in_installed: vec!["CVE-2024-1234".into()],
        }
    }

    fn no_force() -> ClientContext {
        ClientContext {
            installed_version: None,
            force: false,
        }
    }

    fn with_force() -> ClientContext {
        ClientContext {
            installed_version: None,
            force: true,
        }
    }

    // --- Test matrix ---

    #[test]
    fn test_clean_allow() {
        assert_eq!(
            evaluate_policy(&clean_info(), &no_force(), &default_config()),
            PolicyDecision::Allow
        );
    }

    #[test]
    fn test_pending_warn() {
        let result = evaluate_policy(&pending_info(), &no_force(), &default_config());
        assert!(matches!(
            result,
            PolicyDecision::Warn(PolicyWarning::ScanPending)
        ));
    }

    #[test]
    fn test_empty_status_warn() {
        let info = ScanInfo {
            status: "".into(),
            ..clean_info()
        };
        let result = evaluate_policy(&info, &no_force(), &default_config());
        assert!(matches!(
            result,
            PolicyDecision::Warn(PolicyWarning::ScanPending)
        ));
    }

    #[test]
    fn test_error_warn_as_pending() {
        let info = ScanInfo {
            status: "error".into(),
            ..clean_info()
        };
        let result = evaluate_policy(&info, &no_force(), &default_config());
        assert!(matches!(
            result,
            PolicyDecision::Warn(PolicyWarning::ScanPending)
        ));
    }

    #[test]
    fn test_infected_clamav_block() {
        let result = evaluate_policy(&infected_clamav_info(), &no_force(), &default_config());
        assert!(matches!(
            result,
            PolicyDecision::Block(PolicyBlockReason::MalwareDetected { .. })
        ));
    }

    #[test]
    fn test_infected_clamav_force_warn() {
        let result = evaluate_policy(&infected_clamav_info(), &with_force(), &default_config());
        assert!(matches!(result, PolicyDecision::Warn(_)));
    }

    #[test]
    fn test_infected_yara_block() {
        let result = evaluate_policy(&infected_yara_info(), &no_force(), &default_config());
        assert!(matches!(
            result,
            PolicyDecision::Block(PolicyBlockReason::YaraMalware { .. })
        ));
    }

    #[test]
    fn test_infected_yara_force_warn() {
        let result = evaluate_policy(&infected_yara_info(), &with_force(), &default_config());
        assert!(matches!(result, PolicyDecision::Warn(_)));
    }

    #[test]
    fn test_flagged_entropy_warn() {
        let result = evaluate_policy(&flagged_entropy_info(), &no_force(), &default_config());
        assert!(matches!(
            result,
            PolicyDecision::Warn(PolicyWarning::HighEntropy { .. })
        ));
    }

    #[test]
    fn test_flagged_entropy_below_threshold_allow() {
        let info = ScanInfo {
            max_entropy: Some(5.0),
            ..flagged_entropy_info()
        };
        let result = evaluate_policy(&info, &no_force(), &default_config());
        assert_eq!(result, PolicyDecision::Allow);
    }

    #[test]
    fn test_flagged_cve_warn() {
        let result = evaluate_policy(&flagged_cve_info(), &no_force(), &default_config());
        assert!(matches!(
            result,
            PolicyDecision::Warn(PolicyWarning::CveInRequested { count: 2, .. })
        ));
    }

    #[test]
    fn test_flagged_cve_warn_on_cve_disabled() {
        let mut config = default_config();
        config.warn_on_cve = false;
        // With warn_on_cve=false, CVEs are silently ignored
        let result = evaluate_policy(&flagged_cve_info(), &no_force(), &config);
        assert_eq!(result, PolicyDecision::Allow);
    }

    #[test]
    fn test_flagged_cve_already_exposed_warn() {
        let result = evaluate_policy(
            &flagged_cve_already_exposed_info(),
            &no_force(),
            &default_config(),
        );
        assert!(matches!(
            result,
            PolicyDecision::Warn(PolicyWarning::CveAlreadyExposed { count: 1 })
        ));
    }

    #[test]
    fn test_flagged_cve_partial_exposure() {
        // One CVE in installed, one not
        let info = ScanInfo {
            status: "flagged".into(),
            clamav_result: None,
            yara_rules: vec![],
            max_entropy: None,
            cves: vec![
                ("CVE-2024-1234".into(), "high".into()),
                ("CVE-2024-9999".into(), "low".into()),
            ],
            cves_in_installed: vec!["CVE-2024-1234".into()],
        };
        // Already-exposed takes priority
        let result = evaluate_policy(&info, &no_force(), &default_config());
        assert!(matches!(
            result,
            PolicyDecision::Warn(PolicyWarning::CveAlreadyExposed { count: 1 })
        ));
    }

    #[test]
    fn test_infected_block_with_block_on_malware() {
        let result = evaluate_policy(&infected_clamav_info(), &no_force(), &default_config());
        assert!(matches!(result, PolicyDecision::Block(_)));
    }

    #[test]
    fn test_severity_rank() {
        assert_eq!(severity_rank("critical"), 4);
        assert_eq!(severity_rank("HIGH"), 3);
        assert_eq!(severity_rank("medium"), 2);
        assert_eq!(severity_rank("low"), 1);
        assert_eq!(severity_rank("unknown"), 0);
    }

    #[test]
    fn test_cve_worst_severity_reported() {
        let info = ScanInfo {
            status: "flagged".into(),
            cves: vec![
                ("CVE-1".into(), "low".into()),
                ("CVE-2".into(), "critical".into()),
                ("CVE-3".into(), "medium".into()),
            ],
            ..flagged_cve_info()
        };
        let result = evaluate_policy(&info, &no_force(), &default_config());
        if let PolicyDecision::Warn(PolicyWarning::CveInRequested { worst_severity, .. }) = result {
            assert_eq!(worst_severity, "critical");
        } else {
            panic!("Expected CveInRequested warning, got {:?}", result);
        }
    }

    #[test]
    fn test_block_reason_clamav_signature() {
        let result = evaluate_policy(&infected_clamav_info(), &no_force(), &default_config());
        if let PolicyDecision::Block(PolicyBlockReason::MalwareDetected { signature, .. }) = result
        {
            assert_eq!(signature, "Win.Test.EICAR_HDB-1");
        } else {
            panic!("Expected MalwareDetected block");
        }
    }

    #[test]
    fn test_block_reason_yara_rules() {
        let result = evaluate_policy(&infected_yara_info(), &no_force(), &default_config());
        if let PolicyDecision::Block(PolicyBlockReason::YaraMalware { rules }) = result {
            assert_eq!(rules, vec!["SuspiciousInstallScript"]);
        } else {
            panic!("Expected YaraMalware block");
        }
    }

    #[test]
    fn test_unknown_status_warn() {
        let info = ScanInfo {
            status: "weird_status".into(),
            ..clean_info()
        };
        let result = evaluate_policy(&info, &no_force(), &default_config());
        assert!(matches!(
            result,
            PolicyDecision::Warn(PolicyWarning::ScanPending)
        ));
    }

    #[test]
    fn test_flagged_no_cve_no_entropy_allow() {
        // Flagged status but no CVEs and no entropy — edge case
        let info = ScanInfo {
            status: "flagged".into(),
            clamav_result: None,
            yara_rules: vec![],
            max_entropy: None,
            cves: vec![],
            cves_in_installed: vec![],
        };
        let result = evaluate_policy(&info, &no_force(), &default_config());
        assert_eq!(result, PolicyDecision::Allow);
    }
}
