//! Phase C scrubber: integrity verification pass over cached language artifacts.
//!
//! Re-hashes each cached blob with SHA-256 and compares it against the
//! checksum recorded in its JSON sidecar (`<meta_root>/<key>`, the same
//! sidecar family `ObjectStoreBackend` writes under `.meta/<key>`).
//!
//! - **Fresh** blobs (verified within `scrub_min_age_secs`) are skipped.
//! - **Mismatches** are quarantined — the blob is *moved* (never deleted) to
//!   `<storage_root>/.quarantined/<key>` as forensic evidence; the caller
//!   converts report entries into tamper alerts (`TamperAlertInsert` in
//!   monopsony-db — parent wiring, not this module's job).
//! - **Passes** backfill `last_verified_at = now` in the sidecar.
//!
//! This module is deliberately pure and synchronous: it takes explicit roots
//! and a cutoff epoch, touches nothing global, and returns a
//! [`ScrubReport`]. `scrub_interval_secs` gates *when* passes run (parent
//! wiring); this module only consumes `scrub_min_age_secs`.
//!
//! IO errors are never swallowed: a lost/failed sidecar write (ENOSPC class)
//! surfaces loudly via `error!` and lands in `ScrubReport::errors`.

use std::fs;
use std::path::{Path, PathBuf};

use sha2::Digest;
use tracing::{error, warn};

/// One quarantined blob: checksum mismatch between sidecar and re-hash.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ScrubQuarantine {
    /// Cache key (slashed, relative to `storage_root`).
    pub key: String,
    /// SHA-256 recorded in the sidecar (expected).
    pub expected_sha256: String,
    /// SHA-256 of the blob on disk (actual).
    pub actual_sha256: String,
}

/// Result of one scrub pass.
#[derive(Debug, Default, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ScrubReport {
    /// Total blobs found under `storage_root` (excluding meta/quarantine).
    pub scanned: usize,
    /// Blobs skipped because `last_verified_at` is newer than the cutoff.
    pub skipped_fresh: usize,
    /// Blobs whose re-hash matched the sidecar checksum.
    pub verified_ok: usize,
    /// Blobs quarantined for checksum mismatch.
    pub quarantined: Vec<ScrubQuarantine>,
    /// Per-key failures (missing/corrupt sidecar, IO errors). Never silent.
    pub errors: Vec<(String, String)>,
}

/// Sidecar fields the scrubber needs. Mirrors the private `ObjectMeta` in
/// `object_store_backend.rs`; `#[serde(default)]` keeps pre-Sprint-23
/// sidecars (no `last_verified_at`) readable — those deserialize as
/// never-verified and get scrubbed first, then backfilled.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct ScrubSidecar {
    #[serde(default)]
    checksum_sha256: String,
    #[serde(default)]
    size_bytes: u64,
    #[serde(default)]
    cache_class: monopsony_common::config::CacheClass,
    #[serde(default)]
    last_verified_at: i64,
}

/// Run one scrub pass over the cache rooted at `storage_root`.
///
/// * `meta_root` — directory holding `<key>` JSON sidecars (production:
///   `<storage_root>/.meta`).
/// * `cfg` — language-artifact config; only `scrub_min_age_secs` is consumed.
/// * `min_age_cutoff_epoch` — `now − scrub_min_age_secs`; blobs whose sidecar
///   `last_verified_at` is strictly newer are skipped as fresh. `now` is
///   recovered as `cutoff + scrub_min_age_secs` for the backfill stamp, which
///   keeps the pass deterministic under test.
pub fn scrub_pass(
    storage_root: &Path,
    meta_root: &Path,
    cfg: &monopsony_common::config::LanguageArtifactConfig,
    min_age_cutoff_epoch: i64,
) -> ScrubReport {
    let mut report = ScrubReport::default();
    let now = min_age_cutoff_epoch.saturating_add(cfg.scrub_min_age_secs as i64);
    let quarantine_root = storage_root.join(".quarantined");

    let blobs = match collect_blobs(storage_root, meta_root, &quarantine_root) {
        Ok(blobs) => blobs,
        Err(e) => {
            error!(root = %storage_root.display(), error = %e,
                   "scrubber: cannot walk storage root");
            report
                .errors
                .push(("<storage_root>".to_string(), e.to_string()));
            return report;
        }
    };

    for blob_path in blobs {
        report.scanned += 1;
        // Unwrap-safe: collect_blobs only yields paths under storage_root.
        let key = blob_path
            .strip_prefix(storage_root)
            .expect("blob under storage_root")
            .to_string_lossy()
            .replace('\\', "/");

        let sidecar_path = meta_root.join(&key);
        let raw = match fs::read_to_string(&sidecar_path) {
            Ok(raw) => raw,
            Err(e) => {
                error!(key = %key, error = %e, "scrubber: sidecar unreadable");
                report
                    .errors
                    .push((key, format!("sidecar unreadable: {e}")));
                continue;
            }
        };
        let sidecar: ScrubSidecar = match serde_json::from_str(&raw) {
            Ok(sc) => sc,
            Err(e) => {
                error!(key = %key, error = %e, "scrubber: sidecar corrupt");
                report.errors.push((key, format!("sidecar corrupt: {e}")));
                continue;
            }
        };

        // Fresh: verified more recently than the min-age cutoff → skip,
        // sidecar untouched.
        if sidecar.last_verified_at > min_age_cutoff_epoch {
            report.skipped_fresh += 1;
            continue;
        }

        let actual = match hash_blob(&blob_path) {
            Ok(h) => h,
            Err(e) => {
                error!(key = %key, error = %e, "scrubber: blob unreadable");
                report.errors.push((key, format!("blob unreadable: {e}")));
                continue;
            }
        };

        let expected = sidecar.checksum_sha256.to_lowercase();
        if actual == expected {
            // Pass: backfill last_verified_at = now.
            let updated = ScrubSidecar {
                checksum_sha256: sidecar.checksum_sha256,
                size_bytes: sidecar.size_bytes,
                cache_class: sidecar.cache_class,
                last_verified_at: now,
            };
            let json = match serde_json::to_string_pretty(&updated) {
                Ok(json) => json,
                Err(e) => {
                    error!(key = %key, error = %e, "scrubber: sidecar serialize failed");
                    report
                        .errors
                        .push((key, format!("sidecar serialize failed: {e}")));
                    continue;
                }
            };
            match write_atomic(&sidecar_path, json.as_bytes()) {
                Ok(()) => report.verified_ok += 1,
                Err(e) => {
                    // ENOSPC class: loud, never a silent swallow.
                    error!(key = %key, error = %e, "scrubber: sidecar update failed");
                    report
                        .errors
                        .push((key, format!("sidecar update failed: {e}")));
                }
            }
        } else {
            // Mismatch: quarantine (move, never delete) as forensic evidence.
            let q_path = quarantine_root.join(&key);
            if let Some(parent) = q_path.parent() {
                // Slashed keys nest: the full parent chain must exist.
                if let Err(e) = fs::create_dir_all(parent) {
                    error!(key = %key, error = %e, "scrubber: quarantine mkdir failed");
                    report
                        .errors
                        .push((key, format!("quarantine mkdir failed: {e}")));
                    continue;
                }
            }
            match fs::rename(&blob_path, &q_path) {
                Ok(()) => {
                    warn!(key = %key, expected = %expected, actual = %actual,
                          "scrubber: checksum mismatch — blob quarantined");
                    report.quarantined.push(ScrubQuarantine {
                        key,
                        expected_sha256: expected,
                        actual_sha256: actual,
                    });
                }
                Err(e) => {
                    error!(key = %key, error = %e, "scrubber: quarantine move failed");
                    report
                        .errors
                        .push((key.clone(), format!("quarantine move failed: {e}")));
                }
            }
        }
    }

    report
}

/// Recursively collect blob file paths under `storage_root`, skipping the
/// sidecar tree (`meta_root`) and the quarantine root.
fn collect_blobs(
    storage_root: &Path,
    meta_root: &Path,
    quarantine_root: &Path,
) -> std::io::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    visit(storage_root, meta_root, quarantine_root, &mut out)?;
    out.sort();
    Ok(out)
}

fn visit(
    dir: &Path,
    meta_root: &Path,
    quarantine_root: &Path,
    out: &mut Vec<PathBuf>,
) -> std::io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path == meta_root || path.starts_with(meta_root) {
            continue;
        }
        if path == quarantine_root || path.starts_with(quarantine_root) {
            continue;
        }
        let ft = fs::symlink_metadata(&path)?.file_type();
        if ft.is_symlink() {
            // Never follow symlinks out of the cache root.
            continue;
        }
        if ft.is_dir() {
            visit(&path, meta_root, quarantine_root, out)?;
        } else if ft.is_file() {
            out.push(path);
        }
    }
    Ok(())
}

/// SHA-256 hex digest of a file, streamed (blobs can be large).
fn hash_blob(path: &Path) -> std::io::Result<String> {
    use std::io::Read;
    let mut file = fs::File::open(path)?;
    let mut hasher = sha2::Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// Temp-file + rename write so a crash mid-write can't shred a good sidecar.
fn write_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("scrub-tmp");
    fs::write(&tmp, data)?;
    fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use monopsony_common::config::LanguageArtifactConfig;

    fn sha256_hex(data: &[u8]) -> String {
        let mut h = sha2::Sha256::new();
        h.update(data);
        hex::encode(h.finalize())
    }

    /// Realistic cache fixture: blob at `<root>/<key>`, sidecar at
    /// `<root>/.meta/<key>`. Slashed keys — flat keys hide the
    /// create_dir_all parent-chain bug.
    struct Fixture {
        root: PathBuf,
        meta: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let root = tempfile::tempdir().unwrap().keep();
            let meta = root.join(".meta");
            fs::create_dir_all(&meta).unwrap();
            Self { root, meta }
        }

        fn put(&self, key: &str, data: &[u8], last_verified_at: i64) {
            let blob = self.root.join(key);
            fs::create_dir_all(blob.parent().unwrap()).unwrap();
            fs::write(&blob, data).unwrap();
            let sidecar = self.meta.join(key);
            fs::create_dir_all(sidecar.parent().unwrap()).unwrap();
            let sc = ScrubSidecar {
                checksum_sha256: sha256_hex(data),
                size_bytes: data.len() as u64,
                cache_class: monopsony_common::config::CacheClass::LangArtifact,
                last_verified_at,
            };
            fs::write(sidecar, serde_json::to_string(&sc).unwrap()).unwrap();
        }

        fn blob_exists(&self, key: &str) -> bool {
            self.root.join(key).is_file()
        }

        fn quarantined_exists(&self, key: &str) -> bool {
            self.root.join(".quarantined").join(key).is_file()
        }

        fn sidecar(&self, key: &str) -> ScrubSidecar {
            let raw = fs::read_to_string(self.meta.join(key)).unwrap();
            serde_json::from_str(&raw).unwrap()
        }
    }

    fn cfg(min_age: u64) -> LanguageArtifactConfig {
        LanguageArtifactConfig {
            scrub_min_age_secs: min_age,
            ..Default::default()
        }
    }

    const KEY: &str = "registry.npmjs.org/react/-/react-18.2.0.tgz";

    #[test]
    fn skip_fresh_leaves_sidecar_untouched() {
        let fx = Fixture::new();
        let data = b"fresh blob";
        fx.put(KEY, data, 1_000);
        // cutoff 900: last_verified_at 1000 > 900 → fresh.
        let report = scrub_pass(&fx.root, &fx.meta, &cfg(100), 900);

        assert_eq!(report.scanned, 1);
        assert_eq!(report.skipped_fresh, 1);
        assert_eq!(report.verified_ok, 0);
        assert!(report.quarantined.is_empty() && report.errors.is_empty());
        assert_eq!(fx.sidecar(KEY).last_verified_at, 1_000); // untouched
        assert!(fx.blob_exists(KEY));
    }

    #[test]
    fn pass_updates_last_verified_at() {
        let fx = Fixture::new();
        let data = b"intact blob";
        fx.put(KEY, data, 0); // never verified → old sidecar gets scrubbed
                              // cutoff 10_000 = now(10_100) − min_age(100).
        let report = scrub_pass(&fx.root, &fx.meta, &cfg(100), 10_000);

        assert_eq!(report.scanned, 1);
        assert_eq!(report.verified_ok, 1);
        assert!(report.quarantined.is_empty() && report.errors.is_empty());
        assert_eq!(fx.sidecar(KEY).last_verified_at, 10_100); // backfilled to now
        assert!(fx.blob_exists(KEY));
    }

    #[test]
    fn mismatch_quarantines_blob_and_reports_both_hashes() {
        let fx = Fixture::new();
        fx.put(KEY, b"original bytes", 0);
        // Corrupt the blob directly; sidecar still holds the old checksum.
        fs::write(fx.root.join(KEY), b"tampered bytes!").unwrap();

        let report = scrub_pass(&fx.root, &fx.meta, &cfg(100), 10_000);

        assert_eq!(report.scanned, 1);
        assert_eq!(report.quarantined.len(), 1);
        let q = &report.quarantined[0];
        assert_eq!(q.key, KEY);
        assert_eq!(q.expected_sha256, sha256_hex(b"original bytes"));
        assert_eq!(q.actual_sha256, sha256_hex(b"tampered bytes!"));
        // Moved, not copied: original gone, forensic copy under .quarantined/.
        assert!(!fx.blob_exists(KEY));
        assert!(fx.quarantined_exists(KEY));
        assert!(report.errors.is_empty());
    }

    #[test]
    fn nested_slashed_key_quarantines_through_full_parent_chain() {
        let fx = Fixture::new();
        let deep = "files.pythonhosted.org/packages/a9/6e/pkg.whl";
        fx.put(deep, b"evil payload", 0);
        // Corrupt the blob; sidecar still holds the original checksum.
        fs::write(fx.root.join(deep), b"corrupted payload").unwrap();

        let report = scrub_pass(&fx.root, &fx.meta, &cfg(100), 10_000);

        assert_eq!(report.quarantined.len(), 1);
        assert_eq!(report.quarantined[0].key, deep);
        // The multi-level .quarantined parent chain was created, blob moved.
        assert!(fx.quarantined_exists(deep));
        assert!(!fx.blob_exists(deep));
    }

    #[test]
    fn errors_surface_not_silent() {
        let fx = Fixture::new();
        // (a) Unwritable meta root: replace a sidecar with a DIRECTORY —
        // reads/writes of the sidecar path fail even for root, no panic.
        let bad_key = "registry.npmjs.org/bad/pkg.tgz";
        let blob = fx.root.join(bad_key);
        fs::create_dir_all(blob.parent().unwrap()).unwrap();
        fs::write(&blob, b"data").unwrap();
        let sidecar_dir = fx.meta.join(bad_key);
        fs::create_dir_all(&sidecar_dir).unwrap(); // directory where file expected

        // (b) Corrupt sidecar JSON.
        let junk_key = "registry.npmjs.org/junk/pkg.tgz";
        let blob = fx.root.join(junk_key);
        fs::create_dir_all(blob.parent().unwrap()).unwrap();
        fs::write(&blob, b"data").unwrap();
        let sc = fx.meta.join(junk_key);
        fs::create_dir_all(sc.parent().unwrap()).unwrap();
        fs::write(&sc, "{not json").unwrap();

        let report = scrub_pass(&fx.root, &fx.meta, &cfg(100), 10_000);

        assert_eq!(report.scanned, 2);
        assert_eq!(report.errors.len(), 2);
        let err_keys: Vec<&str> = report.errors.iter().map(|(k, _)| k.as_str()).collect();
        assert!(err_keys.contains(&bad_key));
        assert!(err_keys.contains(&junk_key));
        assert!(report.quarantined.is_empty());
        // Blobs untouched on error — never deleted on a failed verify.
        assert!(fx.root.join(bad_key).is_file());
        assert!(fx.root.join(junk_key).is_file());
    }

    #[test]
    fn missing_sidecar_is_an_error_not_a_panic() {
        let fx = Fixture::new();
        let blob = fx.root.join("orphan.blob");
        fs::write(&blob, b"no sidecar").unwrap();

        let report = scrub_pass(&fx.root, &fx.meta, &cfg(100), 10_000);

        assert_eq!(report.scanned, 1);
        assert_eq!(report.errors.len(), 1);
        assert!(report.errors[0].0 == "orphan.blob");
        assert!(fx.blob_exists("orphan.blob"));
    }

    #[test]
    fn meta_and_quarantine_trees_are_not_rescrubbed() {
        let fx = Fixture::new();
        // A blob that was quarantined in an earlier pass must not reappear.
        let q = fx.root.join(".quarantined").join("old/pkg.tgz");
        fs::create_dir_all(q.parent().unwrap()).unwrap();
        fs::write(&q, b"already quarantined").unwrap();

        let report = scrub_pass(&fx.root, &fx.meta, &cfg(100), 10_000);

        assert_eq!(report.scanned, 0);
        assert!(fx.quarantined_exists("old/pkg.tgz")); // still there
    }

    #[test]
    fn unwalkable_root_lands_in_errors() {
        // Pass a FILE as the storage root: read_dir fails, no panic.
        let tmp = tempfile::tempdir().unwrap();
        let file_root = tmp.path().join("not-a-dir");
        fs::write(&file_root, b"x").unwrap();
        let meta = tmp.path().join(".meta");

        let report = scrub_pass(&file_root, &meta, &cfg(100), 10_000);

        assert_eq!(report.scanned, 0);
        assert_eq!(report.errors.len(), 1);
        assert_eq!(report.errors[0].0, "<storage_root>");
    }
}
