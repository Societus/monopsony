# Sprint 20 Phase 4 — Wave 1 Status (2026-09-15)

Client monotonic ledger + downgrade enforcement (plan §6 of
`docs/plans/2026-09-13-https-transport-and-package-integrity.md`).
**Uncommitted — awaiting user GPG signing.** Slice specs + context pack:
`docs/plans/sprint20-phase4/`.

## What landed (wave 1, all verified)

| Slice | File(s) | Status |
|---|---|---|
| Hub contracts | `monopsony-common/src/config.rs` (LedgerConfig on ClientConfig, manual Default), `Cargo.toml` (zstd ws dep), mod decls (client lib.rs/main.rs, common lib.rs) | landed |
| 1 version_ord | `monopsony-common/src/version_ord.rs` (NEW, 12 tests) | landed, hub |
| 2 ledger | `monopsony-client/src/ledger.rs` (NEW, 10 tests) | dispatched → finished in-parent (timeout at 600s with code landed; deadlock found + fixed) |
| 3 index_parse | `monopsony-client/src/index_parse.rs` (NEW, 9 tests) | dispatched, green |
| 4 wiring | client `proxy.rs` + `main.rs` | landed, hub |

Verification (final): full workspace `cargo test --workspace` — every suite
0 failed. Key suites: client 56 lib + 127 bin + 12 integration; common 15
(incl. 12 version_ord); proxy 119 + 3 + 11 (streaming_tests run_proxy call
site updated for the new 6-arg signature — pass-through `None` ledger = inert).
`cargo check -p monopsony-client` clean.

## Design decisions baked (do not re-litigate)

- **Ledger home:** `$HOME/.local/state/monopsony/ledger.sqlite3` — NOT
  cert_dir (re-pairing must not wipe the anti-rewind anchor).
- **LedgerConfig defaults:** enabled=true, policy="warn", max_index_age_secs=0
  (inert staleness). Manual `impl Default` — derive(Default) zeroes `enabled`
  when the whole [ledger] section is absent (TelemetryConfig/DashboardConfig
  trap class).
- **Enforcement point:** client proxy, 200 non-HEAD responses. Order:
  ledger check FIRST (may 503 / add headers), then pin stream-check wraps the
  surviving response — the pin still hashes the exact bytes the PM receives.
- **Policy split:** downgrade + index-parse-error: reject → 503 + tamper
  event; warn → pass + `X-Monopsony-Downgrade` / `X-Monopsony-Index-Parse-Error`
  header + event. Oversize index (>256 MB) is parse-error-class (both
  policies 503 — body already buffered, cannot re-serve; decompression bombs
  are separately capped inside parse_index).
- **Writers vs readers:** only parsed INDEX responses advance the high-water;
  payload URLs get a read-only cross-check (`high_water_for`) so a doctored
  payload URL cannot move the anchor.
- **pm_kind_from_url:** host/path sniffing (archlinux/.pkg.tar/.db → pacman,
  alpine/APKINDEX → apk); unknown → skip enforcement. No backend
  instantiation.
- **Events ride the existing Phase 3 tamper-event spool** (detail prefixes
  "downgrade:" / "index-parse-error:") — the daemon drain ships them with no
  new transport.

## THE bug of the session: re-entrant mutex deadlock in ledger

`check_and_update` (holding `conn.lock()`) called `self.active_exception()`
which re-locks the SAME std::sync::Mutex → every downgrade-path test hung
>60s while first-contact/equal tests passed. Fixed with a lock-free
`active_exception_on(&conn, ...)` free function; public method delegates.
**Same bug class as the MirrorManager RwLock self-deadlock (pairing bug D):
any method that takes the lock must never call a sibling method that also
takes it — write `_on(&conn)` variants instead.**

## Dispatch notes (for next time)

- Slice 2 subagent timed out at 600s AFTER landing its code (its test verify
  failed only because slice 3's file was mid-fix in parallel). Its background
  `cargo test` then hit the deadlock — the >60s "still running" test lines in
  the run output are the diagnostic signature.
- Slice 3's first verify failed on tar fixture API misuse
  (`append_data` needs `Read`; wrap `String`/`Vec<u8>` in `Cursor`) — fixed
  by the child.
- The patch tool duplicated a `#[serde(default)]` line and a doc comment
  during main.rs/config.rs edits (fuzzy-match artifact) — both caught and
  fixed immediately by diff review. ALWAYS read the diff, never trust success.

## Wave 2 (specs written, not started)

- slice 5: dnf primary.xml.gz + apt InRelease (Valid-Until native) parsers
- slice 6: `override` CLI subcommand (signed exception records, local
  Ed25519 key at `<state_dir>/ledger-override.key`)
- slice 7: e2e tests + fleet verification brief (which targets .20 pacman +
  .159 apk enforcement, .62 as inert no-ledger control)

## Fleet verification (not run)

Phase 4 is client-only; .58 needs no changes. Matrix: pre-seed a high-water
above current on .20 (`ledger` has no CLI yet — wave 2 adds one; interim:
delete rows via sqlite3), `pacman -Syu` must trip policy; flip policy
warn↔reject in `/etc/monopsony/client.toml` `[ledger]`; .62 stays green with
no `[ledger]` section (defaults enabled — see not-above caveat: enabled=true
DEFAULT means .62/.235/.242 will also start enforcing once redeployed; if
that is undesired for the bench, set `[ledger] enabled=false` in their
client.toml).
