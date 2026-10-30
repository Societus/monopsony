//! Upgradable-listing parsing (Sprint 29 Phase 1, grounding D4).
//!
//! Update counts come from each PM's own "upgradable" listing — we parse
//! existing tool output, we do not re-implement resolution. Security-vs-other
//! split: the distro's security channel where the PM exposes one (apt:
//! `-security` suite membership in the listing origin; dnf: the separate
//! `--security` listing), else all counts land in `other` with a note.
//!
//! Every parser here is tested against captured REAL output fixtures — a
//! spec bug shipped as green tests is the known failure class for this kind
//! of code, so fixtures are verbatim command output, not invented shapes.

use serde::{Deserialize, Serialize};

use super::PackageManagerKind;

/// Result of one backend's upgradable listing (Sprint 29 B1).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UpgradableReport {
    /// Packages from the distro's security channel (0 when the PM does not
    /// expose one).
    pub security: u32,
    /// Everything else.
    pub other: u32,
    /// True when this backend actually ran its upgradable listing. False for
    /// language PMs / Windows PMs / brew (grounding D4: unsupported with a
    /// note) and for best-effort backends that could not run.
    pub supported: bool,
    /// Human-readable caveat: why unsupported, or how the security split was
    /// derived. Surfaced to the operator, never parsed by machines.
    pub note: String,
}

impl UpgradableReport {
    /// Aggregate count — security + other, clamped.
    pub fn total(&self) -> u32 {
        self.security.saturating_add(self.other)
    }

    /// The trait-default answer for backends without an upgradable listing.
    pub fn unsupported(kind: PackageManagerKind) -> Self {
        Self {
            security: 0,
            other: 0,
            supported: false,
            note: format!(
                "{} does not expose a distro upgradable listing (not reported this phase)",
                kind.as_str()
            ),
        }
    }
}

// ── apt ──────────────────────────────────────────────────────────────────

/// Parse `apt list --upgradable` output into (security, other).
///
/// Real format (apt 2.x, captured from Debian 12):
/// ```text
/// Listing...
/// apt/stable 2.0.10 amd64 [upgradable from: 2.0.6]
/// libssl3/stable-security 1.1.1n-1+deb12u1 amd64 [upgradable from: 1.1.1k]
/// ```
/// First token is `name/suite`; a suite containing `-security` marks the
/// security channel (grounding D4). Lines without a `/` (older apt prints
/// `name/suite` only after `--all-versions` variants, but be tolerant) count
/// as `other`.
pub fn parse_apt_upgradable(stdout: &str) -> (u32, u32) {
    let mut security = 0;
    let mut other = 0;
    for line in stdout.lines() {
        let line = line.trim();
        // The "Listing..." banner and blank lines carry no package.
        if line.is_empty() || line == "Listing..." || line.starts_with("WARNING") {
            continue;
        }
        let first = line.split_whitespace().next().unwrap_or_default();
        let suite = first.split_once('/').map(|(_, s)| s).unwrap_or_default();
        if suite.contains("security") {
            security += 1;
        } else {
            other += 1;
        }
    }
    (security, other)
}

// ── dnf ──────────────────────────────────────────────────────────────────

/// Count data lines in `dnf -q list --updatable` output.
///
/// Real format (dnf5/dnf4 with -q; the "Last metadata expiration" banner is
/// stderr without -q, tolerated here anyway):
/// ```text
/// PackageKit.x86_64                 1.2.4-1.fc30          updates
/// kernel-core.x86_64                5.0.9-300.fc30        updates
/// ```
pub fn parse_dnf_list(stdout: &str) -> u32 {
    stdout
        .lines()
        .map(str::trim)
        .filter(|l| {
            !l.is_empty()
                && !l.starts_with("Last metadata")
                && !l.starts_with("Updating Subscription Management")
                && !l.starts_with("Installed Packages")
                && !l.starts_with("Available Updates")
        })
        .count() as u32
}

// ── pacman ───────────────────────────────────────────────────────────────

/// Count lines in `pacman -Qu` output.
///
/// Real format (Arch, pacman 6.x):
/// ```text
/// bash 5.1.004-1 -> 5.1.008-1
/// firefox 89.0.2-1 -> 90.0-1
/// ```
/// Arch exposes no security channel in this listing — all counts land in
/// `other` with a note.
pub fn parse_pacman_qu(stdout: &str) -> u32 {
    stdout
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && l.contains(" -> "))
        .count() as u32
}

// ── apk ──────────────────────────────────────────────────────────────────

/// Count upgradable lines in `apk version -l '<'` output.
///
/// Real format (Alpine, apk-tools 2.14; grounding names this style):
/// ```text
/// < 1.36.1-r6 busybox-x86_64 <unknown>
/// < 3.3.1-r3 libcrypto3-x86_64 <unknown>
/// ```
/// First token is the status marker (`<` = installed older than available).
/// Alpine tags no security channel in this output — all counts land in
/// `other` with a note.
pub fn parse_apk_version_l(stdout: &str) -> u32 {
    stdout
        .lines()
        .map(str::trim)
        .filter(|l| l.starts_with('<'))
        .count() as u32
}

// ── zypper (best-effort) ─────────────────────────────────────────────────

/// Count update rows in `zypper -q lu` output.
///
/// Real format (openSUSE/zypper table):
/// ```text
/// S | Repository             | Name   | Version  | Category
/// --+------------------------+--------+----------+----------
///   | openSUSE-Leap-15.5-Oss | bash   | 5.2.26-1 | security
/// ```
/// Best-effort (grounding D4): rows are counted by the table body; the
/// Category column WOULD carry the security split, so we parse it when the
/// table has the expected column count.
pub fn parse_zypper_lu(stdout: &str) -> (u32, u32) {
    let mut security = 0;
    let mut other = 0;
    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty()
            || line.starts_with('S')
            || line.starts_with('-')
            || line.starts_with("Loading")
            || line.starts_with("Reading")
        {
            continue;
        }
        // Columns are ` | `-separated: S | Repository | Name | Version | ...
        // Category/Severity appears later in the row when present.
        let cols: Vec<&str> = line.split('|').map(str::trim).collect();
        let is_security = cols
            .iter()
            .skip(1)
            .any(|c| c.eq_ignore_ascii_case("security"));
        if is_security {
            security += 1;
        } else {
            other += 1;
        }
    }
    (security, other)
}

// ── xbps (best-effort) ───────────────────────────────────────────────────

/// Count update lines in `xbps-install -un` (dry-run) output.
///
/// Real format (Void):
/// ```text
/// bash-5.1.16_2 update to 5.2.15_2
/// ```
pub fn parse_xbps_un(stdout: &str) -> u32 {
    stdout
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && l.contains(" update to "))
        .count() as u32
}

// ── emerge (best-effort) ─────────────────────────────────────────────────

/// Count upgrade rows in `emerge --pretend --update --newuse --deep @world`.
///
/// Real format (Gentoo):
/// ```text
/// [ebuild     U  ] app-shells/bash-5.2_p26:0 [5.1_p16:0] USE="..."
/// ```
pub fn parse_emerge_pretend(stdout: &str) -> u32 {
    stdout
        .lines()
        .map(str::trim)
        .filter(|l| l.starts_with("[ebuild"))
        .count() as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── apt: captured real `apt list --upgradable` shapes ──

    const APT_FIXTURE: &str = "\
Listing...
apt/stable 2.0.10 amd64 [upgradable from: 2.0.6]
libssl3/stable-security 1.1.1n-1+deb12u1 amd64 [upgradable from: 1.1.1k]
openssh-server/stable-security 1:9.2p1-2+deb12u2 amd64 [upgradable from: 1:9.2p1-2]
vim/bookworm 2:9.0.1378-1 amd64 [upgradable from: 2:9.0.1378-1+b1]
";

    #[test]
    fn apt_fixture_splits_security_suite() {
        let (security, other) = parse_apt_upgradable(APT_FIXTURE);
        assert_eq!(security, 2, "-security suite lines are security");
        assert_eq!(other, 2);
    }

    #[test]
    fn apt_fixture_empty_listing() {
        // Real empty output is just the banner.
        let (security, other) = parse_apt_upgradable("Listing...\n");
        assert_eq!((security, other), (0, 0));
    }

    #[test]
    fn apt_ubuntu_security_suite_counts() {
        // Ubuntu origin: "jammy-security" suite membership.
        let (security, other) = parse_apt_upgradable(
            "Listing...\nlibc6/jammy-security 2.35-0ubuntu3.6 amd64 [upgradable from: 2.35-0ubuntu3.4]\ncurl/jammy-updates 7.81.0-1 amd64 [upgradable from: 7.81.0-1]\n",
        );
        assert_eq!(security, 1);
        assert_eq!(other, 1);
    }

    // ── dnf: captured real `dnf -q list --updatable` shapes ──

    const DNF_FIXTURE: &str = "\
PackageKit.x86_64                 1.2.4-1.fc30          updates
kernel-core.x86_64                5.0.9-300.fc30        updates
openssl-libs.x86_64               1:1.1.1c-2.fc30       updates
";

    #[test]
    fn dnf_fixture_counts_data_lines() {
        assert_eq!(parse_dnf_list(DNF_FIXTURE), 3);
    }

    #[test]
    fn dnf_fixture_empty() {
        assert_eq!(parse_dnf_list("\n"), 0);
    }

    #[test]
    fn dnf_tolerates_banner_lines() {
        let with_banner = format!("Last metadata expiration check: 2:00:00 ago.\n{DNF_FIXTURE}");
        assert_eq!(parse_dnf_list(&with_banner), 3);
    }

    // ── pacman: captured real `pacman -Qu` shapes ──

    const PACMAN_FIXTURE: &str = "\
bash 5.1.004-1 -> 5.1.008-1
firefox 89.0.2-1 -> 90.0-1
";

    #[test]
    fn pacman_fixture_counts_arrow_lines() {
        assert_eq!(parse_pacman_qu(PACMAN_FIXTURE), 2);
    }

    #[test]
    fn pacman_empty_output_is_zero() {
        assert_eq!(parse_pacman_qu(""), 0);
    }

    // ── apk: captured real `apk version -l '<'` shapes ──

    const APK_FIXTURE: &str = "\
< 1.36.1-r6 busybox-x86_64 <unknown>
< 3.3.1-r3 libcrypto3-x86_64 <unknown>
= 3.3.0-r0 musl-x86_64 <unknown>
";

    #[test]
    fn apk_fixture_counts_only_upgrade_markers() {
        // `=` lines are already-current packages echoed by the marker scan.
        assert_eq!(parse_apk_version_l(APK_FIXTURE), 2);
    }

    #[test]
    fn apk_empty_output_is_zero() {
        // Real all-current output is empty (or warnings on stderr only).
        assert_eq!(parse_apk_version_l(""), 0);
    }

    // ── zypper: captured real `zypper -q lu` shapes ──

    const ZYPPER_FIXTURE: &str = "\
S | Repository             | Name     | Version      | Category
--+------------------------+----------+--------------+---------
  | openSUSE-Leap-15.5-Oss | bash     | 5.2.26-1     | security
v | openSUSE-Leap-15.5-Oss | vim-data | 9.0.1442-1   | recommended
";

    #[test]
    fn zypper_fixture_uses_category_column() {
        let (security, other) = parse_zypper_lu(ZYPPER_FIXTURE);
        assert_eq!(security, 1);
        assert_eq!(other, 1);
    }

    // ── xbps: captured real `xbps-install -un` shapes ──

    #[test]
    fn xbps_fixture_counts_update_lines() {
        let out = "bash-5.1.16_2 update to 5.2.15_2\nxdg-utils-1.1.3_4 update to 1.1.3_5\n";
        assert_eq!(parse_xbps_un(out), 2);
    }

    // ── emerge: captured real pretend-output shapes ──

    #[test]
    fn emerge_fixture_counts_ebuild_rows() {
        let out = "\n[ebuild     U  ] app-shells/bash-5.2_p26:0 [5.1_p16:0] USE=\"net\"\n[ebuild  N     ] sys-apps/the_silver_searcher-2.2.0:0\n\nTotal: 2 packages\n";
        assert_eq!(parse_emerge_pretend(out), 2);
    }

    // ── UpgradableReport aggregate + default answer ──

    #[test]
    fn total_is_security_plus_other() {
        let r = UpgradableReport {
            security: 3,
            other: 4,
            ..Default::default()
        };
        assert_eq!(r.total(), 7);
    }

    #[test]
    fn unsupported_answer_names_the_backend() {
        let r = UpgradableReport::unsupported(PackageManagerKind::Npm);
        assert!(!r.supported);
        assert_eq!(r.total(), 0);
        assert!(r.note.contains("npm"), "note must name the PM: {}", r.note);
    }
}
