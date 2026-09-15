# Slice 2 — DB: pin + tamper CRUD methods

## Read first (ONLY these)
1. docs/plans/sprint20-phase3/context-pack-common.md
2. crates/monopsony-db/src/db.rs — regions ONLY: `run_migrations` (see the new cache_pins +
   tamper_alerts tables the hub added, ~line 325-360), `update_cache_sha256` (~line 702) and
   `get_cache_entry` (~line 711) as style templates, `ScanResultRow` struct (~line 60).

## File set (touch NOTHING else)
- crates/monopsony-db/src/db.rs

## Design to implement
Add a `PinRow` struct + 5 methods on `Database`, matching the file's existing style
(sqlx::query, `.bind`, `fetch_optional`, `Result<>` = crate's own):
```rust
pub struct PinRow {
    pub url: String,
    pub expected_sha256: String,
    pub origin: String,
    pub key_id: String,
    pub observed_at: i64,
    pub signature: String,
}

pub async fn record_pin(&self, row: &PinRow) -> Result<()>          // INSERT ... ON CONFLICT(url) DO UPDATE SET everything
pub async fn get_pin(&self, url: &str) -> Result<Option<PinRow>>
pub async fn delete_pin(&self, url: &str) -> Result<bool>
pub async fn record_tamper_alert(&self, alert: &TamperAlertInsert) -> Result<i64>  // returns id
pub async fn has_tamper_alert(&self, url: &str) -> Result<bool>     // EXISTS query
pub async fn list_tamper_alerts(&self, limit: i64) -> Result<Vec<TamperAlertRow>>
```
with
```rust
pub struct TamperAlertInsert {
    pub url: String,
    pub expected_sha256: String,
    pub actual_sha256: String,
    pub detail: Option<String>,
    pub client_id: Option<String>,
    pub observed_at: i64,
}
pub struct TamperAlertRow { pub id: i64, pub url: String, pub expected_sha256: String,
    pub actual_sha256: String, pub detail: Option<String>, pub client_id: Option<String>,
    pub observed_at: i64, pub recorded_at: Option<chrono::DateTime<chrono::Utc>> }
```
(chrono is already a dep of this crate — verify in Cargo.toml; if the recorded_at type is
awkward, use `Option<sqlx::types::chrono::DateTime<chrono::Utc>>` to match file style.)
Placement: immediately after `update_cache_sha256`/`get_cache_entry` region, before
`cache_stats`. Do not reorder anything.

## Tests
db.rs has no unit tests (needs live PG — by design). Do NOT add tests. `cargo check -p
monopsony-db` green is the bar.

## Do NOT
- Do not touch table definitions (already landed) — if you find a mismatch between your SQL
  and the landed schema, adapt YOUR SQL, not the schema, and report it.
- Do not add dependencies.

## Verify (ONCE, from /home/societus/projects/monopsony)
`cargo check -p monopsony-db`
