//! Client-side trust enforcement for federated recipes.
#![allow(dead_code)]
//!
//! Before executing any recipe (local or federated), the client verifies
//! the recipe's signature against its configured trust policy.

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use rustls_pemfile::certs;
use sha2::{Digest, Sha256};
use std::path::Path;
use tracing::{debug, warn};

use monopsony_common::config::ShadowPackageTrustConfig;
#[cfg(test)]
use monopsony_common::shadow_types::RecipeSignature;
use monopsony_common::shadow_types::SignedRecipe;
use x509_parser::parse_x509_certificate;

/// Verify a signed recipe against the client's trust policy.
///
/// Returns Ok(()) if the recipe passes trust verification.
/// Returns Err with a human-readable message explaining the failure.
pub fn verify_recipe_trust(
    signed: &SignedRecipe,
    trust_config: &ShadowPackageTrustConfig,
    operator_verifying_key: Option<&ed25519_dalek::VerifyingKey>,
) -> Result<()> {
    let policy = &trust_config.policy;

    match policy.as_str() {
        "trusted_server_only" => {
            // Only verify operator signature
            let vk = operator_verifying_key.context(
                "trust policy 'trusted_server_only' requires an operator verifying key, \
                 but none is configured",
            )?;
            verify_operator_signature(signed, vk)?;
            debug!("trust check passed: trusted_server_only");
            Ok(())
        }
        "paranoid" => {
            // Require both operator and author signatures
            let vk = operator_verifying_key
                .context("trust policy 'paranoid' requires an operator verifying key")?;
            verify_operator_signature(signed, vk)?;
            verify_author_signature_present(signed, &trust_config.trusted_author_keys)?;
            debug!("trust check passed: paranoid (both signatures verified)");
            Ok(())
        }
        p if p.starts_with("trusted_authors") => {
            // Verify author signature against trusted keys list
            verify_author_signature_present(signed, &trust_config.trusted_author_keys)?;
            debug!("trust check passed: trusted_authors (author key verified)");
            Ok(())
        }
        other => {
            warn!("unknown trust policy '{}', rejecting recipe", other);
            bail!("unknown trust policy: '{}'", other)
        }
    }
}

/// Verify the operator signature on a signed recipe.
fn verify_operator_signature(
    signed: &SignedRecipe,
    verifying_key: &ed25519_dalek::VerifyingKey,
) -> Result<()> {
    use base64::Engine;
    use ed25519_dalek::{Signature, Verifier};

    let sig_bytes: Vec<u8> = base64::engine::general_purpose::STANDARD
        .decode(&signed.operator_signature.signature)
        .context("operator signature: base64 decode failed")?;

    let sig_bytes: [u8; 64] = sig_bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("operator signature: wrong length"))?;

    let signature = Signature::from_bytes(&sig_bytes);
    verifying_key
        .verify(signed.recipe_toml.as_bytes(), &signature)
        .context("operator signature verification failed")?;

    Ok(())
}

/// Verify that an author signature is present and the key is trusted.
fn verify_author_signature_present(
    signed: &SignedRecipe,
    trusted_author_keys: &[String],
) -> Result<()> {
    let author_sig = signed
        .author_signature
        .as_ref()
        .context("author signature required but missing")?;

    if trusted_author_keys.is_empty() {
        bail!("no trusted author keys configured — cannot verify author signature");
    }

    if !trusted_author_keys.contains(&author_sig.key_id) {
        bail!(
            "author key '{}' is not in the trusted keys list",
            author_sig.key_id
        );
    }

    Ok(())
}

/// ── Client certificate trust state ───────────────────────────────────────
///
/// The daemon tracks the paired client's mTLS certificate expiry and
/// automatically re-issues inside the 30-day renewal window (P1 cert
/// lifecycle). These helpers parse the expiry and hex serial of a PEM-encoded
/// client certificate using the same serial encoding the server assigns
/// (`format!("{:x}", serial)` in `TlsManager::generate_client_cert`).

/// Parse the `not_after` expiry of the first certificate in a PEM bundle.
///
/// Returns `None` when the bundle holds no parseable certificate. The daemon
/// uses this to decide whether a paired client's cert has entered the
/// 30-day renewal window.
pub fn client_cert_expiry(cert_pem: &[u8]) -> Option<DateTime<Utc>> {
    let der = first_cert_der(cert_pem)?;
    let (_rem, cert) = parse_x509_certificate(&der).ok()?;
    let expires_at = cert.validity().not_after.timestamp();
    DateTime::<Utc>::from_timestamp(expires_at, 0)
}

/// Parse the hex serial of the first certificate in a PEM bundle.
///
/// Sent by the client as `RenewClientCertRequest.current_serial` so the
/// server can bind the renewal request to the presented mTLS certificate.
pub fn client_cert_serial_hex(cert_pem: &[u8]) -> Option<String> {
    let der = first_cert_der(cert_pem)?;
    let (_rem, cert) = parse_x509_certificate(&der).ok()?;
    let bytes = cert.serial.to_bytes_be();
    let mut val: u64 = 0;
    for b in &bytes {
        val = (val << 8) | (*b as u64);
    }
    Some(format!("{:x}", val))
}

/// First DER-encoded certificate found in a PEM bundle.
fn first_cert_der(pem: &[u8]) -> Option<Vec<u8>> {
    let mut cursor = std::io::Cursor::new(pem);
    let mut cert_iter = certs(&mut cursor);
    match cert_iter.next() {
        Some(Ok(cert)) => Some(cert.as_ref().to_vec()),
        _ => None,
    }
}

/// SHA-256 fingerprint of a CA certificate PEM, as colon-separated hex
/// (e.g. `a1:b2:c3:...`). Used for TOFU pinning: the client stores this on
/// first successful collect and warns if it changes on subsequent contacts.
pub fn ca_fingerprint(ca_cert_pem: &[u8]) -> Option<String> {
    let der = first_cert_der(ca_cert_pem)?;
    let hash = sha2::Sha256::digest(&der);
    Some(hash.iter().map(|b| format!("{:02x}", b)).collect::<Vec<_>>().join(":"))
}

/// Store the CA fingerprint for TOFU pinning in `cert_dir/ca.fingerprint`.
pub fn store_ca_fingerprint(cert_dir: &Path, fingerprint: &str) -> anyhow::Result<()> {
    std::fs::create_dir_all(cert_dir)?;
    std::fs::write(cert_dir.join("ca.fingerprint"), fingerprint)?;
    Ok(())
}

/// Load the stored CA fingerprint from `cert_dir/ca.fingerprint`.
pub fn load_ca_fingerprint(cert_dir: &Path) -> Option<String> {
    std::fs::read_to_string(cert_dir.join("ca.fingerprint")).ok()
}

/// Verify the current CA fingerprint against the stored TOFU fingerprint.
/// Returns Ok(()) when no fingerprint is stored (first touch = trust on first
/// use) or when they match. Returns Err when they differ — possible CA
/// rotation or MITM — so the user can decide whether to accept the new CA.
pub fn check_ca_fingerprint(
    cert_dir: &Path,
    current_fingerprint: &str,
) -> anyhow::Result<()> {
    match load_ca_fingerprint(cert_dir) {
        None => Ok(()),
        Some(stored) if stored == current_fingerprint => Ok(()),
        Some(stored) => Err(anyhow::anyhow!(
            "CA fingerprint changed: stored={}, current={}. \
             Possible CA rotation or MITM — verify out of band before continuing.",
            stored, current_fingerprint
        )),
    }
}

#[allow(dead_code)]
fn base64_engine() -> base64::engine::GeneralPurpose {
    base64::engine::GeneralPurpose::new(
        &base64::alphabet::STANDARD,
        base64::engine::general_purpose::PAD,
    )
}

fn key_id_from_vk(vk: &ed25519_dalek::VerifyingKey) -> String {
    let bytes = vk.to_bytes();
    let chunk: &[u8; 8] = bytes.first_chunk::<8>().unwrap();
    chunk.iter().map(|b| format!("{:02x}", b)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use ed25519_dalek::Signer;
    use monopsony_common::shadow_types::ShadowRecipe;

    fn test_signing_key() -> ed25519_dalek::SigningKey {
        ed25519_dalek::SigningKey::generate(&mut rand::thread_rng())
    }

    fn make_signed_recipe(
        signing_key: &ed25519_dalek::SigningKey,
        author_sig: Option<RecipeSignature>,
    ) -> SignedRecipe {
        let recipe_toml = "[package]\nname = \"test\"\nsource_type = \"github_release\"\nsource_url = \"https://example.com/test\"\n\n[install]\nmethod = \"extract\"\ntarget_dir = \"/tmp/test\"\n\n[version]\ndetect_command = \"test --version\"\ndetect_pattern = \"(.+)\"";
        let recipe: ShadowRecipe = toml::from_str(recipe_toml).unwrap();
        let sig = signing_key.sign(recipe_toml.as_bytes());
        let vk = signing_key.verifying_key();
        let kid = key_id_from_vk(&vk);
        let b64 = base64_engine();

        SignedRecipe {
            recipe_toml: recipe_toml.to_string(),
            recipe,
            operator_signature: RecipeSignature {
                key_id: kid,
                signature: b64.encode(sig.to_bytes()),
                signed_at: chrono::Utc::now(),
            },
            author_signature: author_sig,
        }
    }

    fn default_trust() -> ShadowPackageTrustConfig {
        ShadowPackageTrustConfig::default()
    }

    #[test]
    fn trusted_server_only_ok() {
        let key = test_signing_key();
        let vk = key.verifying_key();
        let signed = make_signed_recipe(&key, None);

        let result = verify_recipe_trust(&signed, &default_trust(), Some(&vk));
        assert!(result.is_ok());
    }

    #[test]
    fn trusted_server_only_wrong_key() {
        let key = test_signing_key();
        let wrong_key = test_signing_key();
        let wrong_vk = wrong_key.verifying_key();
        let signed = make_signed_recipe(&key, None);

        let result = verify_recipe_trust(&signed, &default_trust(), Some(&wrong_vk));
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("verification failed"));
    }

    #[test]
    fn trusted_server_only_no_key_configured() {
        let key = test_signing_key();
        let signed = make_signed_recipe(&key, None);

        let result = verify_recipe_trust(&signed, &default_trust(), None);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("operator verifying key"));
    }

    #[test]
    fn paranoid_ok() {
        let key = test_signing_key();
        let vk = key.verifying_key();
        let author_key = test_signing_key();
        let author_vk = author_key.verifying_key();
        let author_kid = key_id_from_vk(&author_vk);

        let author_sig = RecipeSignature {
            key_id: author_kid.clone(),
            signature: base64_engine()
                .encode(author_key.sign(b"[package]\nname = \"test\"").to_bytes()),
            signed_at: chrono::Utc::now(),
        };
        let signed = make_signed_recipe(&key, Some(author_sig));

        let trust = ShadowPackageTrustConfig {
            policy: "paranoid".to_string(),
            trusted_author_keys: vec![author_kid],
            trusted_operator_keys: vec![],
            operator_verifying_key_path: None,
            accept_remote_assignment: false,
        };

        let result = verify_recipe_trust(&signed, &trust, Some(&vk));
        assert!(result.is_ok());
    }

    #[test]
    fn paranoid_missing_author() {
        let key = test_signing_key();
        let vk = key.verifying_key();
        let signed = make_signed_recipe(&key, None); // No author sig

        let trust = ShadowPackageTrustConfig {
            policy: "paranoid".to_string(),
            trusted_author_keys: vec!["somekey".into()],
            trusted_operator_keys: vec![],
            operator_verifying_key_path: None,
            accept_remote_assignment: false,
        };

        let result = verify_recipe_trust(&signed, &trust, Some(&vk));
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("author signature required"));
    }

    #[test]
    fn trusted_authors_ok() {
        let key = test_signing_key();
        let author_key = test_signing_key();
        let author_vk = author_key.verifying_key();
        let author_kid = key_id_from_vk(&author_vk);

        let author_sig = RecipeSignature {
            key_id: author_kid.clone(),
            signature: base64_engine().encode(author_key.sign(b"test").to_bytes()),
            signed_at: chrono::Utc::now(),
        };
        let signed = make_signed_recipe(&key, Some(author_sig));

        let trust = ShadowPackageTrustConfig {
            policy: "trusted_authors".to_string(),
            trusted_author_keys: vec![author_kid],
            trusted_operator_keys: vec![],
            operator_verifying_key_path: None,
            accept_remote_assignment: false,
        };

        let result = verify_recipe_trust(&signed, &trust, None);
        assert!(result.is_ok());
    }

    #[test]
    fn trusted_authors_key_not_in_list() {
        let key = test_signing_key();
        let author_key = test_signing_key();
        let author_vk = author_key.verifying_key();
        let author_kid = key_id_from_vk(&author_vk);

        let author_sig = RecipeSignature {
            key_id: author_kid,
            signature: base64_engine().encode(author_key.sign(b"test").to_bytes()),
            signed_at: chrono::Utc::now(),
        };
        let signed = make_signed_recipe(&key, Some(author_sig));

        let trust = ShadowPackageTrustConfig {
            policy: "trusted_authors".to_string(),
            trusted_author_keys: vec!["different_key".into()],
            trusted_operator_keys: vec![],
            operator_verifying_key_path: None,
            accept_remote_assignment: false,
        };

        let result = verify_recipe_trust(&signed, &trust, None);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("not in the trusted keys list"));
    }

    #[test]
    fn unknown_policy_rejected() {
        let key = test_signing_key();
        let vk = key.verifying_key();
        let signed = make_signed_recipe(&key, None);

        let trust = ShadowPackageTrustConfig {
            policy: "unknown_policy".to_string(),
            trusted_author_keys: vec![],
            trusted_operator_keys: vec![],
            operator_verifying_key_path: None,
            accept_remote_assignment: false,
        };

        let result = verify_recipe_trust(&signed, &trust, Some(&vk));
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("unknown trust policy"));
    }

    /// Generate a self-signed client cert PEM: `validity_days` from now,
    /// serial `serial` (encoded as big-endian bytes, hex-formatted server-side).
    fn renewal_test_cert(validity_days: i64, serial: u64) -> Vec<u8> {
        use rcgen::{CertificateParams, KeyPair, SerialNumber};
        use std::time::{Duration, SystemTime};

        let mut params = CertificateParams::new(vec!["renewal-test.local".to_string()]).unwrap();
        params.serial_number = Some(SerialNumber::from_slice(&serial.to_be_bytes()));
        let not_after = SystemTime::now() + Duration::from_secs(validity_days as u64 * 86_400);
        params.not_after = not_after.into();
        let key = KeyPair::generate().unwrap();
        let cert = params.self_signed(&key).unwrap();
        cert.pem().into_bytes()
    }

    #[test]
    fn client_cert_expiry_parses_not_after() {
        let cert_pem = renewal_test_cert(10, 42);
        let expiry = super::client_cert_expiry(&cert_pem).expect("cert should parse");
        let expected = Utc::now() + chrono::Duration::days(10);
        assert!(
            (expiry - expected).num_seconds().abs() < 60,
            "expiry should be ~now+10d, got {expiry}"
        );
    }

    #[test]
    fn client_cert_serial_hex_matches_server_encoding() {
        let cert_pem = renewal_test_cert(10, 42);
        let serial = super::client_cert_serial_hex(&cert_pem).expect("serial should parse");
        assert_eq!(serial, "2a", "serial 42 must encode as hex '2a'");
    }

    #[test]
    fn client_cert_expiry_rejects_invalid_input() {
        assert!(super::client_cert_expiry(b"not a certificate").is_none());
        assert!(super::client_cert_expiry(b"").is_none());
        assert!(super::client_cert_serial_hex(b"not a certificate").is_none());
    }
}
