//! Mirror metadata integrity: signature verification + hash policy.
//!
//! ## Verified upstream wire facts (checked live 2026-10-31, full citations in
//! `docs/plans/2026-10-31-mirror-sync-integrity.md`)
//!
//! - **Debian/Ubuntu**: `InRelease` is a clearsigned OpenPGP message over the
//!   Release text (apt-secure(8): `gpg --clearsign -o InRelease Release`);
//!   `Release.gpg` is a detached armored signature over the exact `Release`
//!   bytes (`gpg -abs -o Release.gpg Release`). The Release text carries
//!   `SHA256:` blocks listing hashes of the index files relative to
//!   `dists/<suite>/`. Key trust = pinned keyrings, the apt `signed-by` model.
//! - **Alpine**: `APKINDEX.tar.gz` is two concatenated gzip members. Member 1
//!   is a tar holding a single `.SIGN.<TYPE>.<keyfile>` entry with a raw RSA
//!   signature; the signed payload is the **compressed bytes of member 2**
//!   (proven live: RSA-PKCS1v1.5-SHA1 over member 2's gzip bytes verified
//!   against the official Alpine key). TYPE selects the digest exactly as in
//!   apk-tools `signature_type[]`: `RSA`→SHA1, `RSA256`→SHA256, `RSA512`→
//!   SHA512, `DSA`→DSA (unsupported here). Keys are SPKI PEM
//!   (`PEM_read_bio_PUBKEY` in apk-tools).
//! - **Arch**: repo databases are **unsigned upstream** — `core.db.sig`
//!   returns 404 on all six mirrors checked (geo.mirror.pkgbuild.com,
//!   mirror.pkgbuild.com, mirrors.kernel.org, rackspace, mit, constant).
//!   Package integrity comes from `%CSHA256%` inside the db + cross-mirror
//!   hash comparison; no database signature verification is possible.
//!
//! ## Trust model
//! Per-mirror pinned keys in mirrors.toml (`trusted_keys`), either inline
//! armored text or filesystem paths. No keyserver fetches, no TOFU: an
//! unpinned mirror simply runs without signature verification (checksum
//! sidecars still apply). Mirrors with pinned keys get signature enforcement
//! on both the sync path and the metadata serve path, governed by the
//! hash-verification policy.

use std::io::{Cursor, Read};

use flate2::read::GzDecoder;
use flate2::{Decompress, FlushDecompress, Status};
use rsa::pkcs1v15::{self, VerifyingKey};
use rsa::pkcs8::DecodePublicKey;
use rsa::signature::DigestVerifier;
#[cfg(test)]
use rsa::RsaPrivateKey;
use rsa::RsaPublicKey;
use sha2::{Digest, Sha256, Sha512};
use thiserror::Error;

/// rpgp types (pub use keeps signatures out of the public API surface).
use pgp::composed::{CleartextSignedMessage, Deserializable, DetachedSignature, SignedPublicKey};

#[cfg(test)]
#[path = "integrity_tests.rs"]
mod tests;

/// Hash-verification policy for a mirror's sync and metadata-serve paths.
///
/// - `FailClosed`: a failed verification rejects the object — the file is not
///   written to the mirror, the sync errors, the serve path refuses.
/// - `FailOpen`: the failure is logged (and counted in metrics) but the object
///   is accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HashPolicy {
    #[default]
    FailClosed,
    FailOpen,
}

impl HashPolicy {
    /// Parse a policy string; anything but "fail-open" resolves to the
    /// fail-closed default (typo-safe: an unknown value never silently
    /// disables enforcement).
    pub fn parse(value: &str) -> Self {
        if value.trim() == "fail-open" {
            HashPolicy::FailOpen
        } else {
            HashPolicy::FailClosed
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            HashPolicy::FailClosed => "fail-closed",
            HashPolicy::FailOpen => "fail-open",
        }
    }
}

#[derive(Debug, Error)]
pub enum IntegrityError {
    #[error("no trusted keys pinned for this mirror")]
    NoTrustedKeys,
    #[error("signature verification failed")]
    SignatureMismatch,
    #[error("metadata is not valid UTF-8")]
    NotUtf8,
    #[error("malformed metadata: {0}")]
    Malformed(String),
    #[error("unsupported signature type: {0}")]
    UnsupportedSignatureType(String),
    #[error("key parse error: {0}")]
    KeyParse(String),
    #[error("io error: {0}")]
    Io(String),
}

impl From<std::io::Error> for IntegrityError {
    fn from(e: std::io::Error) -> Self {
        IntegrityError::Io(e.to_string())
    }
}

/// Keys pinned for one mirror, split by wire format.
#[derive(Debug, Default)]
pub struct TrustedKeys {
    /// OpenPGP public keys (Debian/Ubuntu Release / InRelease signatures).
    pub openpgp: Vec<SignedPublicKey>,
    /// Plain RSA public keys in SPKI PEM form (Alpine `.SIGN.RSA.*` keys).
    pub rsa_spki: Vec<RsaPublicKey>,
}

impl TrustedKeys {
    pub fn is_empty(&self) -> bool {
        self.openpgp.is_empty() && self.rsa_spki.is_empty()
    }
}

/// Load a mirror's pinned keys from mirrors.toml entries.
///
/// Each entry is either inline armored key text (starts with `-----BEGIN`) or
/// a path to such a file. OpenPGP keyrings (`BEGIN PGP PUBLIC KEY BLOCK`) and
/// SPKI RSA public keys (`BEGIN PUBLIC KEY`) are auto-detected by content.
pub fn load_trusted_keys(entries: &[String]) -> Result<TrustedKeys, IntegrityError> {
    let mut keys = TrustedKeys::default();
    for entry in entries {
        let trimmed = entry.trim();
        if trimmed.is_empty() {
            continue;
        }
        let content = if trimmed.starts_with("-----BEGIN") {
            trimmed.to_string()
        } else {
            std::fs::read_to_string(trimmed)
                .map_err(|e| IntegrityError::KeyParse(format!("{}: {}", trimmed, e)))?
        };
        if content.contains("BEGIN PGP PUBLIC KEY BLOCK") {
            let (key, _) = SignedPublicKey::from_armor_single(Cursor::new(content.as_bytes()))
                .map_err(|e| IntegrityError::KeyParse(e.to_string()))?;
            keys.openpgp.push(key);
        } else if content.contains("BEGIN PUBLIC KEY") {
            let key = RsaPublicKey::from_public_key_pem(&content)
                .map_err(|e| IntegrityError::KeyParse(e.to_string()))?;
            keys.rsa_spki.push(key);
        } else {
            return Err(IntegrityError::KeyParse(format!(
                "unrecognized key format (expected OpenPGP armored or SPKI PEM): {}",
                entry
            )));
        }
    }
    Ok(keys)
}

/// A Release document whose signature was verified against a pinned key.
#[derive(Debug)]
pub struct VerifiedRelease {
    /// The signed Release text (as normalized by the cleartext framework).
    /// Only this text may be used as a checksum anchor.
    pub release_text: String,
}

/// Verify a clearsigned `InRelease` against the pinned OpenPGP keys.
///
/// Returns the signed text on success; the caller must extract checksums
/// from `release_text` only (armor parts are attacker-influenced).
pub fn verify_clearsigned_release(
    bytes: &[u8],
    keys: &TrustedKeys,
) -> Result<VerifiedRelease, IntegrityError> {
    if keys.openpgp.is_empty() {
        return Err(IntegrityError::NoTrustedKeys);
    }
    let (msg, _) = CleartextSignedMessage::from_armor(Cursor::new(bytes))
        .map_err(|e| IntegrityError::Malformed(format!("InRelease parse: {}", e)))?;
    for key in &keys.openpgp {
        if msg.verify(key).is_ok() {
            return Ok(VerifiedRelease {
                release_text: msg.signed_text(),
            });
        }
    }
    Err(IntegrityError::SignatureMismatch)
}

/// Verify a detached `Release.gpg` (armored) over the exact `Release` bytes.
pub fn verify_detached_release(
    release_bytes: &[u8],
    sig_armored: &[u8],
    keys: &TrustedKeys,
) -> Result<(), IntegrityError> {
    if keys.openpgp.is_empty() {
        return Err(IntegrityError::NoTrustedKeys);
    }
    let (sig, _) = DetachedSignature::from_armor_single(Cursor::new(sig_armored))
        .map_err(|e| IntegrityError::Malformed(format!("Release.gpg parse: {}", e)))?;
    for key in &keys.openpgp {
        if sig.verify(key, release_bytes).is_ok() {
            return Ok(());
        }
    }
    Err(IntegrityError::SignatureMismatch)
}

/// Extract the SHA-256 a verified Release declares for an index file.
///
/// `rel_path` is relative to `dists/<suite>/` (the scope Release checksums
/// are declared in), e.g. `main/binary-amd64/Packages.gz`.
pub fn release_sha256_for(release_text: &str, rel_path: &str) -> Option<String> {
    let mut in_sha256_block = false;
    for line in release_text.lines() {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            in_sha256_block = false;
            continue;
        }
        if !line.starts_with(' ') {
            in_sha256_block = line.split_once(':').map(|(k, _)| k == "SHA256") == Some(true);
            continue;
        }
        if !in_sha256_block {
            continue;
        }
        let mut parts = line.trim_start().split(' ');
        let (hash, _size, path) = match (parts.next(), parts.next(), parts.next()) {
            (Some(h), Some(s), Some(p)) if !h.is_empty() && !s.is_empty() => (h, s, p),
            _ => continue,
        };
        if path == rel_path {
            return Some(hash.to_ascii_lowercase());
        }
    }
    None
}

/// An APKINDEX whose signature verified against a pinned key.
#[derive(Debug)]
pub struct VerifiedApkIndex {
    pub apkindex_text: String,
}

/// Signature types encoded in Alpine `.SIGN.<TYPE>.<keyfile>` member names.
/// Digest mapping mirrors apk-tools `signature_type[]` (src/package.c).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ApkSigType {
    RsaSha1,
    RsaSha256,
    RsaSha512,
    Dsa,
}

fn parse_sign_member(name: &str) -> Option<(ApkSigType, String)> {
    let rest = name.strip_prefix(".SIGN.")?;
    // Order matters: RSA512/RSA256 must be tested before the bare RSA prefix.
    if let Some(keyfile) = rest.strip_prefix("RSA512.") {
        Some((ApkSigType::RsaSha512, keyfile.to_string()))
    } else if let Some(keyfile) = rest.strip_prefix("RSA256.") {
        Some((ApkSigType::RsaSha256, keyfile.to_string()))
    } else if let Some(keyfile) = rest.strip_prefix("RSA.") {
        Some((ApkSigType::RsaSha1, keyfile.to_string()))
    } else if rest.starts_with("DSA.") {
        Some((ApkSigType::Dsa, String::new()))
    } else {
        None
    }
}

/// Locate the byte ranges of concatenated gzip members (RFC 1952).
///
/// apk bundles indexes as multiple gzip members — the signature lives in the
/// first and covers the COMPRESSED bytes of the following members, so member
/// boundaries must be exact. flate2's `GzDecoder` over-reads into its own
/// buffer and cannot report the boundary, so this parses the gzip header,
/// inflates the raw deflate body with `Decompress::total_in` tracking, and
/// skips the 8-byte CRC32+ISIZE trailer per member.
fn gzip_member_ranges(data: &[u8]) -> Result<Vec<(usize, usize)>, IntegrityError> {
    let mut ranges = Vec::new();
    let mut pos = 0usize;
    while data.len() >= pos + 18 {
        if data[pos] != 0x1f || data[pos + 1] != 0x8b || data[pos + 2] != 8 {
            break;
        }
        let flg = data[pos + 3];
        let mut p = pos + 10;
        if flg & 0x04 != 0 {
            if data.len() < p + 2 {
                return Err(IntegrityError::Malformed("truncated gzip FEXTRA".into()));
            }
            let xlen = u16::from_le_bytes([data[p], data[p + 1]]) as usize;
            p += 2 + xlen;
        }
        for flag in [0x08u8, 0x10u8] {
            if flg & flag != 0 {
                while p < data.len() && data[p] != 0 {
                    p += 1;
                }
                p += 1;
            }
        }
        if flg & 0x02 != 0 {
            p += 2;
        }
        if p >= data.len() {
            return Err(IntegrityError::Malformed("truncated gzip header".into()));
        }

        let mut d = Decompress::new(false);
        let mut out = Vec::with_capacity(64 * 1024);
        loop {
            let status = d
                .decompress_vec(&data[p..], &mut out, FlushDecompress::None)
                .map_err(|e| IntegrityError::Malformed(format!("deflate: {}", e)))?;
            if status == Status::StreamEnd {
                break;
            }
            // No stream end yet: make room and feed more input.
            let grew = out.capacity() * 2;
            out.reserve(grew);
            if d.total_in() as usize == data.len() - p && status == Status::Ok {
                return Err(IntegrityError::Malformed(
                    "deflate stream ended without trailer".into(),
                ));
            }
        }
        let end = p + d.total_in() as usize + 8; // + CRC32 + ISIZE
        if end > data.len() {
            return Err(IntegrityError::Malformed(
                "gzip trailer extends past input".into(),
            ));
        }
        ranges.push((pos, end));
        pos = end;
    }
    Ok(ranges)
}

/// Verify a signed Alpine `APKINDEX.tar.gz` against pinned SPKI RSA keys.
///
/// Layout (verified against live Alpine 3.22): member 1 is a gzip member
/// whose tar contains the `.SIGN.*` entry with the raw signature; member 2 is
/// the gzip member whose compressed bytes the signature covers.
pub fn verify_apkindex_tar_gz(
    bytes: &[u8],
    keys: &TrustedKeys,
) -> Result<VerifiedApkIndex, IntegrityError> {
    if keys.rsa_spki.is_empty() {
        return Err(IntegrityError::NoTrustedKeys);
    }

    let members = gzip_member_ranges(bytes)?;
    if members.len() < 2 {
        return Err(IntegrityError::Malformed(
            "APKINDEX.tar.gz: expected two gzip members (signature + index)".to_string(),
        ));
    }
    let member1_raw = &bytes[members[0].0..members[0].1];
    let member2_raw = &bytes[members[1].0..members[1].1];

    // Decompress member 1, then find the signature entry in its tar.
    let mut member1 = Vec::new();
    GzDecoder::new(Cursor::new(member1_raw)).read_to_end(&mut member1)?;
    let mut signature: Option<(ApkSigType, Vec<u8>)> = None;
    {
        let mut archive = tar::Archive::new(&member1[..]);
        for entry in archive.entries()? {
            let mut entry = entry?;
            let name = entry
                .path()?
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            let Some((sig_type, _keyfile)) = parse_sign_member(&name) else {
                continue;
            };
            let mut buf = Vec::new();
            entry.read_to_end(&mut buf)?;
            signature = Some((sig_type, buf));
        }
    }
    let Some((sig_type, sig_bytes)) = signature else {
        return Err(IntegrityError::Malformed(
            "APKINDEX.tar.gz: no .SIGN.* signature member".to_string(),
        ));
    };
    if sig_type == ApkSigType::Dsa {
        return Err(IntegrityError::UnsupportedSignatureType("DSA".to_string()));
    }
    verify_rsa_member2(sig_type, member2_raw, &sig_bytes, &keys.rsa_spki)?;

    // Member 2: decompress and extract the APKINDEX text.
    let mut member2 = Vec::new();
    GzDecoder::new(Cursor::new(member2_raw)).read_to_end(&mut member2)?;
    let mut archive = tar::Archive::new(&member2[..]);
    for entry in archive.entries()? {
        let mut entry = entry?;
        let name = entry
            .path()?
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        if name == "APKINDEX" {
            let mut text = String::new();
            entry.read_to_string(&mut text)?;
            return Ok(VerifiedApkIndex {
                apkindex_text: text,
            });
        }
    }
    Err(IntegrityError::Malformed(
        "APKINDEX.tar.gz: signature member present but no APKINDEX member".to_string(),
    ))
}

/// Extract the APKINDEX text from a bundle WITHOUT signature verification.
///
/// Used when the mirror pins no keys (metric `no_keys`): parse unverified
/// rather than fail the whole sync. Never used after a failed verification
/// under fail-closed policy.
pub fn extract_apkindex_text(bytes: &[u8]) -> Result<String, IntegrityError> {
    let members = gzip_member_ranges(bytes)?;
    if members.is_empty() {
        return Err(IntegrityError::Malformed(
            "APKINDEX.tar.gz: no gzip members".to_string(),
        ));
    }
    // The APKINDEX payload lives in the LAST member (member 1 holds the
    // signature when present).
    let (start, end) = members[members.len() - 1];
    let mut member = Vec::new();
    GzDecoder::new(Cursor::new(&bytes[start..end])).read_to_end(&mut member)?;
    let mut archive = tar::Archive::new(&member[..]);
    for entry in archive.entries()? {
        let mut entry = entry?;
        let name = entry
            .path()?
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        if name == "APKINDEX" {
            let mut text = String::new();
            entry.read_to_string(&mut text)?;
            return Ok(text);
        }
    }
    Err(IntegrityError::Malformed(
        "APKINDEX.tar.gz: no APKINDEX member".to_string(),
    ))
}

/// RSA-PKCS1v1.5 verification of the member-2 compressed bytes, trying every
/// pinned key. Mirrors apk-tools' EVP_VerifyFinal over the mpart digest.
fn verify_rsa_member2(
    sig_type: ApkSigType,
    member2_raw: &[u8],
    sig_bytes: &[u8],
    keys: &[RsaPublicKey],
) -> Result<(), IntegrityError> {
    let sig = pkcs1v15::Signature::try_from(sig_bytes)
        .map_err(|e| IntegrityError::Malformed(format!("signature bytes: {}", e)))?;
    let verified = match sig_type {
        ApkSigType::RsaSha1 => keys.iter().any(|k| {
            let vk = VerifyingKey::<sha1::Sha1>::new(k.clone());
            vk.verify_digest(sha1::Sha1::new_with_prefix(member2_raw), &sig)
                .is_ok()
        }),
        ApkSigType::RsaSha256 => keys.iter().any(|k| {
            let vk = VerifyingKey::<Sha256>::new(k.clone());
            vk.verify_digest(Sha256::new_with_prefix(member2_raw), &sig)
                .is_ok()
        }),
        ApkSigType::RsaSha512 => keys.iter().any(|k| {
            let vk = VerifyingKey::<Sha512>::new(k.clone());
            vk.verify_digest(Sha512::new_with_prefix(member2_raw), &sig)
                .is_ok()
        }),
        ApkSigType::Dsa => unreachable!("filtered by caller"),
    };
    if verified {
        Ok(())
    } else {
        Err(IntegrityError::SignatureMismatch)
    }
}

/// Sign an Alpine-style index member with a fresh RSA-SHA1 key (fixture
/// generation and tests; never used in production paths). Matches the live
/// Alpine scheme: `.SIGN.RSA.<keyfile>` = RSA-PKCS1v1.5-SHA1.
#[cfg(test)]
pub(crate) fn sign_apkindex_for_test(
    member2_raw: &[u8],
    bits: usize,
) -> Result<(RsaPublicKey, Vec<u8>), IntegrityError> {
    use rsa::pkcs1v15::SigningKey;
    use rsa::signature::{RandomizedSigner, SignatureEncoding};

    let mut rng = rand::thread_rng();
    let private = RsaPrivateKey::new(&mut rng, bits)
        .map_err(|e| IntegrityError::Io(format!("rsa keygen: {}", e)))?;
    let public = RsaPublicKey::from(&private);
    let signing = SigningKey::<sha1::Sha1>::new(private);
    let sig = signing.sign_with_rng(&mut rng, member2_raw);
    Ok((public, sig.to_vec()))
}
