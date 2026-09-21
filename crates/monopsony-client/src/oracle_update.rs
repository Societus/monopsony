//! Client-side oracle delta fetch + verify + merge (fully local; no server imports).

use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use std::path::{Path, PathBuf};

/// A single observed-version record from the oracle snapshot.
/// Fields alphabetical so serde_json::Value canonicalization is byte-stable.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ObservedVersionLite {
    pub observed_at: i64,
    pub observed_by: String,
    pub package_key: String,
    pub scheme: String,
    pub source: String,
    pub version: String,
    pub version_ord: String,
}

/// Lite oracle snapshot (client-side mirror of the server type).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct OracleSnapshotLite {
    pub expires_at: i64,
    pub generated_at: i64,
    pub observations: Vec<ObservedVersionLite>,
    pub scheme_tables: Vec<serde_json::Value>,
    pub version: u32,
}

/// Signed delta envelope: snapshot + Ed25519 signature + key id.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct DeltaEnvelope {
    pub key_id: String,
    pub sig: String,
    pub snapshot: OracleSnapshotLite,
}

#[derive(Debug, PartialEq)]
pub enum OracleError {
    BadSignature,
    Expired { expired_at: i64, now: i64 },
    Http(String),
    Parse(String),
}

impl std::fmt::Display for OracleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OracleError::BadSignature => write!(f, "bad signature"),
            OracleError::Expired { expired_at, now } => {
                write!(f, "oracle snapshot expired at {} (now {})", expired_at, now)
            }
            OracleError::Http(s) => write!(f, "oracle fetch error: {}", s),
            OracleError::Parse(s) => write!(f, "oracle parse error: {}", s),
        }
    }
}

impl std::error::Error for OracleError {}

/// Canonical JSON payload: sorted keys, no spaces. Matches the server's signing input.
pub fn canonical_payload(s: &OracleSnapshotLite) -> Result<String, serde_json::Error> {
    serde_json::to_string(&serde_json::to_value(s)?)
}

/// Pin file lives SEPARATELY from mirror-verify.key.
pub fn pin_path(cert_dir: &Path) -> PathBuf {
    cert_dir.join("oracle-verify.key")
}

fn validate_pin(key_hex: &str) -> Result<[u8; 32], ()> {
    let bytes = key_hex.as_bytes();
    if bytes.len() != 64 || !bytes.iter().all(|b| b.is_ascii_hexdigit()) {
        return Err(());
    }
    // Reject uppercase: pin must be lowercase hex exactly.
    if bytes.iter().any(|b| b.is_ascii_uppercase()) {
        return Err(());
    }
    let mut out = [0u8; 32];
    for i in 0..32 {
        out[i] = u8::from_str_radix(&key_hex[i * 2..i * 2 + 2], 16).map_err(|_| ())?;
    }
    Ok(out)
}

/// Store the oracle verify pin (64 lowercase hex chars) to cert_dir/oracle-verify.key.
pub fn store_pin(cert_dir: &Path, key_hex: &str) -> std::io::Result<()> {
    if validate_pin(key_hex).is_err() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "oracle pin must be exactly 64 lowercase hex chars",
        ));
    }
    std::fs::create_dir_all(cert_dir)?;
    std::fs::write(pin_path(cert_dir), key_hex)
}

/// Load + validate the oracle verify pin.
pub fn load_pin(cert_dir: &Path) -> std::io::Result<String> {
    let raw = std::fs::read_to_string(pin_path(cert_dir))?;
    let key_hex = raw.trim().to_string();
    if validate_pin(&key_hex).is_err() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "oracle pin on disk is not 64 lowercase hex chars",
        ));
    }
    Ok(key_hex)
}

fn hex_decode_64(hex_str: &str) -> Option<[u8; 64]> {
    let bytes = hex_str.as_bytes();
    if bytes.len() != 128 {
        return None;
    }
    let mut out = [0u8; 64];
    for i in 0..64 {
        out[i] = u8::from_str_radix(&hex_str[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

/// Verify a signed delta envelope against the pinned pubkey and merge observations.
///
/// Order: parse envelope -> decode pin/sig -> canonical payload -> Ed25519 verify
/// -> expiry check (expired_at <= now is rejected: freeze alarm, never silent).
pub fn verify_and_merge(
    pin_hex: &str,
    delta_json: &str,
    now: i64,
) -> Result<Vec<ObservedVersionLite>, OracleError> {
    let envelope: DeltaEnvelope = serde_json::from_str(delta_json)
        .map_err(|e| OracleError::Parse(format!("envelope: {}", e)))?;

    let pin_bytes: [u8; 32] = hex::decode(pin_hex)
        .ok()
        .and_then(|v| <[u8; 32]>::try_from(v).ok())
        .ok_or(OracleError::BadSignature)?;
    let sig_bytes: [u8; 64] = hex_decode_64(&envelope.sig).ok_or(OracleError::BadSignature)?;

    let payload = canonical_payload(&envelope.snapshot)
        .map_err(|e| OracleError::Parse(format!("canonical: {}", e)))?;

    let vk = VerifyingKey::from_bytes(&pin_bytes).map_err(|_| OracleError::BadSignature)?;
    vk.verify(payload.as_bytes(), &Signature::from_bytes(&sig_bytes))
        .map_err(|_| OracleError::BadSignature)?;

    if now >= envelope.snapshot.expires_at {
        return Err(OracleError::Expired {
            expired_at: envelope.snapshot.expires_at,
            now,
        });
    }

    Ok(envelope.snapshot.observations)
}

/// Fetch the delta JSON over plain GET (NEVER routed through the cache).
pub async fn fetch_delta(client: &reqwest::Client, url: &str) -> Result<String, OracleError> {
    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| OracleError::Http(format!("request: {}", e)))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(OracleError::Http(format!("status {}", status.as_u16())));
    }
    resp.text()
        .await
        .map_err(|e| OracleError::Http(format!("body: {}", e)))
}

#[cfg(test)]
mod tests {
    use super::*;

    // GOLDEN VECTOR — parent-verified (cryptography sign + openssl verify + negative
    // control). Hardcoded exactly; never recompute.
    const GOLDEN_PUB_HEX: &str = "2d997aa483a5ab4c033534d2f17df6fe6434c3d0663324d482ece053f79e8dd9";
    const GOLDEN_PAYLOAD: &str = r#"{"expires_at":4102444800,"generated_at":1726000000,"observations":[{"observed_at":1726000000,"observed_by":"seed","package_key":"pacman||ripgrep|x86_64","scheme":"default","source":"aur","version":"14.1.0","version_ord":"0000000000000000:14.1.0:000000"},{"observed_at":1726000001,"observed_by":"seed","package_key":"pacman||ripgrep|x86_64","scheme":"default","source":"github:BurntSushi/ripgrep","version":"14.1.1","version_ord":"0000000000000000:14.1.1:000000"}],"scheme_tables":[],"version":3}"#;
    const GOLDEN_SIG_HEX: &str = "f210c07830a48319709b5f915d70692f412aa68568aad1b8c4df582bc335bbf1df266a814e9ce6d0d1d8f58cf63160bcb98dbbb64e022a6f7d94b83166fa5d09";

    fn golden_envelope() -> String {
        format!(
            "{{\"key_id\":\"test\",\"sig\":\"{}\",\"snapshot\":{}}}",
            GOLDEN_SIG_HEX, GOLDEN_PAYLOAD
        )
    }

    #[test]
    fn golden_vector_verifies_and_merges() {
        let obs =
            verify_and_merge(GOLDEN_PUB_HEX, &golden_envelope(), 1_726_000_100).expect("verify");
        assert_eq!(obs.len(), 2);
        assert_eq!(obs[0].package_key, "pacman||ripgrep|x86_64");
    }

    #[test]
    fn expired_snapshot_is_distinct_error() {
        let err = verify_and_merge(GOLDEN_PUB_HEX, &golden_envelope(), 4_200_000_000)
            .expect_err("must be expired");
        assert_eq!(
            err,
            OracleError::Expired {
                expired_at: 4_102_444_800,
                now: 4_200_000_000
            }
        );
    }

    #[test]
    fn tampered_signature_is_rejected() {
        let mut sig = GOLDEN_SIG_HEX.to_string();
        let last = sig.pop().unwrap();
        sig.push(if last == '0' { '1' } else { '0' });
        let envelope = format!(
            "{{\"key_id\":\"test\",\"sig\":\"{}\",\"snapshot\":{}}}",
            sig, GOLDEN_PAYLOAD
        );
        let err = verify_and_merge(GOLDEN_PUB_HEX, &envelope, 1_726_000_100)
            .expect_err("tampered sig must fail");
        assert_eq!(err, OracleError::BadSignature);
    }

    #[test]
    fn wrong_pin_is_rejected() {
        let zeros = "0".repeat(64);
        let err = verify_and_merge(&zeros, &golden_envelope(), 1_726_000_100)
            .expect_err("wrong pin must fail");
        assert_eq!(err, OracleError::BadSignature);
    }

    #[test]
    fn pin_store_load_roundtrip_and_validation() {
        let dir = tempfile::tempdir().expect("tempdir");
        store_pin(dir.path(), GOLDEN_PUB_HEX).expect("store");
        let loaded = load_pin(dir.path()).expect("load");
        assert_eq!(loaded, GOLDEN_PUB_HEX);

        // 63-char hex must be rejected on store...
        assert!(store_pin(dir.path(), &"a".repeat(63)).is_err());
        // ...and on load.
        std::fs::write(dir.path().join("oracle-verify.key"), "a".repeat(63)).unwrap();
        assert!(load_pin(dir.path()).is_err());
    }

    #[test]
    fn canonical_payload_matches_golden_bytes() {
        let envelope: DeltaEnvelope = serde_json::from_str(&golden_envelope()).unwrap();
        let canon = canonical_payload(&envelope.snapshot).expect("canonical");
        assert_eq!(canon, GOLDEN_PAYLOAD);
    }
}
