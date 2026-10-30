//! Portage emerge backend implementation (Gentoo Linux)
//!
//! Gentoo builds packages from source via ebuilds. The proxy does NOT cache
//! binary packages — instead it caches source tarballs (distfiles) fetched
//! from Gentoo mirrors. URL patterns match the distfile host URLs.

use super::upgradable::{parse_emerge_pretend, UpgradableReport};
use crate::{PackageManagerBackend, PackageManagerKind, PackageRef, ProxyConfigAction, UrlPattern};
use async_trait::async_trait;
use std::path::PathBuf;

pub struct EmergeBackend;

impl EmergeBackend {
    pub fn new() -> Self {
        EmergeBackend
    }
}

impl Default for EmergeBackend {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl PackageManagerBackend for EmergeBackend {
    fn kind(&self) -> PackageManagerKind {
        PackageManagerKind::Emerge
    }

    fn is_available(&self) -> bool {
        which::which("emerge").is_ok() || which::which("emaint").is_ok()
    }

    async fn list_installed(&self) -> crate::Result<Vec<PackageRef>> {
        // Parse /var/db/pkg/ directory: category/name-version/
        // This is the authoritative installed package database on Gentoo.
        let mut packages = Vec::new();

        let pkg_db = std::path::Path::new("/var/db/pkg");
        if !pkg_db.is_dir() {
            return Err(crate::Error::Io {
                path: pkg_db.to_path_buf(),
                source: std::io::Error::new(std::io::ErrorKind::NotFound, "/var/db/pkg not found"),
            });
        }

        let cat_entries = std::fs::read_dir(pkg_db)?;
        for cat_entry in cat_entries {
            let cat_entry = cat_entry?;
            if !cat_entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let category = match cat_entry.file_name().to_str() {
                Some(s) => s.to_string(),
                None => continue,
            };

            let pkg_entries = std::fs::read_dir(cat_entry.path())?;
            for pkg_entry in pkg_entries {
                let pkg_entry = pkg_entry?;
                if !pkg_entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    continue;
                }
                let dirname = match pkg_entry.file_name().to_str() {
                    Some(s) => s.to_string(),
                    None => continue,
                };

                // dirname is name-version, split at last hyphen before version
                if let Some((name, version)) = split_gentoo_pv(&dirname) {
                    packages.push(PackageRef {
                        name: format!("{}/{}", category, name),
                        version: Some(version.to_string()),
                        architecture: None,
                        manager: PackageManagerKind::Emerge,
                        installed_at: 0,
                    });
                }
            }
        }

        Ok(packages)
    }

    /// Best-effort (grounding D4): a pretend world-upgrade run; Gentoo tags
    /// no security channel in this listing, so all counts land in "other".
    async fn list_upgradable(&self) -> crate::Result<UpgradableReport> {
        let output = tokio::process::Command::new("emerge")
            .args(["--pretend", "--update", "--newuse", "--deep", "@world"])
            .output()
            .await
            .map_err(|_| crate::Error::CommandFailed {
                command: "emerge --pretend --update --newuse --deep @world".to_string(),
                exit_code: None,
            })?;

        if !output.status.success() {
            return Err(crate::Error::CommandFailed {
                command: "emerge --pretend --update --newuse --deep @world".to_string(),
                exit_code: output.status.code(),
            });
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let other = parse_emerge_pretend(&stdout);
        Ok(UpgradableReport {
            security: 0,
            other,
            supported: true,
            note: "emerge pretend-run exposes no security channel; all counts are other"
                .to_string(),
        })
    }

    fn upstream_url_patterns(&self) -> Vec<UrlPattern> {
        vec![
            // Distfiles — source tarballs fetched during emerge
            UrlPattern::new(
                r"https?://[^/]*distfiles\.gentoo\.org/.*",
                100,
                "Gentoo distfiles (source tarballs)",
            )
            .expect("valid regex"),
            UrlPattern::new(
                r"https?://[^/]*gentoo\.osuosl\.org/.*",
                100,
                "Gentoo OSUOSL mirror (distfiles)",
            )
            .expect("valid regex"),
            UrlPattern::new(
                r"https?://[^/]*ftp\.ossa\-tools\.org/gentoo/.*",
                95,
                "Gentoo OSSA mirror",
            )
            .expect("valid regex"),
            // Portage tree snapshots (for emerge --sync)
            UrlPattern::new(
                r"https?://[^/]*gentoo\.org/snapshots/.*\.tar\..*",
                80,
                "Gentoo portage snapshots",
            )
            .expect("valid regex"),
            // General distfile mirrors
            UrlPattern::new(
                r"https?://[^/]*[a-z]+\.distfiles\.gentoo\.org/.*",
                90,
                "Gentoo distfile mirrors",
            )
            .expect("valid regex"),
        ]
    }

    fn url_to_cache_path(&self, url: &str) -> Option<String> {
        let url = url::Url::parse(url).ok()?;
        let path = url.path().trim_start_matches('/');
        Some(format!("emerge/{}", path))
    }

    fn proxy_config(&self, proxy_addr: &str) -> ProxyConfigAction {
        // Portage reads proxy settings from make.conf via http_proxy/https_proxy
        // variables, or from the FETCHCOMMAND/GENTOO_MIRRORS settings.
        // The simplest approach: write to make.conf which Portage sources.
        ProxyConfigAction::WriteFile {
            path: PathBuf::from("/etc/portage/make.conf"),
            content: format!(
                r#"# Added by Monopsony proxy
http_proxy="http://{}"
https_proxy="http://{}"
RESUMECOMMAND="wget -c -t 3 --no-check-certificate \${{URI}} -O \${{DISTDIR}}/\${{FILE}} --proxy=on -e use_proxy=yes -e http_proxy=http://{}"
FETCHCOMMAND="wget -t 3 --no-check-certificate \${{URI}} -O \${{DISTDIR}}/\${{FILE}} --proxy=on -e use_proxy=yes -e http_proxy=http://{}"
"#,
                proxy_addr, proxy_addr, proxy_addr, proxy_addr
            ),
        }
    }

    fn remove_proxy_config(&self) -> ProxyConfigAction {
        ProxyConfigAction::RemoveFile {
            path: PathBuf::from("/etc/portage/make.conf"),
        }
    }
}

/// Split a Gentoo package directory name `name-version` into (name, version).
///
/// Gentoo package versions (PV) can be complex: 1.2.3, 1.2.3-r1, 9999, etc.
/// The name portion can contain hyphens (e.g. `haskell-platform`).
/// The version always starts with a digit. We look for the last hyphen
/// before a digit to find the boundary.
fn split_gentoo_pv(dir: &str) -> Option<(&str, &str)> {
    let bytes = dir.as_bytes();
    for i in (0..dir.len().saturating_sub(1)).rev() {
        if bytes[i] == b'-' && bytes[i + 1].is_ascii_digit() {
            return Some((&dir[..i], &dir[i + 1..]));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_split_gentoo_pv_simple() {
        let (name, ver) = split_gentoo_pv("bash-5.2_p15").unwrap();
        assert_eq!(name, "bash");
        assert_eq!(ver, "5.2_p15");
    }

    #[test]
    fn test_split_gentoo_pv_hyphenated_name() {
        let (name, ver) = split_gentoo_pv("haskell-platform-9999").unwrap();
        assert_eq!(name, "haskell-platform");
        assert_eq!(ver, "9999");
    }

    #[test]
    fn test_split_gentoo_pv_revision() {
        let (name, ver) = split_gentoo_pv("vim-9.0.0-r1").unwrap();
        assert_eq!(name, "vim");
        assert_eq!(ver, "9.0.0-r1");
    }

    #[test]
    fn test_url_patterns_match() {
        let backend = EmergeBackend::new();
        let patterns = backend.upstream_url_patterns();

        // Distfile
        assert!(patterns
            .iter()
            .any(|p| p.matches("https://distfiles.gentoo.org/distfiles/bash-5.2.tar.gz")));
        // OSUOSL mirror
        assert!(patterns
            .iter()
            .any(|p| p.matches("https://gentoo.osuosl.org/distfiles/bash-5.2.tar.gz")));
        // Portage snapshot
        assert!(patterns
            .iter()
            .any(|p| p.matches("https://gentoo.org/snapshots/portage-latest.tar.xz")));
        // Non-matching
        assert!(!patterns
            .iter()
            .any(|p| p.matches("https://archlinux.org/packages/x86_64/bash.tar.zst")));
    }

    #[test]
    fn test_url_to_cache_path() {
        let backend = EmergeBackend::new();
        let path =
            backend.url_to_cache_path("https://distfiles.gentoo.org/distfiles/bash-5.2.tar.gz");
        assert_eq!(path.as_deref(), Some("emerge/distfiles/bash-5.2.tar.gz"));
    }

    #[test]
    fn test_url_to_cache_path_invalid() {
        let backend = EmergeBackend::new();
        assert!(backend.url_to_cache_path("not a url").is_none());
    }

    #[test]
    fn test_proxy_config() {
        let backend = EmergeBackend::new();
        let action = backend.proxy_config("10.10.20.58:3128");
        match action {
            ProxyConfigAction::WriteFile { path, content } => {
                assert_eq!(path, PathBuf::from("/etc/portage/make.conf"));
                assert!(content.contains("http_proxy=\"http://10.10.20.58:3128\""));
                assert!(content.contains("FETCHCOMMAND="));
            }
            _ => panic!("expected WriteFile action"),
        }
    }

    #[test]
    fn test_kind_and_availability() {
        let backend = EmergeBackend::new();
        assert_eq!(backend.kind(), PackageManagerKind::Emerge);
    }
}
