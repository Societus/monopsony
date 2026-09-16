# Slice 4 (wave 1) — client proxy + daemon wiring: ledger enforcement

## Read first (ONLY these)
1. docs/plans/sprint20-phase4/context-pack-common.md
2. crates/monopsony-client/src/proxy.rs — ClientProxyState, wrap_with_pin_check,
   pin_url, dirs_events_path, the proxy start fn (~line 620-640), run-loop 200
   handling (~line 395-410)
3. crates/monopsony-client/src/ledger.rs + index_parse.rs (wave-1 siblings —
   consume their real APIs; they land in parallel)
4. crates/monopsony-client/src/main.rs — daemon startup region (~line 620-700),
   where PinState is constructed/loaded and the proxy is built

## File set
- crates/monopsony-client/src/proxy.rs (ClientProxyState + wrap fn + start fn)
- crates/monopsony-client/src/main.rs (Ledger construction + handoff)
- crates/monopsony-common/src/config.rs — ONLY IF the hub hasn't landed
  LedgerConfig yet; if landed (grep `LedgerConfig` first), do NOT touch it

## Design
### A. LedgerConfig (hub contract — skip if already landed)
In config.rs beside IntegrityConfig:
```rust
pub struct LedgerConfig { pub enabled: bool, pub policy: String, pub max_index_age_secs: u64 }
```
`#[serde(default = "default_ledger_config")]` on the ClientConfig field +
MANUAL impl Default { enabled: true, policy: "warn", max_index_age_secs: 0 }
(NOT derive(Default) — the derive-bypasses-serde-defaults trap zeroes `enabled`
when the whole [ledger] section is absent). Validate policy ∈ {warn, reject} in
the existing config-validation fn if one exists for client config.

### B. ClientProxyState extension
```rust
pub struct ClientProxyState {
    config: ProxyConfig,
    pub pin: Arc<PinState>,
    pub ledger: Option<Arc<crate::ledger::Ledger>>,   // None = disabled
}
```
`.with_ledger(Arc<Ledger>)` builder (mirror .with_pin style).

### C. Index enforcement path (wrap_with_ledger_check)
Applied to 200 non-HEAD responses AFTER wrap_with_pin_check ordering decision:
- `looks_like_index(uri.path())` false → pass through (payloads cross-check in D).
- True → buffer the body (cap 256 MB → 503 + tamper event beyond), spawn_blocking
  parse_index. Ledger unavailable (None) or enabled=false → pass through
  untouched (feature-inert default path stays available).
- Parse error → policy split: "reject" → 503 + record_event; "warn" → pass +
  record_event + `X-Monopsony-Index-Parse-Error: 1` header.
- Parsed → repo_seen(repo_key, now); per entry: normalize(version) then
  check_and_update(package_key_for(pm, name), ...) in spawn_blocking.
  vercmp must run OUTSIDE rusqlite (plain fn).
- Downgrade verdicts: "reject" → 503 body "package index rewound - held by
  client ledger" + record_event per entry; "warn" → pass with
  `X-Monopsony-Downgrade: <name>=<offered>;<held>` (cap header at 8 entries)
  + record_event.
- Staleness: after repo_seen, if max_index_age_secs > 0 and repo_age_secs >
  max → `X-Monopsony-Index-Stale: <secs>` header (warn-only v1).
- Record events via state.pin.record_event (same TamperEvent spool — detail
  prefix "downgrade:" / "index-parse-error:") so the existing daemon drain
  ships them with no new transport.
- PM kind for package_key: derive from the URL (archlinux → pacman, alpine →
  apk) via a small `pm_kind_from_host(path)` helper INSIDE proxy.rs — do not
  instantiate full backends. Unknown → skip enforcement (pass through).

### D. Payload cross-check (cheap v1)
For non-index 200s: if the URL basename parses as `<name>-<version>-<rel>-<arch>...
.pkg.tar.*` (pacman) extract (name, version); query
ledger.check_and_update WITHOUT writing when verdict is Ok (add a
`check_only: bool` param on check_and_update — coordinate via the shared pack,
not by editing slice 2's file: if landed without it, just skip the write path
and use a read-only `high_water_for()` getter instead — ADD the getter only if
missing; one small additive fn is allowed).
Offered < held → same policy split as C (this catches direct payload rewinds
the index path misses).
Apk payload names don't carry versions cleanly — skip (documented v1 limit).

### E. main.rs wiring
- Daemon startup: after PinState load — if cfg.ledger.enabled: open
  `<state_dir>/ledger.sqlite3` (dirs_events_path().parent() convention),
  Arc it, `.with_ledger(...)`. Log line: `"Integrity ledger: enabled (policy={})"`.
- Open failure → log error + continue WITHOUT ledger (fail-open availability;
  tamper events still work).
- Proxy start fn signature: add ledger param alongside pin (update the one
  call site).

## Do NOT
- Do not change server behavior, proto, or PM backends.
- Do not implement dnf/apt parsing (wave 2).
- Do not make the ledger block CONNECT tunnels (opaque by design).

## Verify (ONCE)
`cargo check -p monopsony-client && cargo test -p monopsony-client -- --nocapture`
(report full test tail; slices 1-3 + wave-1 pin tests must stay green)
