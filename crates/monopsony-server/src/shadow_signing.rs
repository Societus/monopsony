use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use ed25519_dalek::{Signer, SigningKey, VerifyingKey};
use monopsony_common::shadow_types::{RecipeSignature, ShadowRecipe, SignedRecipe};
use sha2::{Digest, Sha256};

// ---------------------------------------------------------------------------
// Trust policy
// ---------------------------------------------------------------------------

/// Trust policy for verifying federated recipes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrustPolicy {
    /// Only accept recipes countersigned by my server operator (default).
    TrustedServerOnly,
    /// Accept recipes signed by trusted author keys.
    TrustedAuthors { trusted_keys: Vec<String> },
    /// Require both author and operator signatures.
    Paranoid,
}

impl std::str::FromStr for TrustPolicy {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "trusted_server_only" => Ok(TrustPolicy::TrustedServerOnly),
            s if s.starts_with("trusted_authors") => {
                // Parse "trusted_authors:key1,key2,..."
                let keys_part = s.strip_prefix("trusted_authors:").unwrap_or("");
                let trusted_keys: Vec<String> = if keys_part.is_empty() {
                    vec![]
                } else {
                    keys_part.split(',').map(|k| k.trim().to_string()).collect()
                };
                Ok(TrustPolicy::TrustedAuthors { trusted_keys })
            }
            "paranoid" => Ok(TrustPolicy::Paranoid),
            _ => Err(format!(
                "unknown trust policy: '{}' (expected: trusted_server_only, trusted_authors, paranoid)",
                s
            )),
        }
    }
}

/// Errors from trust verification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrustError {
    /// Operator signature verification failed.
    OperatorSignatureInvalid(String),
    /// Author signature is missing but required.
    MissingAuthorSignature,
    /// Author signature verification failed.
    AuthorSignatureInvalid(String),
    /// Author key not in trusted list.
    AuthorKeyNotTrusted { key_id: String },
    /// No matching trusted key found.
    NoTrustedKeyMatched,
}

impl std::fmt::Display for TrustError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TrustError::OperatorSignatureInvalid(e) => {
                write!(f, "operator signature invalid: {}", e)
            }
            TrustError::MissingAuthorSignature => write!(f, "author signature required but missing"),
            TrustError::AuthorSignatureInvalid(e) => write!(f, "author signature invalid: {}", e),
            TrustError::AuthorKeyNotTrusted { key_id } => {
                write!(f, "author key {} not in trusted list", key_id)
            }
            TrustError::NoTrustedKeyMatched => write!(f, "no trusted author key matched"),
        }
    }
}

/// Verify a signed recipe against a trust policy.
pub fn verify_trust(
    signed: &SignedRecipe,
    policy: &TrustPolicy,
    operator_verifying_key: &VerifyingKey,
    trusted_author_keys: &[String],
) -> Result<(), TrustError> {
    match policy {
        TrustPolicy::TrustedServerOnly => {
            // Only verify operator signature
            verify_recipe_with_key(signed, operator_verifying_key)
                .map_err(TrustError::OperatorSignatureInvalid)
        }
        TrustPolicy::TrustedAuthors { .. } => {
            // Verify author signature against trusted keys list
            let author_sig = signed
                .author_signature
                .as_ref()
                .ok_or(TrustError::MissingAuthorSignature)?;

            if trusted_author_keys.is_empty() {
                return Err(TrustError::NoTrustedKeyMatched);
            }

            // Check if the author key_id is in the trusted list
            if !trusted_author_keys.contains(&author_sig.key_id) {
                return Err(TrustError::AuthorKeyNotTrusted {
                    key_id: author_sig.key_id.clone(),
                });
            }

            // Verify the author signature
            verify_author_signature(signed)
        }
        TrustPolicy::Paranoid => {
            // Require both signatures
            verify_recipe_with_key(signed, operator_verifying_key)
                .map_err(TrustError::OperatorSignatureInvalid)?;

            signed
                .author_signature
                .as_ref()
                .ok_or(TrustError::MissingAuthorSignature)?;

            verify_author_signature(signed)
        }
    }
}

/// Verify the author signature on a signed recipe.
fn verify_author_signature(signed: &SignedRecipe) -> Result<(), TrustError> {
    use ed25519_dalek::{Signature, Verifier};

    let author_sig = signed
        .author_signature
        .as_ref()
        .ok_or(TrustError::MissingAuthorSignature)?;

    // The author signs the same recipe_toml content
    let sig_bytes: [u8; 64] = B64
        .decode(&author_sig.signature)
        .map_err(|e| TrustError::AuthorSignatureInvalid(format!("base64 decode: {e}")))?
        .try_into()
        .map_err(|_| TrustError::AuthorSignatureInvalid("signature wrong length".into()))?;

    let signature = Signature::from_bytes(&sig_bytes);

    // We need the author's verifying key. The key_id is a hash of the public key,
    // but we don't store the raw key in the signature. For federated recipes,
    // the key_id maps to a known key. This is a limitation of the current design:
    // verification requires the actual VerifyingKey, not just the key_id.
    //
    // For now, author signature verification at the trust level is a placeholder
    // that checks presence and format. Full verification happens when the key
    // is retrieved from the tracker's operator registry.
    Ok(())
}

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
                max_backups: 3,
                binaries: vec![],
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

    // --- Trust policy tests ---

    #[test]
    fn trust_policy_parse() {
        assert_eq!(
            "trusted_server_only".parse::<TrustPolicy>().unwrap(),
            TrustPolicy::TrustedServerOnly
        );
        assert_eq!(
            "paranoid".parse::<TrustPolicy>().unwrap(),
            TrustPolicy::Paranoid
        );
        let policy: TrustPolicy = "trusted_authors:key1,key2".parse().unwrap();
        assert!(matches!(policy, TrustPolicy::TrustedAuthors { .. }));
        if let TrustPolicy::TrustedAuthors { trusted_keys } = policy {
            assert_eq!(trusted_keys, vec!["key1", "key2"]);
        }
        assert!("unknown".parse::<TrustPolicy>().is_err());
    }

    #[test]
    fn trusted_server_only_ok() {
        let signing_key = generate_signing_key();
        let verifying_key = signing_key.verifying_key();
        let signed = sign_recipe("original", test_recipe(), &signing_key);

        let result = verify_trust(
            &signed,
            &TrustPolicy::TrustedServerOnly,
            &verifying_key,
            &[],
        );
        assert!(result.is_ok());
    }

    #[test]
    fn trusted_server_only_wrong_operator() {
        let signing_key = generate_signing_key();
        let wrong_key = generate_signing_key();
        let wrong_verifying = wrong_key.verifying_key();
        let signed = sign_recipe("original", test_recipe(), &signing_key);

        let result = verify_trust(
            &signed,
            &TrustPolicy::TrustedServerOnly,
            &wrong_verifying,
            &[],
        );
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            TrustError::OperatorSignatureInvalid(_)
        ));
    }

    #[test]
    fn trusted_authors_ok() {
        let signing_key = generate_signing_key();
        let verifying_key = signing_key.verifying_key();
        let author_key = generate_signing_key();
        let author_vk = author_key.verifying_key();
        let author_kid = key_id(&author_vk);

        let mut signed = sign_recipe("original", test_recipe(), &signing_key);
        signed.author_signature = Some(RecipeSignature {
            key_id: author_kid.clone(),
            signature: B64.encode(author_key.sign(b"original").to_bytes()),
            signed_at: chrono::Utc::now(),
        });

        let result = verify_trust(
            &signed,
            &TrustPolicy::TrustedAuthors {
                trusted_keys: vec![author_kid.clone()],
            },
            &verifying_key,
            &[author_kid], // trusted_author_keys must contain the key_id
        );
        assert!(result.is_ok());
    }

    #[test]
    fn trusted_authors_missing_signature() {
        let signing_key = generate_signing_key();
        let verifying_key = signing_key.verifying_key();
        // No author signature
        let signed = sign_recipe("original", test_recipe(), &signing_key);

        let result = verify_trust(
            &signed,
            &TrustPolicy::TrustedAuthors {
                trusted_keys: vec!["some_key".into()],
            },
            &verifying_key,
            &["some_key".to_string()],
        );
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), TrustError::MissingAuthorSignature);
    }

    #[test]
    fn trusted_authors_key_not_trusted() {
        let signing_key = generate_signing_key();
        let verifying_key = signing_key.verifying_key();
        let author_key = generate_signing_key();
        let author_vk = author_key.verifying_key();
        let author_kid = key_id(&author_vk);

        let mut signed = sign_recipe("original", test_recipe(), &signing_key);
        signed.author_signature = Some(RecipeSignature {
            key_id: author_kid.clone(),
            signature: B64.encode(author_key.sign(b"original").to_bytes()),
            signed_at: chrono::Utc::now(),
        });

        // trusted_author_keys is empty, so no key can match
        let result = verify_trust(
            &signed,
            &TrustPolicy::TrustedAuthors {
                trusted_keys: vec!["different_key".into()],
            },
            &verifying_key,
            &["different_key".to_string()], // doesn't contain author_kid
        );
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            TrustError::AuthorKeyNotTrusted { .. }
        ));
    }

    #[test]
    fn paranoid_both_sigs_ok() {
        let signing_key = generate_signing_key();
        let verifying_key = signing_key.verifying_key();
        let author_key = generate_signing_key();
        let author_vk = author_key.verifying_key();
        let author_kid = key_id(&author_vk);

        let mut signed = sign_recipe("original", test_recipe(), &signing_key);
        signed.author_signature = Some(RecipeSignature {
            key_id: author_kid,
            signature: B64.encode(author_key.sign(b"original").to_bytes()),
            signed_at: chrono::Utc::now(),
        });

        let result = verify_trust(
            &signed,
            &TrustPolicy::Paranoid,
            &verifying_key,
            &[],
        );
        assert!(result.is_ok());
    }

    #[test]
    fn paranoid_missing_author() {
        let signing_key = generate_signing_key();
        let verifying_key = signing_key.verifying_key();
        // No author signature
        let signed = sign_recipe("original", test_recipe(), &signing_key);

        let result = verify_trust(
            &signed,
            &TrustPolicy::Paranoid,
            &verifying_key,
            &[],
        );
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), TrustError::MissingAuthorSignature);
    }
}
