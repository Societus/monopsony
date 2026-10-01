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
    Pacman {
        entries: Vec<IndexEntry>,
    },
    Apk {
        entries: Vec<IndexEntry>,
    },
    /// dnf/rocky `primary.xml(.gz)` (slice 5).
    Dnf {
        entries: Vec<IndexEntry>,
    },
    /// apt `InRelease` / `Release` (slice 5). `valid_until` is the UTC
    /// epoch of the `Valid-Until:` field when parseable.
    Apt {
        entries: Vec<IndexEntry>,
        valid_until: Option<i64>,
    },
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
    let decoded = decode_container(bytes)?;
    match sniff_text_format(&decoded) {
        Some("apt") => return parse_apt_text(&decoded),
        Some("dnf") => return parse_primary_xml(&decoded),
        _ => {}
    }
    parse_tar(&decoded)
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
        // Multi-member gzip: apk-tools concatenates gzip streams in
        // APKINDEX.tar.gz (signature entries in the first member, the index
        // in later ones). Plain GzDecoder stops after member 1 — seen live
        // on the bench as a truncated-tar parse error on real archives.
        let mut decoder = flate2::read::MultiGzDecoder::new(bytes);
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

/// Cap on a single decompressed dnf `primary.xml` body.
const MAX_XML_BYTES: usize = 256 * 1024 * 1024;
/// Cap on the number of dnf `<package>` records scanned.
const MAX_XML_PACKAGES: usize = 100_000;
/// Cap on an apt `InRelease`/`Release` body.
const MAX_APT_BYTES: usize = 64 * 1024 * 1024;

/// Sniff the DECODED bytes (post zstd/gzip) for a text format: apt
/// clearsigned/Release text or dnf `primary.xml`. `None` = fall through to
/// the tar path (pacman/apk). Detection is by content, never extension.
fn sniff_text_format(bytes: &[u8]) -> Option<&'static str> {
    let head = &bytes[..bytes.len().min(4096)];
    let trimmed = String::from_utf8_lossy(head).trim_start().to_string();
    if trimmed.starts_with("-----BEGIN PGP SIGNED MESSAGE-----") {
        return Some("apt");
    }
    // First 4 KB carry both a Valid-Until field and a SHA256 block header.
    // `SHA256:` is a FIELD line — column 0, matching parse_apt_text's rule
    // and real Debian Release files (only hash continuations are indented).
    let head_str = String::from_utf8_lossy(head);
    if head_str.contains("Valid-Until:") && head_str.contains("\nSHA256:") {
        return Some("apt");
    }
    if trimmed.starts_with('<') {
        let sniff = String::from_utf8_lossy(&bytes[..bytes.len().min(512)]);
        if sniff.contains("<metadata") {
            return Some("dnf");
        }
    }
    None
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

/// Parse a dnf/rocky `primary.xml` body via hand-rolled tag scanning (no
/// quick-xml). Each `<package ...>` record contributes at most one entry.
/// Garbage XML (unterminated tags, non-UTF-8) → `IndexError`, never panic.
fn parse_primary_xml(bytes: &[u8]) -> Result<ParsedIndex, IndexError> {
    if bytes.len() > MAX_XML_BYTES {
        return Err(IndexError(format!(
            "primary.xml exceeds {} byte cap",
            MAX_XML_BYTES
        )));
    }
    let text = String::from_utf8(bytes.to_vec())
        .map_err(|_| IndexError("primary.xml is not valid UTF-8".to_string()))?;

    let mut entries = Vec::new();
    for record in split_package_records(&text) {
        if entries.len() >= MAX_XML_PACKAGES {
            return Err(IndexError(format!(
                "primary.xml exceeds {} package cap",
                MAX_XML_PACKAGES
            )));
        }
        let Some(name) = first_tag_text(record, "name") else {
            continue;
        };
        let version = xml_first_tag_attrs(record, "version")
            .map(|attrs| dnf_version_string(&attrs))
            .unwrap_or_default();
        let sha256 = first_checksum_sha256(record);
        entries.push(IndexEntry {
            name,
            version,
            sha256,
        });
    }
    Ok(ParsedIndex::Dnf { entries })
}

/// Split the XML text on `<package ` / `<package>` open tags; each slice is
/// one record's content (records cannot nest). `<packages>` (the container)
/// does not match because the tag must be followed by whitespace or `>`.
fn split_package_records(text: &str) -> Vec<&str> {
    let find_open = |from: &str| -> Option<usize> {
        let mut search = from;
        let mut offset = 0usize;
        while let Some(pos) = search.find("<package") {
            let after = &search[pos + "<package".len()..];
            match after.bytes().next() {
                Some(b' ') | Some(b'>') | Some(b'\t') | Some(b'\n') | Some(b'\r') => {
                    return Some(offset + pos);
                }
                _ => {
                    offset += pos + "<package".len();
                    search = after;
                }
            }
        }
        None
    };

    let mut records = Vec::new();
    let mut rest = text;
    while let Some(pos) = find_open(rest) {
        rest = &rest[pos + "<package".len()..];
        match find_open(rest) {
            Some(next) => {
                records.push(&rest[..next]);
                rest = &rest[next..];
            }
            None => {
                records.push(rest);
                break;
            }
        }
    }
    records
}

/// Text of the first `<tag>...</tag>` element, or None.
fn first_tag_text(record: &str, tag: &str) -> Option<String> {
    let open = format!("<{}>", tag);
    let start = record.find(&open)? + open.len();
    let rest = &record[start..];
    let close = format!("</{}>", tag);
    let end = rest.find(&close)?;
    Some(xml_unescape(&rest[..end]))
}

/// Attributes of the first `<tag ...>` (self-closing or not) element as
/// (key, value) pairs, or None when the tag never opens.
fn xml_first_tag_attrs(record: &str, tag: &str) -> Option<Vec<(String, String)>> {
    let open = format!("<{}", tag);
    let start = record.find(&open)? + open.len();
    let rest = &record[start..];
    // End of the open tag: first '>' (self-closing `/>' included).
    let end = rest.find('>')?;
    Some(parse_xml_attrs(&rest[..end]))
}

/// Parse `k="v"` / `k='v'` attribute pairs from tag-attribute text.
fn parse_xml_attrs(s: &str) -> Vec<(String, String)> {
    let mut attrs = Vec::new();
    let mut rest = s.trim_start();
    while let Some(eq) = rest.find('=') {
        let key = rest[..eq].trim().rsplit(' ').next().unwrap_or("").trim();
        let after = rest[eq + 1..].trim_start();
        let quote = match after.bytes().next() {
            Some(b'"') | Some(b'\'') => after.as_bytes()[0] as char,
            _ => break,
        };
        let value = match after[1..].find(quote) {
            Some(end) => &after[1..1 + end],
            None => break, // unterminated attribute — do not guess
        };
        attrs.push((key.to_string(), xml_unescape(value)));
        rest = &after[1 + value.len() + 1..];
    }
    attrs
}

/// Version string from a dnf `<version epoch ver rel/>` attribute set:
/// `"epoch:ver-rel"`, dropping the epoch prefix when absent or "0".
fn dnf_version_string(attrs: &[(String, String)]) -> String {
    let get = |k: &str| attrs.iter().find(|(a, _)| a == k).map(|(_, v)| v.clone());
    let ver = get("ver").unwrap_or_default();
    let rel = get("rel").unwrap_or_default();
    let epoch = get("epoch");
    match epoch.as_deref() {
        Some(e) if !e.is_empty() && e != "0" => format!("{}:{}-{}", e, ver, rel),
        _ => format!("{}-{}", ver, rel),
    }
}

/// Text of the first `<checksum type="sha256">HEX</checksum>` element,
/// lowercase hex; other checksum types (sha/md5) are skipped.
fn first_checksum_sha256(record: &str) -> Option<String> {
    let mut rest = record;
    loop {
        let open_pos = rest.find("<checksum")?;
        let after_open = &rest[open_pos + "<checksum".len()..];
        let tag_end = after_open.find('>')?;
        let attrs = parse_xml_attrs(&after_open[..tag_end]);
        let body_start = tag_end + 1;
        let body_and_tail = &after_open[body_start..];
        let close_pos = body_and_tail.find("</checksum>")?;
        let body = &body_and_tail[..close_pos];
        if attrs
            .iter()
            .any(|(k, v)| k == "type" && v.eq_ignore_ascii_case("sha256"))
        {
            let hex = body.trim();
            if hex.bytes().all(|b| b.is_ascii_hexdigit()) && !hex.is_empty() {
                return Some(hex.to_ascii_lowercase());
            }
            return None;
        }
        rest = &body_and_tail[close_pos + "</checksum>".len()..];
    }
}

/// Minimal XML entity unescape for text/attribute values.
fn xml_unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// Parse an apt `InRelease` (clearsigned) or plain `Release` body. Both
/// share the same cleartext fields and hash blocks; the PGP armor header
/// (when present) is skipped. Entries come ONLY from `SHA256:`-block lines
/// whose path ends `.deb`; ambiguous basenames are skipped, never guessed.
fn parse_apt_text(bytes: &[u8]) -> Result<ParsedIndex, IndexError> {
    if bytes.len() > MAX_APT_BYTES {
        return Err(IndexError(format!(
            "InRelease/Release exceeds {} byte cap",
            MAX_APT_BYTES
        )));
    }
    let text = String::from_utf8(bytes.to_vec())
        .map_err(|_| IndexError("InRelease/Release is not valid UTF-8".to_string()))?;

    // Strip PGP armor head: everything up to and including the first blank
    // line after the signed-message header block.
    let body = if let Some(pos) = text.find("-----BEGIN PGP SIGNED MESSAGE-----") {
        let after = &text[pos..];
        match after.find("\n\n") {
            Some(blank) => &after[blank + 2..],
            None => {
                return Err(IndexError(
                    "clearsigned InRelease has no blank line after armor header".to_string(),
                ))
            }
        }
    } else {
        &text[..]
    };

    let mut entries = Vec::new();
    let mut valid_until: Option<i64> = None;
    let mut in_sha256_block = false;
    for line in body.lines() {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            in_sha256_block = false;
            continue;
        }
        // Field lines start at column 0.
        if !line.starts_with(' ') {
            in_sha256_block = false;
            if let Some((key, value)) = line.split_once(':') {
                if key == "Valid-Until" {
                    valid_until = parse_apt_valid_until(value.trim());
                } else if key == "SHA256" {
                    in_sha256_block = true;
                }
            }
            continue;
        }
        // Hash-block continuation line: ` <hash> <size> <path>`.
        if !in_sha256_block {
            continue;
        }
        let mut parts = line.trim_start().split(' ');
        let (hash, _size, path) = match (parts.next(), parts.next(), parts.next()) {
            (Some(h), Some(s), Some(p)) if !h.is_empty() && !s.is_empty() => (h, s, p),
            _ => continue,
        };
        if !path.ends_with(".deb") {
            continue;
        }
        if let Some((name, version)) = apt_deb_basename_info(path) {
            entries.push(IndexEntry {
                name,
                version,
                sha256: Some(hash.to_ascii_lowercase()),
            });
        }
    }
    Ok(ParsedIndex::Apt {
        entries,
        valid_until,
    })
}

/// Parse `Valid-Until:` as UTC. RFC 2822 parsing accepts the named zones
/// Debian emits (`UTC`, `GMT`) plus numeric offsets; unparseable → None,
/// never an error. (DateTime::parse_from_str with a literal " UTC" suffix
/// can never work — it requires a %z offset in the format.)
fn parse_apt_valid_until(value: &str) -> Option<i64> {
    let v = value.trim();
    // chrono's RFC 2822 grammar accepts GMT/UT but not the "UTC" spelling
    // Debian also emits; both denote +0000, so normalize before parsing.
    let normalized = v
        .strip_suffix(" UTC")
        .map(|s| format!("{s} GMT"))
        .unwrap_or_else(|| v.to_string());
    chrono::DateTime::parse_from_rfc2822(&normalized)
        .ok()
        .map(|dt| dt.timestamp())
}

/// Best-effort (name, version) from a `.deb` path's basename. Debian pool
/// naming is `name_VERSION-REV_arch.deb` — UNDERSCORE-separated fields, the
/// version field already being the `upstream-rev` composite (e.g.
/// `bash_5.2-1_amd64.deb` → ("bash", "5.2-1"); names may contain dashes
/// (`util-linux_2.38-2_amd64.deb`)). The pacman dash-split does not apply
/// here and yields zero entries on every real file. Ambiguous → None.
fn apt_deb_basename_info(path: &str) -> Option<(String, String)> {
    let fname = path.rsplit('/').next()?;
    let stem = fname.strip_suffix(".deb")?;
    if stem.is_empty() {
        return None;
    }
    let (front, arch) = stem.rsplit_once('_')?;
    let (name, version) = front.rsplit_once('_')?;
    if name.is_empty() || version.is_empty() || arch.is_empty() {
        return None;
    }
    // A real Debian version always carries a digit (upstream ver or epoch);
    // guards nonsense like `a_b_c.deb` from becoming a ledger key.
    if !version.bytes().any(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((name.to_string(), version.to_string()))
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
        Some(last) => {
            if last == "APKINDEX" || last.starts_with("APKINDEX.") {
                return true;
            }
            // dnf primary metadata.
            if last == "primary.xml.gz" || last == "primary.xml" {
                return true;
            }
            // apt InRelease / Release (Release.gpg deliberately excluded —
            // it is a signature, not an index).
            last == "InRelease" || last == "Release"
        }
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
    fn test_parse_index_multimember_gzip_apkindex() {
        // apk-tools concatenates gzip MEMBERS in APKINDEX.tar.gz: a SINGLE
        // tar stream (signature entries first, APKINDEX last) split at an
        // arbitrary byte boundary into separately-gzipped members. Plain
        // GzDecoder stops after member 1 and the tar parse truncates —
        // found live on the bench against a real archive.
        let mut b = Builder::new(Vec::new());
        b.append_data(
            &mut tar_header(".SIGN.RSA.alpine.pub", 4),
            ".SIGN.RSA.alpine.pub",
            std::io::Cursor::new(b"sig!".as_slice()),
        )
        .unwrap();
        b.append_data(
            &mut tar_header("APKINDEX", apkindex_text().len() as u64),
            "APKINDEX",
            std::io::Cursor::new(apkindex_text()),
        )
        .unwrap();
        let full_tar = b.into_inner().unwrap();
        // Split the stream mid-archive, gzip each half, concatenate.
        let mid = full_tar.len() / 2;
        let mut multi = gzip(&full_tar[..mid]);
        multi.extend_from_slice(&gzip(&full_tar[mid..]));
        let parsed = parse_index(&multi).unwrap();
        let ParsedIndex::Apk { entries } = parsed else {
            panic!("expected apk variant");
        };
        assert_eq!(entries[0].name, "bash");
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

    // ---- slice 5: dnf primary.xml ----

    /// Two-package primary.xml: one epoch attr "0", one epoch "2", with
    /// sha256 + sha checksums interleaved per record.
    fn primary_xml() -> String {
        let sha_a = "a".repeat(64);
        let sha_b = "b".repeat(64);
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<metadata xmlns="http://linux.duke.edu/metadata/common" packages="2">
<package type="rpm">
  <name>bash</name>
  <arch>x86_64</arch>
  <checksum type="sha" pkgid="YES">0123456789abcdef0123456789abcdef</checksum>
  <checksum type="sha256" pkgid="YES">{sha_a}</checksum>
  <version epoch="0" ver="1.2" rel="3"/>
</package>
<package type="rpm">
  <name>kernel</name>
  <arch>x86_64</arch>
  <checksum type="sha256" pkgid="YES">{sha_b}</checksum>
  <checksum type="md5">fedcba9876543210fedcba9876543210</checksum>
  <version epoch="2" ver="9.15" rel="1"/>
</package>
</metadata>
"#
        )
    }

    #[test]
    fn test_parse_index_dnf_gzip_versions_and_sha256() {
        let parsed = parse_index(&gzip(primary_xml().as_bytes())).unwrap();
        let ParsedIndex::Dnf { entries } = parsed else {
            panic!("expected dnf variant");
        };
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, "bash");
        assert_eq!(entries[0].version, "1.2-3");
        assert_eq!(entries[0].sha256.as_deref(), Some("a".repeat(64).as_str()));
        assert_eq!(entries[1].name, "kernel");
        assert_eq!(entries[1].version, "2:9.15-1");
        assert_eq!(entries[1].sha256.as_deref(), Some("b".repeat(64).as_str()));
    }

    #[test]
    fn test_parse_index_dnf_raw_and_zstd_also_parse() {
        // Raw (uncompressed) primary.xml sniffs by content, not extension.
        let parsed = parse_index(primary_xml().as_bytes()).unwrap();
        assert!(matches!(parsed, ParsedIndex::Dnf { .. }));
        let parsed = parse_index(&zstd_compress(primary_xml().as_bytes())).unwrap();
        let ParsedIndex::Dnf { entries } = parsed else {
            panic!("expected dnf variant");
        };
        assert_eq!(entries.len(), 2);
    }

    #[test]
    fn test_parse_index_dnf_unterminated_tag_is_err_no_panic() {
        // Unterminated <name> tag: the version element never closes either.
        let garbage = concat!(
            "<metadata><package type=\"rpm\"><name>bash",
            "<version epoch=\"0\" ver=\"1.2\" rel=\"3\"/>"
        );
        let parsed = parse_index(&gzip(garbage.as_bytes()));
        // The record has no closeable <name> → no entry (Dnf with 0 entries)
        // OR a structured error — either way, no panic.
        match parsed {
            Ok(ParsedIndex::Dnf { entries }) => assert!(entries.is_empty()),
            Err(e) => assert!(!e.0.is_empty()),
            Ok(other) => panic!("unexpected variant: {:?}", other),
        }
        // Non-UTF-8 text must be an IndexError, never a panic.
        let mut bad = primary_xml().into_bytes();
        bad[60] = 0xff;
        assert!(parse_index(&bad).is_err());
    }

    // ---- slice 5: apt InRelease / Release ----

    fn apt_release_body(valid_until: &str) -> String {
        let sha1 = "1".repeat(64);
        let sha2 = "2".repeat(64);
        // NOTE: lines are joined explicitly — `\n\` continuations swallow
        // the next line's leading space, and BOTH the ` SHA256:` sniff
        // marker and the ` <hash> <size> <path>` block format require it.
        [
            "Origin: Debian".to_string(),
            "Suite: bookworm".to_string(),
            "Date: Mon, 14 Sep 2026 00:00:00 UTC".to_string(),
            format!("Valid-Until: {valid_until}"),
            "MD5Sum:".to_string(),
            " aaaa 100 main/binary-amd64/Packages".to_string(),
            "SHA256:".to_string(),
            format!(" {sha1} 1234 pool/main/b/bash/bash_5.2-1_amd64.deb"),
            format!(" {sha2} 4321 pool/main/u/util-linux/util-linux_2.38-2_amd64.deb"),
            " 999999 200 main/binary-amd64/Packages.gz".to_string(),
            "".to_string(),
        ]
        .join("\n")
    }

    fn apt_in_release(valid_until: &str) -> Vec<u8> {
        format!(
            "-----BEGIN PGP SIGNED MESSAGE-----\nHash: SHA256\n\n{}-----BEGIN PGP SIGNATURE-----\n\nrandom signature bytes\n-----END PGP SIGNATURE-----\n",
            apt_release_body(valid_until)
        )
        .into_bytes()
    }

    #[test]
    fn test_parse_index_apt_in_release_future_valid_until() {
        let expected = chrono::Utc::now().timestamp() + 3600;
        let vu = chrono::DateTime::<chrono::Utc>::from_timestamp(expected, 0)
            .unwrap()
            .format("%a, %d %b %Y %H:%M:%S UTC")
            .to_string();
        let parsed = parse_index(&apt_in_release(&vu)).unwrap();
        let ParsedIndex::Apt {
            entries,
            valid_until,
        } = parsed
        else {
            panic!("expected apt variant");
        };
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, "bash");
        assert_eq!(entries[0].version, "5.2-1");
        assert_eq!(entries[0].sha256.as_deref(), Some("1".repeat(64).as_str()));
        assert_eq!(entries[1].name, "util-linux");
        assert_eq!(entries[1].version, "2.38-2");
        assert!(valid_until.is_some());
        assert!(
            (valid_until.unwrap() - expected).abs() <= 60,
            "valid_until {:?} vs expected {}",
            valid_until,
            expected
        );
    }

    #[test]
    fn test_parse_index_apt_past_valid_until_still_parses() {
        // Staleness enforcement is the proxy's call; parse stays Ok.
        let vu = "Mon, 01 Jan 2001 00:00:00 UTC";
        let parsed = parse_index(&apt_in_release(vu)).unwrap();
        let ParsedIndex::Apt {
            valid_until,
            entries,
        } = parsed
        else {
            panic!("expected apt variant");
        };
        assert_eq!(entries.len(), 2);
        // Expectation via the GMT spelling — same instant, and chrono's
        // RFC 2822 grammar rejects the " UTC" suffix (the quirk this test
        // exercises through the parser's normalization).
        let expected = chrono::DateTime::parse_from_rfc2822("Mon, 01 Jan 2001 00:00:00 GMT")
            .unwrap()
            .timestamp();
        assert_eq!(valid_until, Some(expected));
    }

    #[test]
    fn test_parse_index_apt_plain_release_and_gmt() {
        // Plain Release (no armor) with a GMT Valid-Until parses the same.
        let vu = "Tue, 15 Sep 2026 12:00:00 GMT";
        let body = apt_release_body(vu);
        let parsed = parse_index(body.as_bytes()).unwrap();
        let ParsedIndex::Apt {
            entries,
            valid_until,
        } = parsed
        else {
            panic!("expected apt variant");
        };
        assert_eq!(entries.len(), 2);
        let expected = chrono::DateTime::parse_from_rfc2822(vu)
            .unwrap()
            .timestamp();
        assert_eq!(valid_until, Some(expected));
    }

    #[test]
    fn test_parse_index_apt_armor_sniff_routes_by_content_not_name() {
        // Served under a nonsense filename, the PGP armor still routes to
        // the apt parser — detection is content-based.
        let vu = "Mon, 01 Jan 2001 00:00:00 UTC";
        let parsed = parse_index(&apt_in_release(vu)).unwrap();
        assert!(matches!(parsed, ParsedIndex::Apt { .. }));
    }

    #[test]
    fn test_looks_like_index_dnf_and_apt() {
        assert!(looks_like_index(
            "/rocky/9/BaseOS/x86_64/os/repodata/primary.xml.gz"
        ));
        assert!(looks_like_index(
            "/rocky/9/BaseOS/x86_64/os/repodata/primary.xml"
        ));
        assert!(looks_like_index("/debian/dists/bookworm/InRelease"));
        assert!(looks_like_index("/debian/dists/bookworm/Release"));
        // Signature and non-primary gzip stay out.
        assert!(!looks_like_index("/repo/Release.gpg"));
        assert!(!looks_like_index("/foo.xml.gz"));
    }
}
