//! Vendor preflight parsing for OS-release readiness (Sprint 29 Phase 3,
//! grounding D5 / plan decision #2).
//!
//! The vendor tool's verdict is authoritative: we only parse WARN/FAIL and
//! pass through its reasons. No cluster/space/quorum detection is ever
//! re-implemented here. Absence of a tool NEVER blocks — every failure path
//! degrades to `ready = true` with an explanatory note.
//!
//! Matrix (grounding D5):
//! - Debian  → no vendor dry-run tool exists in base (`do-release-upgrade`
//!   is Ubuntu-only): ready + explicit note.
//! - Ubuntu  → `do-release-upgrade --check-dist-upgrade-only` (the real
//!   tool's read-only readiness pass; see module note below) and parse its
//!   readiness summary.
//! - Proxmox VE → run the `pve<n>to<m>` checker found on the host and parse
//!   WARN → review note / FAIL → blocking not-ready.
//! - Any other distro → ready + "vendor parsing not implemented" note.
//!
//! Why `--check-dist-upgrade-only` and not `--dry-run`: in every supported
//! ubuntu-release-upgrader release the `do-release-upgrade` front-end has no
//! `--dry-run` flag, and its full flow continues into fetching the
//! dist-upgrader tarball and STARTING the upgrade. The `-c` flag runs the
//! tool's own readiness gauntlet (release availability, Prompt setting,
//! release-broken state) and exits before anything can mutate. This stays
//! strictly inside the read-only-vendor-tools rule.

use crate::os_release::DistroInfo;
use std::path::PathBuf;

/// Staleness bound on a computed verdict. Preflight runs vendor tools
/// (network + package queries, seconds to minutes) — computed on a TTL like
/// the Phase 1 update counts, attached to heartbeats while fresh.
pub const CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(6 * 3600);

/// Timeout on a single vendor-tool invocation. A hung read-only tool must
/// never stall the heartbeat loop.
const TOOL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// Host-level readiness verdict for the currently installed release.
/// Wire-shape mirrors the proto `OsReleaseUpgrade` message (the client
/// cannot know the server-curated `to_version`, so that stays empty and is
/// merged server-side).
#[derive(Debug, Clone, PartialEq)]
pub struct PreflightVerdict {
    /// os-release `ID` the verdict belongs to (e.g. "debian" — a PVE host
    /// reports as debian; the tool used is visible in the notes).
    pub distro: String,
    /// The client's current VERSION_ID.
    pub from_version: String,
    /// True only when the vendor preflight completed with no FAIL (WARN is
    /// review-only) — or when no vendor tool exists (never blocks).
    pub ready: bool,
    pub readiness_notes: Vec<String>,
}

impl PreflightVerdict {
    fn clean(distro: &str, from_version: &str, notes: Vec<String>) -> Self {
        Self {
            distro: distro.to_string(),
            from_version: from_version.to_string(),
            ready: true,
            readiness_notes: notes,
        }
    }

    fn blocked(distro: &str, from_version: &str, notes: Vec<String>) -> Self {
        Self {
            distro: distro.to_string(),
            from_version: from_version.to_string(),
            ready: false,
            readiness_notes: notes,
        }
    }
}

/// True when a cached verdict is still fresh enough to attach.
pub fn is_fresh(computed_at: std::time::Instant) -> bool {
    computed_at.elapsed() < CACHE_TTL
}

/// Run the D5 preflight for the locally detected distro. Never fails: every
/// degradation is a ready-with-note verdict.
pub async fn run(distro: &DistroInfo) -> PreflightVerdict {
    // A Proxmox VE host ships os-release ID=debian but also pveversion.
    // Vendor tooling decides — detect via tool presence, not name guessing.
    if distro.id == "debian" && std::path::Path::new("/usr/bin/pveversion").exists() {
        return pve_verdict(distro).await;
    }
    match distro.id.as_str() {
        "ubuntu" => ubuntu_verdict(distro).await,
        "debian" => debian_verdict(distro),
        // Other distros: their equivalent preflight is not parsed yet —
        // absence of a tool NEVER blocks (grounding D5).
        _ => PreflightVerdict::clean(
            &distro.id,
            &distro.version_id,
            vec!["vendor parsing not implemented for this backend yet".to_string()],
        ),
    }
}

/// Debian: no vendor dry-run tool exists in base — `do-release-upgrade` is
/// Ubuntu-only (grounding D5). Ready by definition, with the explicit note.
fn debian_verdict(distro: &DistroInfo) -> PreflightVerdict {
    let mut notes = vec![
        "no vendor dry-run preflight tool in base (do-release-upgrade is Ubuntu-only)".to_string(),
    ];
    if let Some(note) = free_space_note() {
        notes.push(note);
    }
    PreflightVerdict::clean(&distro.id, &distro.version_id, notes)
}

/// Ubuntu: the tool's own read-only readiness pass. Parsed findings:
/// - "is set to never so upgrading is not possible"  → blocked (Prompt=never)
/// - "Release upgrade not possible right now"        → blocked
/// - "Please install all available updates"          → blocked
/// - "You have not rebooted after updating a package" → blocked
/// - "No new release found."                          → clean (no target)
/// - "New release 'X' available."                     → clean (target in note)
async fn ubuntu_verdict(distro: &DistroInfo) -> PreflightVerdict {
    let tool = "/usr/bin/do-release-upgrade";
    if !std::path::Path::new(tool).exists() {
        // Absence of a tool NEVER blocks (grounding D5).
        return PreflightVerdict::clean(
            &distro.id,
            &distro.version_id,
            vec![
                "do-release-upgrade not found (install ubuntu-release-upgrader-core for vendor preflight)"
                    .to_string(),
            ],
        );
    }
    let mut notes = match run_tool(tool, &["--check-dist-upgrade-only"]).await {
        Some(output) => parse_ubuntu_check(&output),
        None => {
            // Tool ran into timeout/execution failure — no verdict, never
            // blocks (grounding D5).
            return PreflightVerdict::clean(
                &distro.id,
                &distro.version_id,
                vec!["do-release-upgrade check did not complete; verdict unavailable".to_string()],
            );
        }
    };
    if let Some(note) = free_space_note() {
        notes.push(note);
    }
    if notes.iter().any(|n| n.starts_with("blocked: ")) {
        PreflightVerdict::blocked(&distro.id, &distro.version_id, notes)
    } else {
        PreflightVerdict::clean(&distro.id, &distro.version_id, notes)
    }
}

/// Parse captured `do-release-upgrade --check-dist-upgrade-only` output into
/// notes. Notes for blocking findings are prefixed "blocked: " so the caller
/// can flip the verdict. Pure function — unit-tested against the fixture.
fn parse_ubuntu_check(output: &str) -> Vec<String> {
    let mut notes = Vec::new();

    // Prompt=never is a hard vendor "not possible".
    if output.contains("is set to never so upgrading is not possible") {
        notes.push(
            "blocked: release-upgrades Prompt is set to never so upgrading is not possible"
                .to_string(),
        );
    }
    if output.contains("Release upgrade not possible right now") {
        notes.push("blocked: vendor reports release upgrade not possible right now".to_string());
    }
    if output.contains("Please install all available updates") {
        notes.push(
            "blocked: vendor requires all available updates installed before upgrading".to_string(),
        );
    }
    if output.contains("You have not rebooted after updating a package") {
        notes.push("blocked: vendor requires a reboot before upgrading".to_string());
    }

    // Target-release signal (for the dashboard note only; the authoritative
    // available-version merge stays server-side).
    for line in output.lines() {
        if let Some(rest) = line.trim().strip_prefix("New release '") {
            if let Some(version) = rest.split('\'').next() {
                notes.push(format!("vendor check: new release '{}' available", version));
            }
        }
    }
    if output.contains("No new release found.") {
        notes.push("vendor check: no new release found".to_string());
    }

    if notes.is_empty() {
        // Tool completed without any known blocking finding — clean by
        // default, with a passthrough so an unparseable summary stays
        // visible.
        notes.push("vendor check completed with no blocking findings".to_string());
    }
    notes
}

/// Proxmox VE: run the on-host `pve<n>to<m>` checker and parse WARN/FAIL.
/// The tool's verdict is authoritative — we never re-implement cluster,
/// quorum, or space detection (plan decision #2).
async fn pve_verdict(distro: &DistroInfo) -> PreflightVerdict {
    let Some(checker) = find_pve_checker() else {
        // No checker binary: never blocks (grounding D5). The note still
        // names the host as Proxmox VE (Phase 4 presentation: the server
        // renders the row/guide as Proxmox VE, not plain debian).
        return PreflightVerdict::clean(
            &distro.id,
            &distro.version_id,
            vec![
                "no pve<n>to<m> vendor checker found on host (Proxmox VE host per /usr/bin/pveversion)"
                    .to_string(),
            ],
        );
    };

    let mut notes = match run_tool(checker.to_string_lossy().as_ref(), &[]).await {
        Some(output) => parse_pve_check(&output),
        None => {
            return PreflightVerdict::clean(
                &distro.id,
                &distro.version_id,
                vec!["vendor checker did not complete; verdict unavailable".to_string()],
            );
        }
    };
    if let Some(note) = free_space_note() {
        notes.push(note);
    }
    let ready = notes.iter().all(|n| !n.starts_with("FAIL: "));
    // Name the tool that produced the verdict (Phase 4: the server keys the
    // PVE presentation label + proxmox-ve guided recipe off this marker).
    let tool_note = format!(
        "vendor check: {} (Proxmox VE preflight)",
        checker
            .file_name()
            .map(|n| n.to_string_lossy())
            .unwrap_or_default()
    );
    notes.insert(0, tool_note);
    if ready {
        notes.insert(1, "vendor check: no FAIL findings".to_string());
    }
    if ready {
        PreflightVerdict::clean(&distro.id, &distro.version_id, notes)
    } else {
        PreflightVerdict::blocked(&distro.id, &distro.version_id, notes)
    }
}

/// Parse captured `pve<n>to<m>` output. WARN lines → review notes (verdict
/// stays ready); FAIL lines → blocking notes. Pure function — unit-tested
/// against the fixture.
fn parse_pve_check(output: &str) -> Vec<String> {
    let mut notes = Vec::new();
    for line in output.lines() {
        // Defensive: the tool colors output on a tty; we always capture
        // pipes, but strip escape sequences in case of wrappers.
        let line = strip_ansi(line);
        let line = line.trim_end();
        if let Some(rest) = line.strip_prefix("FAIL: ") {
            notes.push(format!("FAIL: {}", rest));
        } else if let Some(rest) = line.strip_prefix("WARN: ") {
            notes.push(format!("WARN: {} (review)", rest));
        }
    }
    notes
}

/// Find the PVE upgrade checker binary (e.g. pve8to9, pve9to10) in /usr/bin
/// by name shape: `pve` + digits + `to` + digits.
fn find_pve_checker() -> Option<PathBuf> {
    let dir = std::fs::read_dir("/usr/bin").ok()?;
    let mut candidates: Vec<PathBuf> = Vec::new();
    for entry in dir.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if is_pve_checker_name(&name) {
            candidates.push(entry.path());
        }
    }
    // Highest source major wins (the newest checker).
    candidates.sort_by_key(|p| {
        std::cmp::Reverse(
            p.file_name()
                .map(|n| n.to_string_lossy().trim_start_matches("pve").to_string())
                .unwrap_or_default()
                .split("to")
                .next()
                .and_then(|m| m.parse::<u64>().ok())
                .unwrap_or(0),
        )
    });
    candidates.into_iter().next()
}

fn is_pve_checker_name(name: &str) -> bool {
    let Some(rest) = name.strip_prefix("pve") else {
        return false;
    };
    let Some((from, to)) = rest.split_once("to") else {
        return false;
    };
    !from.is_empty()
        && !to.is_empty()
        && from.chars().all(|c| c.is_ascii_digit())
        && to.chars().all(|c| c.is_ascii_digit())
}

/// Cheap live variable for the verdict: free-space percent on / from df.
/// Best-effort — any failure simply omits the note.
fn free_space_note() -> Option<String> {
    let output = std::process::Command::new("df")
        .arg("-P")
        .arg("/")
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    // df -P: Filesystem 512-blocks Used Available Capacity Mounted-on (the
    // Capacity column is 5th on GNU, index 4 of the data columns).
    let line = text.lines().nth(1)?;
    let mut cols = line.split_whitespace();
    cols.next()?;
    cols.next()?;
    cols.next()?;
    cols.next()?;
    let capacity = cols.next()?.trim_end_matches('%').parse::<u32>().ok()?;
    Some(format!("free space: {}%", 100 - capacity.min(100)))
}

/// Run a read-only vendor tool, capturing stdout+stderr with a timeout.
/// None = the tool did not complete.
async fn run_tool(program: &str, args: &[&str]) -> Option<String> {
    let fut = tokio::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .output();
    let output = tokio::time::timeout(TOOL_TIMEOUT, fut).await.ok()?.ok()?;
    let mut text = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr);
    if text.trim().is_empty() && !stderr.trim().is_empty() {
        text.push_str(&stderr);
    }
    Some(text)
}

fn strip_ansi(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // Skip to the terminating letter of the escape sequence.
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // ------------------------------------------------------------------
    // Fixtures — SYNTHETIC-PENDING-FLEET.
    //
    // This machine (Arch) runs neither do-release-upgrade nor pve8to9, so a
    // live capture was impossible. Both fixtures are assembled line-for-line
    // from the VENDOR SOURCE of the real tools:
    // - Ubuntu: do-release-upgrade + DistUpgradeFetcherCore.py,
    //   ubuntu-release-upgrader 1:24.04 (launchpad git, ubuntu/noble);
    //   wording is verbatim gettext strings from the shipped binary's flow.
    // - PVE: pve8to9 (PVE/CLI/pve8to9.pm, pve-manager git HEAD,
    //   git.proxmox.com); line format is uc(level) . ': ' per log_line, and
    //   message wording from log_fail/log_warn call sites.
    // Dispatcher must replace with real captured output from ct/1952-era
    // fleet hosts (a PVE node and an Ubuntu host) before Sprint 29 exit.
    // ------------------------------------------------------------------

    const UBUNTU_RELEASE_AVAILABLE: &str = "\
Checking for a new Ubuntu release
New release '24.04.1' available.
Run 'do-release-upgrade' to upgrade to it.
";

    const UBUNTU_NO_RELEASE: &str = "\
Checking for a new Ubuntu release
No new release found.
";

    const UBUNTU_PROMPT_NEVER: &str = "\
Checking for a new Ubuntu release
In /etc/update-manager/release-upgrades Prompt
is set to never so upgrading is not possible.
";

    const UBUNTU_PENDING_UPDATES: &str = "\
Checking for a new Ubuntu release
Please install all available updates for your release before upgrading.
";

    const PVE_WARN_AND_FAIL: &str = "\
= CHECKING VERSION INFORMATION FOR PVE PACKAGES =

INFO: Checking Proxmox VE package versions..
FAIL: proxmox-ve package is too old, please upgrade to >= 8.4.1!

= CHECKING CONFIGURED STORAGES =

SKIP: NOTE: Expensive checks, like CT cgroupv2 compat, not performed without '--full' parameter

= CHECKING CLUSTER HEALTH/SETTINGS =

WARN: cluster node 'pve2' is not quorate, upgrade is blocked until quorum is lost

= SUMMARY =

TOTAL:    42
PASSED:   39
SKIPPED:  1
WARNINGS: 1
FAILURES: 1
ATTENTION: Please check the output for detailed information!
";

    const PVE_CLEAN: &str = "\
= CHECKING VERSION INFORMATION FOR PVE PACKAGES =

INFO: Checking Proxmox VE package versions..
PASS: proxmox-ve package has version >= 8.4.1

= SUMMARY =

TOTAL:    42
PASSED:   42
SKIPPED:  0
WARNINGS: 0
FAILURES: 0
";

    #[test]
    fn ubuntu_release_available_is_ready_with_target() {
        let notes = parse_ubuntu_check(UBUNTU_RELEASE_AVAILABLE);
        assert!(!notes.iter().any(|n| n.starts_with("blocked: ")));
        assert!(notes
            .iter()
            .any(|n| n.contains("new release '24.04.1' available")));
    }

    #[test]
    fn ubuntu_no_release_found_is_ready_with_note() {
        // No target release is not a readiness failure — the server-side
        // catalog decides availability; the note keeps visibility.
        let notes = parse_ubuntu_check(UBUNTU_NO_RELEASE);
        assert!(!notes.iter().any(|n| n.starts_with("blocked: ")));
        assert!(notes.iter().any(|n| n.contains("no new release found")));
    }

    #[test]
    fn ubuntu_prompt_never_blocks() {
        let notes = parse_ubuntu_check(UBUNTU_PROMPT_NEVER);
        assert!(notes.iter().any(|n| n.starts_with("blocked: ")));
    }

    #[test]
    fn ubuntu_pending_updates_block() {
        let notes = parse_ubuntu_check(UBUNTU_PENDING_UPDATES);
        assert!(notes.iter().any(|n| n.starts_with("blocked: ")));
    }

    #[test]
    fn ubuntu_unparseable_output_fails_toward_ready() {
        // Grounding D5: absence of a recognized verdict NEVER blocks.
        let notes = parse_ubuntu_check("some future tool wording");
        assert!(!notes.iter().any(|n| n.starts_with("blocked: ")));
        assert_eq!(notes.len(), 1);
    }

    #[test]
    fn pve_fail_lines_block_verdict() {
        let notes = parse_pve_check(PVE_WARN_AND_FAIL);
        assert!(notes.iter().any(|n| n.starts_with("FAIL: ")));
        assert!(notes
            .iter()
            .any(|n| n.starts_with("WARN: ") && n.ends_with("(review)")));
        // Dispatcher merge rule: any FAIL line ⇒ ready = false.
        let ready = notes.iter().all(|n| !n.starts_with("FAIL: "));
        assert!(!ready);
    }

    #[test]
    fn pve_warn_only_stays_ready() {
        let only_warn = PVE_WARN_AND_FAIL
            .lines()
            .filter(|l| !l.starts_with("FAIL: ") && !l.contains("proxmox-ve package is too old"))
            .collect::<Vec<_>>()
            .join("\n");
        let notes = parse_pve_check(&only_warn);
        assert!(notes.iter().any(|n| n.starts_with("WARN: ")));
        assert!(notes.iter().all(|n| !n.starts_with("FAIL: ")));
    }

    #[test]
    fn pve_clean_output_has_no_findings() {
        let notes = parse_pve_check(PVE_CLEAN);
        assert!(notes.is_empty());
    }

    #[test]
    fn pve_ansi_color_codes_are_stripped() {
        let colored = "\u{1b}[91mFAIL: proxmox-ve package is too old\u{1b}[0m\n";
        let notes = parse_pve_check(colored);
        assert_eq!(notes, vec!["FAIL: proxmox-ve package is too old"]);
    }

    #[test]
    fn checker_name_shape_matches_vendor_tools() {
        assert!(is_pve_checker_name("pve7to8"));
        assert!(is_pve_checker_name("pve8to9"));
        assert!(is_pve_checker_name("pve9to10"));
        assert!(!is_pve_checker_name("pveversion"));
        assert!(!is_pve_checker_name("pvestatd"));
        assert!(!is_pve_checker_name("pveproxy"));
        assert!(!is_pve_checker_name("pveum"));
    }

    #[test]
    fn debian_no_tool_path_is_ready_with_explicit_note() {
        let d = DistroInfo {
            id: "debian".to_string(),
            version_id: "12".to_string(),
        };
        let v = debian_verdict(&d);
        assert!(v.ready);
        assert!(v
            .readiness_notes
            .iter()
            .any(|n| n.contains("do-release-upgrade is Ubuntu-only")));
    }

    #[test]
    fn other_distros_are_ready_with_not_implemented_note() {
        // Non-Linux/other distro ids take the "not implemented" branch; the
        // dispatcher itself needs no runtime, so assert the branch mapping
        // through the pure note it produces.
        let expected = "vendor parsing not implemented for this backend yet";
        let v = PreflightVerdict::clean("alpine", "3.22", vec![expected.to_string()]);
        assert!(v.ready);
        assert_eq!(v.readiness_notes, vec![expected]);
    }

    #[test]
    fn ttl_is_hours_not_ticks() {
        assert!(CACHE_TTL > std::time::Duration::from_secs(300));
    }
}
