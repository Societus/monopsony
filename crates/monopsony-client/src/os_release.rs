//! /etc/os-release parsing (Sprint 29 Phase 2, grounding D2/plan decision #3).
//!
//! Only the machine can introspect its own OS — the client reports
//! `(ID, VERSION_ID)` and the server merges it against its curated catalog.

/// Distro identity reported to the server (proto UpdateReport fields 8/9).
#[derive(Debug, Clone, PartialEq)]
pub struct DistroInfo {
    /// os-release `ID`, e.g. "debian"
    pub id: String,
    /// os-release `VERSION_ID`, e.g. "12"
    pub version_id: String,
}

/// Detect the running distro from `/etc/os-release` (falling back to
/// `/usr/lib/os-release` per the os-release spec). Returns None when the
/// file is absent or carries no usable ID/VERSION_ID.
pub fn detect() -> Option<DistroInfo> {
    let content = std::fs::read_to_string("/etc/os-release")
        .or_else(|_| std::fs::read_to_string("/usr/lib/os-release"))
        .ok()?;
    parse_os_release(&content)
}

/// Parse os-release content into a [`DistroInfo`]. First `ID=`/`VERSION_ID=`
/// wins; quotes are stripped; other keys are ignored.
pub fn parse_os_release(content: &str) -> Option<DistroInfo> {
    let mut id = None;
    let mut version_id = None;
    for line in content.lines() {
        let line = line.trim();
        if let Some(v) = line.strip_prefix("ID=") {
            id = id.or_else(|| Some(unquote(v)));
        } else if let Some(v) = line.strip_prefix("VERSION_ID=") {
            version_id = version_id.or_else(|| Some(unquote(v)));
        }
    }
    Some(DistroInfo {
        id: id?,
        version_id: version_id?,
    })
}

fn unquote(v: &str) -> String {
    v.trim_matches('"').trim_matches('\'').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEBIAN_12_FIXTURE: &str = "\
PRETTY_NAME=\"Debian GNU/Linux 12 (bookworm)\"
NAME=\"Debian GNU/Linux\"
VERSION_ID=\"12\"
VERSION=\"12 (bookworm)\"
VERSION_CODENAME=bookworm
ID=debian
HOME_URL=\"https://www.debian.org/\"
";

    const ALPINE_FIXTURE: &str = "\
NAME=\"Alpine Linux\"
ID=alpine
VERSION_ID=3.22.1
PRETTY_NAME=\"Alpine Linux v3.22\"
";

    #[test]
    fn parses_debian_12_fixture() {
        let d = parse_os_release(DEBIAN_12_FIXTURE).unwrap();
        assert_eq!(d.id, "debian");
        assert_eq!(d.version_id, "12");
    }

    #[test]
    fn parses_unquoted_alpine_fixture() {
        let d = parse_os_release(ALPINE_FIXTURE).unwrap();
        assert_eq!(d.id, "alpine");
        assert_eq!(d.version_id, "3.22.1");
    }

    #[test]
    fn missing_version_id_is_none() {
        assert!(parse_os_release("ID=debian\n").is_none());
    }

    #[test]
    fn missing_id_is_none() {
        assert!(parse_os_release("VERSION_ID=\"12\"\n").is_none());
    }

    #[test]
    fn keys_with_shared_prefixes_do_not_collide() {
        // VERSION= must not satisfy VERSION_ID= and ID_LIKE= must not
        // satisfy ID=.
        let content = "ID_LIKE=debian\nVERSION=12 (bookworm)\nID=debian\nVERSION_ID=\"12\"\n";
        let d = parse_os_release(content).unwrap();
        assert_eq!(d.id, "debian");
        assert_eq!(d.version_id, "12");
    }
}
