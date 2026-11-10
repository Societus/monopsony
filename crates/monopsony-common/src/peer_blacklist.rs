//! Peer-exchange blacklist propagation (Approach A).
//!
//! Signed ban/retract entries that travel inside
//! `GET /.well-known/monopsony/peers` responses. Design:
//! `docs/plans/2026-10-31-peer-blacklist-propagation.md` (Approach A picked
//! 2026-10-31, open questions 1–8 at documented defaults).
//!
//! Entry format follows the in-tree Ed25519 canonical-JSON house style
//! (`monopsony-mirror/src/manifest.rs`): the signature covers the compact
//! JSON of the entry with recursively sorted object keys, excluding the
//! `signature` field itself; signatures are hex-encoded Ed25519. Unlike the
//! self-referential manifest verify, receivers MUST verify against a PINNED
//! originator key (apt `signed-by` style — no TOFU, no keyserver): an entry
//! is only trusted when its `originator` is in the operator-pinned set.
//!
//! Revocation: a `retract` entry signed by the SAME originator at a higher
//! `seq` beats any lower-seq ban without clock trust. Receivers keep, per
//! `(originator, subject)`, the highest-seq entry; retractions are retained
//! as tombstones (default 7 days) so gossip lag within the window cannot
//! resurrect a ban. Because tombstones are pruned (and the store cap can
//! evict sooner), receivers ALSO keep a per-`(originator, subject)`
//! high-water seq marker (`BlacklistHighWater`) that survives pruning: any
//! replayed entry with `seq <=` the marker is stale, marker or not.

use std::collections::HashMap;

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};

/// Default cap on entries advertised in one peers response (design Q3).
pub const DEFAULT_MAX_ENTRIES: usize = 64;

/// Default tombstone retention for retractions in days (design Q4).
pub const DEFAULT_TOMBSTONE_DAYS: i64 = 7;

/// Failure modes for pinned-key entry verification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlacklistError {
    /// Originator is not in the receiver's pinned set.
    UntrustedOriginator,
    /// `subject` does not parse as `mirror:<id>` or `operator:<64-hex>`.
    BadSubject,
    /// `originator` is not a 32-byte hex public key.
    BadOriginator,
    /// Signature missing, malformed, or invalid over the canonical bytes.
    BadSignature,
    /// `seq` must be non-zero.
    BadSeq,
}

impl std::fmt::Display for BlacklistError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UntrustedOriginator => write!(f, "originator not in pinned operator set"),
            Self::BadSubject => write!(f, "subject must be mirror:<id> or operator:<64-hex>"),
            Self::BadOriginator => write!(f, "originator must be a 64-char hex Ed25519 key"),
            Self::BadSignature => write!(f, "signature missing or invalid"),
            Self::BadSeq => write!(f, "seq must be non-zero"),
        }
    }
}

impl std::error::Error for BlacklistError {}

/// Action carried by a signed blacklist entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlacklistAction {
    /// Ban the subject.
    Ban,
    /// Retract a previous ban from the same originator (tombstone).
    Retract,
}

impl BlacklistAction {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Ban => "ban",
            Self::Retract => "retract",
        }
    }
}

/// A signed blacklist entry (design wire shape). `signature` covers
/// `canonical_json()` — the entry minus the `signature` field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlacklistEntry {
    /// Ban target: `mirror:<mirror_id>` or `operator:<npub-hex>`.
    pub subject: String,
    pub action: BlacklistAction,
    /// Human-readable judgment by the originator (mildly sensitive; design Q6).
    pub reason: String,
    /// Hex Ed25519 public key of the originating authority. Part of the
    /// SIGNED payload (unlike manifest.rs's excluded public_key).
    pub originator: String,
    /// Monotonic per (originator, subject); a lower-seq replay always loses.
    pub seq: u64,
    /// Unix seconds when the entry was signed.
    pub created_at: i64,
    /// Unix seconds expiry. None = until retracted (manual bans);
    /// set = TTL (auto bans).
    #[serde(default)]
    pub expires_at: Option<i64>,
    /// Hex Ed25519 signature over the canonical entry JSON.
    #[serde(default)]
    pub signature: String,
}

/// Parsed blacklist subject.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlacklistSubject {
    /// `mirror:<mirror_id>` — e.g. `mirror:buildhost-archlinux`.
    Mirror(String),
    /// `operator:<64-hex npub>` — bans every mirror of that operator.
    Operator(String),
}

impl BlacklistSubject {
    /// Parse a subject string. Operator values must be 64 hex chars. Mirror
    /// values must be CANONICAL mirror ids ([`is_canonical_mirror_id`] —
    /// red-team A6): a mutated (case/whitespace/lookalike) subject is
    /// rejected fail-closed rather than normalized, so mutated ids can never
    /// enter signed blacklist state through ANY verify path (tracker intake,
    /// mirror store merge, client gossip filter), and stored subjects stay
    /// byte-faithful to their signatures for re-verification. Signers
    /// canonicalize the id BEFORE signing (the ban then covers the canonical
    /// identity every announce is stored under).
    pub fn parse(subject: &str) -> Option<Self> {
        let (kind, value) = subject.split_once(':')?;
        match kind {
            "mirror" if is_canonical_mirror_id(value) => Some(Self::Mirror(value.to_string())),
            "operator" if is_hex_npub(value) => Some(Self::Operator(value.to_string())),
            _ => None,
        }
    }

    /// Build a mirror subject.
    pub fn mirror(mirror_id: &str) -> Self {
        Self::Mirror(mirror_id.to_string())
    }

    /// Canonical wire form.
    pub fn as_subject(&self) -> String {
        match self {
            Self::Mirror(id) => format!("mirror:{}", id),
            Self::Operator(npub) => format!("operator:{}", npub),
        }
    }
}

/// A 64-char (32-byte) hex public key.
fn is_hex_npub(s: &str) -> bool {
    s.len() == 64 && s.chars().all(|c| c.is_ascii_hexdigit())
}

// ---------------------------------------------------------------------------
// mirror_id canonicalization (red-team A6 / discovery-blacklist §9 fix #4)
// ---------------------------------------------------------------------------

/// Allowlist for canonical mirror ids: non-empty printable ASCII with the
/// space excluded (bytes 0x21–0x7E). Unicode confusables, control characters,
/// and all whitespace fail by construction — no confusable table to maintain.
fn is_allowed_mirror_id_bytes(id: &str) -> bool {
    !id.is_empty() && id.bytes().all(|b| (0x21..=0x7e).contains(&b))
}

/// Canonicalize a mirror id at every ingestion boundary (red-team A6). ONE
/// shared rule: every site that stores, signs, or matches a mirror id calls
/// this — a drift between sites is a defect (consistency asserted by test).
///
/// Rule: trim edge whitespace, ASCII-lowercase, then validate the allowlist
/// ([`is_allowed_mirror_id_bytes`]). The trim and ASCII-case normalizations
/// are applied; anything OUTSIDE the allowlist is REJECTED, never rewritten,
/// so the announcer sees the error instead of a silently mutated identity:
/// interior whitespace, control characters, and all non-ASCII (unicode
/// lookalikes) are rejected by construction. ASCII-only folding is deliberate
/// — Unicode case folding could launder lookalikes (Kelvin-sign K → k), so
/// non-ASCII bytes never map into the allowed set.
///
/// Idempotent: canonicalizing a canonical id returns it unchanged (tested).
pub fn canonicalize_mirror_id(raw: &str) -> Result<String, String> {
    let lowered = raw.trim().to_ascii_lowercase();
    if lowered.is_empty() {
        return Err("mirror_id is empty (or whitespace only)".to_string());
    }
    if !is_allowed_mirror_id_bytes(&lowered) {
        return Err(
            "mirror_id must be printable ASCII with no whitespace; control \
             characters and non-ASCII lookalikes are rejected"
                .to_string(),
        );
    }
    Ok(lowered)
}

/// True when `id` is already in canonical form — a fixed point of
/// [`canonicalize_mirror_id`]: non-empty printable ASCII, no whitespace
/// anywhere, ASCII-lowercase. Stored ids and signed `mirror:` subjects must
/// satisfy this (see [`BlacklistSubject::parse`]).
pub fn is_canonical_mirror_id(id: &str) -> bool {
    canonicalize_mirror_id(id).as_deref() == Ok(id)
}

/// Sort JSON object keys recursively for canonical serialization
/// (manifest.rs `sort_json_object` semantics: objects sorted at every depth,
/// array element order preserved).
pub fn sorted_json_value(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            let mut sorted: Vec<_> = map.iter_mut().collect();
            sorted.sort_by(|a, b| a.0.cmp(b.0));
            for (_, v) in sorted.iter_mut() {
                sorted_json_value(v);
            }
            let new_map: serde_json::Map<String, serde_json::Value> = sorted
                .into_iter()
                .map(|(k, v)| (k.clone(), v.take()))
                .collect();
            *map = new_map;
        }
        serde_json::Value::Array(arr) => {
            for v in arr.iter_mut() {
                sorted_json_value(v);
            }
        }
        _ => {}
    }
}

/// Canonical JSON for an arbitrary value: recursively sorted object keys,
/// compact serialization. Shared by entry signing, the peers envelope, and
/// the announce payload signature.
pub fn canonical_json(value: &serde_json::Value) -> String {
    let mut copy = value.clone();
    sorted_json_value(&mut copy);
    serde_json::to_string(&copy).unwrap_or_default()
}

impl BlacklistEntry {
    /// The exact byte string that is signed: entry JSON minus `signature`,
    /// recursively sorted keys, compact.
    pub fn canonical_json(&self) -> String {
        let mut value = serde_json::to_value(self).expect("blacklist entry serializes");
        if let serde_json::Value::Object(map) = &mut value {
            map.remove("signature");
        }
        canonical_json(&value)
    }

    /// Sign in place: stamps `originator` from the key and signs the
    /// canonical bytes.
    pub fn sign(&mut self, signing_key: &SigningKey) {
        self.originator = hex::encode(signing_key.verifying_key().as_bytes());
        let sig = signing_key.sign(self.canonical_json().as_bytes());
        self.signature = hex::encode(sig.to_bytes());
    }

    /// Verify the entry against a PINNED originator key set. Checks trust
    /// first (never TOFU), then subject/seq well-formedness, then the
    /// signature over the canonical bytes.
    pub fn verify_trusted(&self, trusted_operators: &[String]) -> Result<(), BlacklistError> {
        if !trusted_operators
            .iter()
            .any(|k| k.eq_ignore_ascii_case(&self.originator))
        {
            return Err(BlacklistError::UntrustedOriginator);
        }
        self.verify_signature()
    }

    /// Verify the signature against the entry's own claimed originator
    /// (no trust decision — used by the tracker where the middleware is the
    /// authorization gate).
    pub fn verify_signature(&self) -> Result<(), BlacklistError> {
        if BlacklistSubject::parse(&self.subject).is_none() {
            return Err(BlacklistError::BadSubject);
        }
        if self.seq == 0 {
            return Err(BlacklistError::BadSeq);
        }
        let originator_bytes = hex::decode(&self.originator)
            .ok()
            .filter(|b| b.len() == 32)
            .ok_or(BlacklistError::BadOriginator)?;
        let verifying_key = VerifyingKey::from_bytes(
            originator_bytes
                .as_slice()
                .try_into()
                .map_err(|_| BlacklistError::BadOriginator)?,
        )
        .map_err(|_| BlacklistError::BadOriginator)?;
        let sig_bytes = hex::decode(&self.signature)
            .ok()
            .filter(|b| b.len() == 64)
            .ok_or(BlacklistError::BadSignature)?;
        let signature =
            Signature::from_slice(&sig_bytes).map_err(|_| BlacklistError::BadSignature)?;
        verifying_key
            .verify(self.canonical_json().as_bytes(), &signature)
            .map_err(|_| BlacklistError::BadSignature)
    }

    /// True when this entry is an unexpired ban at `now`.
    pub fn is_active_ban(&self, now: i64) -> bool {
        self.action == BlacklistAction::Ban && self.expires_at.is_none_or(|exp| exp > now)
    }
}

/// Key entries by (originator, subject), keeping the highest-`seq` entry.
/// A replayed stale entry (lower or equal seq) is silently dropped — the
/// seq-monotonic rule makes replay rejection part of the merge.
pub fn merge_entries<I: IntoIterator<Item = BlacklistEntry>>(
    entries: I,
) -> HashMap<(String, String), BlacklistEntry> {
    let mut merged: HashMap<(String, String), BlacklistEntry> = HashMap::new();
    for entry in entries {
        let key = (entry.originator.clone(), entry.subject.clone());
        match merged.get(&key) {
            Some(existing) if existing.seq >= entry.seq => {}
            _ => {
                merged.insert(key, entry);
            }
        }
    }
    merged
}

/// Per-`(originator, subject)` high-water seq markers (A1 anti-resurrection
/// fix). Tombstones are pruned after [`DEFAULT_TOMBSTONE_DAYS`] and the store
/// cap can evict low-seq entries sooner; without durable memory of the
/// highest seq ever accepted for a key, a replayed operator-signed ban
/// (`seq <=` the retraction's seq) would merge unopposed and resurrect a
/// retracted ban. Markers keep exactly that memory: one u64 per key,
/// monotonic, never decremented.
///
/// Trust invariant: a marker can only SUPPRESS merges (`seq <=` marker is
/// rejected as stale). It is not an entry — never served, never advertised,
/// and it can never create, resurrect, or remove a ban.
pub type BlacklistHighWater = HashMap<(String, String), u64>;

/// True when `seq` is stale against the high-water marker for `key`
/// (`seq <=` the highest seq ever accepted for that key).
pub fn high_water_rejects(markers: &BlacklistHighWater, key: &(String, String), seq: u64) -> bool {
    markers.get(key).is_some_and(|&high| seq <= high)
}

/// Raise the high-water marker for `key` (never lowers it). `seq == 0` is
/// unreachable through the normal merge path (`BadSeq` gate) and would be a
/// no-op against the `or_insert(0)` default.
pub fn record_high_water(markers: &mut BlacklistHighWater, key: &(String, String), seq: u64) {
    let slot = markers.entry(key.clone()).or_insert(0);
    if seq > *slot {
        *slot = seq;
    }
}

/// Serving/compacted view: the max-seq entry per (originator, subject) is an
/// unexpired ban. Newest-seq first. Retracted and expired entries are absent.
pub fn active_bans(
    merged: &HashMap<(String, String), BlacklistEntry>,
    now: i64,
) -> Vec<&BlacklistEntry> {
    let mut bans: Vec<&BlacklistEntry> = merged.values().filter(|e| e.is_active_ban(now)).collect();
    bans.sort_by(|a, b| b.seq.cmp(&a.seq));
    bans
}

/// True when the mirror has an active ban under subject `mirror:<mirror_id>`
/// from ANY trusted originator.
pub fn is_mirror_banned(
    merged: &HashMap<(String, String), BlacklistEntry>,
    mirror_id: &str,
    now: i64,
) -> bool {
    let subject = BlacklistSubject::mirror(mirror_id).as_subject();
    merged
        .values()
        .any(|e| e.subject == subject && e.is_active_ban(now))
}

/// Mirror IDs under an active `mirror:<id>` ban from any originator.
pub fn banned_mirror_ids(
    merged: &HashMap<(String, String), BlacklistEntry>,
    now: i64,
) -> std::collections::HashSet<String> {
    merged
        .values()
        .filter_map(|e| {
            if e.is_active_ban(now) {
                match BlacklistSubject::parse(&e.subject) {
                    Some(BlacklistSubject::Mirror(id)) => Some(id),
                    _ => None,
                }
            } else {
                None
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;

    /// Deterministic test keys — same pattern as pin_store.rs.
    fn test_key(seed: u8) -> SigningKey {
        let mut bytes = [0u8; 32];
        bytes[0] = seed;
        bytes[31] = seed.wrapping_mul(7);
        SigningKey::from_bytes(&bytes)
    }

    fn ban_entry(subject: &str, seq: u64, key: &SigningKey) -> BlacklistEntry {
        let mut entry = BlacklistEntry {
            subject: subject.to_string(),
            action: BlacklistAction::Ban,
            reason: "cross-check divergence".to_string(),
            originator: String::new(),
            seq,
            created_at: 1_793_500_000,
            expires_at: None,
            signature: String::new(),
        };
        entry.sign(key);
        entry
    }

    fn retract_entry(subject: &str, seq: u64, key: &SigningKey) -> BlacklistEntry {
        let mut entry = ban_entry(subject, seq, key);
        entry.action = BlacklistAction::Retract;
        entry.reason = "false positive".to_string();
        entry.sign(key);
        entry
    }

    fn trusted_of(key: &SigningKey) -> Vec<String> {
        vec![hex::encode(key.verifying_key().as_bytes())]
    }

    #[test]
    fn sign_then_verify_roundtrip() {
        let key = test_key(1);
        let entry = ban_entry("mirror:buildhost-archlinux", 1, &key);
        assert!(entry.verify_trusted(&trusted_of(&key)).is_ok());
    }

    #[test]
    fn verify_rejects_untrusted_originator() {
        let key = test_key(1);
        let other = test_key(2);
        let entry = ban_entry("mirror:buildhost-archlinux", 1, &key);
        assert_eq!(
            entry.verify_trusted(&trusted_of(&other)),
            Err(BlacklistError::UntrustedOriginator)
        );
        // Empty pinned set trusts nobody (never trust-by-default).
        assert_eq!(
            entry.verify_trusted(&[]),
            Err(BlacklistError::UntrustedOriginator)
        );
    }

    #[test]
    fn verify_rejects_tampered_payload() {
        let key = test_key(1);
        let mut entry = ban_entry("mirror:buildhost-archlinux", 1, &key);
        entry.reason = "tampered".to_string();
        assert_eq!(
            entry.verify_trusted(&trusted_of(&key)),
            Err(BlacklistError::BadSignature)
        );
    }

    #[test]
    fn verify_rejects_swapped_originator() {
        let key = test_key(1);
        let mut entry = ban_entry("mirror:buildhost-archlinux", 1, &key);
        // Claim a different (pinned) originator after signing — signature no
        // longer covers the claimed identity.
        entry.originator = hex::encode(test_key(2).verifying_key().as_bytes());
        let pinned = vec![
            hex::encode(test_key(1).verifying_key().as_bytes()),
            hex::encode(test_key(2).verifying_key().as_bytes()),
        ];
        assert_eq!(
            entry.verify_trusted(&pinned),
            Err(BlacklistError::BadSignature)
        );
    }

    #[test]
    fn canonical_json_deterministic_and_excludes_signature() {
        let key = test_key(1);
        let mut entry = ban_entry("mirror:buildhost-archlinux", 1, &key);
        let before = entry.canonical_json();
        entry.signature = "ff".repeat(64);
        assert_eq!(entry.canonical_json(), before);
        assert!(!before.contains("signature"));
        // Canonical form matches manifest.rs semantics: sorted keys, compact.
        assert!(before.starts_with("{\"action\":\"ban\""));
    }

    #[test]
    fn merge_keeps_highest_seq_per_originator_subject() {
        let key = test_key(1);
        let e1 = ban_entry("mirror:m-arch", 1, &key);
        let e3 = ban_entry("mirror:m-arch", 3, &key);
        let merged = merge_entries(vec![e3.clone(), e1.clone()]);
        assert_eq!(merged.len(), 1);
        assert_eq!(
            merged
                .get(&(
                    hex::encode(key.verifying_key().as_bytes()),
                    "mirror:m-arch".to_string()
                ))
                .map(|e| e.seq),
            Some(3)
        );
        // Replayed stale ban after the newer one loses.
        let merged = merge_entries(vec![e3, e1.clone()]);
        assert_eq!(merged.values().next().map(|e| e.seq), Some(3));
    }

    #[test]
    fn retract_at_higher_seq_suppresses_ban() {
        let key = test_key(1);
        let ban = ban_entry("mirror:m-arch", 1, &key);
        let retract = retract_entry("mirror:m-arch", 2, &key);
        let merged = merge_entries(vec![ban, retract]);
        assert!(!is_mirror_banned(&merged, "m-arch", 1_793_500_100));
        // ...and the compacted serving view carries neither.
        assert!(active_bans(&merged, 1_793_500_100).is_empty());
    }

    #[test]
    fn stale_ban_replay_cannot_resurrect_retracted_subject() {
        // Gossip lag: an old snapshot carrying the ban (seq 1) arrives AFTER
        // the retraction (seq 2) was merged.
        let key = test_key(1);
        let retract = retract_entry("mirror:m-arch", 2, &key);
        let merged = merge_entries(vec![retract]);
        let lagged_ban = ban_entry("mirror:m-arch", 1, &key);
        let merged = merge_entries(merged.into_values().chain(std::iter::once(lagged_ban)));
        assert!(!is_mirror_banned(&merged, "m-arch", 1_793_500_100));
    }

    #[test]
    fn different_originators_merge_independently() {
        let a = test_key(1);
        let b = test_key(2);
        let ban_a = ban_entry("mirror:m-arch", 1, &a);
        let retract_b = retract_entry("mirror:m-arch", 9, &b);
        // b cannot retract a's ban — distinct (originator, subject) keys.
        let merged = merge_entries(vec![ban_a, retract_b]);
        assert!(is_mirror_banned(&merged, "m-arch", 1_793_500_100));
        assert_eq!(merged.len(), 2);
    }

    #[test]
    fn high_water_marker_suppresses_only_stale_seqs() {
        let key = test_key(1);
        let npub = hex::encode(key.verifying_key().as_bytes());
        let key_pair = (npub.clone(), "mirror:m-arch".to_string());
        let other_pair = (npub, "mirror:m-debian".to_string());
        let mut markers = BlacklistHighWater::new();
        assert!(!high_water_rejects(&markers, &key_pair, 1));

        record_high_water(&mut markers, &key_pair, 2);
        // seq <= marker is stale — including a ban replay at the retracted seq.
        assert!(high_water_rejects(&markers, &key_pair, 1));
        assert!(high_water_rejects(&markers, &key_pair, 2));
        // A genuine NEW ban (seq > marker) from the same originator lands.
        assert!(!high_water_rejects(&markers, &key_pair, 3));
        // Markers never regress below the max seq seen.
        record_high_water(&mut markers, &key_pair, 1);
        assert!(!high_water_rejects(&markers, &key_pair, 3));
        assert!(high_water_rejects(&markers, &key_pair, 1));
        // Distinct subjects of the same originator are independent keys.
        assert!(!high_water_rejects(&markers, &other_pair, 1));
    }

    #[test]
    fn marker_never_serves_or_unbans_entries() {
        // TRUST INVARIANT: markers exist only to suppress stale merges — they
        // must NEVER cause an un-ban or a trust grant. This test holds the
        // most hostile marker state possible (u64::MAX on every key) and
        // asserts the serving view — the only trust-granting surface — is
        // unaffected: the stored ban still serves, still counts as active.
        let key = test_key(3);
        let ban = ban_entry("mirror:m-arch", 1, &key);
        let merged = merge_entries(vec![ban]);
        let markers: BlacklistHighWater = merged
            .keys()
            .map(|k| (k.clone(), u64::MAX))
            .collect();
        // Markers are not entries: `is_mirror_banned`/`active_bans` accept
        // only the entry store (type-level — a marker map cannot even be
        // passed), so marker-only state can never serve a ban.
        // Hostile markers cannot un-ban a stored entry either.
        assert!(is_mirror_banned(&merged, "m-arch", 1_793_500_100));
        assert_eq!(active_bans(&merged, 1_793_500_100).len(), 1);
        assert_eq!(markers.len(), 1);
    }

    #[test]
    fn expired_ban_not_active() {
        let key = test_key(1);
        let mut entry = ban_entry("mirror:m-arch", 1, &key);
        entry.expires_at = Some(1_000);
        let merged = merge_entries(vec![entry]);
        assert!(!is_mirror_banned(&merged, "m-arch", 2_000));
        let mut live = ban_entry("mirror:m-live", 1, &key);
        live.expires_at = Some(3_000);
        let merged = merge_entries(vec![live]);
        assert!(is_mirror_banned(&merged, "m-live", 2_000));
    }

    #[test]
    fn subject_parsing() {
        assert_eq!(
            BlacklistSubject::parse("mirror:buildhost-archlinux"),
            Some(BlacklistSubject::Mirror("buildhost-archlinux".to_string()))
        );
        let npub = "a".repeat(64);
        assert_eq!(
            BlacklistSubject::parse(&format!("operator:{}", npub)),
            Some(BlacklistSubject::Operator(npub))
        );
        // Operator subjects must be 64 hex.
        assert_eq!(BlacklistSubject::parse("operator:short"), None);
        assert_eq!(BlacklistSubject::parse("mirror:"), None);
        assert_eq!(BlacklistSubject::parse("weird:thing"), None);
        assert_eq!(
            BlacklistSubject::mirror("m-arch").as_subject(),
            "mirror:m-arch"
        );
    }

    // -----------------------------------------------------------------------
    // A6 mirror_id canonicalization (§9 fix #4)
    // -----------------------------------------------------------------------

    #[test]
    fn canonicalization_rule_table() {
        // Legit canonical ids pass through unchanged (no false rejections).
        for legit in [
            "buildhost-archlinux",
            "m-arch",
            "x86_64-host.a",
            "a_b-9",
            "0",
        ] {
            assert_eq!(canonicalize_mirror_id(legit).as_deref(), Ok(legit));
            assert!(is_canonical_mirror_id(legit));
        }
        // Edge whitespace is normalized away...
        assert_eq!(canonicalize_mirror_id("  honest-arch ").unwrap(), "honest-arch");
        assert_eq!(canonicalize_mirror_id("\thonest-arch\n").unwrap(), "honest-arch");
        // ...ASCII case folds...
        assert_eq!(canonicalize_mirror_id("Honest-ARCH").unwrap(), "honest-arch");
        assert_eq!(canonicalize_mirror_id("A_b-9").unwrap(), "a_b-9");
        // ...and the composition is idempotent: canonicalizing a canonical id
        // is a no-op, so a defensive re-canonicalization downstream can never
        // diverge from the stored form.
        let once = canonicalize_mirror_id(" Host-1 ").unwrap();
        assert_eq!(canonicalize_mirror_id(&once).as_deref(), Ok(once.as_str()));
    }

    #[test]
    fn canonicalization_rejects_outside_allowlist() {
        // Interior whitespace of every flavor.
        assert!(canonicalize_mirror_id("honest arch").is_err());
        assert!(canonicalize_mirror_id("honest\tarch").is_err());
        assert!(canonicalize_mirror_id("honest\narch").is_err());
        // Control characters.
        assert!(canonicalize_mirror_id("honest\u{7f}arch").is_err());
        assert!(canonicalize_mirror_id("honest\u{0}arch").is_err());
        // Unicode lookalikes and format characters (cyrillic о, NBSP,
        // zero-width space) — rejected, never folded into ASCII.
        assert!(canonicalize_mirror_id("h\u{043e}nest-arch").is_err());
        assert!(canonicalize_mirror_id("honest\u{00a0}arch").is_err());
        assert!(canonicalize_mirror_id("honest\u{200b}arch").is_err());
        // Empty / whitespace-only.
        assert!(canonicalize_mirror_id("").is_err());
        assert!(canonicalize_mirror_id("   ").is_err());
        // Rule consistency: every canonical output satisfies the fixed-point
        // predicate the subject parser enforces.
        for raw in [" Host-1 ", "A.B_c-9", "honest-arch"] {
            let canonical = canonicalize_mirror_id(raw).unwrap();
            assert!(is_canonical_mirror_id(&canonical));
        }
    }

    #[test]
    fn subject_parse_requires_canonical_mirror_ids() {
        // A6: mutated mirror subjects are rejected fail-closed, not normalized
        // — a signed ban must never carry a subject its announce-side id
        // cannot match.
        assert_eq!(BlacklistSubject::parse("mirror:Honest-Host"), None);
        assert_eq!(BlacklistSubject::parse("mirror:honest host"), None);
        assert_eq!(BlacklistSubject::parse("mirror:h\u{043e}st"), None);
        assert_eq!(BlacklistSubject::parse("mirror:honest-host "), None);
        // Operator subjects unaffected (hex pins compare case-insensitively
        // upstream; A6 scopes mirror ids only).
        let npub = "a".repeat(64);
        assert_eq!(
            BlacklistSubject::parse(&format!("operator:{npub}")),
            Some(BlacklistSubject::Operator(npub))
        );
    }

    #[test]
    fn signed_entry_with_mutated_subject_fails_verification() {
        // The A6 choke point: verify_signature parses the subject, so a
        // signed entry over a mutated mirror id is rejected at EVERY merge
        // path (tracker intake, mirror store, client gossip) — even with a
        // valid signature over the mutated bytes. Mutated ids cannot enter
        // any blacklist state.
        let key = test_key(1);
        let entry = ban_entry("mirror:Honest-Host", 1, &key);
        assert_eq!(
            entry.verify_trusted(&trusted_of(&key)),
            Err(BlacklistError::BadSubject)
        );
    }

    #[test]
    fn ban_covers_mutated_variant_via_shared_rule() {
        // End-to-end closure shape: ban the canonical id, canonicalize the
        // mutated variant with the SAME shared fn — the exact match now hits.
        let key = test_key(1);
        let merged = merge_entries(vec![ban_entry("mirror:buildhost-archlinux", 1, &key)]);
        let canonical = canonicalize_mirror_id("BUILDHOST-ARCHLINUX  ").unwrap();
        assert_eq!(canonical, "buildhost-archlinux");
        assert!(is_mirror_banned(&merged, &canonical, 1_793_500_100));
        // The un-mutated variant matches the same ban (identity preserved).
        assert!(is_mirror_banned(&merged, "buildhost-archlinux", 1_793_500_100));
    }

    #[test]
    fn json_roundtrip_preserves_wire_shape() {
        let key = test_key(1);
        let entry = ban_entry("mirror:m-arch", 4, &key);
        let text = serde_json::to_string(&entry).unwrap();
        let parsed: BlacklistEntry = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed, entry);
        assert!(parsed.verify_trusted(&trusted_of(&key)).is_ok());
    }
}
