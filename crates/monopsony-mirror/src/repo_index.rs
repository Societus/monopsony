//! Repo index parsing — extract package metadata from distro repository databases.
//!
//! Supported formats:
//! - Pacman `.db` (gzip-compressed tar with per-package `desc` files)
//! - Debian `Packages.gz` (gzip-compressed plain text key-value pairs)
//! - Alpine `APKINDEX` text (verified bundles decoded by `integrity.rs`)
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

    let Ok(entries) = archive.entries() else {
        return packages;
    };

    for entry_result in entries {
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
///
/// `suite` is the Debian/Ubuntu suite (default "stable") or Alpine branch
/// (default "latest-stable"); it is ignored for pacman-style distros.
pub fn repo_index_urls(distro: &str, arch: &str, suite: &str) -> Vec<(&'static str, String)> {
    match distro {
        "archlinux" => vec![
            ("core", format!("core/os/{}/core.db", arch)),
            ("extra", format!("extra/os/{}/extra.db", arch)),
        ],
        "debian" => vec![(
            "main",
            format!("dists/{}/main/binary-{}/Packages.gz", suite, arch),
        )],
        "ubuntu" => vec![(
            "main",
            format!("dists/{}/main/binary-{}/Packages.gz", suite, arch),
        )],
        "alpine" => vec![("main", format!("{}/main/{}/APKINDEX.tar.gz", suite, arch))],
        _ => vec![],
    }
}

/// Fill in the download_url and filename fields for each package based on upstream base URL.
///
/// For pacman:
/// - `download_url` uses the original filename (e.g., https://mirror/core/os/x86_64/bash-5.3.15-1-x86_64.pkg.tar.zst)
/// - `filename` is set to the repo-relative path for local storage (e.g., core/os/x86_64/bash-5.3.15-1-x86_64.pkg.tar.zst)
///
/// For alpine, APKINDEX carries no filename field — apk clients construct
/// `{P}-{V}.apk` inside `{suite}/{repo}/{arch}/`; the same construction is
/// used here (filename = repo-local storage path, minus the suite segment).
pub fn resolve_download_urls(
    packages: &mut [UpstreamPackage],
    distro: &str,
    repo: &str,
    upstream_base: &str,
    arch: &str,
    suite: &str,
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
        } else if distro == "alpine" {
            let apk_name = format!("{}-{}.apk", pkg.name, pkg.version);
            pkg.filename = format!("{}/{}/{}", repo, arch, apk_name);
            pkg.download_url = format!("{}/{}/{}/{}/{}", base, suite, repo, arch, apk_name);
        } else {
            // Debian: filename already contains pool/main/... path
            pkg.download_url = format!("{}/{}", base, pkg.filename);
        }
    }
}

/// Parse APKINDEX text (the verified payload extracted by integrity.rs):
/// entries separated by blank lines, `key:value` fields per line. `P:` name,
/// `V:` version, `A:` arch, `S:` size, `C:` checksum (sha256 when the value
/// decodes to exactly 32 bytes — same convention as the client's
/// index_parse.rs).
pub fn parse_apkindex(text: &str) -> Vec<UpstreamPackage> {
    let mut packages = Vec::new();
    for block in text.split("\n\n") {
        let mut name: Option<String> = None;
        let mut version = String::new();
        let mut arch = String::new();
        let mut size: u64 = 0;
        let mut sha256: Option<String> = None;
        for line in block.lines() {
            let line = line.trim_end_matches('\r');
            if line.is_empty() {
                continue;
            }
            let Some((key, value)) = line.split_once(':') else {
                continue;
            };
            match key {
                "P" => {
                    if name.is_none() {
                        name = Some(value.to_string());
                    }
                }
                "V" => {
                    if version.is_empty() {
                        version = value.to_string();
                    }
                }
                "A" => {
                    if arch.is_empty() {
                        arch = value.to_string();
                    }
                }
                "S" => {
                    if size == 0 {
                        size = value.trim().parse().unwrap_or(0);
                    }
                }
                "C" => {
                    if sha256.is_none() {
                        sha256 = apk_checksum_to_sha256(value);
                    }
                }
                _ => {}
            }
        }
        if let Some(name) = name {
            if name.is_empty() {
                continue;
            }
            packages.push(UpstreamPackage {
                name,
                version,
                architecture: arch,
                // Filled in by resolve_download_urls (apk constructs {P}-{V}.apk)
                filename: String::new(),
                download_url: String::new(),
                size_bytes: size,
                sha256,
            });
        }
    }

    debug!("Parsed {} packages from APKINDEX", packages.len());
    packages
}

/// Convert an apk `C:` checksum value to lowercase hex sha256. Only the
/// `c:` prefix (sha256 family) is considered, and only when the remainder
/// base64-decodes to exactly 32 bytes — otherwise None (do not guess).
/// Mirrors the client crate's index_parse.rs decoding.
fn apk_checksum_to_sha256(value: &str) -> Option<String> {
    let encoded = value.strip_prefix("c:")?;
    let decoded = base64_decode_lenient(encoded)?;
    if decoded.len() == 32 {
        Some(to_hex(&decoded))
    } else {
        None
    }
}

/// Base64 decode tolerating both padded and unpadded encodings.
fn base64_decode_lenient(encoded: &str) -> Option<Vec<u8>> {
    use base64::Engine;
    let engine = base64::engine::general_purpose::STANDARD;
    engine.decode(encoded).ok().or_else(|| {
        base64::engine::general_purpose::STANDARD_NO_PAD
            .decode(encoded.trim_end_matches('='))
            .ok()
    })
}

/// Lowercase hex encoding.
fn to_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{:02x}", byte));
    }
    out
}
