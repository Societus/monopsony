//! YARA subprocess scanner for supply-chain malware detection.
//!
//! Invokes the `yara` CLI binary as a subprocess to scan cached package
//! files against user-provided rule sets. This is subprocess-only (no FFI)
//! for simplicity and safety. YARA must be installed on the host system.

use std::path::Path;
use std::time::Duration;
use tokio::process::Command;
use tokio::time::timeout;
use tracing::{debug, warn};

/// YARA scan result.
#[derive(Debug, Clone)]
pub struct YaraResult {
    /// Whether YARA is available on the system.
    pub available: bool,
    /// Matched rules — each entry is (rule_name, namespace/tag).
    pub matches: Vec<YaraMatch>,
    /// Error message if the scan failed (but YARA was available).
    pub error: Option<String>,
}

/// A single YARA rule match.
#[derive(Debug, Clone)]
pub struct YaraMatch {
    /// Rule that matched.
    pub rule: String,
    /// Namespace the rule belongs to (typically the .yar file path).
    pub namespace: String,
}

/// Scan a file at the given path against YARA rules in `rules_dir`.
///
/// Finds all `*.yar` / `*.yara` files in `rules_dir` and invokes
/// `yara -r <rules_dir> <target_file>`.
///
/// If `yara` is not found on PATH, returns `YaraResult { available: false, .. }`.
/// If the rules directory doesn't exist or is empty, returns no matches (not an error).
pub async fn scan_file(
    target_path: &Path,
    rules_dir: &Path,
    scan_timeout: Duration,
) -> YaraResult {
    // Check if yara binary exists
    let yara_bin = match which_yara().await {
        Some(bin) => bin,
        None => {
            return YaraResult {
                available: false,
                matches: vec![],
                error: None,
            }
        }
    };

    // If rules dir doesn't exist, nothing to scan
    if !rules_dir.exists() {
        debug!("YARA rules dir {:?} does not exist, skipping", rules_dir);
        return YaraResult {
            available: true,
            matches: vec![],
            error: None,
        };
    }

    // Check if there are any rule files
    let has_rules = match tokio::fs::read_dir(rules_dir).await {
        Ok(mut entries) => {
            let mut found = false;
            while let Ok(Some(entry)) = entries.next_entry().await {
                if let Some(name) = entry.file_name().to_str() {
                    if name.ends_with(".yar") || name.ends_with(".yara") {
                        found = true;
                        break;
                    }
                }
            }
            found
        }
        Err(e) => {
            return YaraResult {
                available: true,
                matches: vec![],
                error: Some(format!("cannot read rules dir {:?}: {}", rules_dir, e)),
            }
        }
    };

    if !has_rules {
        debug!("No .yar/.yara files found in {:?}", rules_dir);
        return YaraResult {
            available: true,
            matches: vec![],
            error: None,
        };
    }

    // Invoke yara: yara -r <rules_dir> <target_file>
    let result = timeout(
        scan_timeout,
        Command::new(&yara_bin)
            .arg("-r")
            .arg(rules_dir)
            .arg(target_path)
            .output(),
    )
    .await;

    match result {
        Ok(Ok(output)) => parse_yara_output(&output.stdout, &output.stderr, output.status),
        Ok(Err(e)) => {
            warn!("YARA subprocess failed: {}", e);
            YaraResult {
                available: true,
                matches: vec![],
                error: Some(format!("yara subprocess error: {}", e)),
            }
        }
        Err(_) => {
            warn!("YARA scan timed out after {:?}", scan_timeout);
            YaraResult {
                available: true,
                matches: vec![],
                error: Some(format!("yara timeout ({:?})", scan_timeout)),
            }
        }
    }
}

/// Parse YARA stdout into structured matches.
///
/// YARA output format (default, non-JSON):
/// ```text
/// RuleName TargetFilePath
/// RuleName TargetFilePath
/// ```
/// With `-s` it would also print matching strings, but we don't use that here.
pub fn parse_yara_output(stdout: &[u8], stderr: &[u8], exit_status: std::process::ExitStatus) -> YaraResult {
    let stderr_text = String::from_utf8_lossy(stderr);
    if !stderr_text.is_empty() {
        debug!("YARA stderr: {}", stderr_text.trim());
    }

    // YARA exits 0 on success (with or without matches), 1 on error.
    // No matches means stdout is empty — that's a clean result, not an error.
    if !exit_status.success() {
        return YaraResult {
            available: true,
            matches: vec![],
            error: Some(format!(
                "yara exited with code {:?}: {}",
                exit_status.code(),
                stderr_text.trim()
            )),
        };
    }

    let stdout_text = String::from_utf8_lossy(stdout);
    let mut matches = Vec::new();

    for line in stdout_text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // Format: "RuleName /path/to/file"
        if let Some(space_pos) = line.find(' ') {
            let rule = line[..space_pos].to_string();
            let namespace = line[space_pos + 1..].trim().to_string();
            matches.push(YaraMatch { rule, namespace });
        }
    }

    YaraResult {
        available: true,
        matches,
        error: None,
    }
}

/// Try to find the `yara` binary on the system PATH.
async fn which_yara() -> Option<String> {
    // Try `which yara` first
    match Command::new("which").arg("yara").output().await {
        Ok(output) if output.status.success() => {
            let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !path.is_empty() {
                return Some(path);
            }
        }
        _ => {}
    }

    // Fallback: try common paths
    for candidate in &["/usr/bin/yara", "/usr/local/bin/yara"] {
        if tokio::fs::metadata(candidate).await.is_ok() {
            return Some(candidate.to_string());
        }
    }

    None
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::ExitStatus;

    /// Helper to create a fake ExitStatus from a code.
    fn exit_code(code: i32) -> std::process::ExitStatus {
        // ExitStatus can't be constructed directly in stable Rust,
        // so we use the output of a real process.
        // For tests we use a trick: run `true` or `false` and grab status.
        // But that's async and complex. Instead, we test parse_yara_output
        // with real Command outputs from the integration test suite.
        // For unit tests, we use the actual `echo` command.
        unreachable!("use real process output for tests")
    }

    #[test]
    fn parse_yara_output_clean() {
        // No matches — stdout is empty
        let output = std::process::Command::new("true")
            .output()
            .expect("true should exist");
        let result = parse_yara_output(&output.stdout, &output.stderr, output.status);
        assert!(result.available);
        assert!(result.matches.is_empty());
        assert!(result.error.is_none());
    }

    #[test]
    fn parse_yara_output_with_matches() {
        // Simulate YARA output with matches
        let stdout = b"SuspiciousInstallScript /cache/pkg-1.0.tgz\nCryptoMiner /cache/pkg-1.0.tgz\n";
        let output = std::process::Command::new("true")
            .output()
            .expect("true should exist");
        let result = parse_yara_output(stdout, &output.stderr, output.status);
        assert!(result.available);
        assert_eq!(result.matches.len(), 2);
        assert_eq!(result.matches[0].rule, "SuspiciousInstallScript");
        assert_eq!(result.matches[0].namespace, "/cache/pkg-1.0.tgz");
        assert_eq!(result.matches[1].rule, "CryptoMiner");
    }

    #[test]
    fn parse_yara_output_error_exit() {
        let output = std::process::Command::new("false")
            .output()
            .expect("false should exist");
        let result = parse_yara_output(b"", b"error: cannot open rules\n", output.status);
        assert!(result.available);
        assert!(result.error.is_some());
        assert!(result.error.as_ref().unwrap().contains("error: cannot open rules"));
    }

    #[test]
    fn parse_yara_output_empty_lines_ignored() {
        let stdout = b"\n\nSuspiciousInstallScript /cache/pkg.tgz\n\n";
        let output = std::process::Command::new("true")
            .output()
            .expect("true should exist");
        let result = parse_yara_output(stdout, &output.stderr, output.status);
        assert_eq!(result.matches.len(), 1);
    }

    #[test]
    fn parse_yara_output_rule_without_path() {
        // Malformed line without space — should be skipped
        let stdout = b"RuleWithoutPath\n";
        let output = std::process::Command::new("true")
            .output()
            .expect("true should exist");
        let result = parse_yara_output(stdout, &output.stderr, output.status);
        assert_eq!(result.matches.len(), 0);
    }

    #[tokio::test]
    async fn which_yara_detects_or_gracefully_fails() {
        // This test just verifies which_yara doesn't panic.
        // It may return Some or None depending on the test environment.
        let result = which_yara().await;
        if let Some(ref path) = result {
            assert!(!path.is_empty());
        }
        // Either outcome is fine — this is environment-dependent.
    }
}
