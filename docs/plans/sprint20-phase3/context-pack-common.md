# Sprint 20 Phase 3 — context pack (common)

## Mission
Hash pinning integrity core (plan §5 of docs/plans/2026-09-13-https-transport-and-package-integrity.md).
Server pins origin hashes at spool commit with Ed25519 attestations; client proxy verifies
served bytes against the signed pin before handing them to the package manager.

## Hard rules
- HARD ~600s cap: work linearly, write files immediately, verify ONCE at the end.
- You may read ONLY the files named in your slice spec (plus the target file).
- Do NOT commit. Do NOT touch files outside your slice's file set.
- The standalone linter reports "async fn not permitted in Rust 2015" — FALSE POSITIVE
  (workspace is edition 2021). The patch tool may also print pre-existing lint noise for
  db.rs/proxy.rs/main.rs. Ignore; `cargo check -p <crate>` is the only verdict.
- If time runs short, SKIP remaining items and report them under not_done. Never half-edit.

## Contracts already landed by the hub (DO NOT re-create; consume exactly)
- proto/monopsony.proto: `PairResponse.mirror_verify_key = 7` (bytes, 32 raw Ed25519),
  `RegisterResponse.mirror_verify_key = 6`, new `TamperAlert{url=1, expected_sha256=2,
  actual_sha256=3, detail=4, observed_at=5}`, `ClientMessage.tamper_alert = 5` oneof arm.
- crates/monopsony-common/src/config.rs: `IntegrityConfig { pinning_enabled: bool (default
  false), unverified_policy: String (default "flag"; alt "double_fetch") }` +
  `ServerConfig.integrity: IntegrityConfig` (`#[serde(default)]`).
- crates/monopsony-common/src/pin_store.rs (NEW, hub-written, tests green):
  `PinOrigin{Index,Sidecar,Header,Shim,Unverified}` (serde snake_case, `.as_str()`),
  `PinAttestation{url, expected_sha256, origin, key_id, observed_at, signature}` with
  `.canonical_json()`, `.sign(&SigningKey)`, `.verify(&VerifyingKey) -> bool`,
  `key_id(&VerifyingKey) -> String`, `to_json(&PinAttestation) -> String`,
  `from_json(&str) -> Option<PinAttestation>`. Exported at `monopsony_common::pin_store`.
- crates/monopsony-proxy/src/proxy.rs: `ProxyState` has `pub pin_key:
  Option<ed25519_dalek::SigningKey>` and `pub integrity: monopsony_common::config::IntegrityConfig`
  (defaults via `ProxyState::new`); `run_proxy(...)` takes two extra trailing params:
  `pin_key: Option<ed25519_dalek::SigningKey>, integrity: monopsony_common::config::IntegrityConfig`.
- crates/monopsony-db/src/db.rs migrations: `cache_pins(url PK, expected_sha256, origin,
  key_id, observed_at, signature)` and `tamper_alerts(id BIGSERIAL, url, expected_sha256,
  actual_sha256, detail NULL, client_id NULL, observed_at, recorded_at DEFAULT now())`.
- crates/monopsony-admin/src/lib.rs: `AdminState.mirror_verify_key: [u8; 32]`.
- crates/monopsony-server/src/main.rs: run_proxy call currently passes `None` +
  `IntegrityConfig::default()` placeholders (the main-slice replaces them).

## Pin semantics (do not deviate)
- Pin hash = the hash the ORIGIN declares (header/sidecar/shim). Only when no origin source
  exists: origin=Unverified and expected_sha256 = commit-time computed hash (observation only).
- Signing key = the server's dedicated pin key (`<shadow_dir>/pin-signing.key`, loaded via
  `monopsony_shadow::shadow_signing::load_or_generate_signing_key`). NEVER the mTLS CA and
  NEVER the shadow recipe signing key path (`signing.key`).
- URL keys in DB/pins are ALWAYS `normalize_url_scheme(...)` (https://) — same canonicalization
  as cache_entries (see proxy.rs run_tee DB writes for the pattern). NEVER normalize the fetch URL itself.
- Quarantine = the tamper_alerts row makes the URL un-servable (HIT path returns 403).
  Do NOT delete the object (forensic evidence).
- A zeroed verify key ([0u8;32]) means "pinning not provisioned" — treat as inert everywhere.

## Verify (run ONCE, from /home/societus/projects/monopsony)
`cargo check -p <your crate>` (add `-p` args per your slice). Full-suite runs are the hub's job.

## Reporting
Report: done / not_done / deviations. Verbatim final `cargo check` tail. Do not claim green
without the check output in your transcript.
