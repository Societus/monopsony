//! Canonical hashing for the server-global shadow recipe catalog (Sprint 22, 22-C1).
//!
//! Canonical form: JSON object with sorted keys and no insignificant whitespace,
//! produced by `serde_json::to_value(...)` then `to_string(...)` (serde_json's
//! `Value` maps are BTree-backed, so keys are always sorted).

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use sha2::{Digest, Sha256};

/// Canonical JSON bytes for a catalog entry — the preimage for both the entry
/// hash and its Ed25519 signature.
pub fn canonical_entry_json(name: &str, recipe_toml: &str) -> String {
    let value = serde_json::json!({
        "name": name,
        "recipe_toml": recipe_toml,
    });
    value.to_string()
}

/// SHA-256 over the canonical JSON of `{"name":..., "recipe_toml":...}`,
/// hex-encoded (lowercase).
pub fn entry_hash(name: &str, recipe_toml: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(canonical_entry_json(name, recipe_toml).as_bytes());
    hex::encode(hasher.finalize())
}

/// SHA-256 over the canonical JSON of the SORTED array of entry-hash hex
/// strings, hex-encoded. Sort-only: callers pass hashes already deduped (the
/// catalog table's PK on `name` guarantees uniqueness upstream).
pub fn catalog_hash(entry_hashes: &[String]) -> String {
    let mut sorted: Vec<&String> = entry_hashes.iter().collect();
    sorted.sort();
    let mut hasher = Sha256::new();
    hasher.update(
        serde_json::to_string(&sorted)
            .unwrap_or_else(|_| "[]".to_string())
            .as_bytes(),
    );
    hex::encode(hasher.finalize())
}

/// Sign a catalog entry's canonical JSON with the server recipe key.
/// Returns the base64-encoded Ed25519 signature (64 bytes, `to_bytes()` form).
pub fn sign_entry(
    signing_key: &ed25519_dalek::SigningKey,
    name: &str,
    recipe_toml: &str,
) -> String {
    use ed25519_dalek::Signer;
    let sig = signing_key.sign(canonical_entry_json(name, recipe_toml).as_bytes());
    B64.encode(sig.to_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOLDEN_HASH: &str = "da796477338559e73eaa0258940b671d45918f4ab9c7a0f77a91985cbc5415bf";

    #[test]
    fn golden_entry_hash() {
        let h = entry_hash(
            "ripgrep",
            "[package]\nname = \"ripgrep\"\nversion = \"14.1.1\"\n",
        );
        assert_eq!(h, GOLDEN_HASH);
    }

    #[test]
    fn golden_canonical_json() {
        let c = canonical_entry_json(
            "ripgrep",
            "[package]\nname = \"ripgrep\"\nversion = \"14.1.1\"\n",
        );
        assert_eq!(
            c,
            "{\"name\":\"ripgrep\",\"recipe_toml\":\"[package]\\nname = \\\"ripgrep\\\"\\nversion = \\\"14.1.1\\\"\\n\"}"
        );
    }

    #[test]
    fn catalog_hash_is_order_independent() {
        let a = entry_hash("alpha", "[package]\nname = \"alpha\"\n");
        let b = entry_hash("beta", "[package]\nname = \"beta\"\n");
        let h1 = catalog_hash(&[a.clone(), b.clone()]);
        let h2 = catalog_hash(&[b, a]);
        assert_eq!(h1, h2);
    }

    #[test]
    fn empty_catalog_hash_is_deterministic() {
        assert_eq!(catalog_hash(&[]), catalog_hash(&[]));
        // sha256("[]")
        assert_eq!(
            catalog_hash(&[]),
            "4f53cda18c2baa0c0354bb5f9a3ecbe5ed12ab4d8e11ba873c2f11161202b945"
        );
    }
}
