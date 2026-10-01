//! Signed delta delivery (Sprint 21 slice 21-4a): server-side oracle
//! snapshots signed with Ed25519.
//!
//! The server produces an [`OracleSnapshot`] of observations, serializes it
//! canonically (sorted keys, no whitespace), signs it with a dedicated oracle
//! key, and ships it with the signature so clients can verify without
//! trusting the transport. This key is SEPARATE from the mirror pin key —
//! never reuse MirrorManager keys. The signing seed lives in
//! `oracle-signing.key`.

use std::io::Write;
use std::path::Path;

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};

/// An oracle snapshot: a set of observations plus expiry metadata, signed as
/// a whole. Field order is load-bearing for the canonical golden vector.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OracleSnapshot {
    pub expires_at: i64,
    pub generated_at: i64,
    pub observations: Vec<crate::sourcedb::ObservedVersion>,
    pub scheme_tables: Vec<serde_json::Value>,
    pub version: u32,
}

/// Canonical payload: JSON with sorted object keys and no extra whitespace.
/// `serde_json::Value` maps sort keys by default, so this is byte-for-byte
/// stable; the golden test in this module proves it (if the workspace ever
/// enables `preserve_order`, the test fails loudly — acceptable).
pub fn canonical_payload(s: &OracleSnapshot) -> Result<String, serde_json::Error> {
    serde_json::to_string(&serde_json::to_value(s)?)
}

/// Oracle signing/verifying keypair. `signing` is `None` when constructed
/// from a public key only (verify-only mode).
pub struct OracleKeyPair {
    pub signing: Option<SigningKey>,
    pub verifying: VerifyingKey,
}

/// Load the oracle signing seed from `path`, or generate and persist one.
/// The file holds the 64-hex-char seed. Either way the returned pair's
/// verifying key matches the signing key.
pub fn load_or_generate(path: &Path) -> std::io::Result<OracleKeyPair> {
    let signing = if path.exists() {
        let hex_str = std::fs::read_to_string(path)?;
        let hex_str = hex_str.trim();
        if hex_str.len() != 64 {
            return Err(std::io::Error::other(format!(
                "oracle signing seed must be 64 hex chars, got {}",
                hex_str.len()
            )));
        }
        let mut seed = [0u8; 32];
        decode_hex_into(hex_str, &mut seed)?;
        SigningKey::from_bytes(&seed)
    } else {
        let mut seed = [0u8; 32];
        getrandom::getrandom(&mut seed).map_err(std::io::Error::other)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut f = std::fs::File::create(path)?;
        f.write_all(hex::encode(seed).as_bytes())?;
        SigningKey::from_bytes(&seed)
    };
    let verifying = signing.verifying_key();
    Ok(OracleKeyPair {
        signing: Some(signing),
        verifying,
    })
}

fn decode_hex_into(s: &str, out: &mut [u8]) -> std::io::Result<()> {
    let bytes = hex::decode(s).map_err(|e| std::io::Error::other(e.to_string()))?;
    if bytes.len() != out.len() {
        return Err(std::io::Error::other(format!(
            "expected {} bytes, got {}",
            out.len(),
            bytes.len()
        )));
    }
    out.copy_from_slice(&bytes);
    Ok(())
}

/// Sign the canonical payload of `s` with the oracle signing key; returns
/// the hex-encoded signature.
pub fn sign_snapshot(kp: &OracleKeyPair, s: &OracleSnapshot) -> Result<String, anyhow::Error> {
    let signing = kp
        .signing
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("oracle keypair has no signing key"))?;
    let payload = canonical_payload(s)?;
    let sig = signing.sign(payload.as_bytes());
    Ok(hex::encode(sig.to_bytes()))
}

/// A signed delta as delivered to clients.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignedDelta {
    pub snapshot: OracleSnapshot,
    pub sig_hex: String,
    pub key_id: String,
}

/// Verification failure modes. Expiry is a DISTINCT error (freeze-alarm):
/// a stale-but-validly-signed snapshot is never silently accepted.
#[derive(Debug, PartialEq)]
pub enum DeltaError {
    BadSignature,
    Expired { expired_at: i64, now: i64 },
}

/// Verify a delta: signature over the canonical payload first, then expiry
/// (`now >= expires_at` → Expired).
pub fn verify_delta(
    verifying: &VerifyingKey,
    delta: &SignedDelta,
    now: i64,
) -> Result<(), DeltaError> {
    let payload = canonical_payload(&delta.snapshot).map_err(|_| DeltaError::BadSignature)?;
    let sig_bytes: [u8; 64] = hex::decode(&delta.sig_hex)
        .ok()
        .and_then(|v| <[u8; 64]>::try_from(v).ok())
        .ok_or(DeltaError::BadSignature)?;
    verifying
        .verify(payload.as_bytes(), &Signature::from_bytes(&sig_bytes))
        .map_err(|_| DeltaError::BadSignature)?;
    if now >= delta.snapshot.expires_at {
        return Err(DeltaError::Expired {
            expired_at: delta.snapshot.expires_at,
            now,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sourcedb::ObservedVersion;

    // GOLDEN VECTOR — parent-verified via cryptography sign + openssl verify
    // + negative control. Hardcoded EXACTLY; never recompute.
    const GOLDEN_PRIV_HEX: &str =
        "7d69ba2215aad50abd7898d87b1ac6588177fd82ab70c8762fb82b1efec7f9f2";
    const GOLDEN_PUB_HEX: &str = "2d997aa483a5ab4c033534d2f17df6fe6434c3d0663324d482ece053f79e8dd9";
    const GOLDEN_PAYLOAD: &str = r#"{"expires_at":4102444800,"generated_at":1726000000,"observations":[{"observed_at":1726000000,"observed_by":"seed","package_key":"pacman||ripgrep|x86_64","scheme":"default","source":"aur","version":"14.1.0","version_ord":"0000000000000000:14.1.0:000000"},{"observed_at":1726000001,"observed_by":"seed","package_key":"pacman||ripgrep|x86_64","scheme":"default","source":"github:BurntSushi/ripgrep","version":"14.1.1","version_ord":"0000000000000000:14.1.1:000000"}],"scheme_tables":[],"version":3}"#;
    const GOLDEN_SIG_HEX: &str = "f210c07830a48319709b5f915d70692f412aa68568aad1b8c4df582bc335bbf1df266a814e9ce6d0d1d8f58cf63160bcb98dbbb64e022a6f7d94b83166fa5d09";

    fn golden_priv() -> [u8; 32] {
        let mut seed = [0u8; 32];
        seed.copy_from_slice(&hex::decode(GOLDEN_PRIV_HEX).unwrap());
        seed
    }

    fn golden_pub() -> [u8; 32] {
        let mut pk = [0u8; 32];
        pk.copy_from_slice(&hex::decode(GOLDEN_PUB_HEX).unwrap());
        pk
    }

    fn golden_snapshot() -> OracleSnapshot {
        let obs1 = ObservedVersion {
            package_key: "pacman||ripgrep|x86_64".to_string(),
            source: "aur".to_string(),
            version: "14.1.0".to_string(),
            version_ord: "0000000000000000:14.1.0:000000".to_string(),
            scheme: "default".to_string(),
            observed_at: 1_726_000_000,
            observed_by: "seed".to_string(),
        };
        let obs2 = ObservedVersion {
            package_key: "pacman||ripgrep|x86_64".to_string(),
            source: "github:BurntSushi/ripgrep".to_string(),
            version: "14.1.1".to_string(),
            version_ord: "0000000000000000:14.1.1:000000".to_string(),
            scheme: "default".to_string(),
            observed_at: 1_726_000_001,
            observed_by: "seed".to_string(),
        };
        OracleSnapshot {
            expires_at: 4_102_444_800,
            generated_at: 1_726_000_000,
            observations: vec![obs1, obs2],
            scheme_tables: vec![],
            version: 3,
        }
    }

    // Canonical payload is byte-for-byte the golden string.
    #[test]
    fn test_canonical_payload_golden() {
        let s = golden_snapshot();
        assert_eq!(canonical_payload(&s).unwrap(), GOLDEN_PAYLOAD);
    }

    // Signature with the golden private key over the canonical payload
    // reproduces the golden signature hex exactly.
    #[test]
    fn test_sign_snapshot_golden() {
        let signing = SigningKey::from_bytes(&golden_priv());
        let kp = OracleKeyPair {
            signing: Some(signing),
            verifying: SigningKey::from_bytes(&golden_priv()).verifying_key(),
        };
        let s = golden_snapshot();
        assert_eq!(sign_snapshot(&kp, &s).unwrap(), GOLDEN_SIG_HEX);
    }

    fn golden_delta() -> SignedDelta {
        SignedDelta {
            snapshot: golden_snapshot(),
            sig_hex: GOLDEN_SIG_HEX.to_string(),
            key_id: "oracle-test".to_string(),
        }
    }

    // Valid signature + unexpired -> Ok.
    #[test]
    fn test_verify_delta_ok() {
        let pub_key = VerifyingKey::from_bytes(&golden_pub()).unwrap();
        assert!(verify_delta(&pub_key, &golden_delta(), 1_726_000_100).is_ok());
    }

    // Expired (now >= expires_at) -> DISTINCT Expired error, even though the
    // signature is valid. Freeze-alarm is never silently ignored.
    #[test]
    fn test_verify_delta_expired() {
        let pub_key = VerifyingKey::from_bytes(&golden_pub()).unwrap();
        assert_eq!(
            verify_delta(&pub_key, &golden_delta(), 4_200_000_000),
            Err(DeltaError::Expired {
                expired_at: 4_102_444_800,
                now: 4_200_000_000,
            })
        );
    }

    // Tampering with the snapshot after signing -> BadSignature.
    #[test]
    fn test_verify_delta_tampered() {
        let pub_key = VerifyingKey::from_bytes(&golden_pub()).unwrap();
        let mut delta = golden_delta();
        delta.snapshot.observations[1].version = "14.1.9".to_string();
        assert_eq!(
            verify_delta(&pub_key, &delta, 1_726_000_100),
            Err(DeltaError::BadSignature)
        );
    }

    // Tampered sig_hex -> BadSignature.
    #[test]
    fn test_verify_delta_bad_sig_hex() {
        let pub_key = VerifyingKey::from_bytes(&golden_pub()).unwrap();
        let mut delta = golden_delta();
        delta.sig_hex = "00".to_string() + &GOLDEN_SIG_HEX[2..];
        assert_eq!(
            verify_delta(&pub_key, &delta, 1_726_000_100),
            Err(DeltaError::BadSignature)
        );
    }

    // load_or_generate roundtrip: first call creates the key file, second
    // call reloads the SAME verifying key.
    #[test]
    fn test_load_or_generate_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("keys/oracle-signing.key");
        assert!(!path.exists());
        let kp1 = load_or_generate(&path).unwrap();
        assert!(path.exists());
        let kp2 = load_or_generate(&path).unwrap();
        assert_eq!(kp1.verifying.as_bytes(), kp2.verifying.as_bytes());
    }
}
