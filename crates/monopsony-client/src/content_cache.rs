//! Apple Content Caching orchestration (Sprint 26 Phase 3.5).
//!
//! Turns the local Mac into an Apple Content Cache so OS updates + App Store
//! content for OTHER Apple devices on the LAN are served locally. Apple's own
//! machinery (locator, signatures, updates) does all the protocol work — this
//! module only drives the sanctioned, root-scriptable configuration surface:
//!
//! ```text
//! sudo AssetCacheManagerUtil activate
//! sudo defaults write /Library/Preferences/com.apple.AssetCache \
//!      AllowPersonalCaching -bool false
//! sudo AssetCacheManagerUtil reloadSettings
//! ```
//!
//! `AllowPersonalCaching=false` scopes the cache to SHARED content only — no
//! iCloud personal-blob caching (privacy-preserving default, kickoff §117–121).
//! Status and capability probes (`canActivate`, `status`, `settings`,
//! `-j isActivated`) work UNPRIVILEGED (bench-verified, macOS 26.3.1); only
//! activate/deactivate/reloadSettings need root (unprivileged → "must run as
//! root", rc=77).
//!
//! The three root steps run as ONE batched `sudo sh -c` invocation: a single
//! password prompt, ordered, stop-on-first-failure (`&&`).
//!
//! Not implemented here (future, per kickoff step 3): `ListenRangesOnly` /
//! cache-size-limit keys per site config. Phase 3.5b (TXT records, Parents
//! hierarchy, mDNS reflection) is a separate design.

use monopsony_common::package_manager::ProxyConfigAction;

/// Absolute path — the util is not on non-interactive PATHs (same lesson as
/// /opt/homebrew/bin/brew on the bench).
pub const ASSET_CACHE_UTIL: &str = "/usr/bin/AssetCacheManagerUtil";

/// defaults(1) manages the cache daemon's preference domain here.
pub const DEFAULTS: &str = "/usr/bin/defaults";

/// Preference domain for the Content Caching daemon.
pub const CONFIG_DOMAIN: &str = "/Library/Preferences/com.apple.AssetCache";

/// `false` = shared content only (no iCloud personal-blob caching).
pub const ALLOW_PERSONAL_CACHING_KEY: &str = "AllowPersonalCaching";

/// Content Caching is a macOS-only feature.
pub fn is_supported() -> bool {
    cfg!(target_os = "macos") && std::path::Path::new(ASSET_CACHE_UTIL).exists()
}

/// Build the enable action: activate + scope to shared content + reload, as
/// ONE `sudo sh -c` invocation so the operator gets a single password prompt
/// and the steps run in order (stop on first failure).
pub fn enable_action() -> ProxyConfigAction {
    let script = format!(
        "{} activate && {} write {} {} -bool false && {} reloadSettings",
        ASSET_CACHE_UTIL, DEFAULTS, CONFIG_DOMAIN, ALLOW_PERSONAL_CACHING_KEY, ASSET_CACHE_UTIL
    );
    ProxyConfigAction::RunCommand {
        program: "sudo".to_string(),
        args: vec!["sh".to_string(), "-c".to_string(), script],
    }
}

/// Build the disable action. Deactivation stops the cache service; already
/// cached content stays on disk under the DataPath (harmless; flush via
/// `AssetCacheManagerUtil flushPersonalCache`/`flushSharedCache` if desired).
pub fn disable_action() -> ProxyConfigAction {
    ProxyConfigAction::RunCommand {
        program: "sudo".to_string(),
        args: vec![
            "sh".to_string(),
            "-c".to_string(),
            format!("{} deactivate", ASSET_CACHE_UTIL),
        ],
    }
}

/// Unprivileged capability probe — "can this Mac host a cache at all?"
/// Real output: `Content caching can be activated.` (rc=0) on **stderr**
/// (NSLog goes to stderr; bench-proven — a stdout-only capture sees an empty
/// string and misreports "no"). Negative case reads "cannot be activated",
/// which does NOT contain the positive phrase.
pub fn can_activate() -> Result<bool, String> {
    let out = std::process::Command::new(ASSET_CACHE_UTIL)
        .arg("canActivate")
        .output()
        .map_err(|e| format!("{}: {}", ASSET_CACHE_UTIL, e))?;
    let mut combined = String::from_utf8_lossy(&out.stdout).to_string();
    combined.push(' ');
    combined.push_str(&String::from_utf8_lossy(&out.stderr));
    Ok(parse_can_activate(&combined))
}

/// Parse `AssetCacheManagerUtil canActivate` output (both streams combined).
pub fn parse_can_activate(output: &str) -> bool {
    output.to_ascii_lowercase().contains("can be activated")
}

/// Activate/config/reload status, all collected UNPRIVILEGED.
#[derive(Debug, Clone, PartialEq)]
pub struct ContentCacheStatus {
    /// `AssetCacheManagerUtil isActivated` (JSON `result` field).
    pub activated: bool,
    /// `AllowPersonalCaching` from `settings` output; None = not reported.
    pub allow_personal_caching: Option<bool>,
    /// All `<Key>: <value>` settings rows as reported.
    pub settings: Vec<(String, String)>,
}

/// Collect current status. Never needs root on current macOS; if a future
/// release gates these verbs, the error surfaces to the caller.
pub fn status() -> Result<ContentCacheStatus, String> {
    let raw = is_activated_raw()?;
    let activated = parse_is_activated(&raw).ok_or_else(|| {
        format!(
            "could not parse isActivated output (unexpected format): {}",
            first_line(&raw)
        )
    })?;
    let settings_out = run(ASSET_CACHE_UTIL, &["settings".to_string()])?;
    let settings = parse_settings(&settings_out);
    let allow_personal_caching = settings
        .iter()
        .find(|(k, _)| k == ALLOW_PERSONAL_CACHING_KEY)
        .and_then(|(_, v)| parse_bool(v));
    Ok(ContentCacheStatus {
        activated,
        allow_personal_caching,
        settings,
    })
}

/// ON verdict = daemon activated AND scoped to shared content. Returns
/// (on, detail) — the detail is the honest status line either way.
pub fn enabled_and_private(st: &ContentCacheStatus) -> (bool, String) {
    if !st.activated {
        return (false, "deactivated".to_string());
    }
    match st.allow_personal_caching {
        Some(false) => (
            true,
            "active — shared content only (personal/iCloud caching off)".to_string(),
        ),
        Some(true) => (
            true,
            "ACTIVE but personal (iCloud) caching is ON — run `setup mac --enable` to scope to shared content"
                .to_string(),
        ),
        None => (
            true,
            "active; AllowPersonalCaching not reported by this macOS".to_string(),
        ),
    }
}

/// Parse `AssetCacheManagerUtil -j isActivated` output. The util exits rc=1
/// when deactivated and prints an NSLog line to stderr plus the JSON object —
/// pass combined stdout+stderr (see `is_activated_raw`). Real shapes:
/// `{"name":"isActivated","result":true}` / `..."result":false}`.
pub fn parse_is_activated(output: &str) -> Option<bool> {
    if output.contains("\"result\":true") {
        Some(true)
    } else if output.contains("\"result\":false") {
        Some(false)
    } else {
        None
    }
}

/// Parse `AssetCacheManagerUtil settings` output into `(Key, value)` rows.
/// Real shape: 4-space-indented `<Key>: <value>` lines under a
/// `Content caching settings:` header. The header (its key contains spaces)
/// is skipped — real keys are CamelCase with no spaces.
pub fn parse_settings(output: &str) -> Vec<(String, String)> {
    output
        .lines()
        .filter_map(|l| {
            let (k, v) = l.trim().split_once(':')?;
            let k = k.trim();
            if k.is_empty() || k.contains(' ') {
                return None;
            }
            Some((k.to_string(), v.trim().to_string()))
        })
        .collect()
}

/// Parse a settings value into a bool ("true"/"false", case-insensitive).
pub fn parse_bool(v: &str) -> Option<bool> {
    let v = v.trim().to_ascii_lowercase();
    match v.as_str() {
        "true" | "yes" | "1" => Some(true),
        "false" | "no" | "0" => Some(false),
        _ => None,
    }
}

/// `-j isActivated` needs special handling: rc=1 when deactivated, and the
/// JSON rides stdout while an NSLog line rides stderr. Never treat rc as the
/// signal — capture BOTH streams and let `parse_is_activated` find the JSON.
fn is_activated_raw() -> Result<String, String> {
    let out = std::process::Command::new(ASSET_CACHE_UTIL)
        .args(["-j", "isActivated"])
        .output()
        .map_err(|e| format!("{}: {}", ASSET_CACHE_UTIL, e))?;
    let mut combined = String::from_utf8_lossy(&out.stdout).to_string();
    combined.push(' ');
    combined.push_str(&String::from_utf8_lossy(&out.stderr));
    Ok(combined)
}

fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or("").trim().to_string()
}

fn run(program: &str, args: &[String]) -> Result<String, String> {
    let out = std::process::Command::new(program)
        .args(args)
        .output()
        .map_err(|e| format!("{}: {}", program, e))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    } else {
        Err(format!(
            "{} {} failed: {}",
            program,
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Fixtures below are BYTE-REAL output captured on the bench
    // (m2mini, macOS 26.3.1, 2026-10-03) — see fleet-bench-verification rule:
    // parsers get tested against real upstream shapes only.

    #[test]
    fn test_parse_is_activated_false() {
        // Deactivated: rc=1, NSLog line + JSON (both streams, as captured).
        let raw = "2026-10-03 12:52:35.344 AssetCacheManagerUtil[25169:1181962] Content caching is deactivated: (no error)\n{\"name\":\"isActivated\",\"result\":false}\n";
        assert_eq!(parse_is_activated(raw), Some(false));
    }

    #[test]
    fn test_parse_is_activated_true() {
        let raw = "{\"name\":\"isActivated\",\"result\":true}";
        assert_eq!(parse_is_activated(raw), Some(true));
    }

    #[test]
    fn test_parse_is_activated_garbage() {
        assert_eq!(parse_is_activated("must run as root"), None);
        assert_eq!(parse_is_activated(""), None);
    }

    const SETTINGS_SAMPLE: &str = "Content caching settings:\n    Activated: true\n    AllowPersonalCaching: true\n    AllowSharedCaching: true\n    AllowTetheredCaching: false\n    CacheLimit: unlimited\n    DataPath: /Library/Application Support/Apple/AssetCache/Data\n    ListenRangesOnly: false\n    LocalSubnetsOnly: true\n";

    #[test]
    fn test_parse_settings_real_shape() {
        let rows = parse_settings(SETTINGS_SAMPLE);
        assert_eq!(rows.len(), 8); // header line skipped
        assert_eq!(rows[0], ("Activated".to_string(), "true".to_string()));
        assert!(rows
            .iter()
            .any(|(k, v)| k == "AllowPersonalCaching" && v == "true"));
        assert!(rows
            .iter()
            .any(|(k, v)| k == "DataPath"
                && v == "/Library/Application Support/Apple/AssetCache/Data"));
    }

    #[test]
    fn test_parse_settings_no_spaces_in_keys() {
        // The settings HEADER must not become a row.
        let rows = parse_settings(SETTINGS_SAMPLE);
        assert!(!rows.iter().any(|(k, _)| k.contains(' ')));
    }

    #[test]
    fn test_parse_bool() {
        assert_eq!(parse_bool("true"), Some(true));
        assert_eq!(parse_bool("False"), Some(false));
        assert_eq!(parse_bool("unlimited"), None);
        assert_eq!(parse_bool(""), None);
    }

    #[test]
    fn test_status_collection_real_fixture() {
        // The full status pipeline over real-shaped inputs.
        let raw = "{\"name\":\"isActivated\",\"result\":false}";
        let activated = parse_is_activated(raw).unwrap();
        let settings = parse_settings(SETTINGS_SAMPLE);
        let allow = settings
            .iter()
            .find(|(k, _)| k == ALLOW_PERSONAL_CACHING_KEY)
            .and_then(|(_, v)| parse_bool(v));
        let st = ContentCacheStatus {
            activated,
            allow_personal_caching: allow,
            settings,
        };
        assert!(!st.activated);
        assert_eq!(st.allow_personal_caching, Some(true));
        // Deactivated → OFF regardless of the settings key.
        let (on, detail) = enabled_and_private(&st);
        assert!(!on);
        assert_eq!(detail, "deactivated");
    }

    #[test]
    fn test_verdict_active_but_personal() {
        let st = ContentCacheStatus {
            activated: true,
            allow_personal_caching: Some(true),
            settings: vec![],
        };
        let (on, detail) = enabled_and_private(&st);
        assert!(on);
        assert!(detail.contains("personal"));
        assert!(detail.contains("--enable"));
    }

    #[test]
    fn test_verdict_active_and_private() {
        let st = ContentCacheStatus {
            activated: true,
            allow_personal_caching: Some(false),
            settings: vec![],
        };
        let (on, detail) = enabled_and_private(&st);
        assert!(on);
        assert!(detail.contains("shared content only"));
    }

    #[test]
    fn test_enable_action_single_sudo_invocation() {
        match enable_action() {
            ProxyConfigAction::RunCommand { program, args } => {
                assert_eq!(program, "sudo");
                assert_eq!(args[0], "sh");
                assert_eq!(args[1], "-c");
                let script = &args[2];
                // All three steps, in order, joined with && (single prompt,
                // stop on first failure).
                let a = format!("{} activate", ASSET_CACHE_UTIL);
                let b = format!(
                    "{} write {} {} -bool false",
                    DEFAULTS, CONFIG_DOMAIN, ALLOW_PERSONAL_CACHING_KEY
                );
                let c = format!("{} reloadSettings", ASSET_CACHE_UTIL);
                let expected = format!("{} && {} && {}", a, b, c);
                assert_eq!(script, &expected);
                // Script is quoting-safe: no quote characters at all.
                assert!(!script.contains('"') && !script.contains('\''));
            }
            other => panic!("expected RunCommand, got {:?}", other),
        }
    }

    #[test]
    fn test_disable_action_shape() {
        match disable_action() {
            ProxyConfigAction::RunCommand { program, args } => {
                assert_eq!(program, "sudo");
                assert_eq!(args[2], format!("{} deactivate", ASSET_CACHE_UTIL));
            }
            other => panic!("expected RunCommand, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_can_activate_positive() {
        // Real bench output (m2mini): NSLog line + phrase, both on STDERR —
        // which is exactly why the stdout-only capture misreported "no".
        let raw = "2026-10-03 12:52:13.476 AssetCacheManagerUtil[25154:1181683] Content caching can be activated.\n";
        assert!(parse_can_activate(raw));
    }

    #[test]
    fn test_parse_can_activate_negative() {
        // "cannot be activated" must NOT match the positive phrase.
        assert!(!parse_can_activate(
            "2026-10-03 12:00:00.000 AssetCacheManagerUtil[1:1] Content caching cannot be activated.\n"
        ));
        assert!(!parse_can_activate(""));
    }

    #[test]
    fn test_is_supported_off_macos() {
        // Signal check only: false on the Linux dev box, true on a macOS
        // host with the util present. Either way it must not panic.
        let _ = is_supported();
    }
}
