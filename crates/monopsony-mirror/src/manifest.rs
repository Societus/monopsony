//! Mirror manifest — signed inventory and health document.
//!
//! Each mirror publishes a signed manifest describing its inventory and health.
//! Served at `GET /.well-known/monopsony/manifest.json`.
//! Signed with the operator's Ed25519 key (reuses shadow packages signing key).

use serde::{Deserialize, Serialize};

/// Signed mirror manifest describing inventory and health.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MirrorManifest {
    pub mirror_id: String,
    pub distros: Vec<DistroManifest>,
    pub generated_at: i64,
    pub total_packages: u64,
    pub total_bytes: u64,
    pub last_sync: Option<i64>,
    pub hash_algorithm: String,
    pub verified_count: u64,
    pub verification_failures: u64,
    pub signature: Option<String>,
    pub public_key: Option<String>,
}

/// Per-distro breakdown within a manifest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DistroManifest {
    pub distro: String,
    pub packages: u64,
    pub total_bytes: u64,
    pub last_sync: Option<i64>,
    pub verified: u64,
    pub failures: u64,
}

impl MirrorManifest {
    /// Create canonical JSON for signing (deterministic serialization).
    /// Excludes signature and public_key fields.
    pub fn canonical_json(&self) -> String {
        let mut copy = self.clone();
        copy.signature = None;
        copy.public_key = None;
        // Sort keys for determinism
        let mut value = serde_json::to_value(&copy).unwrap();
        sort_json_object(&mut value);
        serde_json::to_string(&value).unwrap()
    }

    /// Sign the manifest with an Ed25519 signing key.
    pub fn sign(&mut self, signing_key: &ed25519_dalek::SigningKey) {
        use ed25519_dalek::Signer;
        let canonical = self.canonical_json();
        let signature = signing_key.sign(canonical.as_bytes());
        self.signature = Some(signature.to_bytes().iter().map(|b| format!("{:02x}", b)).collect());
        let verifying = ed25519_dalek::VerifyingKey::from(signing_key);
        self.public_key = Some(verifying.to_bytes().iter().map(|b| format!("{:02x}", b)).collect());
    }

    /// Verify the manifest signature.
    pub fn verify(&self) -> bool {
        use ed25519_dalek::Verifier;
        let sig_hex = match &self.signature {
            Some(s) => s,
            None => return false,
        };
        let pk_hex = match &self.public_key {
            Some(s) => s,
            None => return false,
        };

        let sig_bytes: Vec<u8> = match hex_decode(sig_hex) {
            Some(b) if b.len() == 64 => b,
            _ => return false,
        };
        let pk_bytes: Vec<u8> = match hex_decode(pk_hex) {
            Some(b) if b.len() == 32 => b,
            _ => return false,
        };

        let verifying_key = match ed25519_dalek::VerifyingKey::from_bytes(
            <&[u8; 32]>::try_from(pk_bytes.as_slice()).unwrap(),
        ) {
            Ok(k) => k,
            Err(_) => return false,
        };

        let signature = ed25519_dalek::Signature::from_bytes(
            &<[u8; 64]>::try_from(sig_bytes.as_slice()).unwrap(),
        );

        let canonical = self.canonical_json();
        verifying_key.verify(canonical.as_bytes(), &signature).is_ok()
    }
}

/// Sort JSON object keys recursively for canonical serialization.
fn sort_json_object(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            let mut sorted: Vec<_> = map.iter_mut().collect();
            sorted.sort_by(|a, b| a.0.cmp(b.0));
            for (_, v) in sorted.iter_mut() {
                sort_json_object(v);
            }
            // Rebuild the map sorted
            let new_map: serde_json::Map<String, serde_json::Value> = sorted
                .into_iter()
                .map(|(k, v)| (k.clone(), v.take()))
                .collect();
            *map = new_map;
        }
        serde_json::Value::Array(arr) => {
            for v in arr.iter_mut() {
                sort_json_object(v);
            }
        }
        _ => {}
    }
}

fn hex_decode(hex: &str) -> Option<Vec<u8>> {
    if !hex.len().is_multiple_of(2) {
        return None;
    }
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_test_manifest() -> MirrorManifest {
        MirrorManifest {
            mirror_id: "test-mirror".to_string(),
            distros: vec![DistroManifest {
                distro: "archlinux".to_string(),
                packages: 12000,
                total_bytes: 500_000_000_000,
                last_sync: Some(1700000000),
                verified: 11900,
                failures: 5,
            }],
            generated_at: 1700001000,
            total_packages: 12000,
            total_bytes: 500_000_000_000,
            last_sync: Some(1700000000),
            hash_algorithm: "sha256".to_string(),
            verified_count: 11900,
            verification_failures: 5,
            signature: None,
            public_key: None,
        }
    }

    #[test]
    fn test_sign_and_verify() {
        let signing_key = ed25519_dalek::SigningKey::generate(&mut rand::thread_rng());

        let mut manifest = make_test_manifest();
        assert!(manifest.signature.is_none());
        assert!(!manifest.verify());

        manifest.sign(&signing_key);
        assert!(manifest.signature.is_some());
        assert!(manifest.public_key.is_some());
        assert!(manifest.verify());
    }

    #[test]
    fn test_tamper_fails_verification() {
        let signing_key = ed25519_dalek::SigningKey::generate(&mut rand::thread_rng());
        let mut manifest = make_test_manifest();
        manifest.sign(&signing_key);
        assert!(manifest.verify());

        // Tamper
        manifest.total_packages = 999;
        assert!(!manifest.verify());
    }

    #[test]
    fn test_no_signature_fails() {
        let manifest = make_test_manifest();
        assert!(!manifest.verify());
    }

    #[test]
    fn test_canonical_json_deterministic() {
        let manifest = make_test_manifest();
        let json1 = manifest.canonical_json();
        let json2 = manifest.canonical_json();
        assert_eq!(json1, json2);
    }
}
