//! Repository index parsers (Sprint 20 Phase 4, slice 3).
//!
//! Parses pacman `.db`/`.files` archives (tar of `%KEY%` descriptor files)
//! and Alpine `APKINDEX` archives (tar of a single `APKINDEX` text file),
//! extracting `(name, version, sha256)` triples for the downgrade ledger.
//!
//! Compression is detected by magic bytes (zstd `28 B5 2F FD`, gzip
//! `1F 8B`, else raw) — never by file extension. All decoders are sync;
//! call sites should wrap them in `spawn_blocking`. Hostile input yields
//! a structured [`IndexError`], never a panic, and resource caps bound
//! decompressed size, tar entry count, and per-descriptor size.

use std::fmt;
use std::io::Read;

/// A single package record extracted from a repository index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexEntry {
    pub name: String,
    /// Raw version string exactly as the index declared it.
    pub version: String,
    /// Lowercase hex SHA-256 when the index declared one.
    pub sha256: Option<String>,
}

/// The parsed index, tagged by detected repository format.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParsedIndex {
    Pacman { entries: Vec<IndexEntry> },
    Apk { entries: Vec<IndexEntry> },
}

/// Structured parse/decode failure (garbage input, corrupt archive, or a
/// resource cap exceeded). Never panics on hostile input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexError(pub String);

impl fmt::Display for IndexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "index parse error: {}", self.0)
    }
}

impl std::error::Error for IndexError {}

/// Cap on total decompressed tar bytes (tar-bomb guard).
const MAX_DECOMPRESSED_BYTES: usize = 256 * 1024 * 1024;
/// Cap on tar entry count (tar-bomb guard).
const MAX_TAR_ENTRIES: usize = 100_000;
/// Cap on a single pacman `desc` file's size.
const MAX_DESC_BYTES: usize = 1024 * 1024;
/// Cap on a single `APKINDEX` file's size.
const MAX_APKINDEX_BYTES: usize = 16 * 1024 * 1024;

/// Decode + parse a repository index. Format and compression are detected
/// from content, not the URL: zstd/gzip magic selects the decompressor,
/// then the tar contents select pacman (descriptor directories) vs apk
/// (a single `APKINDEX` file).
pub fn parse_index(bytes: &[u8]) -> Result<ParsedIndex, IndexError> {
    let tar_bytes = decode_container(bytes)?;
    parse_tar(&tar_bytes)
}

/// Decompress per magic bytes; raw bytes pass through untouched.
fn decode_container(bytes: &[u8]) -> Result<Vec<u8>, IndexError> {
    if bytes.len() >= 4 && bytes[..4] == [0x28, 0xB5, 0x2F, 0xFD] {
        let mut out = Vec::new();
        let decoder = zstd::stream::Decoder::new(bytes)
            .map_err(|e| IndexError(format!("zstd decode failed: {e}")))?;
        read_limited(decoder, &mut out, MAX_DECOMPRESSED_BYTES, "zstd")?;
        return Ok(out);
    }
    if bytes.len() >= 2 && bytes[..2] == [0x1F, 0x8B] {
        let mut out = Vec::new();
        let decoder = flate2::read::GzDecoder::new(bytes);
        read_limited(decoder, &mut out, MAX_DECOMPRESSED_BYTES, "gzip")?;
        return Ok(out);
    }
    if bytes.len() > MAX_DECOMPRESSED_BYTES {
        return Err(IndexError(format!(
            "raw container exceeds {} byte cap",
            MAX_DECOMPRESSED_BYTES
        )));
    }
    Ok(bytes.to_vec())
}

/// Read `reader` to end, enforcing `cap` on total bytes written.
fn read_limited(
    mut reader: impl Read,
    out: &mut Vec<u8>,
    cap: usize,
    what: &str,
) -> Result<(), IndexError> {
    let mut buf = [0u8; 64 * 1024];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => return Ok(()),
            Ok(n) => {
                if out.len() + n > cap {
                    return Err(IndexError(format!(
                        "{what} stream exceeds {} byte decompression cap",
                        cap
                    )));
                }
                out.extend_from_slice(&buf[..n]);
            }
            Err(e) => return Err(IndexError(format!("{what} decode failed: {e}"))),
        }
    }
}

/// Walk the tar once, collecting pacman `desc` payloads and any `APKINDEX`
/// file; whichever is present decides the format. Caps apply to entry
/// count and per-file size.
fn parse_tar(tar_bytes: &[u8]) -> Result<ParsedIndex, IndexError> {
    let mut archive = tar::Archive::new(std::io::Cursor::new(tar_bytes));
    let mut descs: Vec<Vec<u8>> = Vec::new();
    let mut apkindex: Option<Vec<u8>> = None;
    let mut entries = 0usize;

    let it = archive
        .entries()
        .map_err(|e| IndexError(format!("tar read failed: {e}")))?;
    for entry in it {
        entries += 1;
        if entries > MAX_TAR_ENTRIES {
            return Err(IndexError(format!(
                "tar exceeds {} entry cap",
                MAX_TAR_ENTRIES
            )));
        }
        let mut entry = entry.map_err(|e| IndexError(format!("tar entry read failed: {e}")))?;
        let path = entry
            .path()
            .map_err(|e| IndexError(format!("tar entry path invalid: {e}")))?
            .to_path_buf();
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let file_name = path
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        if file_name == "APKINDEX" {
            if apkindex.is_some() {
                return Err(IndexError("multiple APKINDEX files in tar".to_string()));
            }
            let mut data = Vec::new();
            read_limited(&mut entry, &mut data, MAX_APKINDEX_BYTES, "APKINDEX")?;
            apkindex = Some(data);
        } else if file_name == "desc" {
            let mut data = Vec::new();
            read_limited(&mut entry, &mut data, MAX_DESC_BYTES, "pacman desc")?;
            descs.push(data);
        }
    }

    if let Some(data) = apkindex {
        let text = String::from_utf8(data)
            .map_err(|_| IndexError("APKINDEX is not valid UTF-8".to_string()))?;
        let entries = parse_apkindex_text(&text)?;
        return Ok(ParsedIndex::Apk { entries });
    }
    if !descs.is_empty() {
        let entries = descs
            .iter()
            .map(|d| parse_desc_bytes(d))
            .collect::<Result<Vec<_>, _>>()?;
        return Ok(ParsedIndex::Pacman {
            entries: entries.into_iter().flatten().collect(),
        });
    }
    Err(IndexError(
        "tar contains neither pacman desc files nor an APKINDEX".to_string(),
    ))
}

/// Parse one pacman `desc` payload (`%KEY%\nvalue` blocks). Returns the
/// entries with a `%NAME%`; a desc without `%NAME%` yields no entry.
fn parse_desc_bytes(data: &[u8]) -> Result<Vec<IndexEntry>, IndexError> {
    let text = String::from_utf8(data.to_vec())
        .map_err(|_| IndexError("pacman desc is not valid UTF-8".to_string()))?;
    let mut name: Option<String> = None;
    let mut version = String::new();
    let mut sha256: Option<String> = None;
    let mut current: Option<String> = None;

    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        if line.len() >= 2 && line.starts_with('%') && line.ends_with('%') {
            current = Some(line[1..line.len() - 1].to_ascii_uppercase());
            continue;
        }
        if line.is_empty() {
            continue;
        }
        match current.as_deref() {
            Some("NAME") => {
                if name.is_none() {
                    name = Some(line.to_string());
                }
            }
            Some("VERSION") => {
                if version.is_empty() {
                    version = line.to_string();
                }
            }
            Some("SHA256") => {
                if sha256.is_none() && !line.is_empty() {
                    sha256 = Some(line.to_ascii_lowercase());
                }
            }
            _ => {}
        }
    }

    match name {
        Some(name) => Ok(vec![IndexEntry {
            name,
            version,
            sha256,
        }]),
        None => Ok(Vec::new()),
    }
}

/// Parse APKINDEX text: entries separated by blank lines, `key:value`
/// fields per line. `P:` name, `V:` version, `C:` checksum (sha256 when
/// the value decodes to exactly 32 bytes).
fn parse_apkindex_text(text: &str) -> Result<Vec<IndexEntry>, IndexError> {
    let mut entries = Vec::new();
    for block in text.split("\n\n") {
        let mut name: Option<String> = None;
        let mut version = String::new();
        let mut sha256: Option<String> = None;
        for line in block.lines() {
            let line = line.trim_end_matches('\r');
            if line.is_empty() {
                continue;
            }
            let Some((key, value)) = line.split_once(':') else {
                return Err(IndexError(format!(
                    "APKINDEX line is not key:value: {line:?}"
                )));
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
                "C" => {
                    if sha256.is_none() {
                        sha256 = apk_checksum_to_sha256(value);
                    }
                }
                _ => {}
            }
        }
        if let Some(name) = name {
            entries.push(IndexEntry {
                name,
                version,
                sha256,
            });
        }
    }
    Ok(entries)
}

/// Convert an apk `C:` checksum value to lowercase hex sha256. Only the
/// `c:` prefix (sha256 family) is considered, and only when the remainder
/// base64-decodes to exactly 32 bytes — otherwise `None` (do not guess).
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
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// True when a URL path looks like a repository index this module can
/// parse: pacman `.db`/`.db.tar.gz` databases or an Alpine `APKINDEX`
/// (plain or `.tar.gz`). Used to decide whether to buffer-and-parse a
/// proxied response body.
pub fn looks_like_index(url_path: &str) -> bool {
    if url_path.ends_with(".db") || url_path.ends_with(".db.tar.gz") {
        return true;
    }
    match url_path.rsplit('/').next() {
        Some(last) => last == "APKINDEX" || last.starts_with("APKINDEX."),
        None => false,
    }
}

/// Repo identity key: `scheme://host` plus the directory portion of the
/// path (last segment stripped). `canonical_url` must already have
/// `normalize_url_scheme` applied (https://), matching the pin/cache
/// canonicalization.
pub fn repo_key_from_url(canonical_url: &str) -> String {
    let Some((scheme, rest)) = canonical_url.split_once("://") else {
        return canonical_url.to_string();
    };
    let Some(first_slash) = rest.find('/') else {
        return format!("{scheme}://{}", rest);
    };
    // Strip the last path segment (the index file itself); keep the
    // directory with its trailing slash. Idempotent for URLs that already
    // end in a directory.
    let trimmed = rest.trim_end_matches('/');
    match trimmed.rfind('/') {
        Some(last_slash) => format!("{scheme}://{}", &rest[..last_slash + 1]),
        None => format!("{scheme}://{}/", &rest[..first_slash]),
    }
}

/// Package identity key: `"<manager>:<name>"` (e.g. `pacman:bash`).
/// Single definition site so the format cannot drift.
pub fn package_key_for(manager_kind: &str, name: &str) -> String {
    format!("{manager_kind}:{name}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    use base64::Engine;
    use flate2::write::GzEncoder;
    use tar::Builder;

    /// A pacman-style desc payload with all keys of interest.
    fn desc(name: &str, version: &str, sha256: Option<&str>) -> Vec<u8> {
        let mut text = format!("%NAME%\n{name}\n%VERSION%\n{version}\n%BASE%\n{name}\n");
        if let Some(sha) = sha256 {
            text.push_str(&format!("%SHA256%\n{sha}\n"));
        }
        text.into_bytes()
    }

    /// Build a pacman .db-style tar: `<pkg>-<version>/desc` entries.
    fn pacman_tar_bytes() -> Vec<u8> {
        let mut builder = Builder::new(Vec::new());
        let bash_desc = desc(
            "bash",
            "5.2.037-1",
            Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
        );
        builder
            .append_data(
                &mut tar_header("bash-5.2.037-1/desc", bash_desc.len() as u64),
                "bash-5.2.037-1/desc",
                std::io::Cursor::new(bash_desc),
            )
            .unwrap();
        let coreutils_desc = desc("coreutils", "9.5-2", None);
        builder
            .append_data(
                &mut tar_header("coreutils-9.5-2/desc", coreutils_desc.len() as u64),
                "coreutils-9.5-2/desc",
                std::io::Cursor::new(coreutils_desc),
            )
            .unwrap();
        builder.into_inner().unwrap()
    }

    fn tar_header(path: &str, size: u64) -> tar::Header {
        let mut header = tar::Header::new_gnu();
        header.set_size(size);
        header.set_mode(0o644);
        let _ = path;
        header
    }

    fn gzip(bytes: &[u8]) -> Vec<u8> {
        let mut encoder = GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(bytes).unwrap();
        encoder.finish().unwrap()
    }

    fn zstd_compress(bytes: &[u8]) -> Vec<u8> {
        zstd::stream::encode_all(std::io::Cursor::new(bytes), 3).unwrap()
    }

    /// APKINDEX text with one full entry, one entry whose checksum does
    /// not decode to 32 bytes, and one entry with no checksum at all.
    fn apkindex_text() -> String {
        let good = base64::engine::general_purpose::STANDARD_NO_PAD.encode([7u8; 32]);
        format!(
            "P:bash\nV:5.2.037-r0\nA:x86_64\nC:c:{good}\nT:shell\n\n\
             P:busybox\nV:1.36.1-r5\nA:x86_64\nC:c:notavalidhash\n\n\
             P:musl\nV:1.2.5-r0\nA:x86_64\n\n"
        )
    }

    fn apk_tar_bytes() -> Vec<u8> {
        let mut builder = Builder::new(Vec::new());
        builder
            .append_data(
                &mut tar_header("APKINDEX", apkindex_text().len() as u64),
                "APKINDEX",
                std::io::Cursor::new(apkindex_text()),
            )
            .unwrap();
        builder.into_inner().unwrap()
    }

    #[test]
    fn test_parse_index_pacman_zstd_extracts_entries_and_sha256() {
        let parsed = parse_index(&zstd_compress(&pacman_tar_bytes())).unwrap();
        let ParsedIndex::Pacman { entries } = parsed else {
            panic!("expected pacman variant");
        };
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, "bash");
        assert_eq!(entries[0].version, "5.2.037-1");
        assert_eq!(
            entries[0].sha256.as_deref(),
            Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
        );
        assert_eq!(entries[1].name, "coreutils");
        assert_eq!(entries[1].sha256, None);
    }

    #[test]
    fn test_parse_index_pacman_gzip_extracts_entries() {
        let parsed = parse_index(&gzip(&pacman_tar_bytes())).unwrap();
        let ParsedIndex::Pacman { entries } = parsed else {
            panic!("expected pacman variant");
        };
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[1].version, "9.5-2");
    }

    #[test]
    fn test_parse_index_raw_tar_named_db_still_parses_by_magic() {
        // Detection is by magic bytes, not extension: a raw (uncompressed)
        // tar must parse as pacman even though it "looks like" a .db.
        let parsed = parse_index(&pacman_tar_bytes()).unwrap();
        let ParsedIndex::Pacman { entries } = parsed else {
            panic!("expected pacman variant");
        };
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, "bash");
    }

    #[test]
    fn test_parse_index_apk_extracts_name_version_checksum() {
        let parsed = parse_index(&gzip(&apk_tar_bytes())).unwrap();
        let ParsedIndex::Apk { entries } = parsed else {
            panic!("expected apk variant");
        };
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].name, "bash");
        assert_eq!(entries[0].version, "5.2.037-r0");
        assert_eq!(
            entries[0].sha256.as_deref(),
            Some(to_hex(&[7u8; 32]).as_str())
        );
        // Checksum that does not decode to 32 bytes → None, not a guess.
        assert_eq!(entries[1].name, "busybox");
        assert_eq!(entries[1].sha256, None);
        assert_eq!(entries[2].name, "musl");
        assert_eq!(entries[2].sha256, None);
    }

    #[test]
    fn test_parse_index_garbage_bytes_is_err_no_panic() {
        let garbage = b"not an index at all, just hostile bytes \xff\xfe\x00garbage";
        assert!(parse_index(garbage).is_err());
        // zstd magic with corrupt payload must also error, not panic.
        let mut corrupt = vec![0x28u8, 0xB5, 0x2F, 0xFD];
        corrupt.extend_from_slice(b"broken payload after real magic");
        assert!(parse_index(&corrupt).is_err());
    }

    #[test]
    fn test_parse_index_oversized_desc_is_err() {
        let huge = vec![b'x'; MAX_DESC_BYTES + 1];
        let mut builder = Builder::new(Vec::new());
        builder
            .append_data(
                &mut tar_header("bash-1/desc", huge.len() as u64),
                "bash-1/desc",
                std::io::Cursor::new(huge),
            )
            .unwrap();
        let tar_bytes = builder.into_inner().unwrap();
        let err = parse_index(&tar_bytes).unwrap_err();
        assert!(err.0.contains("cap"), "unexpected error: {}", err.0);
    }

    #[test]
    fn test_looks_like_index_contract() {
        assert!(looks_like_index("/repo/os/x86_64/core.db"));
        assert!(looks_like_index("/repo/os/x86_64/core.db.tar.gz"));
        assert!(looks_like_index("/alpine/v3.20/main/x86_64/APKINDEX"));
        assert!(looks_like_index(
            "/alpine/v3.20/main/x86_64/APKINDEX.tar.gz"
        ));
        assert!(!looks_like_index("/repo/os/x86_64/bash-5.2.pkg.tar.zst"));
        assert!(!looks_like_index("/some/random/path"));
    }

    #[test]
    fn test_repo_key_from_url_contract() {
        assert_eq!(
            repo_key_from_url("https://mirror.example.com/repo/os/x86_64/core.db"),
            "https://mirror.example.com/repo/os/x86_64/"
        );
        assert_eq!(
            repo_key_from_url(
                "https://dl-cdn.alpinelinux.org/alpine/v3.20/main/x86_64/APKINDEX.tar.gz"
            ),
            "https://dl-cdn.alpinelinux.org/alpine/v3.20/main/x86_64/"
        );
        assert_eq!(
            repo_key_from_url("https://mirror.example.com/core.db"),
            "https://mirror.example.com/"
        );
        assert_eq!(
            repo_key_from_url("https://mirror.example.com"),
            "https://mirror.example.com"
        );
    }

    #[test]
    fn test_package_key_for_contract() {
        assert_eq!(package_key_for("pacman", "bash"), "pacman:bash");
        assert_eq!(package_key_for("apk", "busybox"), "apk:busybox");
    }
}
