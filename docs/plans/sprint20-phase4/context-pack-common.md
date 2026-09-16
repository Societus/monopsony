# Sprint 20 Phase 4 — context pack (common)

## Mission
Client monotonic ledger + downgrade enforcement + index parsing/staleness
(plan §6 of docs/plans/2026-09-13-https-transport-and-package-integrity.md).
The client proxy already intercepts every index and payload fetch (absolute-URI
path + CONNECT tunnel). Phase 3 pin verification (wrap_with_pin_check) landed;
Phase 4 adds the L3/L4 ladder rungs: a per-package high-water ledger fed by
parsed index responses, downgrade policy enforcement, and payload-vs-index
hash cross-checks.

## Wave 1 (LANDED — commit 4912271, tree clean)
1. version_ord.rs (monopsony-common) — normalizer + vercmp ✅
2. ledger.rs (monopsony-client) — SQLite high-water store ✅
3. index_parse.rs (monopsony-client) — pacman .db + APKINDEX parsers ✅
4. proxy/main wiring — wrap_with_ledger_check + [ledger] config + policy ✅

## Wave 2 (CURRENT — specs below are EXPANDED, follow them exactly)
5. slice5-dnf-apt-parsers.md — dnf primary.xml.gz + apt InRelease/Release +
   Valid-Until native + pm_kind_from_url extension (owns proxy.rs this wave)
6. slice6-override-command.md — `override add/list` CLI, signed exception
   records, migration for signature columns (owns ledger.rs + main.rs)
7. slice7-e2e-and-fleet-brief.md — tests/ledger_e2e.rs + fleet brief
   (owns tests/ + this dir's brief; NOTHING in src/)

File sets are DISJOINT. Parallel dispatch is safe under the foreign-failure
rule below.

## Hard rules
- HARD ~600s cap: work linearly, write files immediately, verify ONCE at the end.
- You may read ONLY the files named in your slice spec (plus the target file).
- Do NOT commit. Do NOT touch files outside your slice's file set.
- The standalone linter reports "async fn not permitted in Rust 2015" — FALSE
  POSITIVE (edition 2021). Ignore lint noise; `cargo check -p <crate>` is the verdict.
- If time runs short, SKIP remaining items and report under not_done. Never half-edit.
- Same-crate parallel wave: all three slices compile inside monopsony-client,
  so YOUR verify may hit a sibling's mid-edit file (their E0277 is not your
  bug). Filter the error's file path before diagnosing; mark out-of-scope
  failures FOREIGN; re-run your verify ONCE after ~60s (sibling settles) and
  if it still fails on THEIR file, report it and stop — never touch a file
  outside your file set.

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
- rusqlite (bundled) is a workspace dep and is ALREADY in
  monopsony-client's Cargo.toml — do not add it again.

## Landed wave-1 contracts (hub-verified 2026-09-15 against commit 4912271 — TRUST, then grep if unsure)
- ledger.rs: tables high_water / repo_seen / exceptions (5-col, NO signature
  columns yet — slice 6 migrates); `check_and_update(key, version,
  &NormalizedVersion, source, now) -> CheckVerdict::{Ok, Downgrade{held}}`;
  `high_water_for(key)` read-only; `repo_seen(key, now)` / `repo_age_secs(key)`;
  `add_exception(...)` (5-arg, unsigned); `active_exception_on(&conn, ...)`
  is THE lock-free exception path — extend verification THERE, never in
  callers.
- index_parse.rs: `parse_index` decodes by MAGIC (zstd 28B52FFD / gzip 1F8B /
  raw) then sniffs tar contents; `IndexEntry{name, version, sha256:Option}`;
  `ParsedIndex::{Pacman, Apk}{entries}`; caps 256MB/100k entries/1MB desc;
  `looks_like_index(path)` (.db/.db.tar.gz/APKINDEX*); `repo_key_from_url`
  (dir portion, https-canonical); `package_key_for(pm, name)`.
- proxy.rs wiring: `wrap_with_ledger_check` runs BEFORE `wrap_with_pin_check`
  in proxy_handler's 200 path. Index = buffered → spawn_blocking parse →
  per-entry `check_and_update`; `IndexOutcome::{Clean, Oversize,
  ParseError, Downgrades}` → `ledger_index_verdict_response` applies policy.
  Headers: `X-Monopsony-Downgrade`, `X-Monopsony-Index-Parse-Error`,
  `X-Monopsony-Index-Stale` (age secs). Reject bodies say "rewound"
  (downgrade) / "failed parsing" (parse error). Tamper events share the
  Phase 3 spool with `downgrade:` / `index-parse-error:` detail prefixes.
  `pm_kind_from_url` (host/path sniff) → "pacman" | "apk" | None — slice 5
  extends this.
- main.rs daemon: ledger opens at `dirs_ledger_parent(&cert_dir).join(
  "ledger.sqlite3")` when `cfg.ledger.enabled`; `dirs_ledger_parent` =
  `$HOME/.local/state/monopsony` (fallback /tmp/monopsony). Open failure =
  fail-open (daemon runs ledger-less).
- proxy.rs (post-hub-refactor): `run_proxy(addr, ...)` binds then delegates
  to `pub run_proxy_with_listener(listener, config, pin, ledger, policy,
  max_age_secs)` — slice 7's port-discovery entry point.
- ClientProxyState fields are PRIVATE (config) + pub (pin, ledger,
  ledger_policy, ledger_max_index_age_secs); builders `.with_pin(arc)` /
  `.with_ledger(arc, policy, max)`. Events: `PinState::new(None, events_path)`;
  default events path = dirs_events_path() (same state dir,
  tamper-events.jsonl).
- HUB FIX during wave 2 (caught by slice 7's e2e): client `pin_url()` was
  authority-only for http URIs — it now mirrors the server's
  `normalize_url_scheme` exactly (prefix swap, path preserved). Repo keys and
  pin URLs for http mirrors were previously mis-keyed (pin verification
  silently disengaged). Canonical URL = `https://<host>:<port>/<full path>`.
- Deps present in client crate: zstd, flate2, tar, rusqlite (bundled),
  ed25519-dalek v2 (serde, rand_core), rand 0.8, base64, hex, sha2, chrono.
  quick-xml is NOT a workspace dep and must stay out.

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
