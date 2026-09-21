//! Arch packaging-git (.SRCINFO / PKGBUILD) source adapter (Sprint 21 slice 21-3b).
//!
//! Parses `.SRCINFO` key=value text into [`ResolvedMapping`] values.
//! Offline-testable: no network in parsing.

use crate::sourcedb::sources::ResolvedMapping;

/// Adapter for the Arch Linux packaging git tree
/// (<https://gitlab.archlinux.org/archlinux/packaging>).
pub struct ArchSource {
    base_url: String,
}

impl ArchSource {
    pub fn new() -> Self {
        Self {
            base_url: "https://gitlab.archlinux.org/archlinux/packaging".into(),
        }
    }

    /// Override the packaging tree base URL (e.g. for tests or mirrors).
    pub fn set_base_url(&mut self, url: String) {
        self.base_url = url;
    }

    /// Build the raw URL for a package's `.SRCINFO` in the packaging tree.
    pub fn srcinfo_url(&self, repo: &str, pkgbase: &str) -> String {
        format!(
            "{}/{}/{}/raw/branch/main/.SRCINFO",
            self.base_url, repo, pkgbase
        )
    }

    /// Parse `.SRCINFO` text. `pkgbase=` becomes `origin`, `url=` becomes
    /// `homepage`. Sections (`[package]` etc.) do not reset these keys; the
    /// first `pkgbase=`/`url=` encountered wins. Comment lines (leading `#`,
    /// possibly after whitespace) and blank lines are skipped.
    pub fn parse_srcinfo(&self, body: &str) -> Result<ResolvedMapping, String> {
        let mut origin: Option<String> = None;
        let mut homepage: Option<String> = None;

        for raw in body.lines() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if line.starts_with('[') {
                // Section header ([pkgbase], [package], ...) — key lookup
                // below is unaffected; first values still win.
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let key = key.trim();
            let value = value.trim();
            if value.is_empty() {
                continue;
            }
            match key {
                "pkgbase" => {
                    if origin.is_none() {
                        origin = Some(value.to_string());
                    }
                }
                "url" => {
                    if homepage.is_none() {
                        homepage = Some(value.to_string());
                    }
                }
                _ => {}
            }
        }

        match origin {
            Some(origin) => Ok(ResolvedMapping {
                source: "arch:packaging".into(),
                origin: Some(origin),
                homepage,
            }),
            None => Err("no pkgbase= key found in .SRCINFO".into()),
        }
    }
}

impl Default for ArchSource {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOLDEN_SRCINFO: &str = "\
#
# example comment at top
pkgbase = ripgrep
pkgdesc = A tool that combines the usability of The Silver Searcher
url = https://github.com/BurntSushi/ripgrep
arch = x86_64
license = MIT
makedepends = cargo

[package]
pkgname = ripgrep
url = https://evil.example.com/override-should-lose
";

    #[test]
    fn srcinfo_golden_first_values_win() {
        let m = ArchSource::new().parse_srcinfo(GOLDEN_SRCINFO).unwrap();
        assert_eq!(m.source, "arch:packaging");
        assert_eq!(m.origin.as_deref(), Some("ripgrep"));
        assert_eq!(
            m.homepage.as_deref(),
            Some("https://github.com/BurntSushi/ripgrep")
        );
    }

    #[test]
    fn srcinfo_missing_pkgbase_is_err() {
        let body = "url = https://example.com/no-base\n";
        assert!(ArchSource::new().parse_srcinfo(body).is_err());
    }

    #[test]
    fn srcinfo_empty_body_is_err() {
        assert!(ArchSource::new().parse_srcinfo("").is_err());
    }

    #[test]
    fn srcinfo_comments_and_blank_lines_skipped() {
        let body = "# a full-line comment\n\n   # indented comment\npkgbase = ripgrep\n\n";
        let m = ArchSource::new().parse_srcinfo(body).unwrap();
        assert_eq!(m.origin.as_deref(), Some("ripgrep"));
        assert_eq!(m.homepage, None);
    }

    #[test]
    fn srcinfo_url_golden() {
        assert_eq!(
            ArchSource::new().srcinfo_url("extra", "ripgrep"),
            "https://gitlab.archlinux.org/archlinux/packaging/extra/ripgrep/raw/branch/main/.SRCINFO"
        );
    }

    #[test]
    fn srcinfo_url_respects_base_override() {
        let mut s = ArchSource::new();
        s.set_base_url("http://localhost:9/packaging".into());
        assert_eq!(
            s.srcinfo_url("core", "bash"),
            "http://localhost:9/packaging/core/bash/raw/branch/main/.SRCINFO"
        );
    }
}
