# Sprint 20 Phase 4 — context pack (common)

## Mission
Client monotonic ledger + downgrade enforcement + index parsing/staleness
(plan §6 of docs/plans/2026-09-13-https-transport-and-package-integrity.md).
The client proxy already intercepts every index and payload fetch (absolute-URI
path + CONNECT tunnel). Phase 3 pin verification (wrap_with_pin_check) landed;
Phase 4 adds the L3/L4 ladder rungs: a per-package high-water ledger fed by
parsed index responses, downgrade policy enforcement, and payload-vs-index
hash cross-checks.

## Wave 1 slices (this wave)
1. version_ord.rs (monopsony-common) — normalizer + vercmp
2. ledger.rs (monopsony-client) — SQLite high-water store
3. index_parse.rs (monopsony-client) — pacman .db + APKINDEX parsers
4. proxy/main wiring — wrap_with_ledger_check + [ledger] config + policy

## Wave 2 (specs are stubs; expand before dispatch)
5. dnf primary.xml.gz + apt InRelease parsers + Valid-Until staleness
6. override command (signed exception records with TTL)
7. tamper/downgrade e2e tests + fleet verification

## Hard rules
- HARD ~600s cap: work linearly, write files immediately, verify ONCE at the end.
- You may read ONLY the files named in your slice spec (plus the target file).
- Do NOT commit. Do NOT touch files outside your slice's file set.
- The standalone linter reports "async fn not permitted in Rust 2015" — FALSE
  POSITIVE (edition 2021). Ignore lint noise; `cargo check -p <crate>` is the verdict.
- If time runs short, SKIP remaining items and report under not_done. Never half-edit.

## Contracts already landed by the hub (DO NOT re-create; consume exactly)
- crates/monopsony-common/src/pin_store.rs: PinAttestation / PinOrigin / sign-verify
  (Phase 3). Client PinState: crates/monopsony-client/src/pin_verify.rs —
  PinState::new/set_key/verify_attestation_json/record_event/drain_events,
  pin_check_stream, TamperEvent{url, expected_sha256, actual_sha256, detail, observed_at}.
- Client proxy interception: crates/monopsony-client/src/proxy.rs
  wrap_with_pin_check(state, &uri, response) is applied to 200 non-HEAD responses.
  ClientProxyState{config, pin: Arc<PinState>} with .with_pin builder.
- crates/monopsony-common/src/config.rs: `LedgerConfig { enabled: bool,
  policy: String ("warn"|"reject"), max_index_age_secs: u64 }` — hub-landed with
  MANUAL impl Default (enabled=true, policy="warn", max=0=inert) per the
  derive(Default)-bypasses-serde-defaults trap. Field on ClientConfig:
  `#[serde(default)] pub ledger: LedgerConfig`.
- Workspace + client Cargo.toml: `zstd` dep added (pacman .db is zstd).
  flate2 + tar already in client crate (APKINDEX.tar.gz).
- rusqlite 0.31 (bundled) is a workspace dep; add `rusqlite = { workspace = true }`
  to monopsony-client's Cargo.toml when your slice needs it.

## Phase 4 semantics (do not deviate)
- **Ledger home:** `<state_dir>/ledger.sqlite3`
  (`$HOME/.local/state/monopsony/ledger.sqlite3`, same convention as the tamper
  event spool). Deliberately OUTSIDE cert_dir: re-pairing must not wipe the
  anti-rewind anchor (plan open-question 5 rationale).
- **Identity keys:** `package_key = "<pm>:<name>"` (e.g. "pacman:bash");
  `repo_key` = directory portion of the canonicalized index URL
  (`normalize_url_scheme`, https:// — same canonicalization as cache_entries).
- **Downgrade detection:** on a parsed index response, compare every entry
  against the stored high-water with vercmp. Less → downgrade. Record new
  high-water AFTER the check (monotonic max; first contact just records).
- **Policy application point:** the INDEX response. policy="reject" → 503 the
  index + tamper event; policy="warn" → pass + `X-Monopsony-Downgrade` header +
  log. **Parse/decode failure follows the same policy split** — reject=503
  (fail-closed; garbage indexes are a bypass attempt), warn=pass+event.
- **Payload cross-check:** for payload URLs parseable to (name, version), if a
  pin attestation exists AND the index declared a sha256 for that
  (repo_key, name, version), they must match — mismatch = tamper event (do not
  enforce a separate reject here; the pin gate already truncates on hash
  mismatch of served bytes).
- **Fail-open vs fail-closed rule:** availability features (DB-down quarantine,
  missing pin) fail open; INTEGRITY policy violations under reject fail closed.
  Never invert this.
- **Staleness v1 (wave 1):** ledger records last_seen per repo_key; warn-only
  header when now - last_seen exceeds max_index_age_secs (0 = disabled). apt
  Valid-Until native enforcement is wave 2.
- A zeroed/absent pin key does NOT disable the ledger — they are independent
  rungs (L2 vs L3). The ledger runs whenever [ledger] enabled=true.

## Verify (run ONCE, from /home/martin/projects/monopsony)
`cargo check -p <your crate>` then your slice's targeted tests. Full-suite runs
are the hub's job. PROTOC=/usr/bin/protoc for anything touching proto (none
should in wave 1).

## Reporting
Report: done / not_done / deviations. Verbatim final `cargo check` tail. Do not
claim green without the check output in your transcript.
