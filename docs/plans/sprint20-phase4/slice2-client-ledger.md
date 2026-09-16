# Slice 2 (wave 1) — monopsony-client: SQLite high-water ledger

## Read first (ONLY these)
1. docs/plans/sprint20-phase4/context-pack-common.md
2. crates/monopsony-client/src/pin_verify.rs — TamperEvent struct + record_event
   pattern (the ledger's tamper events use the same shape/spool)

## File set
- crates/monopsony-client/src/ledger.rs (NEW)
- crates/monopsony-client/src/lib.rs (add `pub mod ledger;` — check main.rs also
  declares it if main-reachable)
- crates/monopsony-client/Cargo.toml (add `rusqlite = { workspace = true }`)

## Design
SQLite store, sync rusqlite (calls wrapped in tokio::task::spawn_blocking at the
proxy call site — slice 4's job; this slice is sync API only).

### Schema
```sql
CREATE TABLE IF NOT EXISTS high_water (
    package_key  TEXT PRIMARY KEY,   -- "<pm>:<name>" e.g. "pacman:bash"
    version      TEXT NOT NULL,
    version_ord  TEXT NOT NULL,      -- order_key() form (display only)
    source       TEXT NOT NULL,      -- canonical index URL (normalize_url_scheme)
    seen_at      INTEGER NOT NULL    -- unix seconds
);
CREATE TABLE IF NOT EXISTS repo_seen (
    repo_key     TEXT PRIMARY KEY,   -- canonicalized index URL directory
    last_seen    INTEGER NOT NULL    -- unix seconds of last index response
);
CREATE TABLE IF NOT EXISTS exceptions (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    package_key  TEXT NOT NULL,
    to_version   TEXT NOT NULL,
    reason       TEXT NOT NULL,
    created_at   INTEGER NOT NULL,
    expires_at   INTEGER NOT NULL
);
```

### API (Ledger struct, opened per daemon run; single connection + Mutex)
```rust
pub struct Ledger { /* conn: Mutex<rusqlite::Connection> */ }
impl Ledger {
    pub fn open(path: &Path) -> rusqlite::Result<Self>;      // runs migrations, parent dir create_dir_all
    pub fn check_and_update(&self, package_key: &str, version_ord_input: &str,
        normalized: &monopsony_common::version_ord::NormalizedVersion,
        source: &str, now: i64) -> Result<CheckVerdict, rusqlite::Error>;
    pub fn repo_seen(&self, repo_key: &str, now: i64) -> rusqlite::Result<()>;
    pub fn repo_age_secs(&self, repo_key: &str) -> Option<i64>;
    pub fn add_exception(&self, package_key: &str, to_version: &str,
        reason: &str, ttl_secs: i64, now: i64) -> rusqlite::Result<()>;
    pub fn active_exception(&self, package_key: &str, now: i64) -> Option<String>;
}

pub enum CheckVerdict { Ok, Downgrade { held: String } }
```
- check_and_update: lookup high-water for package_key → vercmp(new, held):
  - Equal or Greater → UPDATE row (monotonic max), return Ok.
  - Less → do NOT touch row; if an unexpired exception for the package matches
    the offered version (or any version — record-scoped), return Ok; else
    Downgrade{held}.
  - First contact (no row) → INSERT, return Ok.
- WAL mode on open (`PRAGMA journal_mode=WAL`) — the proxy task and daemon both
  touch it.
- **Test keys must be realistic:** package_key uses `:` (e.g. "pacman:bash") and
  source uses a canonicalized URL with a path — flat keys hide schema bugs (see
  the pin parent-dir lesson in the dev skill).

### Tests (in-file, ≥8; rusqlite tempdir file DBs)
- first contact records + Ok
- monotonic max: upgrade then attempted downgrade holds
- downgrade returns held version string
- exception allows the downgrade, expires after ttl (inject `now`)
- repo_seen / repo_age_secs round-trip
- open() creates parent dirs; re-open preserves rows (WAL)
- equal version re-seen → Ok (no false downgrade)

## Do NOT
- Do not touch proxy.rs/main.rs (slice 4 wires it).
- Do not implement vercmp (slice 1 owns monopsony-common::version_ord; consume it).

## Verify (ONCE)
`cargo check -p monopsony-client && cargo test -p monopsony-client ledger -- --nocapture`
