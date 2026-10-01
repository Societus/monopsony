# Slice 2 — Phase C: scrubber (re-hash, quarantine, last_verified_at)

**Pack first:** `docs/plans/sprint23-remaining/context-pack-common.md`
**Safe-list:** `crates/monopsony-proxy/src/scrubber.rs` (NEW file — placeholder +
`mod scrubber;` already landed in lib.rs by the parent; replace the placeholder
comment, never edit lib.rs).

## Deliverable

One new module `crates/monopsony-proxy/src/scrubber.rs` — the verification pass as a
pure, unit-tested module. Plan §2.3 is the authority.

## Design (follow exactly)

`scrub_pass(storage_root, meta_root, cfg, min_age_cutoff_epoch) -> ScrubReport`:

1. Iterate cached LangArtifact blobs. **Skip** if `now − last_verified_at <
   scrub_min_age_secs` (read from the sidecar; `last_verified_at` already exists on
   ObjectMeta, serde-defaulted 0 → old sidecars get scrubbed first, backfilled after).
   Note: `scrub_interval_secs` gates WHEN passes run (parent wiring concern); your
   module consumes `scrub_min_age_secs` only.
2. Per blob: re-hash with sha2, compare to the sidecar's stored checksum.
3. **Mismatch → quarantine**: move the blob to `<storage_root>/.quarantined/<key>`
   (create_dir_all the parent — cache keys contain slashes; this exact
   full-parent-chain trap has bitten this codebase twice), record a
   `ScrubQuarantine { key, expected_sha256, actual_sha256 }` in the report, log WARN
   (`tracing` is already a proxy-crate dep). NEVER delete the blob — forensic
   evidence. Return quarantine entries; the PARENT wiring converts them to tamper
   alerts (DB + alert path already exist: `TamperAlertInsert` in monopsony-db).
4. Pass: update the sidecar's `last_verified_at` to now.
5. Report struct: `{ scanned, skipped_fresh, verified_ok, quarantined: Vec<...>,
   errors: Vec<(key, String)> }`. A lost/failed scrub write is `error!`-loud, never
   a silent `if let Ok` swallow (ENOSPC class — do not swallow IO errors).

## Tests (minimum)

- Skip-fresh: blob verified within min_age → skipped, sidecar untouched.
- Pass: checksum matches → verified_ok, last_verified_at updated.
- Mismatch: corrupt the blob file directly (real layout: blob `<root>/<key>` +
  sidecar `<root>/.meta/<key>`; backend `ObjectStoreBackend::local(tempdir)` +
  `ProxyState::new(storage, None)` is the established fixture pattern — but you only
  need the sidecar layout, not ProxyState) → file MOVED to `.quarantined/`, original
  gone, report entry has both hashes.
- Nested key quarantine: use a REALISTIC slashed cache key (`host/path/file`) — a
  flat key hides the create_dir_all bug.
- Errors surface: unwritable root → entries in `errors`, not a panic, not silent.

## Verify (once, from repo root)

`cargo test -p monopsony-proxy --lib scrubber` — all pass, no new warnings naming
scrubber.rs.
