//! Pin store — origin-integrity attestations for cached objects (Sprint 20 Phase 3).
//!
//! At spool-commit time the proxy captures the origin-declared checksum for the
//! fetched object (from PM index semantics, sidecar files, or explicit headers),
//! wraps it in a canonical-JSON attestation, and signs it with the server's
//! mirror/pin Ed25519 key (NEVER the mTLS pairing CA). Attestations are stored
//! alongside cache metadata and re-served as `X-Monopsony-Pin` headers so the
//! client proxy can verify the object hash before handing it to the package
//! manager.
//!
//! Origin classes (Phase 3 scope — plan §5):
//! - `index`         — hash declared by the PM's signed index for this URL
//! - `sidecar`       — sibling checksum file fetched from the same origin
//!                     (`.sha256` / `.sha512`)
//! - `header`        — origin checksum header (`x-checksum-sha256` et al.)
//! - `shim`          — shim-declared checksum (winget InstallerSha256, scoop
//!                     bucket hash, choco nuspec checksum) passed at sync time
//! - `unverified`    — no origin integrity source found; pin records the
//!                     commit-time hash as an observation only (policy-gated)
//!
//! Trust note (plan §9): a compromised server can sign lies with its own pin
//! key. The pin store defends the CACHE path (bit rot, cache poisoning,
//! MITM between origin and cache), not a fully compromised server.

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Where the pinned hash came from. Serialized as a string in attestations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PinOrigin {
    /// Hash declared by the package manager's (signed) index for this URL.
    Index,
    /// Sibling checksum file from the same origin (`.sha256` / `.sha512`).
    Sidecar,
    /// Origin checksum header (`x-checksum-sha256` and friends).
    Header,
    /// Shim-declared checksum (winget InstallerSha256 / scoop hash / choco
    /// nuspec checksum) captured at sync time.
    Shim,
    /// No origin source. `expected_sha256` is the commit-time observation —
    /// useful as a tamper-evident record, NOT as an origin guarantee.
    Unverified,
}

impl PinOrigin {
    pub fn as_str(&self) -> &'static str {
        match self {
            PinOrigin::Index => "index",
            PinOrigin::Sidecar => "sidecar",
            PinOrigin::Header => "header",
            PinOrigin::Shim => "shim",
            PinOrigin::Unverified => "unverified",
        }
    }
}

/// A signed pin attestation for one cached object.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PinAttestation {
    /// Canonical cache URL (scheme-normalized, same key as cache_entries).
    pub url: String,
    /// Hex sha256 the origin declares for this object.
    pub expected_sha256: String,
    /// Origin class of the pinned hash.
    pub origin: PinOrigin,
    /// Key id (hex of SHA-256 over the verifying key) — lets clients pin and
    /// rotate keys without a second channel.
    pub key_id: String,
    /// Unix seconds when the pin was captured.
    pub observed_at: i64,
    /// Ed25519 signature over `canonical_json()` (hex).
    pub signature: String,
}

impl PinAttestation {
    /// Canonical JSON — the exact byte string that is signed/verified.
    /// Field order fixed; signature field excluded.
    pub fn canonical_json(&self) -> String {
        format!(
            concat!(
                r#"{{"expected_sha256":"{}","#,
                r#""key_id":"{}","#,
                r#""origin":"{}","#,
                r#""observed_at":{},"#,
                r#""url":"{}"}}"#
            ),
            self.expected_sha256,
            self.key_id,
            self.origin.as_str(),
            self.observed_at,
            self.url,
        )
    }

    /// Sign this attestation in place with the server's pin key.
    pub fn sign(&mut self, signing_key: &SigningKey) {
        self.key_id = key_id(&signing_key.verifying_key());
        let sig = signing_key.sign(self.canonical_json().as_bytes());
        self.signature = hex::encode(sig.to_bytes());
    }

    /// Verify the signature against a client-pinned verifying key.
    /// Checks BOTH signature validity and key-id agreement.
    pub fn verify(&self, verifying_key: &VerifyingKey) -> bool {
        if self.signature.len() != 128 {
            return false;
        }
        let Ok(sig_bytes) = hex::decode(&self.signature) else {
            return false;
        };
        let Ok(sig) = Signature::from_slice(&sig_bytes) else {
            return false;
        };
        if key_id(verifying_key) != self.key_id {
            return false;
        }
        verifying_key
            .verify(self.canonical_json().as_bytes(), &sig)
            .is_ok()
    }
}

/// Key id: hex SHA-256 over the 32-byte verifying key (first 16 hex chars).
pub fn key_id(verifying_key: &VerifyingKey) -> String {
    let mut hasher = Sha256::new();
    hasher.update(verifying_key.as_bytes());
    let digest = hasher.finalize();
    hex::encode(&digest[..8])
}

/// Serialize an attestation to the on-disk JSON form.
pub fn to_json(attestation: &PinAttestation) -> String {
    serde_json::to_string(attestation).expect("attestation serializes")
}

/// Parse an attestation from the on-disk JSON form or an X-Monopsony-Pin header.
pub fn from_json(data: &str) -> Option<PinAttestation> {
    serde_json::from_str(data).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;

    /// Deterministic test keys — no rand dependency needed in this crate.
    fn test_key(seed: u8) -> SigningKey {
        let mut bytes = [0u8; 32];
        bytes[0] = seed;
        bytes[31] = seed.wrapping_mul(7);
        SigningKey::from_bytes(&bytes)
    }

    fn sample_attestation(key: &SigningKey) -> PinAttestation {
        let mut att = PinAttestation {
            url: "https://mirror.example.com/repo/pkg-1.0.pkg.tar.zst".to_string(),
            expected_sha256: "aa".repeat(32),
            origin: PinOrigin::Index,
            key_id: String::new(),
            observed_at: 1_700_000_000,
            signature: String::new(),
        };
        att.sign(key);
        att
    }

    #[test]
    fn sign_then_verify_roundtrip() {
        let key = test_key(1);
        let att = sample_attestation(&key);
        assert!(att.verify(&key.verifying_key()));
    }

    #[test]
    fn verify_rejects_tampered_url() {
        let key = test_key(1);
        let mut att = sample_attestation(&key);
        att.url = "https://evil.example.com/pkg-1.0.pkg.tar.zst".to_string();
        assert!(!att.verify(&key.verifying_key()));
    }

    #[test]
    fn verify_rejects_tampered_hash() {
        let key = test_key(1);
        let mut att = sample_attestation(&key);
        att.expected_sha256 = "bb".repeat(32);
        assert!(!att.verify(&key.verifying_key()));
    }

    #[test]
    fn verify_rejects_wrong_key() {
        let signer = test_key(1);
        let other = test_key(2);
        let att = sample_attestation(&signer);
        assert!(!att.verify(&other.verifying_key()));
    }

    #[test]
    fn verify_rejects_key_id_mismatch() {
        let key = test_key(1);
        let mut att = sample_attestation(&key);
        att.key_id = "deadbeefdeadbeef".to_string();
        assert!(!att.verify(&key.verifying_key()));
    }

    #[test]
    fn canonical_json_is_stable() {
        let key = test_key(1);
        let att = sample_attestation(&key);
        let a = att.canonical_json();
        let mut att2 = att.clone();
        att2.sign(&key);
        assert_eq!(a, att2.canonical_json());
        assert!(a.contains(r#""origin":"index""#));
        assert!(!a.contains("signature"));
    }

    #[test]
    fn json_roundtrip() {
        let key = test_key(1);
        let att = sample_attestation(&key);
        let text = to_json(&att);
        let parsed = from_json(&text).expect("parses");
        assert_eq!(parsed.url, att.url);
        assert_eq!(parsed.expected_sha256, att.expected_sha256);
        assert!(parsed.verify(&key.verifying_key()));
    }

    #[test]
    fn origin_string_mapping() {
        assert_eq!(PinOrigin::Unverified.as_str(), "unverified");
        assert_eq!(PinOrigin::Shim.as_str(), "shim");
    }
}
