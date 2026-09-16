# Slice 6 (wave 2) — override command (signed downgrade exceptions)

## Read first (ONLY these)
1. docs/plans/sprint20-phase4/context-pack-common.md
2. crates/monopsony-client/src/ledger.rs (whole file — exceptions table,
   active_exception_on lock-free helper, tests module style)
3. crates/monopsony-client/src/main.rs — the Commands enum + imports
   (~lines 1-120), the `run_pair` region for handler style, and
   `dirs_ledger_parent` (~line 521) + the daemon ledger-open block (~902-926)
4. crates/monopsony-common/src/pin_store.rs lines ~90-160 — sign/verify +
   deterministic `SigningKey::from_bytes` test-key pattern (style reference
   ONLY; never reuse the pin key)

## File set (strict)
- crates/monopsony-client/src/ledger.rs (ADDITIVE: migration + signed-record
  API + signature-verified exception consult)
- crates/monopsony-client/src/main.rs (new `Override` subcommand + handler)
- Nothing else. lib.rs already declares both modules.

## Design — ledger.rs additive changes

### Migration (both fresh and existing DBs)
`Ledger::open`'s `execute_batch`: new DBs get the two extra columns directly
in the CREATE TABLE. Existing DBs: run, separately and AFTER the batch:
`ALTER TABLE exceptions ADD COLUMN signature TEXT` and
`ALTER TABLE exceptions ADD COLUMN signing_key TEXT`, each with errors
swallowed ONLY when the message contains "duplicate column" (rusqlite emits
that exact phrase); any other error propagates.

### Signed record type + API
```rust
pub struct SignedException {
    pub package_key: String,
    pub to_version: String,
    pub reason: String,
    pub created_at: i64,
    pub expires_at: i64,
    pub signature: Option<String>,   // lowercase hex Ed25519 over canonical JSON
    pub signing_key: Option<String>, // lowercase hex verifying key
}
```
- Canonical record JSON = `serde_json::json!({ "package_key":…, "to_version":…,
  "reason":…, "created_at":…, "expires_at":… })` then `serde_json::to_string`.
  serde_json's default map is a BTreeMap → deterministic alphabetical field
  order with NO feature flags. Sign those bytes with ed25519-dalek
  `SigningKey`; store sig + verifying key hex. Verification reconstructs the
  canonical string the same way from the row's fields. Do NOT hand-format
  with `format!` (escaping bugs silently change the signed payload).
- `pub fn add_signed_exception(&self, rec: &SignedException) ->
   rusqlite::Result<()>` — INSERT including signature/signing_key.
- `pub fn list_active_exceptions(&self, now: i64) ->
   rusqlite::Result<Vec<SignedException>>` — unexpired, id ASC.

### Signature verification on consult
Rewrite `active_exception_on` (keep it lock-free, `&Connection`) to select the
full row set (`id, package_key, to_version, reason, created_at, expires_at,
signature, signing_key`) WHERE `package_key = ? AND expires_at > ?` ORDER BY
`id DESC`; return the FIRST row that is valid:
- `signature`/`signing_key` both NULL → legacy unsigned row → honored as-is
  (documented migration path; plan §6 keeps old records working).
- otherwise decode both hex fields (32-byte key, 64-byte sig), reconstruct
  canonical JSON from the row, `VerifyingKey::verify` — valid → honored;
  invalid/undecodable → SKIP (row ignored). No panic, no silent unwrap.
Both callers (`check_and_update`'s downgrade branch via the lock-free helper,
and the public `active_exception`) get verification automatically — that is
why the check lives in `active_exception_on`, NOT in the callers.

## Design — main.rs Override subcommand

### Key management
- Key path: `dirs_ledger_parent(&cert_dir).join("ledger-override.key")` — same
  parent as ledger.sqlite3. Load-or-generate: 32 bytes from
  `rand::rngs::OsRng` (`RngCore::fill_bytes`), stored as lowercase hex text;
  on generation `chmod 0600` via `std::os::unix::fs::PermissionsExt` (non-unix:
  skip silently). Deterministic test keys come from `SigningKey::from_bytes`
  in ledger.rs tests — the CLI path uses OsRng.

### CLI
```
monopsony-client override add --package pacman:bash --to 5.2.37-1 \
    --reason "downgrading for kernel compat" [--ttl-secs 604800]
monopsony-client override list
```
`enum OverrideMode { Add { package, to, reason, ttl_secs: Option<i64> }, List }`
nested under `Commands::Override { #[command(subcommand)] mode: OverrideMode }`
— match the existing Commands/`SelfUpdate` nesting style exactly.
- Validation (Add): package_key contains exactly one ':' with non-empty pm and
  name; reason non-empty (clap + a runtime check); to_version non-empty;
  ttl default 604800.
- Handler: open the ledger at
  `dirs_ledger_parent(&cert_dir).join("ledger.sqlite3")` (same computation as
  the daemon block; duplicating the 3 lines is fine — do NOT refactor the
  daemon block). Build the record (`created_at = now`, `expires_at = now +
  ttl`), sign, `add_signed_exception`. Footgun UX print: package, target
  version, reason, expiry as RFC3339, the held high-water via
  `high_water_for(&package_key)` when present, and the sentence
  `"Downgrades of <key> will be permitted until <expiry>."` Exit 0.
- List: `list_active_exceptions(now)` → one line per row:
  `id  package_key  to_version  <expiry RFC3339>  <reason truncated 60>`; empty
  → `"No active exceptions."` Exit 0.
- Wire into the dispatch match as `async fn run_override(...)` following the
  `run_self_update` pattern (async fn, called with `.await`).

## Tests (ledger.rs tests module)
1. Legacy migration: raw rusqlite Connection creates the OLD 5-column
   exceptions table in a temp file, drop it, then `Ledger::open` succeeds and
   `add_signed_exception` works (columns exist).
2. Sign→verify roundtrip: deterministic key (`SigningKey::from_bytes([7u8;
   32])` pattern), `add_signed_exception`, `active_exception` → Some (valid
   sig).
3. Tampered row: after insert, raw-SQL `UPDATE exceptions SET signature =
   <64 'f' chars>` → `active_exception` → None.
4. Legacy unsigned row: raw-INSERT with NULL signature/key → honored.
5. Expiry still applies to signed rows (inside TTL → Some; past → None).
6. Tampered rows are skipped but a VALID later row still matches
   (insert valid row id=1, tampered id=2 → id 1 honored — DESC order walks
   past the bad one).

## Do NOT
- Do not touch index_parse.rs or proxy.rs — slice 5 owns proxy.rs this wave.
  A compile error in a file outside your set is FOREIGN (see pack); re-run
  once, then report it, never fix it.
- No new dependencies (ed25519-dalek with rand_core, rand, hex, serde_json
  are all already client deps).
- Do not change check_and_update's verdict semantics or add policy code to
  ledger.rs.

## Verify (ONCE, from /home/martin/projects/monopsony)
`cargo test -p monopsony-client ledger -- --nocapture && cargo run -p monopsony-client -- override --help`

## Reporting
done / not_done / deviations + verbatim verify tail. Floor: migration +
signed add + verify-on-consult + `override --help` runs. `list` and tests 5-6
may go not_done — never half-edit.
