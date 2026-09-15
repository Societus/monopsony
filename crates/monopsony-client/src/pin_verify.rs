//! Client-side pin verification (Sprint 20 Phase 3).
//!
//! The client proxy verifies served bytes against the server's signed pin
//! (`X-Monopsony-Pin` header) BEFORE the package manager finishes consuming
//! them. The pin key is provisioned from `PairResponse`/`RegisterResponse`
//! (`mirror_verify_key`, 32 raw Ed25519 bytes); a zeroed/absent key means
//! pinning is not provisioned and every check is inert.
//!
//! Tamper events (hash mismatches) are spooled to an append-only JSONL file
//! under the client state dir and drained by the daemon's heartbeat loop,
//! which forwards them to the server as gRPC TamperAlerts. Spooling happens
//! BEFORE the response is truncated so the alarm survives PM retries.

use ed25519_dalek::VerifyingKey;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io;
use std::path::PathBuf;
use std::sync::RwLock;

/// One client-observed pin failure, spooled until the daemon drains it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TamperEvent {
    pub url: String,
    pub expected_sha256: String,
    pub actual_sha256: String,
    pub detail: String,
    pub observed_at: i64,
}

/// Encode a spooled event into the gRPC TamperAlert message (slice-6 wiring
/// sends this over the ControlStream).
pub fn encode_tamper_alert(
    event: &TamperEvent,
) -> monopsony_common::proto::monopsony::TamperAlert {
    monopsony_common::proto::monopsony::TamperAlert {
        url: event.url.clone(),
        expected_sha256: event.expected_sha256.clone(),
        actual_sha256: event.actual_sha256.clone(),
        detail: event.detail.clone(),
        observed_at: event.observed_at,
    }
}

/// Shared pin-verification state. Interior-mutable so the daemon can
/// provision the key after pairing without rebuilding the proxy state.
pub struct PinState {
    key: RwLock<Option<VerifyingKey>>,
    events_path: PathBuf,
}

impl PinState {
    /// `verify_key`: 32 raw Ed25519 bytes (PairResponse/RegisterResponse
    /// `mirror_verify_key`). None/zeroed = pinning not provisioned.
    pub fn new(verify_key: Option<[u8; 32]>, events_path: PathBuf) -> Self {
        let key = verify_key
            .filter(|k| *k != [0u8; 32])
            .map(|k| VerifyingKey::from_bytes(&k).expect("32 bytes always build a VerifyingKey"));
        Self {
            key: RwLock::new(key),
            events_path,
        }
    }

    /// Provision or rotate the pin key (daemon startup / re-register).
    pub fn set_key(&self, bytes: [u8; 32]) {
        let key = if bytes == [0u8; 32] {
            None
        } else {
            Some(VerifyingKey::from_bytes(&bytes).expect("32 bytes always build a VerifyingKey"))
        };
        *self.key.write().expect("pin key lock poisoned") = key;
    }

    /// True when a real (non-zeroed) key is provisioned.
    pub fn provisioned(&self) -> bool {
        self.key.read().expect("pin key lock poisoned").is_some()
    }

    /// Verify a signed pin attestation (the X-Monopsony-Pin header JSON)
    /// against the provisioned key AND the URL it is being served for.
    /// Err carries a human-readable reason (also used in tamper events).
    pub fn verify_attestation_json(
        &self,
        pin_json: &str,
        url: &str,
    ) -> Result<monopsony_common::pin_store::PinAttestation, String> {
        let key = self.key.read().expect("pin key lock poisoned");
        let Some(vk) = key.as_ref() else {
            return Err("pinning not provisioned".to_string());
        };
        let Some(att) = monopsony_common::pin_store::from_json(pin_json) else {
            return Err("unparseable pin attestation".to_string());
        };
        if !att.verify(vk) {
            return Err("pin attestation signature/key_id invalid".to_string());
        }
        if att.url != url {
            return Err(format!(
                "pin url mismatch: pin is for {} but serving {}",
                att.url, url
            ));
        }
        Ok(att)
    }

    /// Append a tamper event to the JSONL spool. Synchronous and tiny —
    /// called before the response is truncated so the alarm cannot be lost.
    pub fn record_event(&self, event: &TamperEvent) {
        use std::io::Write;
        if let Some(parent) = self.events_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        // A spool write failure must be LOUD (a silently lost tamper alarm is
        // the worst failure mode) but must not crash the response path.
        match serde_json::to_string(event) {
            Ok(line) => match std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.events_path)
                .and_then(|mut f| writeln!(f, "{}", line).map_err(|e| e.into()))
            {
                Ok(()) => {}
                Err(e) => tracing::error!(
                    "Tamper event spool write FAILED ({}): {} — tamper alarm for {} lost",
                    self.events_path.display(),
                    e,
                    event.url
                ),
            },
            Err(e) => tracing::error!("Tamper event serialize failed: {}", e),
        }
    }

    /// Drain all spooled events (daemon heartbeat loop). Truncates the file
    /// so each event is delivered once per drain; a failed truncate may
    /// re-deliver — the server dedupes on (url, expected, actual, observed_at).
    pub fn drain_events(&self) -> Vec<TamperEvent> {
        match std::fs::read_to_string(&self.events_path) {
            Ok(content) => {
                let events: Vec<TamperEvent> = content
                    .lines()
                    .filter(|l| !l.trim().is_empty())
                    .filter_map(|l| match serde_json::from_str(l) {
                        Ok(e) => Some(e),
                        Err(e) => {
                            tracing::warn!("Tamper event spool line unparseable, dropped: {}", e);
                            None
                        }
                    })
                    .collect();
                // Always truncate (even on partial parse loss) so a corrupt
                // line cannot wedge the spool forever.
                let _ = std::fs::write(&self.events_path, b"");
                events
            }
            Err(e) => {
                // Missing file is the normal empty case (ENOENT); anything
                // else is worth a warn.
                if e.kind() != std::io::ErrorKind::NotFound {
                    tracing::warn!("Tamper event spool read failed: {}", e);
                }
                Vec::new()
            }
        }
    }
}

/// Hash-checking stream: wraps the server response body's data stream,
/// hashing every chunk, and compares the final digest to the pinned hash
/// at end-of-stream. Mismatch → spool the tamper event, log the alarm, and
/// yield an error into the body (the PM sees a failed download — the
/// reject policy). Chunks are forwarded untouched while streaming.
pub struct PinCheckStream<S> {
    inner: S,
    state: std::sync::Arc<PinState>,
    url: String,
    expected_sha256: String,
    hasher: Sha256,
    /// Terminal latch — once the stream has ended (clean or error), every
    /// later poll returns Ready(None). Without this, poll_after_end loops
    /// forever on a closed stream and collect() never returns.
    finished: bool,
}

/// Build a hash-checking stream over `inner`. The error-type bound lives on
/// the Stream impl, so construction needs only `S`.
pub fn pin_check_stream<S>(
    inner: S,
    state: std::sync::Arc<PinState>,
    url: String,
    expected_sha256: String,
) -> PinCheckStream<S> {
    PinCheckStream {
        inner,
        state,
        url,
        expected_sha256,
        hasher: Sha256::new(),
        finished: false,
    }
}

impl<S, E> futures_util::Stream for PinCheckStream<S>
where
    S: futures_util::Stream<Item = Result<axum::body::Bytes, E>> + Unpin,
    E: std::fmt::Display + Send + Sync + 'static,
{
    type Item = Result<axum::body::Bytes, io::Error>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        // One-shot latch: after the terminal decision, keep returning Ready(None)
        // so collect() terminates instead of looping forever.
        if self.finished {
            return std::task::Poll::Ready(None);
        }
        match std::pin::Pin::new(&mut self.inner).poll_next(cx) {
            std::task::Poll::Ready(Some(Ok(chunk))) => {
                self.hasher.update(&chunk);
                std::task::Poll::Ready(Some(Ok(chunk)))
            }
            std::task::Poll::Ready(None) => {
                self.finished = true;
                let digest = self.hasher.clone().finalize();
                let actual = hex::encode(digest);
                if actual.eq_ignore_ascii_case(&self.expected_sha256) {
                    tracing::info!("Pin verified for {} (sha256 ok)", self.url);
                    std::task::Poll::Ready(None)
                } else {
                    let event = TamperEvent {
                        url: self.url.clone(),
                        expected_sha256: self.expected_sha256.clone(),
                        actual_sha256: actual.clone(),
                        detail: "client stream hash mismatch vs signed pin".to_string(),
                        observed_at: chrono::Utc::now().timestamp(),
                    };
                    self.state.record_event(&event);
                    tracing::error!(
                        "TAMPER: pin mismatch for {}: expected {} actual {} — truncating body",
                        self.url,
                        self.expected_sha256,
                        actual
                    );
                    std::task::Poll::Ready(Some(Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "monopsony pin mismatch: expected {} got {}",
                            self.expected_sha256, actual
                        ),
                    ))))
                }
            }
            std::task::Poll::Ready(Some(Err(e))) => {
                self.finished = true;
                std::task::Poll::Ready(Some(Err(io::Error::other(e.to_string()))))
            }
            std::task::Poll::Pending => std::task::Poll::Pending,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;

    fn test_key(seed: u8) -> SigningKey {
        let mut bytes = [0u8; 32];
        bytes[0] = seed;
        bytes[31] = seed.wrapping_mul(7);
        SigningKey::from_bytes(&bytes)
    }

    fn att_for(url: &str, hash: &str) -> monopsony_common::pin_store::PinAttestation {
        let mut att = monopsony_common::pin_store::PinAttestation {
            url: url.to_string(),
            expected_sha256: hash.to_string(),
            origin: monopsony_common::pin_store::PinOrigin::Header,
            key_id: String::new(),
            observed_at: 1_700_000_000,
            signature: String::new(),
        };
        att.sign(&test_key(3));
        att
    }

    fn tmp_events_path(tag: &str) -> PathBuf {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(format!("{}.jsonl", tag));
        // The PinState borrows nothing; only the path must outlive the test.
        // Leak the tempdir (tests are short; OS cleans /tmp).
        std::mem::forget(dir);
        path
    }

    #[test]
    fn verify_ok_roundtrip() {
        let signer = test_key(3);
        let att = att_for("https://example.com/p.bin", &"ab".repeat(32));
        let pin = PinState::new(
            Some(signer.verifying_key().to_bytes()),
            tmp_events_path("vok"),
        );
        assert!(pin.provisioned());
        let parsed = pin
            .verify_attestation_json(
                &monopsony_common::pin_store::to_json(&att),
                "https://example.com/p.bin",
            )
            .expect("verify ok");
        assert_eq!(parsed.expected_sha256, "ab".repeat(32));
    }

    #[test]
    fn verify_rejects_wrong_key() {
        let signer = test_key(3);
        let other = test_key(4);
        let att = att_for("https://example.com/p.bin", &"ab".repeat(32));
        let pin = PinState::new(
            Some(other.verifying_key().to_bytes()),
            tmp_events_path("vwk"),
        );
        let err = pin
            .verify_attestation_json(
                &monopsony_common::pin_store::to_json(&att),
                "https://example.com/p.bin",
            )
            .expect_err("wrong key must fail");
        assert!(err.contains("signature"));
    }

    #[test]
    fn verify_rejects_url_mismatch() {
        let signer = test_key(5);
        let att = att_for("https://example.com/p.bin", &"ab".repeat(32));
        let pin = PinState::new(
            Some(signer.verifying_key().to_bytes()),
            tmp_events_path("vum"),
        );
        assert!(pin
            .verify_attestation_json(
                &monopsony_common::pin_store::to_json(&att),
                "https://evil.com/p.bin"
            )
            .is_err());
    }

    #[test]
    fn inert_when_unprovisioned() {
        // Zeroed key == not provisioned.
        let pin = PinState::new(Some([0u8; 32]), tmp_events_path("inert"));
        assert!(!pin.provisioned());
        // set_key later provisions (interior mutability).
        let signer = test_key(6);
        pin.set_key(signer.verifying_key().to_bytes());
        assert!(pin.provisioned());
    }

    #[test]
    fn event_spool_roundtrip() {
        let path = tmp_events_path("spool");
        let pin = PinState::new(None, path.clone());
        pin.record_event(&TamperEvent {
            url: "https://a/x".to_string(),
            expected_sha256: "aa".repeat(32),
            actual_sha256: "bb".repeat(32),
            detail: "d1".to_string(),
            observed_at: 111,
        });
        pin.record_event(&TamperEvent {
            url: "https://a/y".to_string(),
            expected_sha256: "cc".repeat(32),
            actual_sha256: "dd".repeat(32),
            detail: "d2".to_string(),
            observed_at: 222,
        });
        let drained = pin.drain_events();
        assert_eq!(drained.len(), 2);
        assert_eq!(drained[0].url, "https://a/x");
        assert_eq!(drained[1].observed_at, 222);
        // File truncated after drain — second drain is empty.
        assert!(pin.drain_events().is_empty());
    }

    #[test]
    fn encode_tamper_alert_fields() {
        let event = TamperEvent {
            url: "https://a/x".to_string(),
            expected_sha256: "aa".repeat(32),
            actual_sha256: "bb".repeat(32),
            detail: "d".to_string(),
            observed_at: 42,
        };
        let alert = encode_tamper_alert(&event);
        assert_eq!(alert.url, event.url);
        assert_eq!(alert.observed_at, 42);
        assert_eq!(alert.expected_sha256, "aa".repeat(32));
    }

    /// Drive the hash-checking stream: matching bytes pass through; a pin
    /// whose hash does not match yields the mismatch error AND an event.
    #[tokio::test]
    async fn stream_wrapper_passes_and_truncates() {
        use futures_util::StreamExt;

        let body = bytes_from_str("hello monopsony pin check");
        let good_hash = hex::encode(Sha256::digest(&body));

        // Case 1: hash matches → all chunks forwarded, clean end.
        let pin_ok = std::sync::Arc::new(PinState::new(None, tmp_events_path("s1")));
        let stream = pin_check_stream(
            futures_util::stream::iter(vec![Ok::<_, io::Error>(body.clone())]),
            pin_ok,
            "https://x/ok.bin".to_string(),
            good_hash.clone(),
        );
        let chunks: Vec<_> = stream.collect().await;
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].is_ok());

        // Case 2: wrong expected hash → forwarded chunk, then error; event spooled.
        let path = tmp_events_path("s2");
        let pin_bad = std::sync::Arc::new(PinState::new(None, path.clone()));
        let stream = pin_check_stream(
            futures_util::stream::iter(vec![Ok::<_, io::Error>(body.clone())]),
            pin_bad.clone(),
            "https://x/bad.bin".to_string(),
            "ee".repeat(32),
        );
        let chunks: Vec<_> = stream.collect().await;
        assert_eq!(chunks.len(), 2);
        assert!(chunks[0].is_ok(), "payload chunk still delivered");
        assert!(chunks[1].is_err(), "mismatch terminates with error");
        let events = pin_bad.drain_events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].actual_sha256, good_hash);
        assert_eq!(events[0].expected_sha256, "ee".repeat(32));
    }

    fn bytes_from_str(s: &str) -> axum::body::Bytes {
        axum::body::Bytes::from(s.as_bytes().to_vec())
    }
}
