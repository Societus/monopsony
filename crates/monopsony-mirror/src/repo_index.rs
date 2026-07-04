//! Repo index parsing — extract package metadata from distro repository databases.
//!
//! Supported formats:
//! - Pacman `.db` (gzip-compressed tar with per-package `desc` files)
//! - Debian `Packages.gz` (gzip-compressed plain text key-value pairs)
//!
//! Returns `Vec<UpstreamPackage>` for storage in the `upstream_packages` table.

use std::io::Read;

use tracing::debug;

use monopsony_db::db::UpstreamPackage;

/// Parse a pacman `.db` file (gzip-compressed tar) into package metadata.
///
/// Each tar entry directory contains a `desc` file with fields like:
/// `%NAME%`, `%VERSION%`, `%ARCH%`, `%FILENAME%`, `%CSHA256%`, `%SIZE%`
pub fn parse_pacman_db(db_bytes: &[u8]) -> Vec<UpstreamPackage> {
    // Decompress gzip
    let decompressed = {
        let mut decoder = flate2::read::GzDecoder::new(db_bytes);
        let mut buf = Vec::new();
        if decoder.read_to_end(&mut buf).is_err() {
            // Maybe not gzipped — try raw bytes
            db_bytes.to_vec()
        } else {
            buf
        }
    };

    let mut packages = Vec::new();
    let mut archive = tar::Archive::new(&decompressed[..]);

    let Ok(mut entries) = archive.entries() else {
        return packages;
    };

    while let Some(entry_result) = entries.next() {
        let mut entry = match entry_result {
            Ok(e) => e,
            Err(_) => continue,
        };

        let path = match entry.path() {
            Ok(p) => p,
            Err(_) => continue,
        };

        let fname = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if fname != "desc" {
            continue;
        }

        let mut content = String::new();
        if entry.read_to_string(&mut content).is_err() {
            continue;
        }

        if let Some(pkg) = parse_pacman_desc(&content) {
            packages.push(pkg);
        }
    }

    debug!("Parsed {} packages from pacman db", packages.len());
    packages
}

/// Parse a pacman `desc` file content into an UpstreamPackage.
fn parse_pacman_desc(content: &str) -> Option<UpstreamPackage> {
    let mut name = String::new();
    let mut version = String::new();
    let mut arch = String::new();
    let mut filename = String::new();
    let mut sha256: Option<String> = None;
    let mut size: u64 = 0;

    let mut lines = content.lines();
    while let Some(line) = lines.next() {
        match line.trim() {
            "%NAME%" => name = lines.next().unwrap_or("").trim().to_string(),
            "%VERSION%" => version = lines.next().unwrap_or("").trim().to_string(),
            "%ARCH%" => arch = lines.next().unwrap_or("").trim().to_string(),
            "%FILENAME%" => filename = lines.next().unwrap_or("").trim().to_string(),
            "%CSHA256%" | "%SHA256SUM%" => {
                sha256 = lines.next().map(|s| s.trim().to_string());
            }
            "%SIZE%" => {
                if let Some(s) = lines.next() {
                    size = s.trim().parse().unwrap_or(0);
                }
            }
            _ => {}
        }
    }

    if name.is_empty() || filename.is_empty() {
        return None;
    }

    Some(UpstreamPackage {
        name,
        version,
        architecture: arch,
        filename,
        download_url: String::new(), // filled by caller
        size_bytes: size,
        sha256,
    })
}

/// Parse a Debian `Packages.gz` file into package metadata.
pub fn parse_debian_packages(gz_bytes: &[u8]) -> Vec<UpstreamPackage> {
    let content = {
        let mut decoder = flate2::read::GzDecoder::new(gz_bytes);
        let mut buf = String::new();
        if decoder.read_to_string(&mut buf).is_err() {
            return Vec::new();
        }
        buf
    };

    let mut packages = Vec::new();

    for stanza in content.split("\n\n") {
        let stanza = stanza.trim();
        if stanza.is_empty() {
            continue;
        }

        let mut name = String::new();
        let mut version = String::new();
        let mut arch = String::new();
        let mut filename = String::new();
        let mut sha256: Option<String> = None;
        let mut size: u64 = 0;

        for line in stanza.lines() {
            if let Some(v) = line.strip_prefix("Package: ") {
                name = v.trim().to_string();
            } else if let Some(v) = line.strip_prefix("Version: ") {
                version = v.trim().to_string();
            } else if let Some(v) = line.strip_prefix("Architecture: ") {
                arch = v.trim().to_string();
            } else if let Some(v) = line.strip_prefix("Filename: ") {
                filename = v.trim().to_string();
            } else if let Some(v) = line.strip_prefix("SHA256: ") {
                sha256 = Some(v.trim().to_string());
            } else if let Some(v) = line.strip_prefix("Size: ") {
                size = v.trim().parse().unwrap_or(0);
            }
        }

        if !name.is_empty() && !filename.is_empty() {
            packages.push(UpstreamPackage {
                name,
                version,
                architecture: arch,
                filename,
                download_url: String::new(),
                size_bytes: size,
                sha256,
            });
        }
    }

    debug!("Parsed {} packages from Debian Packages.gz", packages.len());
    packages
}

/// Known repo metadata index paths for each distro, relative to the upstream URL.
/// Returns (repo_name, index_path) pairs. These are small files (~5-20MB total).
pub fn repo_index_urls(distro: &str, arch: &str) -> Vec<(&'static str, String)> {
    match distro {
        "archlinux" => vec![
            ("core", format!("core/os/{}/core.db", arch)),
            ("extra", format!("extra/os/{}/extra.db", arch)),
        ],
        "debian" => vec![
            ("main", format!("dists/stable/main/binary-{}/Packages.gz", arch)),
        ],
        "ubuntu" => vec![
            ("main", format!("dists/stable/main/binary-{}/Packages.gz", arch)),
        ],
        _ => vec![],
    }
}

/// Fill in the download_url and filename fields for each package based on upstream base URL.
/// 
/// For pacman:
/// - `download_url` uses the original filename (e.g., https://mirror/core/os/x86_64/bash-5.3.15-1-x86_64.pkg.tar.zst)
/// - `filename` is set to the repo-relative path for local storage (e.g., core/os/x86_64/bash-5.3.15-1-x86_64.pkg.tar.zst)
pub fn resolve_download_urls(
    packages: &mut [UpstreamPackage],
    distro: &str,
    repo: &str,
    upstream_base: &str,
    arch: &str,
) {
    let base = upstream_base.trim_end_matches('/');
    for pkg in packages.iter_mut() {
        if distro == "archlinux" {
            // Save the original filename for URL construction
            let original_filename = pkg.filename.clone();
            // Construct the repo-relative path for local storage
            pkg.filename = format!("{}/os/{}/{}", repo, arch, original_filename);
            // Construct download URL using original filename
            pkg.download_url = format!("{}/{}/os/{}/{}", base, repo, arch, original_filename);
        } else {
            // Debian: filename already contains pool/main/... path
            pkg.download_url = format!("{}/{}", base, pkg.filename);
        }
    }
}