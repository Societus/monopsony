//! End-to-end ledger/downgrade enforcement tests (Sprint 20 Phase 4, wave 2
//! slice 7). Drives the REAL client proxy (`run_proxy_with_listener`) against
//! a hand-rolled HTTP/1.1 upstream, with a real SQLite ledger and a real
//! tamper-event spool — the same assembly the daemon runs.
//!
//! Covered semantics (plan §6):
//! - index downgrade under "reject" → 503 "rewound" + spooled event
//! - index downgrade under "warn" → 200 + `X-Monopsony-Downgrade`, ledger
//!   still holds the higher high-water
//! - garbage index under "reject" → 503 + `index-parse-error:` event
//! - ledger absent → fully inert (200, no events file, no ledger headers)
//! - staleness v1 → `X-Monopsony-Index-Stale` warn header
//! - payload cross-check (read-only) → warn header on downgraded payload
//!
//! State seeding goes through the public Ledger API only (`check_and_update`
//! / `repo_seen`) — never raw SQL in this file.

use std::collections::HashMap;
use std::io::Write;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use flate2::write::GzEncoder;
use monopsony_client::ledger::{CheckVerdict, Ledger};
use monopsony_client::pin_verify::PinState;
use monopsony_client::proxy::{run_proxy_with_listener, ProxyConfig};
use monopsony_common::version_ord::normalize;

// ---- fixtures (copied pattern from index_parse.rs tests; tests/ cannot see
// `#[cfg(test)]` code in src/) ----

fn desc(name: &str, version: &str) -> Vec<u8> {
    format!("%NAME%\n{name}\n%VERSION%\n{version}\n%BASE%\n{name}\n").into_bytes()
}

fn tar_header(path: &str, size: u64) -> tar::Header {
    let mut header = tar::Header::new_gnu();
    header.set_size(size);
    header.set_mode(0o644);
    let _ = path;
    header
}

/// A pacman .db-style tar (zstd on the wire) with `bash` at `bash_version`
/// plus a second package at a constant version.
fn pacman_db_bytes(bash_version: &str) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    let bash_desc = desc("bash", bash_version);
    builder
        .append_data(
            &mut tar_header(&format!("bash-{bash_version}/desc"), bash_desc.len() as u64),
            format!("bash-{bash_version}/desc"),
            std::io::Cursor::new(bash_desc),
        )
        .unwrap();
    let coreutils_desc = desc("coreutils", "9.5-2");
    builder
        .append_data(
            &mut tar_header("coreutils-9.5-2/desc", coreutils_desc.len() as u64),
            "coreutils-9.5-2/desc",
            std::io::Cursor::new(coreutils_desc),
        )
        .unwrap();
    let tar_bytes = builder.into_inner().unwrap();
    zstd_compress(&tar_bytes)
}

fn zstd_compress(bytes: &[u8]) -> Vec<u8> {
    zstd::stream::encode_all(std::io::Cursor::new(bytes), 3).unwrap()
}

#[allow(dead_code)]
fn gzip(bytes: &[u8]) -> Vec<u8> {
    let mut encoder = GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(bytes).unwrap();
    encoder.finish().unwrap()
}

/// 64 bytes that are neither zstd nor gzip nor a valid tar — the parse-error
/// fixture for tests 3/4.
fn garbage_index_bytes() -> Vec<u8> {
    vec![b'x'; 64]
}

// ---- harness ----

/// Leak a tempdir so the ledger DB outlives the handle (ledger.rs test
/// pattern; the OS reclaims /tmp).
fn leaked_ledger_path(tag: &str) -> PathBuf {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join(format!("{tag}-ledger.sqlite3"));
    std::mem::forget(dir);
    path
}

fn leaked_events_path(tag: &str) -> PathBuf {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join(format!("{tag}-tamper-events.jsonl"));
    std::mem::forget(dir);
    path
}

/// Minimal HTTP/1.1 upstream: one canned 200 per connection, body swappable
/// between fetches via the shared buffer.
async fn spawn_upstream(body: Arc<Mutex<Vec<u8>>>) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let body = body.clone();
            tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                // Read one request head (the client proxy forwards via hyper).
                let mut buf: Vec<u8> = Vec::new();
                let mut chunk = [0u8; 1024];
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
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\
                     Content-Type: application/octet-stream\r\nConnection: close\r\n\r\n",
                    bytes.len()
                );
                if sock.write_all(head.as_bytes()).await.is_err() {
                    return;
                }
                let _ = sock.write_all(&bytes).await;
                let _ = sock.flush().await;
                // Drop closes the socket (Connection: close).
            });
        }
    });
    addr
}

struct Harness {
    proxy_addr: SocketAddr,
    upstream_addr: SocketAddr,
    events_path: PathBuf,
    /// Swappable upstream body (canned bytes of the next response).
    body: Arc<Mutex<Vec<u8>>>,
}

impl Harness {
    /// The index URL as a package manager would request it (http origin,
    /// .db suffix → pm_kind_from_url → pacman).
    fn index_url(&self) -> String {
        format!(
            "http://127.0.0.1:{}/repo/os/x86_64/core.db",
            self.upstream_addr.port()
        )
    }

    /// The https-canonical form the proxy derives for repo_key computation
    /// (pin_url maps http→https, authority kept).
    fn canonical_index_url(&self) -> String {
        format!(
            "https://127.0.0.1:{}/repo/os/x86_64/core.db",
            self.upstream_addr.port()
        )
    }

    fn swap_body(&self, bytes: Vec<u8>) {
        *self.body.lock().expect("body lock") = bytes;
    }
}

/// Assemble and serve the real client proxy exactly as the daemon does.
async fn setup(
    tag: &str,
    ledger: Option<Arc<Ledger>>,
    policy: &str,
    max_index_age_secs: u64,
    initial_body: Vec<u8>,
) -> Harness {
    let body = Arc::new(Mutex::new(initial_body));
    let upstream_addr = spawn_upstream(body.clone()).await;
    let events_path = leaked_events_path(tag);
    let pin = Arc::new(PinState::new(None, events_path.clone()));
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
        pin,
        ledger,
        policy.to_string(),
        max_index_age_secs,
        None,
        None,
    ));
    Harness {
        proxy_addr,
        upstream_addr,
        events_path,
        body,
    }
}

#[derive(Debug)]
struct RawResponse {
    status: u16,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

impl RawResponse {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .get(&name.to_ascii_lowercase())
            .map(|s| s.as_str())
    }
}

/// Hand-rolled absolute-URI GET through the client proxy (forward-proxy
/// semantics; Connection: close keeps read_to_end bounded).
async fn fetch_via_proxy(proxy: SocketAddr, url: &str) -> RawResponse {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let authority = url
        .strip_prefix("http://")
        .and_then(|rest| rest.find('/').map(|i| &rest[..i]))
        .expect("http url with authority");
    let mut sock = tokio::net::TcpStream::connect(proxy).await.unwrap();
    let request = format!(
        "GET {url} HTTP/1.1\r\nHost: {authority}\r\nUser-Agent: ledger-e2e\r\n\
         Connection: close\r\n\r\n"
    );
    sock.write_all(request.as_bytes()).await.unwrap();
    let mut raw = Vec::new();
    sock.read_to_end(&mut raw).await.unwrap();
    let head_end = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("complete response head");
    let head = String::from_utf8_lossy(&raw[..head_end]).to_string();
    let body = raw[head_end + 4..].to_vec();
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
    RawResponse {
        status,
        headers,
        body,
    }
}

// ---- tests ----

/// Test 1: reject policy — first fetch records the high-water (200); a
/// downgraded index is 503'd with a "rewound" body and a `downgrade:` event.
#[tokio::test]
async fn index_downgrade_reject_503s() {
    let ledger = Arc::new(Ledger::open(&leaked_ledger_path("reject")).unwrap());
    let h = setup(
        "reject",
        Some(ledger),
        "reject",
        0,
        pacman_db_bytes("5.2.37-1"),
    )
    .await;
    let url = h.index_url();

    // FIRST fetch: 200 — first contact just records the high-water.
    let first = fetch_via_proxy(h.proxy_addr, &url).await;
    assert_eq!(first.status, 200, "first fetch must pass: {first:?}");

    // Swap upstream bytes to a DOWNGRADED bash.
    h.swap_body(pacman_db_bytes("5.2.36-1"));
    let second = fetch_via_proxy(h.proxy_addr, &url).await;
    assert_eq!(second.status, 503, "downgraded index must be rejected");
    let body = String::from_utf8_lossy(&second.body).to_lowercase();
    assert!(
        body.contains("rewound"),
        "reject body must say rewound: {body}"
    );

    // The tamper spool gained a `downgrade:` event.
    let events = std::fs::read_to_string(&h.events_path).expect("events file exists");
    assert!(
        events.contains("downgrade:"),
        "events must contain a downgrade event: {events}"
    );
}

/// Test 2: warn policy — downgraded index passes with
/// `X-Monopsony-Downgrade`, and the ledger still holds the higher version.
#[tokio::test]
async fn index_downgrade_warn_headers() {
    let ledger = Arc::new(Ledger::open(&leaked_ledger_path("warn")).unwrap());
    let ledger_for_probe = ledger.clone();
    let h = setup("warn", Some(ledger), "warn", 0, pacman_db_bytes("5.2.37-1")).await;
    let url = h.index_url();

    let first = fetch_via_proxy(h.proxy_addr, &url).await;
    assert_eq!(first.status, 200, "first fetch must pass");

    h.swap_body(pacman_db_bytes("5.2.36-1"));
    let second = fetch_via_proxy(h.proxy_addr, &url).await;
    assert_eq!(
        second.status, 200,
        "warn policy must pass the index through"
    );
    let dg = second
        .header("X-Monopsony-Downgrade")
        .expect("warn policy sets X-Monopsony-Downgrade")
        .to_string();
    assert!(
        dg.contains("pacman:bash"),
        "header must name the downgraded package: {dg}"
    );

    // The ledger still holds the higher high-water (downgrade did not move it).
    assert_eq!(
        ledger_for_probe.high_water_for("pacman:bash"),
        Some("5.2.37-1".to_string()),
        "high-water must remain at the higher version"
    );
}

/// Test 3: garbage at an index URL under reject → 503 + parse-error event.
#[tokio::test]
async fn garbage_index_reject() {
    let ledger = Arc::new(Ledger::open(&leaked_ledger_path("garbage")).unwrap());
    let h = setup("garbage", Some(ledger), "reject", 0, garbage_index_bytes()).await;
    let url = h.index_url();

    let resp = fetch_via_proxy(h.proxy_addr, &url).await;
    assert_eq!(resp.status, 503, "garbage index must fail closed");
    let body = String::from_utf8_lossy(&resp.body).to_lowercase();
    assert!(
        body.contains("failed parsing"),
        "reject body must name the parse failure: {body}"
    );
    let events = std::fs::read_to_string(&h.events_path).expect("events file exists");
    assert!(
        events.contains("index-parse-error:"),
        "events must contain a parse-error event: {events}"
    );
}

/// Test 4: no ledger attached → fully inert. Garbage at a .db URL passes,
/// no events file is created, no ledger headers appear.
#[tokio::test]
async fn disabled_ledger_inert() {
    let h = setup("inert", None, "warn", 0, garbage_index_bytes()).await;
    let url = h.index_url();

    let resp = fetch_via_proxy(h.proxy_addr, &url).await;
    assert_eq!(resp.status, 200, "disabled ledger must not police anything");
    for hdr in [
        "X-Monopsony-Downgrade",
        "X-Monopsony-Index-Parse-Error",
        "X-Monopsony-Index-Stale",
    ] {
        assert!(
            resp.header(hdr).is_none(),
            "{hdr} must be absent with the ledger disabled"
        );
    }
    assert!(
        !h.events_path.exists(),
        "no events file may be created on the inert path"
    );
}

/// Test 5: staleness v1 — a repo last seen 1h ago, max age 1s → the fresh
/// index passes with `X-Monopsony-Index-Stale` (warn-only).
#[tokio::test]
async fn stale_index_header_only() {
    let ledger = Arc::new(Ledger::open(&leaked_ledger_path("stale")).unwrap());
    let now = chrono::Utc::now().timestamp();
    let h = setup(
        "stale",
        Some(ledger.clone()),
        "warn",
        1,
        pacman_db_bytes("5.2.37-1"),
    )
    .await;

    // Seed last_seen the same way the proxy computes it: canonical https
    // index URL → repo_key_from_url.
    let repo_key = monopsony_client::index_parse::repo_key_from_url(&h.canonical_index_url());
    ledger.repo_seen(&repo_key, now - 3600).unwrap();

    let resp = fetch_via_proxy(h.proxy_addr, &h.index_url()).await;
    assert_eq!(resp.status, 200, "staleness is warn-only: must pass");
    let stale = resp
        .header("X-Monopsony-Index-Stale")
        .expect("stale repo must set X-Monopsony-Index-Stale")
        .to_string();
    let age: i64 = stale.parse().expect("stale header carries age seconds");
    assert!(
        (3500..=3700).contains(&age),
        "age should be ~3600s, got {age}"
    );
}

/// Test 6 (optional per spec): payload cross-check — a payload URL parsing
/// to (bash, 5.2.36-1) below the held high-water (5.2.38-1) passes under
/// warn with `X-Monopsony-Downgrade` (read-only, no body buffering).
#[tokio::test]
async fn payload_downgrade_warn_header() {
    let ledger = Arc::new(Ledger::open(&leaked_ledger_path("payload")).unwrap());
    let now = chrono::Utc::now().timestamp();
    let verdict = ledger.check_and_update(
        "pacman:bash",
        "5.2.38-1",
        &normalize("5.2.38-1"),
        "seed",
        now,
    );
    assert_eq!(
        verdict.unwrap(),
        CheckVerdict::Ok,
        "seed records first contact"
    );

    let h = setup("payload", Some(ledger), "warn", 0, vec![0xAB; 512]).await;
    let payload_url = format!(
        "http://127.0.0.1:{}/repo/os/x86_64/bash-5.2.36-1-x86_64.pkg.tar.zst",
        h.upstream_addr.port()
    );

    let resp = fetch_via_proxy(h.proxy_addr, &payload_url).await;
    assert_eq!(resp.status, 200, "payload check is read-only under warn");
    let dg = resp
        .header("X-Monopsony-Downgrade")
        .expect("downgraded payload must set X-Monopsony-Downgrade")
        .to_string();
    assert!(
        dg.contains("pacman:bash") && dg.contains("5.2.36-1"),
        "header must name package and offered version: {dg}"
    );
}
