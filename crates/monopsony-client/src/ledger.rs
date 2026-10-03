//! Client monotonic high-water ledger (Sprint 20 Phase 4, slice 2).
//!
//! SQLite-backed per-package high-water store. Every parsed index response
//! records the newest version seen for each `"<pm>:<name>"` package key; a
//! later index offering a LOWER version is a downgrade attempt. The ledger
//! itself is policy-free: it reports [`CheckVerdict::Downgrade`] and the
//! proxy layer (slice 4) decides warn vs reject. Sync rusqlite — the async
//! proxy call site wraps calls in `tokio::task::spawn_blocking`.
//!
//! The DB lives at `<state_dir>/ledger.sqlite3`, deliberately OUTSIDE
//! cert_dir so re-pairing cannot wipe the anti-rewind anchor. WAL mode lets
//! the proxy task and daemon share it.

use std::path::Path;
use std::sync::Mutex;

/// Verdict of a high-water check. `held` is the stored version string the
/// downgrade was measured against (for tamper events / headers).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckVerdict {
    /// New version >= held (or first contact): row updated/inserted.
    Ok,
    /// Offered version sorts LESS than the stored high-water; row untouched.
    Downgrade { held: String },
}

/// Shared ledger state: one SQLite connection behind a mutex, opened once
/// per daemon run.
pub struct Ledger {
    conn: Mutex<rusqlite::Connection>,
}

impl Ledger {
    /// Open (creating if needed) the ledger DB at `path`. Runs migrations
    /// and creates parent directories. Enables WAL so concurrent readers
    /// (proxy task) do not block the daemon writer.
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let conn = rusqlite::Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS high_water (
                package_key  TEXT PRIMARY KEY,
                version      TEXT NOT NULL,
                version_ord  TEXT NOT NULL,
                source       TEXT NOT NULL,
                seen_at      INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS repo_seen (
                repo_key     TEXT PRIMARY KEY,
                last_seen    INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS exceptions (
                id           INTEGER PRIMARY KEY AUTOINCREMENT,
                package_key  TEXT NOT NULL,
                to_version   TEXT NOT NULL,
                reason       TEXT NOT NULL,
                created_at   INTEGER NOT NULL,
                expires_at   INTEGER NOT NULL,
                signature    TEXT,
                signing_key  TEXT
            );",
        )?;
        // Wave-2 slice 6 migration: DBs created before the override command
        // lack the two signature columns. ALTER them in additively; rusqlite
        // emits "duplicate column name" when the column already exists
        // (fresh DBs got them from the CREATE TABLE above) — swallow ONLY
        // that, propagate anything else.
        for ddl in [
            "ALTER TABLE exceptions ADD COLUMN signature TEXT",
            "ALTER TABLE exceptions ADD COLUMN signing_key TEXT",
        ] {
            if let Err(e) = conn.execute_batch(ddl) {
                if !e.to_string().contains("duplicate column") {
                    return Err(e);
                }
            }
        }
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Compare the offered version against the stored high-water for
    /// `package_key` and record the monotonic max.
    ///
    /// - First contact: INSERT, `Ok`.
    /// - Equal or Greater: UPDATE, `Ok`.
    /// - Less: row untouched; an unexpired exception for the package
    ///   (record-scoped — any to_version) downgrades to `Ok`, otherwise
    ///   `Downgrade { held }`.
    ///
    /// `version` is the raw offered version string (stored for display);
    /// ordering decisions always go through `normalized` via
    /// `monopsony_common::version_ord::vercmp`.
    pub fn check_and_update(
        &self,
        package_key: &str,
        version: &str,
        normalized: &monopsony_common::version_ord::NormalizedVersion,
        source: &str,
        now: i64,
    ) -> Result<CheckVerdict, rusqlite::Error> {
        let order_key = monopsony_common::version_ord::order_key(normalized);
        let conn = self.conn.lock().expect("ledger lock poisoned");

        let held: Option<(String, String)> = conn
            .query_row(
                "SELECT version, version_ord FROM high_water WHERE package_key = ?1",
                rusqlite::params![package_key],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(other),
            })?;

        let Some((held_version, held_ord)) = held else {
            // First contact just records.
            conn.execute(
                "INSERT INTO high_water (package_key, version, version_ord, source, seen_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![package_key, version, order_key, source, now],
            )?;
            return Ok(CheckVerdict::Ok);
        };

        // Parse the stored order key back into components so the comparison
        // runs through vercmp, never a string compare. The stored key is
        // always self-produced, so a parse failure is corruption — treat it
        // as "held unknown" and record the new max (fail-open on storage
        // damage; the integrity policy still sees a coherent verdict).
        let held_norm = parse_order_key(&held_ord).unwrap_or_else(|| {
            monopsony_common::version_ord::NormalizedVersion {
                epoch: 0,
                version: held_version.clone(),
                release: 0,
            }
        });

        use std::cmp::Ordering;
        match monopsony_common::version_ord::vercmp(normalized, &held_norm) {
            Ordering::Less => {
                // NOTE: use the connection-taking helper — the public
                // active_exception() re-locks the mutex this fn already
                // holds (re-entrant deadlock; every downgrade check hung).
                if active_exception_on(&conn, package_key, now).is_some() {
                    return Ok(CheckVerdict::Ok);
                }
                Ok(CheckVerdict::Downgrade { held: held_version })
            }
            // Equal or Greater: monotonic max (equal re-seen refreshes recency).
            _ => {
                conn.execute(
                    "INSERT INTO high_water (package_key, version, version_ord, source, seen_at)
                     VALUES (?1, ?2, ?3, ?4, ?5)
                     ON CONFLICT(package_key) DO UPDATE SET
                        version = excluded.version,
                        version_ord = excluded.version_ord,
                        source = excluded.source,
                        seen_at = excluded.seen_at",
                    rusqlite::params![package_key, version, order_key, source, now],
                )?;
                Ok(CheckVerdict::Ok)
            }
        }
    }

    /// Read-only held high-water (raw version string) — used by the proxy's
    /// payload cross-check, which must observe without writing (only parsed
    /// index responses advance the high-water).
    pub fn high_water_for(&self, package_key: &str) -> Option<String> {
        let conn = self.conn.lock().expect("ledger lock poisoned");
        conn.query_row(
            "SELECT version FROM high_water WHERE package_key = ?1",
            rusqlite::params![package_key],
            |row| row.get::<_, String>(0),
        )
        .ok()
    }

    /// Record that a canonical index response for `repo_key` was just seen.
    pub fn repo_seen(&self, repo_key: &str, now: i64) -> rusqlite::Result<()> {
        let conn = self.conn.lock().expect("ledger lock poisoned");
        conn.execute(
            "INSERT INTO repo_seen (repo_key, last_seen) VALUES (?1, ?2)
             ON CONFLICT(repo_key) DO UPDATE SET last_seen = excluded.last_seen",
            rusqlite::params![repo_key, now],
        )?;
        Ok(())
    }

    /// Seconds since the last index response for `repo_key`, or None when
    /// never seen (staleness v1: warn-only at the call site; 0-age config
    /// disables the check entirely).
    pub fn repo_age_secs(&self, repo_key: &str) -> Option<i64> {
        let conn = self.conn.lock().expect("ledger lock poisoned");
        conn.query_row(
            "SELECT last_seen FROM repo_seen WHERE repo_key = ?1",
            rusqlite::params![repo_key],
            |row| row.get::<_, i64>(0),
        )
        .ok()
        .map(|last_seen| chrono::Utc::now().timestamp() - last_seen)
    }

    /// Record a signed-exception request: downgrades of `package_key` are
    /// permitted until `now + ttl_secs`.
    #[allow(dead_code)] // ledger exception API (Sprint 23); bin wiring pending
    pub fn add_exception(
        &self,
        package_key: &str,
        to_version: &str,
        reason: &str,
        ttl_secs: i64,
        now: i64,
    ) -> rusqlite::Result<()> {
        let conn = self.conn.lock().expect("ledger lock poisoned");
        conn.execute(
            "INSERT INTO exceptions (package_key, to_version, reason, created_at, expires_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![package_key, to_version, reason, now, now + ttl_secs],
        )?;
        Ok(())
    }

    /// Store a fully-signed override record (signature + verifying key hex
    /// included). The signature MUST cover `rec`'s canonical JSON — see
    /// [`SignedException::canonical_json`].
    pub fn add_signed_exception(&self, rec: &SignedException) -> rusqlite::Result<()> {
        let conn = self.conn.lock().expect("ledger lock poisoned");
        conn.execute(
            "INSERT INTO exceptions
                (package_key, to_version, reason, created_at, expires_at, signature, signing_key)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![
                rec.package_key,
                rec.to_version,
                rec.reason,
                rec.created_at,
                rec.expires_at,
                rec.signature,
                rec.signing_key
            ],
        )?;
        Ok(())
    }

    /// All unexpired exceptions, oldest first (id ASC) — the `override list`
    /// view. NOT signature-checked: display-only; enforcement goes through
    /// `active_exception_on`, which verifies.
    pub fn list_active_exceptions(&self, now: i64) -> rusqlite::Result<Vec<SignedException>> {
        let conn = self.conn.lock().expect("ledger lock poisoned");
        let mut stmt = conn.prepare(
            "SELECT id, package_key, to_version, reason, created_at, expires_at, signature, signing_key
             FROM exceptions WHERE expires_at > ?1 ORDER BY id ASC",
        )?;
        let rows = stmt.query_map(rusqlite::params![now], row_to_signed_exception)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Most recent unexpired exception for `package_key` (its to_version),
    /// or None. Record-scoped: a live exception waives ANY downgrade of the
    /// package (the wave-2 override command refines per-version matching).
    #[allow(dead_code)] // exception query API (Sprint 23); bin wiring pending
    pub fn active_exception(&self, package_key: &str, now: i64) -> Option<String> {
        let conn = self.conn.lock().expect("ledger lock poisoned");
        active_exception_on(&conn, package_key, now)
    }
}

/// A downgrade-exception record as stored in the `exceptions` table.
/// `signature`/`signing_key` are `None` for legacy (pre-slice-6) rows, which
/// stay honored as-is; new rows carry a lowercase-hex Ed25519 signature over
/// [`SignedException::canonical_json`] plus the verifying key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignedException {
    /// Row id (AUTOINCREMENT) — 0 on freshly built, not-yet-inserted records.
    pub id: i64,
    pub package_key: String,
    pub to_version: String,
    pub reason: String,
    pub created_at: i64,
    pub expires_at: i64,
    /// Lowercase hex Ed25519 signature over the canonical JSON.
    pub signature: Option<String>,
    /// Lowercase hex 32-byte Ed25519 verifying key.
    pub signing_key: Option<String>,
}

impl SignedException {
    /// The exact byte string that gets signed. serde_json's default map is a
    /// BTreeMap, so field order is deterministic (alphabetical) with no
    /// feature flags. Never hand-format this with `format!` — escaping bugs
    /// silently change the signed payload.
    pub fn canonical_json(&self) -> String {
        serde_json::to_string(&serde_json::json!({
            "package_key": self.package_key,
            "to_version": self.to_version,
            "reason": self.reason,
            "created_at": self.created_at,
            "expires_at": self.expires_at,
        }))
        .expect("exception record serializes")
    }

    /// Sign `self` in place: fills `signature` and `signing_key` (lowercase
    /// hex) using `signing_key`.
    pub fn sign(&mut self, signing_key: &ed25519_dalek::SigningKey) {
        use ed25519_dalek::Signer;
        let sig = signing_key.sign(self.canonical_json().as_bytes());
        self.signature = Some(hex::encode(sig.to_bytes()));
        self.signing_key = Some(hex::encode(signing_key.verifying_key().as_bytes()));
    }

    /// Verify the stored signature against the stored verifying key.
    /// Legacy unsigned rows (both fields None) are honored → true.
    /// Any decode failure, wrong length, or bad signature → false.
    fn signature_valid(&self) -> bool {
        use ed25519_dalek::Verifier;
        let (Some(sig_hex), Some(key_hex)) = (&self.signature, &self.signing_key) else {
            // Legacy unsigned row — documented migration path (plan §6).
            return self.signature.is_none() && self.signing_key.is_none();
        };
        let (Ok(sig_bytes), Ok(key_bytes)) = (hex::decode(sig_hex), hex::decode(key_hex)) else {
            return false;
        };
        let (Ok(key_arr), Ok(sig_arr)) = (
            <[u8; 32]>::try_from(key_bytes.as_slice()),
            <[u8; 64]>::try_from(sig_bytes.as_slice()),
        ) else {
            return false;
        };
        let Ok(verifying) = ed25519_dalek::VerifyingKey::from_bytes(&key_arr) else {
            return false;
        };
        let Ok(sig) = ed25519_dalek::Signature::from_slice(&sig_arr) else {
            return false;
        };
        verifying
            .verify(self.canonical_json().as_bytes(), &sig)
            .is_ok()
    }
}

/// Lock-free exception query over a borrowed connection — callable from
/// methods that already hold the ledger mutex (see check_and_update's
/// downgrade branch; re-locking there deadlocks, the MirrorManager
/// RwLock bug class).
///
/// Walks unexpired rows newest-first and returns the FIRST row that is
/// valid: legacy unsigned rows (both signature fields NULL) are honored
/// as-is; signed rows must decode + verify against the row's own verifying
/// key over the reconstructed canonical JSON — invalid or undecodable rows
/// are SKIPPED (ignored, not fatal), and the walk continues to older rows.
/// That is why verification lives HERE and not in the callers: both the
/// downgrade branch of check_and_update and the public active_exception get
/// it automatically.
fn active_exception_on(conn: &rusqlite::Connection, package_key: &str, now: i64) -> Option<String> {
    let mut stmt = conn
        .prepare(
            "SELECT id, package_key, to_version, reason, created_at, expires_at, signature, signing_key
             FROM exceptions
             WHERE package_key = ?1 AND expires_at > ?2
             ORDER BY id DESC",
        )
        .ok()?;
    let rows = stmt
        .query_map(rusqlite::params![package_key, now], row_to_signed_exception)
        .ok()?;
    for row in rows {
        let Ok(rec) = row else {
            continue; // unreadable column → skip, never panic
        };
        if !rec.signature_valid() {
            continue; // tampered / undecodable → ignored
        }
        return Some(rec.to_version);
    }
    None
}

/// Project one exceptions row onto a [`SignedException`].
fn row_to_signed_exception(row: &rusqlite::Row<'_>) -> rusqlite::Result<SignedException> {
    Ok(SignedException {
        id: row.get(0)?,
        package_key: row.get(1)?,
        to_version: row.get(2)?,
        reason: row.get(3)?,
        created_at: row.get(4)?,
        expires_at: row.get(5)?,
        signature: row.get(6)?,
        signing_key: row.get(7)?,
    })
}

/// Parse a self-produced `order_key` (`epoch:version:release`, zero-padded
/// per `version_ord::order_key`) back into components.
fn parse_order_key(key: &str) -> Option<monopsony_common::version_ord::NormalizedVersion> {
    let (epoch, rest) = key.split_once(':')?;
    let (version, release) = rest.rsplit_once(':')?;
    // Version body is whatever order_key stored; it cannot itself contain
    // ':' from the order_key side, but a raw version could — rsplit keeps
    // the LAST field as release and everything before as the body.
    Some(monopsony_common::version_ord::NormalizedVersion {
        epoch: epoch.parse().ok()?,
        version: version.to_string(),
        release: release.parse().ok()?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use monopsony_common::version_ord::normalize;

    fn tmp_db(tag: &str) -> std::path::PathBuf {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(format!("{}.sqlite3", tag));
        // Tests are short-lived; leak the tempdir so the DB file outlives
        // the handle (the OS reclaims /tmp).
        std::mem::forget(dir);
        path
    }

    const SRC: &str = "https://mirror.example.com/arch/core/x86_64/";

    fn check(l: &Ledger, key: &str, ver: &str, now: i64) -> CheckVerdict {
        l.check_and_update(key, ver, &normalize(ver), SRC, now)
            .expect("check ok")
    }

    #[test]
    fn test_first_contact_records_and_ok() {
        let l = Ledger::open(&tmp_db("first")).unwrap();
        assert_eq!(check(&l, "pacman:bash", "5.2.37-1", 100), CheckVerdict::Ok);
        // Stored row is queryable via a re-check at the same version.
        assert_eq!(check(&l, "pacman:bash", "5.2.37-1", 101), CheckVerdict::Ok);
    }

    #[test]
    fn test_monotonic_max_upgrade_then_downgrade_holds() {
        let l = Ledger::open(&tmp_db("mono")).unwrap();
        assert_eq!(check(&l, "pacman:gcc", "13.2.1", 100), CheckVerdict::Ok);
        assert_eq!(check(&l, "pacman:gcc", "14.1.1", 200), CheckVerdict::Ok);
        // Downgrade attempt must not move the row...
        assert_eq!(
            check(&l, "pacman:gcc", "13.2.1", 300),
            CheckVerdict::Downgrade {
                held: "14.1.1".to_string()
            }
        );
        // ...and a later legit upgrade still records over the held max.
        assert_eq!(check(&l, "pacman:gcc", "14.2.0", 400), CheckVerdict::Ok);
    }

    #[test]
    fn test_downgrade_returns_held_version_string() {
        let l = Ledger::open(&tmp_db("held")).unwrap();
        check(&l, "apk:openssl", "3.3.2-r0", 100);
        let v = check(&l, "apk:openssl", "3.3.1-r0", 200);
        assert_eq!(
            v,
            CheckVerdict::Downgrade {
                held: "3.3.2-r0".to_string()
            }
        );
    }

    #[test]
    fn test_exception_allows_then_expires_after_ttl() {
        let l = Ledger::open(&tmp_db("exc")).unwrap();
        check(&l, "pacman:linux", "6.10.1", 100);
        l.add_exception("pacman:linux", "6.9.0", "vendor pin", 600, 1000)
            .unwrap();
        // Inside the TTL window the downgrade is waived.
        assert_eq!(check(&l, "pacman:linux", "6.9.0", 1500), CheckVerdict::Ok);
        // Past expiry the same downgrade is flagged again.
        assert_eq!(
            check(&l, "pacman:linux", "6.9.0", 1700),
            CheckVerdict::Downgrade {
                held: "6.10.1".to_string()
            }
        );
    }

    #[test]
    fn test_repo_seen_age_roundtrip() {
        let l = Ledger::open(&tmp_db("repo")).unwrap();
        let repo = "https://mirror.example.com/arch/core/x86_64";
        assert_eq!(l.repo_age_secs(repo), None);
        let t0 = chrono::Utc::now().timestamp();
        l.repo_seen(repo, t0 - 120).unwrap();
        let age = l.repo_age_secs(repo).expect("age after seen");
        assert!((118..=122).contains(&age), "age ~120, got {}", age);
        // Re-seen refreshes the timestamp.
        l.repo_seen(repo, t0 - 5).unwrap();
        let age2 = l.repo_age_secs(repo).expect("age after refresh");
        assert!(age2 <= 10, "refreshed age small, got {}", age2);
    }

    #[test]
    fn test_open_creates_parent_dirs_and_reopen_preserves_rows() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("state/monopsony/ledger.sqlite3");
        {
            let l = Ledger::open(&path).expect("open creates parents");
            assert!(path.exists());
            check(&l, "pacman:zsh", "5.9-1", 100);
        }
        // Re-open (WAL files present) preserves rows.
        let l2 = Ledger::open(&path).expect("reopen");
        assert_eq!(
            check(&l2, "pacman:zsh", "5.8-1", 200),
            CheckVerdict::Downgrade {
                held: "5.9-1".to_string()
            }
        );
    }

    #[test]
    fn test_equal_version_reseen_ok_no_false_downgrade() {
        let l = Ledger::open(&tmp_db("equal")).unwrap();
        check(&l, "pacman:curl", "8.9.1-1", 100);
        for t in [200, 300, 400] {
            assert_eq!(check(&l, "pacman:curl", "8.9.1-1", t), CheckVerdict::Ok);
        }
        // Numeric-equal spellings are not downgrades either (dpkg rules).
        assert_eq!(check(&l, "pacman:curl", "08.9.1-01", 500), CheckVerdict::Ok);
    }

    #[test]
    fn test_wal_mode_enabled_on_open() {
        let l = Ledger::open(&tmp_db("walmode")).unwrap();
        let conn = l.conn.lock().unwrap();
        let mode: String = conn
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap();
        assert_eq!(mode.to_ascii_lowercase(), "wal");
    }

    #[test]
    fn test_exception_scoped_to_package_only() {
        let l = Ledger::open(&tmp_db("scope")).unwrap();
        check(&l, "pacman:aaa", "2.0", 100);
        check(&l, "pacman:bbb", "2.0", 100);
        l.add_exception("pacman:aaa", "1.0", "one-off", 3600, 100)
            .unwrap();
        // The exception covers its own package...
        assert_eq!(check(&l, "pacman:aaa", "1.0", 200), CheckVerdict::Ok);
        // ...but not a sibling package.
        assert_eq!(
            check(&l, "pacman:bbb", "1.0", 200),
            CheckVerdict::Downgrade {
                held: "2.0".to_string()
            }
        );
    }

    #[test]
    fn test_parse_order_key_roundtrip() {
        let n = normalize("1:2.3-4");
        let key = monopsony_common::version_ord::order_key(&n);
        let back = parse_order_key(&key).expect("parse own key");
        assert_eq!(back.epoch, 1);
        assert_eq!(back.version, "2.3");
        assert_eq!(back.release, 4);
        assert!(parse_order_key("garbage").is_none());
    }

    // ---- slice 6: signed override records ----

    use ed25519_dalek::SigningKey;

    /// Deterministic test key — style reference from pin_store.rs; NEVER the
    /// production pin key (different DB, different purpose).
    fn test_key(seed: u8) -> SigningKey {
        let mut bytes = [0u8; 32];
        bytes[0] = seed;
        bytes[31] = seed.wrapping_mul(7);
        SigningKey::from_bytes(&bytes)
    }

    fn signed_rec(
        key: &SigningKey,
        package: &str,
        to: &str,
        now: i64,
        ttl: i64,
    ) -> super::SignedException {
        let mut rec = super::SignedException {
            id: 0,
            package_key: package.to_string(),
            to_version: to.to_string(),
            reason: "kernel compat".to_string(),
            created_at: now,
            expires_at: now + ttl,
            signature: None,
            signing_key: None,
        };
        rec.sign(key);
        rec
    }

    #[test]
    fn test_legacy_migration_adds_signature_columns() {
        // Build the OLD 5-column exceptions table with a raw connection.
        let path = tmp_db("migrate");
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE exceptions (
                    id           INTEGER PRIMARY KEY AUTOINCREMENT,
                    package_key  TEXT NOT NULL,
                    to_version   TEXT NOT NULL,
                    reason       TEXT NOT NULL,
                    created_at   INTEGER NOT NULL,
                    expires_at   INTEGER NOT NULL
                );",
            )
            .unwrap();
        }
        // Ledger::open must migrate additively and stay usable.
        let l = Ledger::open(&path).unwrap();
        let k = test_key(3);
        let rec = signed_rec(&k, "pacman:bash", "5.2.36-1", 100, 600);
        l.add_signed_exception(&rec).unwrap();
        assert_eq!(
            l.active_exception("pacman:bash", 200),
            Some("5.2.36-1".to_string())
        );
    }

    #[test]
    fn test_signed_roundtrip_verifies() {
        let l = Ledger::open(&tmp_db("signed")).unwrap();
        let k = test_key(7);
        let rec = signed_rec(&k, "pacman:bash", "5.2.36-1", 100, 600);
        l.add_signed_exception(&rec).unwrap();
        assert_eq!(
            l.active_exception("pacman:bash", 200),
            Some("5.2.36-1".to_string())
        );
        // And it flows through the downgrade verdict too.
        check(&l, "pacman:bash", "5.2.37-1", 300);
        assert_eq!(check(&l, "pacman:bash", "5.2.30-1", 400), CheckVerdict::Ok);
    }

    #[test]
    fn test_tampered_signature_rejected() {
        let l = Ledger::open(&tmp_db("tamper")).unwrap();
        let k = test_key(7);
        let rec = signed_rec(&k, "apk:openssl", "3.3.1-r0", 100, 600);
        l.add_signed_exception(&rec).unwrap();
        {
            let conn = l.conn.lock().unwrap();
            conn.execute(
                "UPDATE exceptions SET signature = ?1",
                rusqlite::params!["f".repeat(64)],
            )
            .unwrap();
        }
        assert_eq!(l.active_exception("apk:openssl", 200), None);
    }

    #[test]
    fn test_legacy_unsigned_row_still_honored() {
        let l = Ledger::open(&tmp_db("legacy")).unwrap();
        {
            let conn = l.conn.lock().unwrap();
            conn.execute(
                "INSERT INTO exceptions (package_key, to_version, reason, created_at, expires_at, signature, signing_key)
                 VALUES (?1, ?2, ?3, ?4, ?5, NULL, NULL)",
                rusqlite::params!["pacman:linux", "6.9.0", "vendor pin", 100, 900],
            )
            .unwrap();
        }
        assert_eq!(
            l.active_exception("pacman:linux", 200),
            Some("6.9.0".to_string())
        );
    }

    #[test]
    fn test_signed_expiry_enforced() {
        let l = Ledger::open(&tmp_db("sexp")).unwrap();
        let k = test_key(11);
        let rec = signed_rec(&k, "pacman:linux", "6.9.0", 100, 500);
        l.add_signed_exception(&rec).unwrap();
        assert_eq!(
            l.active_exception("pacman:linux", 400),
            Some("6.9.0".to_string())
        );
        assert_eq!(l.active_exception("pacman:linux", 601), None);
    }

    #[test]
    fn test_tampered_row_skipped_valid_older_row_honored() {
        let l = Ledger::open(&tmp_db("walk")).unwrap();
        let k = test_key(13);
        // id 1: valid.
        let good = signed_rec(&k, "pacman:zsh", "5.9-1", 100, 600);
        l.add_signed_exception(&good).unwrap();
        // id 2: valid signature column, but garbage key → undecodable.
        {
            let conn = l.conn.lock().unwrap();
            conn.execute(
                "INSERT INTO exceptions (package_key, to_version, reason, created_at, expires_at, signature, signing_key)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                rusqlite::params![
                    "pacman:zsh",
                    "5.8-1",
                    "evil",
                    150,
                    700,
                    "a".repeat(128),
                    "b".repeat(64)
                ],
            )
            .unwrap();
        }
        // DESC order hits the tampered id 2 first, skips it, and honors id 1.
        assert_eq!(
            l.active_exception("pacman:zsh", 200),
            Some("5.9-1".to_string())
        );
    }
}
