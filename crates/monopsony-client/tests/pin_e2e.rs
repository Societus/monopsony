//! Pin verification e2e (Sprint 20 Phase 3 regression, fleet verification
//! 2026-09-16).
//!
//! The two bugs the fleet matrix caught that every existing test missed:
//! - Bug 2 (framing): the hash verdict lands at END of the wrapped stream.
//!   With `content-length` framing, hyper's h1 encoder stops polling the
//!   body after the declared bytes — the terminal poll NEVER runs and the
//!   check silently no-ops. The ledger e2e harness masked this because its
//!   upstreams served `Connection: close` (close-delimited bodies force the
//!   terminal poll). These tests serve keep-alive + Content-Length — the
//!   production shape — and assert enforcement actually fires.
//! - Bug 1 (provisioning): PinState built at daemon boot from the key file;
//!   a key delivered by register later in the same boot must go live via
//!   set_key. Covered at unit level in proxy.rs tests (pin_wrap_strips
//!   provisions AFTER construction); here the proxy state starts
//!   unprovisioned and is provisioned after serving starts.
//!
//! Covered semantics:
//! - tampered body under keep-alive + Content-Length framing → transfer
//!   aborts mid-body (curl-style reader sees an error / truncated read),
//!   NOT a clean 200; tamper event spooled
//! - clean body under the same framing → full 200, body byte-identical,
//!   no events
//! - unprovisioned client → inert (200, full body, no re-framing)

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use ed25519_dalek::SigningKey;
use sha2::{Digest, Sha256};

use monopsony_client::pin_verify::PinState;
use monopsony_client::proxy::{run_proxy_with_listener, ProxyConfig};

// ---- pin/url helpers ----

fn tmp_events_path(tag: &str) -> std::path::PathBuf {
    let p = std::env::temp_dir().join(format!(
        "pin-e2e-{}-{}.jsonl",
        tag,
        std::process::id()
    ));
    let _ = std::fs::remove_file(&p);
    p
}

fn pkg_url(port: u16) -> String {
    format!("http://127.0.0.1:{port}/extra/os/x86_64/pkg-1.0-1-x86_64.pkg.tar.zst", port = port)
}

fn canonical_pkg_url(port: u16) -> String {
    format!("https://127.0.0.1:{port}/extra/os/x86_64/pkg-1.0-1-x86_64.pkg.tar.zst", port = port)
}

fn signed_pin_for(url: &str, sha: &str) -> (String, [u8; 32]) {
    let key = SigningKey::from_bytes(&[11u8; 32]);
    let mut att = monopsony_common::pin_store::PinAttestation {
        url: url.to_string(),
        expected_sha256: sha.to_string(),
        origin: monopsony_common::pin_store::PinOrigin::Sidecar,
        key_id: String::new(),
        observed_at: 1_700_000_000,
        signature: String::new(),
    };
    att.sign(&key);
    (monopsony_common::pin_store::to_json(&att), key.verifying_key().to_bytes())
}

#[derive(Debug)]
struct RawResponse {
    status: u16,
    headers: HashMap<String, String>,
    body: Vec<u8>,
    /// True when the read ended with an error/EOF before the declared
    /// Content-Length bytes arrived (a mid-transfer abort).
    aborted: bool,
}

/// Read one response over a raw socket with a hard deadline. Detects the
/// abort case: connection torn down (EOF/reset) before the length-delimited
/// body completed.
async fn fetch_via_proxy_deadline(proxy: SocketAddr, url: &str) -> RawResponse {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let authority = url
        .strip_prefix("http://")
        .and_then(|rest| rest.find('/').map(|i| &rest[..i]))
        .expect("http url with authority");
    let mut sock = tokio::net::TcpStream::connect(proxy).await.unwrap();
    let request = format!(
        "GET {url} HTTP/1.1\r\nHost: {authority}\r\nUser-Agent: pin-e2e\r\n\r\n"
    );
    sock.write_all(request.as_bytes()).await.unwrap();
    let mut raw = Vec::new();
    let mut chunk = [0u8; 8192];
    // Deadline + idle-cap: enforcement aborts mid-body, so EOF may never
    // come on a keep-alive connection. Stop at 2s of silence (well past the
    // ~15ms enforcement latency observed) or hard 10s.
    let read = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            match tokio::time::timeout(std::time::Duration::from_secs(2), sock.read(&mut chunk))
                .await
            {
                Ok(Ok(0)) => break, // clean EOF
                Ok(Ok(n)) => raw.extend_from_slice(&chunk[..n]),
                Ok(Err(_)) => break, // reset mid-body = abort
                Err(_) => break,     // idle: nothing more is coming
            }
        }
    })
    .await;
    let _ = read;
    let head_end = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n");
    let (head, body) = match head_end {
        Some(i) => (
            String::from_utf8_lossy(&raw[..i]).to_string(),
            raw[i + 4..].to_vec(),
        ),
        // Reset before/during the head: nothing was served — that IS an
        // abort. Report zero status so callers treat it as a failed download.
        None => (String::new(), Vec::new()),
    };
    let mut lines = head.split("\r\n");
    let status_line = lines.next().unwrap_or_default();
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse::<u16>().ok())
        .unwrap_or(0);
    let mut headers = HashMap::new();
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }
    let declared = headers
        .get("content-length")
        .and_then(|v| v.parse::<usize>().ok());
    // Decode chunked framing if present so body comparisons work on content.
    let chunked = headers
        .get("transfer-encoding")
        .map(|v| v.to_ascii_lowercase().contains("chunked"))
        .unwrap_or(false);
    let body = if chunked {
        decode_chunked(&body)
    } else {
        body
    };
    let aborted = match declared {
        Some(n) => body.len() < n, // short read before clean end = abort
        None => false,             // chunked: complete terminator unknown here
    };
    RawResponse {
        status,
        headers,
        body,
        aborted,
    }
}

// ---- tests ----

/// Tampered body under keep-alive + Content-Length framing: the download
/// must NOT complete cleanly. With the fix, the wrapper strips the length
/// and the mismatch's error aborts the transfer mid-body — the reader sees
/// a truncated/errored read, and the event is spooled.
#[tokio::test]
async fn tampered_body_aborts_under_content_length_framing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("monopsony_client=debug")
        .with_test_writer()
        .try_init();
    let clean = b"clean package bytes for the pin e2e test body".to_vec();
    let clean_sha = hex::encode(Sha256::digest(&clean));

    let events_path = tmp_events_path("tamper");
    let pin = Arc::new(PinState::new(None, events_path.clone()));

    // We need the upstream port BEFORE we can sign the pin (URL-bound), but
    // the pin must be served BY the upstream. Two-phase: spawn upstream with
    // a mutable header holder, then set the pin header once we know the port.
    let body = Arc::new(Mutex::new(clean.clone()));
    let headers: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let upstream_addr = spawn_upstream_framed_with_headers(body.clone(), headers.clone()).await;
    let url = pkg_url(upstream_addr.port());
    let (pin_json, verify) = signed_pin_for(&canonical_pkg_url(upstream_addr.port()), &clean_sha);
    headers
        .lock()
        .unwrap()
        .push(("x-monopsony-pin".to_string(), pin_json));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_addr = listener.local_addr().unwrap();
    let config = ProxyConfig {
        server_addr: format!("127.0.0.1:{}", upstream_addr.port()),
        timeout_ms: 5_000,
        retry_deadline_ms: 20_000,
        retry_backoff_initial_ms: 50,
        ..Default::default()
    };
    tokio::spawn(run_proxy_with_listener(
        listener,
        config,
        pin.clone(),
        None,
        "warn".to_string(),
        0,
    ));

    // Bug 1 flow: key provisioned AFTER serving starts (register path).
    pin.set_key(verify);
    assert!(pin.provisioned());

    // Swap in the tampered body — the pin still vouches for clean_sha.
    *body.lock().unwrap() = {
        let mut t = clean.clone();
        t[3] ^= 0xFF;
        t
    };

    let resp = fetch_via_proxy_deadline(proxy_addr, &url).await;
    // Accept either abort shape — both are failed downloads to the PM:
    // (a) hyper may abort the connection when the wrapped stream errors
    //     before/while writing the head (reset, empty or partial response),
    // (b) or the head went out and the chunked body terminates with an
    //     error frame mid-body (truncated read).
    // What is NEVER acceptable: a length-complete clean body.
    let clean_complete = resp.status == 200
        && !resp.aborted
        && match resp.headers.get("content-length") {
            Some(cl) => resp.body.len() == cl.parse::<usize>().unwrap_or(usize::MAX),
            None => resp.body == tampered_of(&clean),
        };
    assert!(
        !clean_complete,
        "tampered download completed cleanly (status={}, len={}, declared={:?}, body_sha={}) — enforcement did not fire",
        resp.status,
        resp.body.len(),
        resp.headers.get("content-length"),
        hex::encode(Sha256::digest(&resp.body))
    );
    // Zero-byte response with non-200/zero status = connection reset by the
    // enforcement abort — also a failed download. Either way we must land here.
    assert!(
        resp.status != 200 || resp.aborted || resp.headers.get("content-length").is_none(),
        "status 200 with intact content-length and no abort — framing fix regressed"
    );

    // Event spooled with both hashes.
    let events = pin.drain_events();
    assert_eq!(events.len(), 1, "exactly one tamper event: {events:?}");
    assert_eq!(events[0].expected_sha256, clean_sha);
    assert_eq!(events[0].actual_sha256, hex::encode(Sha256::digest(tampered_of(&clean))));
    assert!(events[0].url.contains("pkg-1.0"));
}

/// Clean body, same framing: full 200, byte-identical body, zero events.
#[tokio::test]
async fn clean_body_serves_clean_under_framing() {
    let clean = b"clean package bytes for the pin e2e test body".to_vec();
    let clean_sha = hex::encode(Sha256::digest(&clean));

    let events_path = tmp_events_path("clean");
    let pin = Arc::new(PinState::new(None, events_path.clone()));

    let body = Arc::new(Mutex::new(clean.clone()));
    let headers: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let upstream_addr = spawn_upstream_framed_with_headers(body.clone(), headers.clone()).await;
    let url = pkg_url(upstream_addr.port());
    let (pin_json, verify) = signed_pin_for(&canonical_pkg_url(upstream_addr.port()), &clean_sha);
    headers
        .lock()
        .unwrap()
        .push(("x-monopsony-pin".to_string(), pin_json));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_addr = listener.local_addr().unwrap();
    let config = ProxyConfig {
        server_addr: format!("127.0.0.1:{}", upstream_addr.port()),
        timeout_ms: 5_000,
        retry_deadline_ms: 20_000,
        retry_backoff_initial_ms: 50,
        ..Default::default()
    };
    tokio::spawn(run_proxy_with_listener(
        listener,
        config,
        pin.clone(),
        None,
        "warn".to_string(),
        0,
    ));
    pin.set_key(verify);

    let resp = fetch_via_proxy_deadline(proxy_addr, &url).await;
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body, clean, "byte-identical clean body");
    assert!(pin.drain_events().is_empty(), "no events on clean serve");
}

/// Unprovisioned client: pin header present but no key → inert passthrough
/// with the ORIGINAL framing (content-length preserved).
#[tokio::test]
async fn unprovisioned_client_is_inert() {
    let clean = b"inert test body".to_vec();
    let clean_sha = hex::encode(Sha256::digest(&clean));

    let events_path = tmp_events_path("inert");
    let pin = Arc::new(PinState::new(None, events_path.clone()));

    let body: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(clean.clone()));
    let headers: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let upstream_addr = spawn_upstream_framed_with_headers(body.clone(), headers.clone()).await;
    let url = pkg_url(upstream_addr.port());
    let (pin_json, _verify) = signed_pin_for(&canonical_pkg_url(upstream_addr.port()), &clean_sha);
    headers
        .lock()
        .unwrap()
        .push(("x-monopsony-pin".to_string(), pin_json));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_addr = listener.local_addr().unwrap();
    let config = ProxyConfig {
        server_addr: format!("127.0.0.1:{}", upstream_addr.port()),
        timeout_ms: 5_000,
        retry_deadline_ms: 20_000,
        retry_backoff_initial_ms: 50,
        ..Default::default()
    };
    tokio::spawn(run_proxy_with_listener(
        listener,
        config,
        pin.clone(),
        None,
        "warn".to_string(),
        0,
    ));
    // Deliberately NOT provisioning the key.

    let resp = fetch_via_proxy_deadline(proxy_addr, &url).await;
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body, clean);
    assert!(
        resp.headers.contains_key("content-length"),
        "inert path keeps original framing"
    );
    assert!(pin.drain_events().is_empty());
}

// ---- small helpers ----

/// Minimal chunked-transfer decoder: parses `<hex>\r\n<data>\r\n` frames
/// until the `0\r\n\r\n` terminator OR an incomplete/error frame (the
/// enforcement abort shape — returns what decoded so far).
fn decode_chunked(raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos < raw.len() {
        // size line
        let Some(nl) = raw[pos..].windows(2).position(|w| w == b"\r\n") else {
            break; // truncated mid-frame — that IS the abort shape
        };
        let size_line = &raw[pos..pos + nl];
        let size_str = String::from_utf8_lossy(size_line);
        let size_hex = size_str.split(';').next().unwrap_or("").trim();
        let Ok(size) = usize::from_str_radix(size_hex, 16) else {
            break; // not a valid size (e.g. error text) — abort shape
        };
        pos += nl + 2;
        if size == 0 {
            break; // terminator
        }
        if pos + size > raw.len() {
            out.extend_from_slice(&raw[pos..]); // partial final chunk
            break;
        }
        out.extend_from_slice(&raw[pos..pos + size]);
        pos += size + 2; // skip data + CRLF
    }
    out
}

fn tampered_of(clean: &[u8]) -> Vec<u8> {
    let mut t = clean.to_vec();
    t[3] ^= 0xFF;
    t
}

/// Same as spawn_upstream_framed but with a mutable header set (tests set
/// the pin header after learning the port).
async fn spawn_upstream_framed_with_headers(
    body: Arc<Mutex<Vec<u8>>>,
    headers: Arc<Mutex<Vec<(String, String)>>>,
) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let body = body.clone();
            let headers = headers.clone();
            tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buf: Vec<u8> = Vec::new();
                let mut chunk = [0u8; 4096];
                loop {
                    buf.clear();
                    loop {
                        match sock.read(&mut chunk).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => {
                                buf.extend_from_slice(&chunk[..n]);
                                if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                                    break;
                                }
                                if buf.len() > 64 * 1024 {
                                    return;
                                }
                            }
                        }
                    }
                    let bytes = body.lock().expect("body lock").clone();
                    let hs = headers.lock().expect("header lock").clone();
                    let mut head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\
                         Content-Type: application/octet-stream\r\n",
                        bytes.len()
                    );
                    for (k, v) in &hs {
                        head.push_str(&format!("{k}: {v}\r\n"));
                    }
                    head.push_str("\r\n");
                    if sock.write_all(head.as_bytes()).await.is_err() {
                        return;
                    }
                    if sock.write_all(&bytes).await.is_err() {
                        return;
                    }
                    let _ = sock.flush().await;
                }
            });
        }
    });
    addr
}
