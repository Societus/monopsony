//! CA lifecycle guard: prevents silent CA rotation on installs with paired clients.
//!
//! The private CA is the trust anchor for all mTLS pairing. It persists at
//! `<cache_dir>/tls/` — which sits inside the *evictable* cache directory. A wiped
//! or relocated cache dir therefore silently regenerates the CA on next boot and
//! permanently breaks every paired client (gRPC `UnknownIssuer`). This happened
//! once in production (2026-07-04: client paired 00:58, CA regenerated 17:38 after
//! a cache-dir reset; every client failed until re-pairing).
//!
//! This module makes that failure mode impossible to hit silently:
//!
//! 1. **Backup**: every loaded or generated CA is mirrored to `--ca-backup-dir`
//!    (default `/var/lib/monopsony/ca-backup`) — OUTSIDE the cache dir — together
//!    with a `ca-fingerprint.txt` for out-of-band verification.
//! 2. **Auto-restore (plan B)**: if the live CA is missing but the backup exists,
//!    it is restored automatically. Generation is never reached while a backup
//!    survives.
//! 3. **Fail-closed**: no live CA, no backup, but paired clients in the DB ->
//!    abort with recovery instructions instead of silently rotating. An operator
//!    who truly wants a fresh CA passes `--regenerate-ca` after seeing the list
//!    of clients that will need re-pairing.

use std::fs;
use std::path::Path;

use anyhow::{bail, Context, Result};
use monopsony_control::tls::TlsManager;
use monopsony_db::Database;

pub const CA_CERT_FILE: &str = "ca.crt";
pub const CA_KEY_FILE: &str = "ca.key";
pub const FINGERPRINT_FILE: &str = "ca-fingerprint.txt";

/// What [`decide`] says should happen at startup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaDecision {
    /// Live CA present: load it (and refresh the backup if it drifted).
    LoadExisting,
    /// Live CA missing but backup present: restore, then load.
    RestoreFromBackup,
    /// Nothing to lose: generate a fresh CA and back it up.
    GenerateFresh,
    /// No CA anywhere but paired clients exist: refuse to generate.
    AbortMissingWithClients,
}

/// Pure decision logic for CA initialization.
///
/// - `ca_exists` — both `ca.crt` and `ca.key` present in the live TLS dir.
/// - `backup_exists` — both files present in the backup dir.
/// - `client_count` — `Some(n)` when the DB answered; `None` when there is no
///   client registry to consult.
/// - `db_unreachable` — a DB is configured but the connection failed (we cannot
///   prove the install is clientless, so this fails closed).
/// - `force_regenerate` — operator passed `--regenerate-ca` (explicit override).
pub fn decide(
    ca_exists: bool,
    backup_exists: bool,
    client_count: Option<i64>,
    db_unreachable: bool,
    force_regenerate: bool,
) -> CaDecision {
    if ca_exists {
        return CaDecision::LoadExisting;
    }
    if backup_exists && !force_regenerate {
        return CaDecision::RestoreFromBackup;
    }
    if force_regenerate {
        return CaDecision::GenerateFresh;
    }
    match client_count {
        Some(0) => CaDecision::GenerateFresh,
        Some(_) => CaDecision::AbortMissingWithClients,
        // DB configured but unreachable: we cannot prove no clients exist.
        None if db_unreachable => CaDecision::AbortMissingWithClients,
        // Explicit --no-db: no pairing registry exists, generation is safe.
        None => CaDecision::GenerateFresh,
    }
}

fn pair_present(dir: &Path) -> bool {
    dir.join(CA_CERT_FILE).is_file() && dir.join(CA_KEY_FILE).is_file()
}

/// Copy `src_file` to `dst_file` via a temp file + rename (atomic per file).
fn atomic_copy(src: &Path, dst: &Path) -> Result<()> {
    let tmp = dst.with_extension("tmp");
    fs::copy(src, &tmp)
        .with_context(|| format!("copying {} to {}", src.display(), tmp.display()))?;
    fs::rename(&tmp, dst).with_context(|| format!("renaming {}", tmp.display()))?;
    Ok(())
}

#[cfg(unix)]
fn tighten_perms(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = fs::metadata(path)?.permissions();
    perms.set_mode(0o600);
    fs::set_permissions(path, perms)?;
    Ok(())
}

#[cfg(not(unix))]
fn tighten_perms(_path: &Path) -> Result<()> {
    Ok(())
}

/// Mirror the live CA pair into `backup_dir` and record the fingerprint.
///
/// Skips the write when the backup already matches byte-for-byte (idempotent
/// across restarts). The backup key is chmod 0600.
pub fn backup_ca(tls_dir: &Path, backup_dir: &Path, fingerprint: &str) -> Result<()> {
    fs::create_dir_all(backup_dir)
        .with_context(|| format!("creating backup dir {}", backup_dir.display()))?;

    let live_cert = tls_dir.join(CA_CERT_FILE);
    let live_key = tls_dir.join(CA_KEY_FILE);
    let bak_cert = backup_dir.join(CA_CERT_FILE);
    let bak_key = backup_dir.join(CA_KEY_FILE);

    let cert_matches = bak_cert.is_file()
        && fs::read(&bak_cert).ok().as_deref() == fs::read(&live_cert).ok().as_deref();
    let key_matches = bak_key.is_file()
        && fs::read(&bak_key).ok().as_deref() == fs::read(&live_key).ok().as_deref();

    if !(cert_matches && key_matches) {
        atomic_copy(&live_cert, &bak_cert)?;
        atomic_copy(&live_key, &bak_key)?;
        tighten_perms(&bak_key)?;
        tracing::info!(
            "CA backed up to {} (fingerprint {})",
            backup_dir.display(),
            &fingerprint[..fingerprint.len().min(23)]
        );
    }

    // Always refresh the fingerprint file (cheap, and self-heals if deleted).
    let fp_path = backup_dir.join(FINGERPRINT_FILE);
    fs::write(&fp_path, fingerprint).with_context(|| format!("writing {}", fp_path.display()))?;
    Ok(())
}

/// Restore the CA pair from `backup_dir` into `tls_dir`.
///
/// Fails if the backup is incomplete (one file present, one missing) — a partial
/// backup must never masquerade as a valid trust anchor.
pub fn restore_ca(backup_dir: &Path, tls_dir: &Path) -> Result<()> {
    let bak_cert = backup_dir.join(CA_CERT_FILE);
    let bak_key = backup_dir.join(CA_KEY_FILE);
    if !bak_cert.is_file() || !bak_key.is_file() {
        bail!(
            "backup at {} is incomplete (ca.crt: {}, ca.key: {}); refusing partial restore",
            backup_dir.display(),
            bak_cert.is_file(),
            bak_key.is_file()
        );
    }
    fs::create_dir_all(tls_dir)
        .with_context(|| format!("creating tls dir {}", tls_dir.display()))?;
    atomic_copy(&bak_cert, &tls_dir.join(CA_CERT_FILE))?;
    atomic_copy(&bak_key, &tls_dir.join(CA_KEY_FILE))?;
    tighten_perms(&tls_dir.join(CA_KEY_FILE))?;
    tracing::warn!(
        "CA restored from backup at {} (live CA was missing)",
        backup_dir.display()
    );
    Ok(())
}

/// Initialize the TLS manager with backup/restore/fail-closed semantics.
///
/// `db` is `None` either because the operator passed `--no-db` (no registry
/// exists -> generation is safe) or because the configured DB was unreachable
/// (`db_unreachable` -> fail closed when the CA is missing).
pub async fn init_ca(
    tls_dir: &Path,
    backup_dir: &Path,
    db: Option<&Database>,
    db_unreachable: bool,
    force_regenerate: bool,
) -> Result<TlsManager> {
    let ca_exists = pair_present(tls_dir);
    let backup_exists = pair_present(backup_dir);

    let client_count: Option<i64> = match db {
        Some(d) => Some(
            d.count_clients()
                .await
                .context("counting paired clients for CA guard")?,
        ),
        None => None,
    };

    let decision = decide(
        ca_exists,
        backup_exists,
        client_count,
        db_unreachable,
        force_regenerate,
    );

    match decision {
        CaDecision::LoadExisting => {
            let tm = TlsManager::load_or_create(
                &tls_dir.join(CA_CERT_FILE),
                &tls_dir.join(CA_KEY_FILE),
            )?;
            let fp = tm.ca_fingerprint();
            // Backup is best-effort: an unwritable backup dir (permissions,
            // read-only fs, sandboxed test env) must not brick startup. A
            // missing backup only degrades to the fail-closed abort path at
            // next boot — which is loud — so warn and continue.
            if let Err(e) = backup_ca(tls_dir, backup_dir, &fp) {
                tracing::warn!(
                    "CA backup failed ({}); continuing WITHOUT backup — \
                     CA loss will require manual restore or --regenerate-ca",
                    e
                );
            }
            Ok(tm)
        }
        CaDecision::RestoreFromBackup => {
            restore_ca(backup_dir, tls_dir)?;
            let tm = TlsManager::load_or_create(
                &tls_dir.join(CA_CERT_FILE),
                &tls_dir.join(CA_KEY_FILE),
            )?;
            let fp = tm.ca_fingerprint();
            // Verify the restored pair is the pair we recorded.
            let recorded =
                fs::read_to_string(backup_dir.join(FINGERPRINT_FILE)).unwrap_or_default();
            if !recorded.trim().is_empty() && recorded.trim() != fp {
                bail!(
                    "restored CA fingerprint {} does not match recorded {} — \
                     backup may be corrupt or tampered; resolve manually",
                    fp,
                    recorded.trim()
                );
            }
            backup_ca(tls_dir, backup_dir, &fp)?;
            Ok(tm)
        }
        CaDecision::GenerateFresh => {
            if let Some(count) = client_count.filter(|c| *c > 0) {
                tracing::warn!(
                    "--regenerate-ca given with {count} paired client(s); \
                     they will fail with UnknownIssuer until re-paired"
                );
            } else if db_unreachable {
                tracing::warn!("--regenerate-ca given while DB unreachable");
            }
            let tm = TlsManager::load_or_create(
                &tls_dir.join(CA_CERT_FILE),
                &tls_dir.join(CA_KEY_FILE),
            )?;
            let fp = tm.ca_fingerprint();
            if let Err(e) = backup_ca(tls_dir, backup_dir, &fp) {
                tracing::warn!(
                    "CA backup failed ({}); continuing WITHOUT backup — \
                     CA loss will require manual restore or --regenerate-ca",
                    e
                );
            }
            Ok(tm)
        }
        CaDecision::AbortMissingWithClients => {
            let count = client_count
                .map(|c| c.to_string())
                .unwrap_or_else(|| "unknown (DB unreachable)".to_string());
            bail!(
                "CA missing at {tls} and no backup at {bak}, but {count} paired client(s) \
                 exist in the database. Generating a fresh CA would break every client \
                 (UnknownIssuer). Options:\n\
                  1. Restore the CA: place ca.crt + ca.key back into {tls} \
                 (or into {bak} for auto-restore), then restart.\n\
                  2. Re-pair all clients after a deliberate rotation: restart with \
                 --regenerate-ca, then re-pair each client.",
                tls = tls_dir.display(),
                bak = backup_dir.display(),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;
    use std::path::PathBuf;

    fn tempdir(name: &str) -> PathBuf {
        let dir = env::temp_dir().join(format!(
            "monopsony-ca-guard-{}-{}",
            name,
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn make_ca(tls_dir: &Path) -> String {
        let tm =
            TlsManager::load_or_create(&tls_dir.join(CA_CERT_FILE), &tls_dir.join(CA_KEY_FILE))
                .unwrap();
        tm.ca_fingerprint()
    }

    fn cleanup(dir: &Path) {
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn decide_matrix() {
        // Live CA wins over everything.
        assert_eq!(
            decide(true, false, Some(5), false, false),
            CaDecision::LoadExisting
        );
        assert_eq!(
            decide(true, true, Some(5), false, false),
            CaDecision::LoadExisting
        );
        // Backup present -> restore (plan B), unless forced.
        assert_eq!(
            decide(false, true, Some(3), false, false),
            CaDecision::RestoreFromBackup
        );
        assert_eq!(
            decide(false, true, Some(0), false, true),
            CaDecision::GenerateFresh
        );
        // Nothing anywhere: client count decides.
        assert_eq!(
            decide(false, false, Some(0), false, false),
            CaDecision::GenerateFresh
        );
        assert_eq!(
            decide(false, false, Some(1), false, false),
            CaDecision::AbortMissingWithClients
        );
        assert_eq!(
            decide(false, false, Some(42), false, false),
            CaDecision::AbortMissingWithClients
        );
        // DB configured but unreachable -> cannot prove clientless -> fail closed.
        assert_eq!(
            decide(false, false, None, true, false),
            CaDecision::AbortMissingWithClients
        );
        // Explicit --no-db -> no registry exists -> safe to generate.
        assert_eq!(
            decide(false, false, None, false, false),
            CaDecision::GenerateFresh
        );
        // Force overrides everything except an existing live CA.
        assert_eq!(
            decide(false, false, Some(7), false, true),
            CaDecision::GenerateFresh
        );
        assert_eq!(
            decide(true, false, Some(7), false, true),
            CaDecision::LoadExisting
        );
    }

    #[test]
    fn backup_restore_roundtrip() {
        let tls = tempdir("roundtrip-tls");
        let bak = tempdir("roundtrip-bak");

        let fp = make_ca(&tls);
        backup_ca(&tls, &bak, &fp).unwrap();

        let orig_cert = fs::read(tls.join(CA_CERT_FILE)).unwrap();
        let orig_key = fs::read(tls.join(CA_KEY_FILE)).unwrap();

        // Simulate a cache-dir wipe.
        fs::remove_file(tls.join(CA_CERT_FILE)).unwrap();
        fs::remove_file(tls.join(CA_KEY_FILE)).unwrap();
        assert!(!pair_present(&tls));

        restore_ca(&bak, &tls).unwrap();
        assert_eq!(fs::read(tls.join(CA_CERT_FILE)).unwrap(), orig_cert);
        assert_eq!(fs::read(tls.join(CA_KEY_FILE)).unwrap(), orig_key);

        // Restored pair loads and matches the recorded fingerprint.
        let tm =
            TlsManager::load_or_create(&tls.join(CA_CERT_FILE), &tls.join(CA_KEY_FILE)).unwrap();
        assert_eq!(tm.ca_fingerprint(), fp);
        assert_eq!(
            fs::read_to_string(bak.join(FINGERPRINT_FILE))
                .unwrap()
                .trim(),
            fp
        );

        cleanup(&tls);
        cleanup(&bak);
    }

    #[test]
    fn backup_is_idempotent() {
        let tls = tempdir("idem-tls");
        let bak = tempdir("idem-bak");
        let fp = make_ca(&tls);

        backup_ca(&tls, &bak, &fp).unwrap();
        let first = fs::read(bak.join(CA_CERT_FILE)).unwrap();
        backup_ca(&tls, &bak, &fp).unwrap();
        let second = fs::read(bak.join(CA_CERT_FILE)).unwrap();
        assert_eq!(first, second);

        cleanup(&tls);
        cleanup(&bak);
    }

    #[test]
    fn backup_detects_drift() {
        let tls = tempdir("drift-tls");
        let bak = tempdir("drift-bak");
        let fp = make_ca(&tls);
        backup_ca(&tls, &bak, &fp).unwrap();

        // Replace the live CA with a different one (simulating out-of-band rotation).
        let tls2 = tempdir("drift-tls2");
        let _fp2 = make_ca(&tls2);
        fs::copy(tls2.join(CA_CERT_FILE), tls.join(CA_CERT_FILE)).unwrap();
        fs::copy(tls2.join(CA_KEY_FILE), tls.join(CA_KEY_FILE)).unwrap();

        backup_ca(&tls, &bak, &fp).unwrap();
        // Backup must now hold the NEW cert, not the old one.
        let live = fs::read(tls.join(CA_CERT_FILE)).unwrap();
        let backed = fs::read(bak.join(CA_CERT_FILE)).unwrap();
        assert_eq!(live, backed);

        cleanup(&tls);
        cleanup(&tls2);
        cleanup(&bak);
    }

    #[test]
    fn restore_rejects_partial_backup() {
        let tls = tempdir("partial-tls");
        let bak = tempdir("partial-bak");
        let fp = make_ca(&tls);
        backup_ca(&tls, &bak, &fp).unwrap();

        fs::remove_file(bak.join(CA_KEY_FILE)).unwrap();
        assert!(restore_ca(&bak, &tls).is_err());

        cleanup(&tls);
        cleanup(&bak);
    }

    /// The full fail-closed flow requires a PostgreSQL DB; covered live on the
    /// test bench instead (10.10.20.58 runbook: wipe tls dir + backup dir with
    /// clients present -> server must abort with recovery instructions).
    #[test]
    fn decision_aborts_are_fail_closed_by_construction() {
        // Belt-and-braces: every "abort" input shape maps to the abort variant.
        for (count, unreachable) in [(Some(1i64), false), (Some(100), false), (None, true)] {
            assert_eq!(
                decide(false, false, count, unreachable, false),
                CaDecision::AbortMissingWithClients
            );
        }
    }
}
