# Slice 7 (wave 2) — tamper/downgrade e2e tests + fleet verification brief

## Read first (ONLY these)
1. docs/plans/sprint20-phase4/context-pack-common.md
2. crates/monopsony-client/src/proxy.rs — run_proxy /
   **run_proxy_with_listener** (hub-landed pre-wave), ClientProxyState +
   builders (~lines 76-130), wrap_with_ledger_check (~612-790),
   pm_kind_from_url (~570), dirs_events_path (~555), tests module at bottom
3. crates/monopsony-client/src/ledger.rs (public API — seed state through
   `check_and_update` / `repo_seen`, never raw SQL in the e2e file)
4. crates/monopsony-client/src/index_parse.rs — tests module fixture pattern
   (`pacman_tar_bytes`, `zstd_compress`, `gzip`) — copy those helpers into
   the test file (tests/ cannot see `#[cfg(test)]` code in src/)
5. docs/plans/sprint20-phase3/fleet-verification-brief.md (THE model for the
   brief deliverable)
6. crates/monopsony-client/tests/retry_tests.rs (integration-file style only)

## File set (strict)
- crates/monopsony-client/tests/ledger_e2e.rs (NEW — the only code file)
- docs/plans/sprint20-phase4/fleet-verification-brief.md (NEW prose)
- NOTHING in src/. If a public API you need is missing, report it, do not add it.

## Harness facts (hub-verified, trust these)
- Enforcement entry: `proxy::run_proxy_with_listener(listener, config, pin,
  ledger, ledger_policy, ledger_max_index_age_secs)` — hub-landed refactor of
  run_proxy; bind `TcpListener::bind("127.0.0.1:0")`, read
  `listener.local_addr()`, pass the listener in, drive HTTP against that addr.
  If it is missing when you compile (E0599), STOP and report not_done with the
  exact error — do NOT patch proxy.rs.
- State builder:
  `ClientProxyState::new(config).with_pin(pin).with_ledger(Arc<Ledger>,
  "reject".into(), 0)` is what run_proxy_with_listener assembles; you just
  pass the parts. `ProxyConfig` comes from the client proxy module's imports
  (grep the use-statements at the top of proxy.rs for its path; it is NOT
  monopsony_common::config::ProxyConfig — do not guess).
- Tamper events: `PinState::new(None, events_path)` — point events_path at
  `<tempdir>/tamper-events.jsonl` per test. Every test gets its OWN tempdir
  (std::mem::forget the TempDir like ledger.rs tests do, so the DB/outfiles
  outlive the handle).
- Ledger seeding: `ledger.check_and_update("pacman:bash", "5.2.38-1",
  &version_ord::normalize("5.2.38-1"), "seed", now)` — real API, no SQL.
- Upstream: ProxyConfig's upstream/server address field (grep ProxyConfig for
  `upstream`/`server`) → point at an in-test HTTP server. Simplest reliable
  upstream: `tokio::net::TcpListener` + a small hand-rolled HTTP/1.1 responder
  task (read one request head off the socket, write a canned `HTTP/1.1 200
  OK\r\nContent-Length: N\r\n\r\n<bytes>`, flush, continue loop). Swap the
  canned bytes via an `Arc<std::sync::Mutex<Vec<u8>>>` between fetches.
- Index URL must classify: use path `/repo/os/x86_64/core.db` on any host —
  pm_kind_from_url matches `.db` suffix → pacman.
- Proxy request: the client proxy expects ABSOLUTE-URI request lines
  (`GET http://host/repo/os/x86_64/core.db HTTP/1.1`). Use
  `reqwest::Proxy::http(proxy_addr)` (reqwest is a client-crate dep) or
  hand-roll the absolute-URI request over TcpStream — reqwest::Proxy is
  less code; `.get(format!("http://upstream-host/repo/os/x86_64/core.db"))`
  through the proxy.

## Tests (all #[tokio::test]; own tempdir + own upstream per test)
1. `index_downgrade_reject_503s`: upstream serves zstd pacman .db (bash
   5.2.37-1 + a second package); policy "reject". FIRST fetch → 200
   (high-water recorded). Swap upstream bytes to bash 5.2.36-1 (lower);
   SECOND fetch → 503, body contains "rewound", events file gained a line
   containing `downgrade:`.
2. `index_downgrade_warn_headers`: policy "warn", same two-step; second fetch
   → 200 with `X-Monopsony-Downgrade` header present; ledger still holds the
   higher version (`high_water_for("pacman:bash")` == "5.2.37-1").
3. `garbage_index_reject`: policy "reject"; 64 garbage bytes at a `.db` URL →
   503; events contain `index-parse-error:`.
4. `disabled_ledger_inert`: NO `.with_ledger` (ledger None); garbage .db →
   200, no events file created, no X-Monopsony-* response headers.
5. `stale_index_header_only`: policy "warn", max_index_age_secs = 1; BEFORE
   the first fetch, seed
   `ledger.repo_seen(&repo_key, now - 3600)` where repo_key =
   `index_parse::repo_key_from_url(&pin_url_of_index)` — compute it the same
   way the proxy does (canonical https:// URL of the index you request →
   repo_key_from_url). Fetch a fresh valid index → 200 +
   `X-Monopsony-Index-Stale` header.
6. OPTIONAL (not_done allowed): payload cross-check — with high-water
   5.2.38-1 seeded, fetch
   `/repo/os/x86_64/bash-5.2.36-1-x86_64.pkg.tar.zst` under warn policy →
   200 + `X-Monopsony-Downgrade` header (read-only check, no body buffering).

## Fleet verification brief (fleet-verification-brief.md)
Follow the phase-3 brief's structure exactly (deploy steps → verification
matrix → failure table → hard rails → reporting contract). Wave-2 content:
- Deploy: .58 server UNTOUCHED (phase 4 is client-side). Clients: .20 pacman
  (primary, gnu target), .159 apk (primary, musl), .62 apt (inert control,
  ledger disabled in config, gnu). Give exact cross/scp commands and sha256
  record slots, matching the phase-3 brief's format.
- Matrix (use the override/ledger CLI, NEVER doctored upstream indexes):
  - M1 (.20): seed a high-water ABOVE current repo state (exact CLI/seed
    command in the brief), then `pacman -Sy` through the local proxy →
    policy trips: reject → pacman 503 failure visible; warn → header +
    pass-through (verify via curl -v through the proxy).
  - M2 (.20): flip `[ledger] policy` warn→reject in /etc/monopsony/client.toml
    (backup first), restart client, repeat M1, behavior must change.
  - M3 (.62): ledger disabled → `apt update` succeeds normally, no
    tamper-events.jsonl growth — the inert path proof.
  - M4 (.20): `override add` with a real reason + short TTL (120s) →
    previously-503ing fetch passes; after expiry it trips again;
    `override list` shows the record and then drops it.
- Failure table: symptom → bounded action (client restart recipe, log paths,
  config restore). Two failures or any auth anomaly → STOP and report.
- Hard rails: no commits (owner signs), `cp -a` config backups before edits,
  never weaken auth to make a test pass, restore https→http repo rewrites
  after testing per the standard fleet procedure.
- Reporting contract: verbatim outputs per matrix item, binary sha256s, an
  explicit unverified list.

## Do NOT
- Do not touch ANY src/ file. Do not hit the real fleet from this slice —
  the brief is a spec for the bench session.
- No git add/commit.

## Verify (ONCE, from /home/martin/projects/monopsony)
`cargo test -p monopsony-client --test ledger_e2e -- --nocapture`

## Reporting
done / not_done / deviations + verbatim verify tail + which matrix items the
brief covers. Floor: tests 1-4 green + the brief file. Tests 5-6 may go
not_done — never half-edit.
