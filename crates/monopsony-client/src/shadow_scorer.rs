//! Shadow package similarity scorer (Batch 6 / P4b, T4.3–T4.6).
//!
//! At register time the locally detected package is scored against the
//! server's shadow-registry matches to decide whether to:
//!   * stay silent           (score < 2)
//!   * annotate the draft    (2 <= score < 4)
//!   * offer an interactive  register-vs-adopt-from-peer choice (score >= 4)
//!
//! Pre-registered weights (fixed for this phase; no automation flags yet):
//!   sha256 exact +5 · name (ci) +2 · service unit +2 · source repo +1 · target_dir +1
//!
//! Because `ShadowRegistryMatch` carries only `recipe_name` (plus signing /
//! fleet state), service / source / target_dir are scored as "query hint
//! contributed" signals: the server only returns matches consistent with the
//! hints we sent, so a non-empty local hint that returned matches counts.

use anyhow::Context;
use std::path::Path;

use crate::detect::{ConfigFinding, DetectReport};
use monopsony_common::config::ShadowPackageTrustConfig;
use monopsony_common::proto::monopsony::ShadowRegistryMatch;
use monopsony_common::shadow_types::{RecipeSignature, ShadowRecipe, SignedRecipe};

/// Pre-registered similarity weights (T4.3).
pub const WEIGHT_SHA256: u32 = 5; // exact content-address match
pub const WEIGHT_NAME: u32 = 2;   // recipe name, case-insensitive
pub const WEIGHT_SERVICE: u32 = 2; // service unit name
pub const WEIGHT_SOURCE: u32 = 1; // source repo basename
pub const WEIGHT_TARGET_DIR: u32 = 1; // install target dir

/// Decision thresholds (T4.3).
pub const THRESHOLD_INTERACTIVE: u32 = 4; // >= 4 → interactive choice prompt
pub const THRESHOLD_DRAFT: u32 = 2;       // 2 ..= 3 → draft annotation only

/// Maximum attainable score (all five criteria).
#[allow(dead_code)]
pub const MAX_SCORE: u32 =
    WEIGHT_SHA256 + WEIGHT_NAME + WEIGHT_SERVICE + WEIGHT_SOURCE + WEIGHT_TARGET_DIR; // 11

/// What the caller should do given a score.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScoringAction {
    /// Score < 2: register silently, no user-facing annotation.
    Silent,
    /// 2 <= score < 4: annotate the draft with a similarity comment.
    DraftAnnotation,
    /// Score >= 4: offer an interactive register-vs-adopt-from-peer choice.
    InteractiveChoice,
}

/// Outcome of scoring a detected package against the registry matches.
#[derive(Debug, Clone)]
pub struct ScoreResult {
    /// Best score across all matches (0..=MAX_SCORE).
    pub score: u32,
    /// Recommended caller action.
    pub action: ScoringAction,
    /// The match that produced the best score, if any matches exist.
    pub best_match: Option<ShadowRegistryMatch>,
}

/// Score a single registry match against locally detected inputs.
fn score_match(
    local_sha256: Option<&str>,
    local_name: &str,
    local_service: Option<&str>,
    local_source_url: &str,
    local_target_dir: &str,
    m: &ShadowRegistryMatch,
) -> u32 {
    let mut score = 0u32;

    // +5 exact content match: we queried with a non-empty binary_sha256, so
    // every returned match is an exact content-address hit.
    if local_sha256.map(|s| !s.is_empty()).unwrap_or(false) {
        score += WEIGHT_SHA256;
    }

    // +2 name (case-insensitive).
    if !m.recipe_name.is_empty()
        && m.recipe_name.to_lowercase() == local_name.to_lowercase()
    {
        score += WEIGHT_NAME;
    }

    // +2 service unit name: we included the local service as a query hint.
    if local_service.map(|s| !s.is_empty()).unwrap_or(false) {
        score += WEIGHT_SERVICE;
    }

    // +1 source repo basename vs recipe name (case-insensitive, `.git` stripped).
    if source_repo_basename(local_source_url) == recipe_name_basename(&m.recipe_name) {
        score += WEIGHT_SOURCE;
    }

    // +1 target dir hint contributed.
    if !local_target_dir.is_empty() {
        score += WEIGHT_TARGET_DIR;
    }

    score
}

/// Score the detected report against all registry matches; return the best.
///
/// `local_name` is the package name being registered; `local_source_url` and
/// `local_target_dir` are the locally detected source and install target.
pub fn score_report(
    report: &DetectReport,
    matches: &[ShadowRegistryMatch],
    local_name: &str,
    local_source_url: &str,
    local_target_dir: &str,
) -> ScoreResult {
    let local_sha256 = report.sha256.as_deref();
    let local_service = report.service.as_ref().map(|s| s.unit_name.as_str());

    let mut best: Option<(u32, ShadowRegistryMatch)> = None;
    for m in matches {
        let s = score_match(
            local_sha256,
            local_name,
            local_service,
            local_source_url,
            local_target_dir,
            m,
        );
        let better = best.as_ref().map(|(b, _)| s > *b).unwrap_or(true);
        if better {
            best = Some((s, m.clone()));
        }
    }

    match best {
        Some((score, m)) => {
            let action = if score >= THRESHOLD_INTERACTIVE {
                ScoringAction::InteractiveChoice
            } else if score >= THRESHOLD_DRAFT {
                ScoringAction::DraftAnnotation
            } else {
                ScoringAction::Silent
            };
            ScoreResult {
                score,
                action,
                best_match: Some(m),
            }
        }
        None => ScoreResult {
            score: 0,
            action: ScoringAction::Silent,
            best_match: None,
        },
    }
}

/// Basename of a source URL, lowercased, trailing `.git` stripped.
/// e.g. `https://github.com/Sonarr/Sonarr.git` → `sonarr`.
fn source_repo_basename(source_url: &str) -> String {
    let base = Path::new(source_url)
        .file_name()
        .map(|f| f.to_string_lossy().to_string())
        .unwrap_or_default();
    let base = if base.len() >= 4
        && base.as_bytes()[base.len() - 4..].eq_ignore_ascii_case(b".git")
    {
        &base[..base.len() - 4]
    } else {
        &base
    };
    base.to_lowercase()
}

/// Lowercased recipe name used for basename-style comparison.
fn recipe_name_basename(name: &str) -> String {
    name.to_lowercase()
}

/// T4.4: compare locally detected config paths (existing findings) against the
/// peer recipe's `config_paths`. Returns `(agree, comment)`.
#[allow(dead_code)]
pub fn config_agreement(
    local_findings: &[ConfigFinding],
    peer_config_paths: &[String],
) -> (bool, String) {
    let local_existing: Vec<String> = local_findings
        .iter()
        .filter(|f| f.exists)
        .map(|f| f.path.clone())
        .collect();

    let agree = !local_existing.is_empty()
        && local_existing
            .iter()
            .any(|p| peer_config_paths.iter().any(|q| q == p));

    let comment = if agree {
        format!(
            "config paths agree with peer recipe ({})",
            local_existing.join(", ")
        )
    } else if local_existing.is_empty() {
        "no local config findings to cross-check".to_string()
    } else {
        format!(
            "config paths differ from peer recipe; local: {}",
            local_existing.join(", ")
        )
    };
    (agree, comment)
}

/// T4.4: if the local version probe failed but the peer recipe carries a
/// field-tested `detect_command`, prefer the peer's command.
///
/// `local_version` is `None` or the probe failed (`needs_manual_verify`) when
/// the local detect did not produce a version.
#[allow(dead_code)]
pub fn preferred_detect_command(
    local_version: Option<&str>,
    local_needs_manual_verify: bool,
    peer: &ShadowRecipe,
) -> Option<String> {
    let probe_failed = local_version.is_none() || local_needs_manual_verify;
    if probe_failed && !peer.version.detect_command.is_empty() {
        Some(peer.version.detect_command.clone())
    } else {
        None
    }
}

/// T4.5: mutate a peer recipe to locally detected values (target_dir,
/// config_paths, service) so it can be registered as a local adoption.
pub fn mutate_recipe_for_local(
    recipe_toml: &str,
    local_target_dir: &str,
    local_config_paths: &[String],
    local_service: Option<&str>,
) -> anyhow::Result<String> {
    let mut recipe: ShadowRecipe =
        toml::from_str(recipe_toml).context("peer recipe failed to parse as TOML")?;
    recipe.install.target_dir = local_target_dir.to_string();
    recipe.install.config_paths = if local_config_paths.is_empty() {
        None
    } else {
        Some(local_config_paths.to_vec())
    };
    if let Some(svc) = local_service {
        recipe.install.service = Some(svc.to_string());
    }
    toml::to_string(&recipe).context("serializing mutated peer recipe")
}

/// T4.5: build a `SignedRecipe` + trust config from a fetched peer recipe so
/// the caller can verify it with `trust::verify_recipe_trust`.
pub fn signed_peer_recipe(
    recipe_toml: String,
    key_id: String,
    signature: String,
    signed_at: i64,
) -> anyhow::Result<SignedRecipe> {
    let recipe: ShadowRecipe =
        toml::from_str(&recipe_toml).context("peer recipe failed to parse as TOML")?;
    Ok(SignedRecipe {
        recipe_toml,
        recipe,
        operator_signature: RecipeSignature {
            key_id,
            signature,
            signed_at: chrono::DateTime::<chrono::Utc>::from_timestamp(signed_at, 0)
                .unwrap_or_default(),
        },
        author_signature: None,
    })
}

/// T4.4/T4.5: load the client shadow trust config from disk.
pub fn load_trust_config() -> ShadowPackageTrustConfig {
    let config_path = std::env::var("MONOPSONY_CLIENT_CONFIG")
        .unwrap_or_else(|_| "/etc/monopsony/client.toml".to_string());
    std::fs::read_to_string(&config_path)
        .ok()
        .and_then(|c| toml::from_str::<monopsony_common::config::ClientConfig>(&c).ok())
        .map(|c| c.shadow_trust)
        .unwrap_or_default()
}

/// T4.7: base repo of a URL — scheme-stripped host plus the first two path
/// segments (the repo identity). e.g.
/// `https://github.com/Sonarr/Sonarr/releases` → `github.com/sonarr/sonarr`.
/// Used to detect when an update's asset URL has moved to a different repo.
pub fn repo_base(source_url: &str) -> String {
    let after_scheme = source_url.split("://").nth(1).unwrap_or(source_url);
    let mut parts = after_scheme.split('/');
    let host = parts.next().unwrap_or("");
    let mut segs: Vec<&str> = Vec::new();
    for seg in parts {
        if seg.is_empty() {
            continue;
        }
        segs.push(seg);
        if segs.len() >= 2 {
            break;
        }
    }
    let base = if segs.is_empty() {
        host.to_string()
    } else {
        format!("{}/{}", host, segs.join("/"))
    };
    base.to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::detect::{ConfigSource, Confidence, ServiceMatch};

    fn m(name: &str) -> ShadowRegistryMatch {
        ShadowRegistryMatch {
            recipe_name: name.to_string(),
            recipe_version: String::new(),
            signed: false,
            key_id: String::new(),
            fleet_installs: vec![],
        }
    }

    fn svc(unit: &str) -> ServiceMatch {
        ServiceMatch {
            unit_name: unit.to_string(),
            unit_path: format!("/tmp/{}.service", unit),
            method: "execstart".to_string(),
            confidence: Confidence::High,
            pid: None,
        }
    }

    fn report(
        sha: Option<&str>,
        _name: &str,
        service: Option<&str>,
        _source: &str,
        _target_dir: &str,
    ) -> DetectReport {
        DetectReport {
            path_arg: "/opt/bin/foo".into(),
            resolved_path: Some("/opt/bin/foo".into()),
            sha256: sha.map(|s| s.to_string()),
            size: Some(10),
            elf: None,
            version: Some("1.2.3".into()),
            detect_command: None,
            detect_pattern: None,
            needs_manual_verify: false,
            service: service.map(svc),
            config_findings: vec![],
            exec_args: vec![],
            unit_user: None,
            pm_managed: None,
        }
    }

    #[test]
    fn weights_and_thresholds_are_preregistered() {
        assert_eq!(WEIGHT_SHA256, 5);
        assert_eq!(WEIGHT_NAME, 2);
        assert_eq!(WEIGHT_SERVICE, 2);
        assert_eq!(WEIGHT_SOURCE, 1);
        assert_eq!(WEIGHT_TARGET_DIR, 1);
        assert_eq!(MAX_SCORE, 11);
        assert_eq!(THRESHOLD_INTERACTIVE, 4);
        assert_eq!(THRESHOLD_DRAFT, 2);
    }

    // T4.6 golden fixture: exact-match pair → score 11 → interactive.
    #[test]
    fn golden_exact_match_score_11_interactive() {
        let report = report(
            Some("deadbeef"),
            "sonarr",
            Some("sonarr.service"),
            "https://github.com/Sonarr/Sonarr",
            "/opt/bin",
        );
        let res = score_report(
            &report,
            &[m("sonarr")],
            "sonarr",
            "https://github.com/Sonarr/Sonarr",
            "/opt/bin",
        );
        assert_eq!(res.score, 11);
        assert_eq!(res.action, ScoringAction::InteractiveChoice);
        assert_eq!(res.best_match.as_ref().unwrap().recipe_name, "sonarr");
    }

    // T4.6 golden fixture: near-match name + service + source = 5 → interactive.
    #[test]
    fn golden_near_match_name_service_source_score_5_interactive() {
        let report = report(
            None,
            "sonarr",
            Some("sonarr.service"),
            "https://github.com/Sonarr/Sonarr",
            "",
        );
        let res = score_report(
            &report,
            &[m("sonarr")],
            "sonarr",
            "https://github.com/Sonarr/Sonarr",
            "",
        );
        assert_eq!(res.score, 5);
        assert_eq!(res.action, ScoringAction::InteractiveChoice);
    }

    // T4.6 golden fixture: near-match name + target_dir = 3 → draft annotation.
    #[test]
    fn golden_near_match_name_target_dir_score_3_draft() {
        let report = report(
            None,
            "sonarr",
            None,
            "https://github.com/other/other",
            "/opt/bin",
        );
        let res = score_report(
            &report,
            &[m("sonarr")],
            "sonarr",
            "https://github.com/other/other",
            "/opt/bin",
        );
        assert_eq!(res.score, 3);
        assert_eq!(res.action, ScoringAction::DraftAnnotation);
    }

    // T4.6 golden fixture: no-match → score < 2 → silent.
    #[test]
    fn golden_no_match_score_0_silent() {
        let report = report(None, "nomatch", None, "", "");
        let res = score_report(&report, &[m("sonarr")], "nomatch", "", "");
        assert_eq!(res.score, 0);
        assert_eq!(res.action, ScoringAction::Silent);
    }

    // Empty match list → silent.
    #[test]
    fn empty_matches_silent() {
        let report = report(
            Some("deadbeef"),
            "sonarr",
            Some("sonarr.service"),
            "https://github.com/Sonarr/Sonarr",
            "/opt/bin",
        );
        let res = score_report(
            &report,
            &[],
            "sonarr",
            "https://github.com/Sonarr/Sonarr",
            "/opt/bin",
        );
        assert_eq!(res.score, 0);
        assert_eq!(res.action, ScoringAction::Silent);
        assert!(res.best_match.is_none());
    }

    // Name match is case-insensitive.
    #[test]
    fn name_match_case_insensitive() {
        let report = report(
            None,
            "Sonarr",
            None,
            "https://github.com/other/other",
            "",
        );
        let res = score_report(&report, &[m("sonarr")], "Sonarr", "", "");
        assert_eq!(res.score, 2); // name only
        assert_eq!(res.action, ScoringAction::DraftAnnotation);
    }

    // source_repo_basename strips `.git` case-insensitively.
    #[test]
    fn source_repo_basename_strips_git() {
        assert_eq!(source_repo_basename("https://github.com/Sonarr/Sonarr.git"), "sonarr");
        assert_eq!(source_repo_basename("https://github.com/Sonarr/Sonarr.GIT"), "sonarr");
        assert_eq!(source_repo_basename("https://github.com/owner/foo"), "foo");
        assert_eq!(source_repo_basename(""), "");
    }

    // repo_base (T4.7): host + first two path segments, scheme stripped.
    #[test]
    fn repo_base_identity() {
        assert_eq!(
            repo_base("https://github.com/Sonarr/Sonarr/releases"),
            "github.com/sonarr/sonarr"
        );
        assert_eq!(
            repo_base("https://github.com/Sonarr/Sonarr"),
            "github.com/sonarr/sonarr"
        );
        assert_ne!(
            repo_base("https://github.com/Sonarr/Sonarr"),
            repo_base("https://gitlab.com/other/other")
        );
    }

    // config_agreement: existing local path found in peer → agree.
    #[test]
    fn config_agreement_agrees() {
        let local = vec![ConfigFinding {
            path: "/etc/sonarr/config.xml".into(),
            source: ConfigSource::UnitArg,
            exists: true,
            confidence: Confidence::High,
        }];
        let peer = vec!["/etc/sonarr/config.xml".into(), "/var/lib/sonarr".into()];
        let (agree, comment) = config_agreement(&local, &peer);
        assert!(agree);
        assert!(comment.contains("agree"));
    }

    // config_agreement: no overlap → disagree.
    #[test]
    fn config_agreement_disagrees() {
        let local = vec![ConfigFinding {
            path: "/etc/sonarr/config.xml".into(),
            source: ConfigSource::Conventional,
            exists: true,
            confidence: Confidence::Medium,
        }];
        let peer = vec!["/opt/sonarr/config.xml".into()];
        let (agree, comment) = config_agreement(&local, &peer);
        assert!(!agree);
        assert!(comment.contains("differ"));
    }

    // config_agreement: no local findings → neutral.
    #[test]
    fn config_agreement_no_local() {
        let local: Vec<ConfigFinding> = vec![];
        let peer = vec!["/etc/sonarr/config.xml".into()];
        let (agree, comment) = config_agreement(&local, &peer);
        assert!(!agree);
        assert!(comment.contains("no local config findings"));
    }

    // preferred_detect_command: local probe failed + peer has command → prefer peer.
    #[test]
    fn preferred_detect_command_prefers_peer_when_probe_failed() {
        let peer_toml = "[package]\nname = \"x\"\nsource_type = \"direct_url\"\nsource_url = \"u\"\n\n[install]\nmethod = \"extract\"\ntarget_dir = \"/x\"\n\n[version]\ndetect_command = \"x --version\"\ndetect_pattern = \"([0-9]+)\"\n";
        let peer: ShadowRecipe = toml::from_str(peer_toml).unwrap();
        let cmd = preferred_detect_command(None, true, &peer);
        assert_eq!(cmd, Some("x --version".into()));
    }

    // preferred_detect_command: local probe OK → keep local.
    #[test]
    fn preferred_detect_command_keeps_local_when_probe_ok() {
        let peer_toml = "[package]\nname = \"x\"\nsource_type = \"direct_url\"\nsource_url = \"u\"\n\n[install]\nmethod = \"extract\"\ntarget_dir = \"/x\"\n\n[version]\ndetect_command = \"x --version\"\ndetect_pattern = \"([0-9]+)\"\n";
        let peer: ShadowRecipe = toml::from_str(peer_toml).unwrap();
        let cmd = preferred_detect_command(Some("1.0"), false, &peer);
        assert_eq!(cmd, None);
    }

    // mutate_recipe_for_local rewrites target_dir/config_paths/service.
    #[test]
    fn mutate_recipe_for_local_rewrites_fields() {
        let toml = "[package]\nname = \"x\"\nsource_type = \"direct_url\"\nsource_url = \"u\"\n\n[install]\nmethod = \"extract\"\ntarget_dir = \"/old\"\nservice = \"oldsvc\"\nconfig_paths = [\"/old/cfg\"]\n\n[version]\ndetect_command = \"x --version\"\ndetect_pattern = \"([0-9]+)\"\n";
        let out = mutate_recipe_for_local(toml, "/new", &["/new/cfg".into()], Some("newsvc")).unwrap();
        assert!(out.contains("target_dir = \"/new\""));
        assert!(out.contains("service = \"newsvc\""));
        assert!(out.contains("/new/cfg"));
        assert!(!out.contains("/old"));
    }
}
