//! ClamAV TCP client integration for malware scanning.
//!
//! Sends cached package data to a clamd instance via the INSTREAM protocol
//! using the `clamav-client` crate's async tokio backend. Connection is
//! derived from `ScanConfig::clamd_socket` which currently supports
//! `tcp://host:port` format only.

use std::time::Duration;
use tokio::time::timeout;
use tracing::{debug, warn};

/// ClamAV scan result.
#[derive(Debug, Clone)]
pub enum ClamAvResult {
    /// Data is clean — no signatures matched.
    Clean,
    /// ClamAV detected a threat. `signature` is the raw signature string
    /// from clamd (e.g., `"Win.Test.EICAR_HDB-1"`).
    Infected { signature: String },
    /// Could not reach clamd or a protocol error occurred.
    Error(String),
}

/// Scan a byte buffer against ClamAV signatures via clamd INSTREAM.
///
/// `clamd_socket` is the address string from config (e.g., `"tcp://127.0.0.1:3310"`).
/// `scan_timeout` caps how long we wait for the full scan to complete.
/// `chunk_size` is forwarded to `clamav_client` — `None` uses the crate default.
pub async fn scan_buffer(
    data: &[u8],
    clamd_socket: &str,
    scan_timeout: Duration,
    chunk_size: Option<usize>,
) -> ClamAvResult {
    let host_address = match parse_tcp_address(clamd_socket) {
        Ok(addr) => addr,
        Err(e) => return ClamAvResult::Error(e),
    };

    let connection = clamav_client::tokio::Tcp { host_address };

    let scan_fut = clamav_client::tokio::scan_buffer(data, connection, chunk_size);

    let response = match timeout(scan_timeout, scan_fut).await {
        Ok(Ok(bytes)) => bytes,
        Ok(Err(e)) => {
            warn!("ClamAV scan failed: {}", e);
            return ClamAvResult::Error(format!("clamd I/O error: {}", e));
        }
        Err(_) => {
            warn!("ClamAV scan timed out after {:?}", scan_timeout);
            return ClamAvResult::Error(format!("clamd timeout ({:?})", scan_timeout));
        }
    };

    parse_response(&response)
}

/// Parse a clamd response byte buffer into a typed result.
pub fn parse_response(response: &[u8]) -> ClamAvResult {
    // clamd responses are UTF-8 text, typically ending with a null byte
    let text = match std::str::from_utf8(response) {
        Ok(s) => s.trim_end_matches('\0').trim(),
        Err(_) => {
            return ClamAvResult::Error("non-UTF-8 response from clamd".into());
        }
    };

    debug!("clamd response: {:?}", text);

    // Check if data is clean using the crate's helper
    match clamav_client::clean(response) {
        Ok(true) => ClamAvResult::Clean,
        Ok(false) => {
            // Infected — extract the signature from the response.
            // clamd format: "<stream>: <SIGNATURE> FOUND"
            let signature = extract_signature(text);
            ClamAvResult::Infected { signature }
        }
        Err(e) => ClamAvResult::Error(format!("failed to parse clamd response: {}", e)),
    }
}

/// Extract the ClamAV signature name from a clamd response line.
///
/// clamd returns `<path_or_stream>: <SIGNATURE> FOUND` for infections.
/// We extract just the `<SIGNATURE>` part.
fn extract_signature(response: &str) -> String {
    // Look for the pattern: "... : SIGNATURE FOUND"
    if let Some(colon_pos) = response.rfind(": ") {
        let after_colon = &response[colon_pos + 2..];
        // Remove trailing " FOUND" if present
        if let Some(stripped) = after_colon.strip_suffix(" FOUND") {
            return stripped.trim().to_string();
        }
        // Fallback: return everything after the colon
        return after_colon.trim().to_string();
    }
    // Last resort: return the whole trimmed response
    response.trim().to_string()
}

/// Parse `clamd_socket` config string and extract the host:port address.
///
/// Only `tcp://host:port` is supported. Unix sockets can be added later.
fn parse_tcp_address(socket_str: &str) -> Result<String, String> {
    if let Some(addr) = socket_str.strip_prefix("tcp://") {
        if addr.is_empty() {
            return Err("empty TCP address after tcp:// prefix".into());
        }
        Ok(addr.to_string())
    } else if socket_str.starts_with("unix://") || socket_str.starts_with('/') {
        Err("Unix socket transport is not yet supported. Use tcp://host:port.".into())
    } else {
        Err(format!(
            "unsupported clamd_socket format {:?} — expected tcp://host:port",
            socket_str
        ))
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_response_clean() {
        // clamd returns "<stream>: OK\0" for clean data
        let response = b"stream: OK\0";
        let result = parse_response(response);
        assert!(matches!(result, ClamAvResult::Clean));
    }

    #[test]
    fn parse_response_infected_eicar() {
        let response = b"stream: Win.Test.EICAR_HDB-1 FOUND\0";
        let result = parse_response(response);
        match result {
            ClamAvResult::Infected { signature } => {
                assert_eq!(signature, "Win.Test.EICAR_HDB-1");
            }
            other => panic!("expected Infected, got {:?}", other),
        }
    }

    #[test]
    fn parse_response_infected_with_path() {
        let response = b"/tmp/evil.tar.gz: Unix.Trojan.Mirai-123 FOUND\0";
        let result = parse_response(response);
        match result {
            ClamAvResult::Infected { signature } => {
                assert_eq!(signature, "Unix.Trojan.Mirai-123");
            }
            other => panic!("expected Infected, got {:?}", other),
        }
    }

    #[test]
    fn parse_response_non_utf8() {
        let response: &[u8] = &[0xFF, 0xFE, 0x00];
        let result = parse_response(response);
        assert!(matches!(result, ClamAvResult::Error(_)));
    }

    #[test]
    fn parse_tcp_address_standard() {
        assert_eq!(
            parse_tcp_address("tcp://127.0.0.1:3310").unwrap(),
            "127.0.0.1:3310"
        );
    }

    #[test]
    fn parse_tcp_address_custom_host() {
        assert_eq!(
            parse_tcp_address("tcp://clamav.internal:3310").unwrap(),
            "clamav.internal:3310"
        );
    }

    #[test]
    fn parse_tcp_address_empty_rejected() {
        let result = parse_tcp_address("tcp://");
        assert!(result.is_err());
    }

    #[test]
    fn parse_tcp_address_unix_rejected() {
        let result = parse_tcp_address("unix:///var/run/clamd.sock");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not yet supported"));
    }

    #[test]
    fn parse_tcp_address_bare_path_rejected() {
        let result = parse_tcp_address("/var/run/clamd.sock");
        assert!(result.is_err());
    }

    #[test]
    fn parse_tcp_address_invalid_format() {
        let result = parse_tcp_address("udp://host:3310");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("unsupported"));
    }

    #[test]
    fn extract_signature_standard() {
        assert_eq!(
            extract_signature("stream: Win.Test.EICAR_HDB-1 FOUND"),
            "Win.Test.EICAR_HDB-1"
        );
    }

    #[test]
    fn extract_signature_with_path_prefix() {
        assert_eq!(
            extract_signature("/cache/pkg-1.0.tgz: Html.Phishing-123 FOUND"),
            "Html.Phishing-123"
        );
    }

    #[test]
    fn extract_signature_no_found_suffix() {
        // Edge case: malformed response without " FOUND"
        assert_eq!(extract_signature("stream: SOME-SIG"), "SOME-SIG");
    }

    #[test]
    fn extract_signature_no_colon() {
        // Last resort: whole response
        assert_eq!(extract_signature("SOMETHING"), "SOMETHING");
    }

    // Note: Live EICAR test against clamd belongs in tests/ with #[ignore].
    // Response parsing is fully covered by the unit tests above.
}
