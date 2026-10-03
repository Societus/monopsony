//! Sprint 23 Phase B+ — serve-time artifact-body rewrite.
//!
//! pip simple pages embed absolute `https://files.pythonhosted.org/...` links
//! and cargo's sparse-index `config.json` carries a `dl` template pointing at
//! upstream https; artifact downloads following those links bypass the
//! `/pypi/packages` and `/crates` shim routes (CONNECT = opaque, uncached).
//!
//! Upstream bytes are stored UNCHANGED (sidecar checksums stay origin-true);
//! this module is a pure, serve-time transform applied only to `lang_metadata`
//! bodies served via the shim routes. Plain string replacement — no HTML or
//! JSON parsing, no new dependencies.

/// Upstream host prefix for pip artifact files (exact-host match, trailing
/// slash so `files.pythonhosted.org.evil.example` never matches).
const PYPI_FILES_HOST: &str = "https://files.pythonhosted.org/";

/// Shim route prefix that maps to files.pythonhosted.org in the proxy.
const PYPI_PACKAGES_ROUTE: &str = "/pypi/packages/";

/// Upstream `dl` template embedded in cargo sparse-index config.json.
const CARGO_DL_UPSTREAM: &str = "https://crates.io/api/v1/crates";

/// Rewrite a pip simple-index page: absolute `https://files.pythonhosted.org/`
/// artifact links become `<proxy_base>/pypi/packages/...` links. The
/// `#sha256=...` fragment (verified client-side by pip) is preserved because
/// replacement operates strictly on the host prefix. Already-relative or
/// already-proxied links pass through untouched, so the transform is
/// idempotent. `proxy_base` is used verbatim as a prefix string, e.g.
/// `"http://10.0.0.5:3128"`.
pub fn rewrite_pypi_simple(body: &str, proxy_base: &str) -> String {
    if proxy_base.is_empty() {
        return body.to_string();
    }
    body.replace(
        PYPI_FILES_HOST,
        &format!("{}{}", proxy_base, PYPI_PACKAGES_ROUTE),
    )
}

/// Rewrite a cargo sparse-index `config.json` body: the `"dl"` template
/// `"https://crates.io/api/v1/crates"` becomes `"<proxy_base>/crates"` (cargo
/// appends `/<name>/<ver>/download`; the `/crates` shim re-adds the
/// static.crates.io path segment). The `"api"` key and every other key are
/// left byte-identical. Only the exact upstream `dl` value is rewritten, so an
/// already-proxied config passes through unchanged (idempotent). Bodies whose
/// `dl` value differs from upstream are returned unchanged.
pub fn rewrite_cargo_config(body: &str, proxy_base: &str) -> String {
    if proxy_base.is_empty() {
        return body.to_string();
    }
    match find_json_string_value(body, "dl") {
        Some((value_start, value_end)) if &body[value_start..value_end] == CARGO_DL_UPSTREAM => {
            let mut out = String::with_capacity(body.len() + proxy_base.len());
            out.push_str(&body[..value_start]);
            out.push_str(proxy_base);
            out.push_str("/crates");
            out.push_str(&body[value_end..]);
            out
        }
        _ => body.to_string(),
    }
}

/// Serve-path entry point for METADATA bodies on shim routes. `encoding` is
/// the sidecar's stored upstream content-encoding (e.g. `"gzip"`); `bytes`
/// is the byte-exact cached body.
///
/// pip simple pages arrive gzip-encoded when the spool fetched them from
/// origin with `Accept-Encoding: gzip` — the rewrites above operate on TEXT,
/// so an encoded body must be decoded before rewriting and re-encoded after
/// (serving decoded bytes under a `content-encoding: gzip` header, or
/// lossily UTF-8-converting encoded bytes, both break clients; the latter is
/// the fleet-caught 2026-10-02 pip regression).
///
/// Returns `Some((body, encoding))` — the bytes to serve plus the
/// content-encoding header value to attach (`""` = attach none) — or `None`
/// when the route/body is not a rewrite target (caller serves the original
/// bytes byte-faithful with the ORIGINAL encoding). Any decode/encode
/// surprise also yields `None`: stored bytes are never mangled to force a
/// rewrite through.
pub fn rewrite_metadata_body(
    encoding: Option<&str>,
    bytes: &[u8],
    shim_route: &str,
    proxy_base: &str,
) -> Option<(Vec<u8>, String)> {
    // Normalize the encoding tag. Only gzip is decode-able here; anything
    // else non-identity stays byte-faithful.
    let normalized = encoding
        .map(|e| e.trim().to_ascii_lowercase())
        .filter(|e| !e.is_empty() && e != "identity");
    let (plain, was_gzip) = match normalized.as_deref() {
        Some("gzip") => match gunzip(bytes) {
            Ok(decoded) => (decoded, true),
            // Sidecar says gzip but the bytes aren't (or are corrupt):
            // serve byte-faithful rather than guessing.
            Err(_) => return None,
        },
        Some(_) => return None,
        None => (bytes.to_vec(), false),
    };
    // Strict UTF-8: a non-text body is not a rewrite target (the serve path
    // then streams the original bytes unchanged).
    let text = String::from_utf8(plain).ok()?;
    let rewritten = rewrite_if_needed(shim_route, &text, proxy_base)?;
    let out = rewritten.into_bytes();
    if was_gzip {
        match gzip(&out) {
            Ok(reencoded) => Some((reencoded, "gzip".to_string())),
            // Re-encode failed: serve the DECODED rewrite without an
            // encoding header — still a faithful representation of the doc.
            Err(_) => Some((out, String::new())),
        }
    } else {
        Some((out, String::new()))
    }
}

/// Multi-member gzip decode (`MultiGzDecoder` — externally-sourced gzip can
/// be multi-member; single-member `GzDecoder` silently truncates).
fn gunzip(bytes: &[u8]) -> std::io::Result<Vec<u8>> {
    use flate2::read::MultiGzDecoder;
    use std::io::Read;
    let mut out = Vec::new();
    MultiGzDecoder::new(bytes).read_to_end(&mut out)?;
    Ok(out)
}

fn gzip(bytes: &[u8]) -> std::io::Result<Vec<u8>> {
    use flate2::{write::GzEncoder, Compression};
    use std::io::Write;
    let mut enc = GzEncoder::new(Vec::new(), Compression::default());
    enc.write_all(bytes)?;
    enc.finish()
}

/// Dispatch: `Some(rewritten)` when the (shim_route, body) pair warrants a
/// rewrite, `None` otherwise.
///
/// Routing table:
/// - `"/pypi/simple"` → pip simple-page rewrite (any body).
/// - `"/cargo-index"` → cargo config rewrite, but only when the body looks
///   like a sparse-index `config.json` (a JSON object carrying a `"dl"` key);
///   any other body on that route → `None`.
/// - everything else (including distro/npm routes) → `None`.
pub fn rewrite_if_needed(shim_route: &str, body: &str, proxy_base: &str) -> Option<String> {
    match shim_route {
        "/pypi/simple" => Some(rewrite_pypi_simple(body, proxy_base)),
        "/cargo-index" => {
            if is_cargo_config_body(body) {
                Some(rewrite_cargo_config(body, proxy_base))
            } else {
                None
            }
        }
        _ => None,
    }
}

/// A cargo sparse-index config.json is a JSON object carrying a `"dl"` key.
/// Deliberately conservative (no JSON parser): object-shaped body with a
/// `"dl":` member.
fn is_cargo_config_body(body: &str) -> bool {
    let trimmed = body.trim_start();
    trimmed.starts_with('{') && find_json_string_value(trimmed, "dl").is_some()
}

/// Locate the string value of `key` in a flat JSON object body, returning the
/// byte range `(start, end)` of the value's *contents* (between the quotes).
/// Handles whitespace around the colon and standard JSON string escapes while
/// scanning. Returns `None` if the key is absent or its value is not a string.
fn find_json_string_value(body: &str, key: &str) -> Option<(usize, usize)> {
    let needle = format!("\"{}\"", key);
    let mut search_from = 0usize;
    while let Some(key_pos) = body[search_from..].find(&needle) {
        let key_pos = search_from + key_pos;
        search_from = key_pos + needle.len();

        // Confirm this occurrence is actually a key: next non-ws char is ':'.
        let mut i = search_from;
        let bytes = body.as_bytes();
        while i < bytes.len()
            && (bytes[i] == b' ' || bytes[i] == b'\t' || bytes[i] == b'\n' || bytes[i] == b'\r')
        {
            i += 1;
        }
        if i >= bytes.len() || bytes[i] != b':' {
            continue;
        }
        i += 1;
        while i < bytes.len()
            && (bytes[i] == b' ' || bytes[i] == b'\t' || bytes[i] == b'\n' || bytes[i] == b'\r')
        {
            i += 1;
        }
        if i >= bytes.len() || bytes[i] != b'"' {
            continue; // value is not a string; no rewrite candidate
        }
        i += 1;
        let value_start = i;
        // Scan to the closing quote, honoring backslash escapes.
        while i < bytes.len() {
            match bytes[i] {
                b'\\' => i += 2,
                b'"' => return Some((value_start, i)),
                _ => i += 1,
            }
        }
        return None; // unterminated string
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROXY: &str = "http://10.0.0.5:3128";

    // Real PyPI artifact URL shapes: /packages/<2 hex>/<2 hex>/<~59-60 hex>/
    // <name>-<ver>-<tags>.<ext> — hash segment is NOT fixed-length.
    const HASH_59: &str = "3f2d8a1c9b4e7f60a5d8c2b1e4f7a0c3d6e9b2a5c8f1d4e7a0b3c6d9e";
    const HASH_60: &str = "9c8b7a6f5e4d3c2b1a09f8e7d6c5b4a39281f0e1d2c3b4a5968778695a4b3c2d";

    fn wheel_link(hash: &str, name: &str, ver: &str) -> String {
        format!(
            "https://files.pythonhosted.org/packages/ab/cd/{}/{}-{}-py3-none-any.whl",
            hash, name, ver
        )
    }

    fn simple_page(links: &[String]) -> String {
        let mut page = String::from(
            "<!DOCTYPE html><html><head><title>Links for example</title></head><body><h1>Links for example</h1>",
        );
        for link in links {
            let fname = link.rsplit('/').next().unwrap().split('#').next().unwrap();
            page.push_str(&format!(
                "<a href=\"{}#sha256=deadbeef\">{}</a><br/>",
                link, fname
            ));
        }
        page.push_str("</body></html>");
        page
    }

    #[test]
    fn pip_rewrites_absolute_links_and_preserves_fragment() {
        let page = simple_page(&[
            wheel_link(HASH_59, "example", "1.0"),
            wheel_link(HASH_60, "example", "1.2.3"),
        ]);
        let out = rewrite_pypi_simple(&page, PROXY);
        // Replacement is host-prefix → route-prefix, so the upstream path
        // (which itself begins with "packages/") is preserved verbatim:
        // /pypi/packages/packages/ab/cd/<hash>/...
        assert!(out.contains(&format!(
            "href=\"{}{}packages/ab/cd/{}/example-1.0-py3-none-any.whl#sha256=deadbeef\"",
            PROXY, PYPI_PACKAGES_ROUTE, HASH_59
        )));
        assert!(out.contains(&format!(
            "href=\"{}{}packages/ab/cd/{}/example-1.2.3-py3-none-any.whl#sha256=deadbeef\"",
            PROXY, PYPI_PACKAGES_ROUTE, HASH_60
        )));
        assert!(!out.contains("files.pythonhosted.org"));
    }

    #[test]
    fn pip_leaves_proxied_and_relative_links_untouched() {
        let proxied = format!(
            "{}{}ab/cd/{}/example-1.0-py3-none-any.whl",
            PROXY, PYPI_PACKAGES_ROUTE, HASH_59
        );
        let relative = "packages/ab/cd/example-1.0-py3-none-any.whl".to_string();
        let page = simple_page(&[proxied.clone(), relative]);
        let out = rewrite_pypi_simple(&page, PROXY);
        assert!(out.contains(&format!("href=\"{}#sha256=", proxied)));
        assert!(out.contains("href=\"packages/ab/cd/example-1.0-py3-none-any.whl#sha256="));
    }

    #[test]
    fn pip_exact_host_match_only() {
        // A lookalike host with the upstream name as a substring must not match.
        let page = simple_page(&[format!(
            "https://files.pythonhosted.org.evil.example/packages/ab/cd/{}/evil-1.0-py3-none-any.whl",
            HASH_59
        )]);
        let out = rewrite_pypi_simple(&page, PROXY);
        assert!(out.contains("https://files.pythonhosted.org.evil.example/packages/"));
        // And a non-https scheme on the real host is not our match either.
        let http_page = simple_page(&[format!(
            "http://files.pythonhosted.org/packages/ab/cd/{}/example-1.0-py3-none-any.whl",
            HASH_59
        )]);
        assert!(rewrite_pypi_simple(&http_page, PROXY).contains("http://files.pythonhosted.org/"));
    }

    #[test]
    fn pip_idempotent() {
        let page = simple_page(&[
            wheel_link(HASH_59, "example", "1.0"),
            format!(
                "{}{}ab/cd/{}/example-0.9-py3-none-any.whl",
                PROXY, PYPI_PACKAGES_ROUTE, HASH_60
            ),
        ]);
        let once = rewrite_pypi_simple(&page, PROXY);
        let twice = rewrite_pypi_simple(&once, PROXY);
        assert_eq!(once, twice);
    }

    #[test]
    fn pip_empty_proxy_base_is_noop() {
        let page = simple_page(&[wheel_link(HASH_59, "example", "1.0")]);
        assert_eq!(rewrite_pypi_simple(&page, ""), page);
    }

    #[test]
    fn cargo_rewrites_dl_leaves_api_and_other_keys() {
        let body = format!(
            "{{\"dl\":\"{}\",\"api\":\"https://crates.io\",\"other-key\":{{\"dl\":true}}}}",
            CARGO_DL_UPSTREAM
        );
        let out = rewrite_cargo_config(&body, PROXY);
        assert!(out.contains(&format!("\"dl\":\"{}/crates\"", PROXY)));
        assert!(out.contains("\"api\":\"https://crates.io\""));
        assert!(out.contains("\"dl\":true"));
        assert_eq!(
            out.len(),
            body.len() - CARGO_DL_UPSTREAM.len() + PROXY.len() + "/crates".len()
        );
    }

    #[test]
    fn cargo_handles_whitespace_variants() {
        let body = format!(
            "{{\n  \"dl\" : \"{}\",\n  \"api\": \"https://crates.io\"\n}}",
            CARGO_DL_UPSTREAM
        );
        let out = rewrite_cargo_config(&body, PROXY);
        assert!(out.contains(&format!("\"dl\" : \"{}/crates\"", PROXY)));
        assert!(out.contains("\"api\": \"https://crates.io\""));
    }

    #[test]
    fn cargo_non_upstream_dl_untouched() {
        // Already-proxied config from an earlier serve: idempotent no-op.
        let proxied = format!(
            "{{\"dl\":\"{}/crates\",\"api\":\"https://crates.io\"}}",
            PROXY
        );
        assert_eq!(rewrite_cargo_config(&proxied, PROXY), proxied);
        // A third-party sparse index must not be rewritten.
        let custom = "{\"dl\":\"https://example.internal/api/v1/crates\",\"api\":\"https://example.internal\"}";
        assert_eq!(rewrite_cargo_config(custom, PROXY), custom);
    }

    #[test]
    fn cargo_empty_proxy_base_is_noop() {
        let body = format!(
            "{{\"dl\":\"{}\",\"api\":\"https://crates.io\"}}",
            CARGO_DL_UPSTREAM
        );
        assert_eq!(rewrite_cargo_config(&body, ""), body);
    }

    #[test]
    fn cargo_idempotent() {
        let body = format!(
            "{{\"dl\":\"{}\",\"api\":\"https://crates.io\"}}",
            CARGO_DL_UPSTREAM
        );
        let once = rewrite_cargo_config(&body, PROXY);
        let twice = rewrite_cargo_config(&once, PROXY);
        assert_eq!(once, twice);
    }

    #[test]
    fn metadata_body_gzip_roundtrip_preserves_bytes_and_encoding() {
        // The 2026-10-02 fleet regression: pip simple pages are cached
        // gzip-encoded; the old call site UTF-8-lossy'd the ENCODED bytes
        // and served mush. The entry point must decode, rewrite, re-encode.
        let page = simple_page(&[wheel_link(HASH_59, "example", "1.0")]);
        let encoded = gzip(page.as_bytes()).expect("gzip fixture");
        let (body, enc) = rewrite_metadata_body(Some("gzip"), &encoded, "/pypi/simple", PROXY)
            .expect("gzip pip page is a rewrite target");
        assert_eq!(enc, "gzip");
        // Round-trips to the rewritten plain text:
        let decoded = gunzip(&body).expect("re-encoded body is valid gzip");
        let decoded = String::from_utf8(decoded).expect("utf8");
        assert!(decoded.contains(&format!("{}{}", PROXY, PYPI_PACKAGES_ROUTE)));
        assert!(!decoded.contains("files.pythonhosted.org"));
        // And is NOT the lossy-mangled encoding of the gzip bytes:
        assert_ne!(body, encoded);
    }

    #[test]
    fn metadata_body_identity_gzip_decodes_and_clears_encoding() {
        // Plain (identity) cached body: rewritten, encoding header cleared.
        let page = simple_page(&[wheel_link(HASH_59, "example", "1.0")]);
        let (body, enc) = rewrite_metadata_body(None, page.as_bytes(), "/pypi/simple", PROXY)
            .expect("plain pip page rewrites");
        assert_eq!(enc, "");
        assert!(String::from_utf8_lossy(&body).contains(PYPI_PACKAGES_ROUTE));
        // "identity" is normalized to no-encoding, same path:
        let (body2, enc2) =
            rewrite_metadata_body(Some("identity"), page.as_bytes(), "/pypi/simple", PROXY)
                .expect("identity is treated as plain");
        assert_eq!(enc2, "");
        assert_eq!(body, body2);
    }

    #[test]
    fn metadata_body_unknown_encoding_stays_byte_faithful() {
        // br (brotli) or anything non-gzip must NOT be touched: None →
        // caller serves original bytes + original header.
        let body = b"\xce\xba\xe1\xbd\xb9\xcf\x83\xce\xbc\xce\xb5 compressed-ish";
        assert!(rewrite_metadata_body(Some("br"), body, "/pypi/simple", PROXY).is_none());
    }

    #[test]
    fn metadata_body_gzip_labeled_non_gzip_bytes_stay_faithful() {
        // Sidecar says gzip but bytes are not gzip (corrupt/mislabeled):
        // never guess, never mangle.
        let junk: &[u8] = b"\x1f\x8b NOT ACTUALLY GZIP \xff\xfe";
        assert!(
            rewrite_metadata_body(Some("gzip"), junk, "/pypi/simple", PROXY).is_none(),
            "corrupt gzip must fall back to byte-faithful serve"
        );
    }

    #[test]
    fn metadata_body_non_utf8_is_not_a_rewrite_target() {
        // Binary (non-UTF-8) body on a pip route: None → byte-faithful.
        let binary: &[u8] = &[0x00, 0xff, 0xfe, 0x80, 0x01];
        assert!(rewrite_metadata_body(None, binary, "/pypi/simple", PROXY).is_none());
    }

    #[test]
    fn metadata_body_cargo_route_gzip_roundtrip() {
        let cfg = format!(
            "{{\"dl\":\"{}\",\"api\":\"https://crates.io\"}}",
            CARGO_DL_UPSTREAM
        );
        let encoded = gzip(cfg.as_bytes()).expect("gzip fixture");
        let (body, enc) = rewrite_metadata_body(Some("gzip"), &encoded, "/cargo-index", PROXY)
            .expect("gzip cargo config rewrites");
        assert_eq!(enc, "gzip");
        let decoded = String::from_utf8(gunzip(&body).expect("gzip")).expect("utf8");
        assert!(decoded.contains(&format!("\"dl\":\"{}/crates\"", PROXY)));
    }

    #[test]
    fn dispatch_routes() {
        let page = simple_page(&[wheel_link(HASH_59, "example", "1.0")]);
        let cargo_cfg = format!(
            "{{\"dl\":\"{}\",\"api\":\"https://crates.io\"}}",
            CARGO_DL_UPSTREAM
        );
        let other_json = "{\"downloading\":\"array\"}";
        let distro_page =
            "<html><a href=\"https://geo.mirror.pkgbuild.com/core/os/x86_64/core.db\"></a></html>";

        // pip simple route: rewritten.
        let got = rewrite_if_needed("/pypi/simple", &page, PROXY).expect("pypi simple rewrites");
        assert!(got.contains(&format!("{}{}", PROXY, PYPI_PACKAGES_ROUTE)));

        // cargo route + config.json body: rewritten.
        let got =
            rewrite_if_needed("/cargo-index", &cargo_cfg, PROXY).expect("cargo config rewrites");
        assert!(got.contains(&format!("\"dl\":\"{}/crates\"", PROXY)));

        // cargo route + non-config JSON: None.
        assert!(rewrite_if_needed("/cargo-index", other_json, PROXY).is_none());

        // Distro / npm / unknown routes: None regardless of body.
        assert!(rewrite_if_needed("/archlinux", &page, PROXY).is_none());
        assert!(rewrite_if_needed("/npm", &page, PROXY).is_none());
        assert!(rewrite_if_needed("/pypi/packages", &page, PROXY).is_none());
        assert!(rewrite_if_needed("/cargo-index", distro_page, PROXY).is_none());
        assert!(rewrite_if_needed("", &page, PROXY).is_none());

        // Artifact (non-metadata) bodies on shim routes: None — byte-faithful.
        let wheel_bytes = "PK\x03\x04 fake wheel bytes";
        assert!(rewrite_if_needed("/pypi/packages", wheel_bytes, PROXY).is_none());
    }
}
