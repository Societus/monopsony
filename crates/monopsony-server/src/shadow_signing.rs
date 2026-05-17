use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use ed25519_dalek::{Signer, SigningKey, VerifyingKey};
use monopsony_common::shadow_types::{RecipeSignature, ShadowRecipe, SignedRecipe};
use sha2::{Digest, Sha256};

/// Generate a new Ed25519 signing key
pub fn generate_signing_key() -> SigningKey {
    let mut secret = [0u8; 32];
    getrandom::getrandom(&mut secret).expect("failed to generate random key");
    SigningKey::from_bytes(&secret)
}

/// Key ID = first 16 hex chars of SHA-256 of the public key bytes
pub fn key_id(verifying_key: &VerifyingKey) -> String {
    let hash = Sha256::digest(verifying_key.to_bytes());
    hex::encode(&hash[..8])
}

/// Sign a recipe's canonical TOML representation
pub fn sign_recipe(
    recipe_toml: &str,
    recipe: ShadowRecipe,
    signing_key: &SigningKey,
) -> SignedRecipe {
    let signature_bytes = signing_key.sign(recipe_toml.as_bytes());
    let verifying_key = signing_key.verifying_key();
    SignedRecipe {
        recipe_toml: recipe_toml.to_string(),
        recipe,
        operator_signature: RecipeSignature {
            key_id: key_id(&verifying_key),
            signature: B64.encode(signature_bytes.to_bytes()),
            signed_at: chrono::Utc::now(),
        },
        author_signature: None,
    }
}

/// Verify a recipe against a known verifying key
pub fn verify_recipe_with_key(
    signed: &SignedRecipe,
    verifying_key: &VerifyingKey,
) -> Result<(), String> {
    use ed25519_dalek::{Signature, Verifier};
    let sig_bytes: [u8; 64] = B64
        .decode(&signed.operator_signature.signature)
        .map_err(|e| format!("base64 decode: {e}"))?
        .try_into()
        .map_err(|_| "signature wrong length".to_string())?;
    let signature = Signature::from_bytes(&sig_bytes);
    verifying_key
        .verify(signed.recipe_toml.as_bytes(), &signature)
        .map_err(|e| format!("signature verification failed: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use monopsony_common::shadow_types::*;

    fn test_recipe() -> ShadowRecipe {
        ShadowRecipe {
            package: RecipePackage {
                name: "test-pkg".into(),
                description: None,
                homepage: None,
                source_type: ShadowSource::GithubRelease,
                source_url: "https://github.com/example/tool".into(),
            },
            install: RecipeInstall {
                method: InstallMethod::Extract,
                target_dir: "/usr/local/bin".into(),
                chmod: None,
                service: None,
                config_paths: None,
                backup_old: false,
                health_check: None,
                pre_update: None,
                post_update: None,
                on_failure: None,
            },
            asset_selector: None,
            build: None,
            script: None,
            version: RecipeVersion {
                detect_command: "tool --version".into(),
                detect_pattern: "([0-9]+\\.[0-9]+\\.[0-9]+)".into(),
            },
            notifications: None,
            direct_url: None,
        }
    }

    #[test]
    fn sign_and_verify_ok() {
        let signing_key = generate_signing_key();
        let verifying_key = signing_key.verifying_key();
        let recipe = test_recipe();
        let toml_str = "dummy toml";
        let signed = sign_recipe(toml_str, recipe, &signing_key);
        assert!(verify_recipe_with_key(&signed, &verifying_key).is_ok());
    }

    #[test]
    fn tampered_recipe_fails() {
        let signing_key = generate_signing_key();
        let verifying_key = signing_key.verifying_key();
        let mut signed = sign_recipe("original", test_recipe(), &signing_key);
        signed.recipe_toml = "tampered".into();
        assert!(verify_recipe_with_key(&signed, &verifying_key).is_err());
    }

    #[test]
    fn tampered_signature_fails() {
        let signing_key = generate_signing_key();
        let verifying_key = signing_key.verifying_key();
        let mut signed = sign_recipe("original", test_recipe(), &signing_key);
        // Flip a byte in the base64 signature
        let mut sig_bytes = B64.decode(&signed.operator_signature.signature).unwrap();
        sig_bytes[0] ^= 0xff;
        signed.operator_signature.signature = B64.encode(sig_bytes);
        assert!(verify_recipe_with_key(&signed, &verifying_key).is_err());
    }

    #[test]
    fn wrong_key_fails() {
        let signing_key = generate_signing_key();
        let wrong_key = generate_signing_key();
        let wrong_verifying = wrong_key.verifying_key();
        let signed = sign_recipe("original", test_recipe(), &signing_key);
        assert!(verify_recipe_with_key(&signed, &wrong_verifying).is_err());
    }

    #[test]
    fn key_id_deterministic() {
        let key = generate_signing_key();
        let vk = key.verifying_key();
        let id1 = key_id(&vk);
        let id2 = key_id(&vk);
        assert_eq!(id1, id2);
        assert_eq!(id1.len(), 16);
    }
}
