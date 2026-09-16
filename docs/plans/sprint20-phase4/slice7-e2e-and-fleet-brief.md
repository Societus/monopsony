# Slice 7 (wave 2) — tamper/downgrade e2e tests + fleet verification brief

## Read first (ONLY these)
1. docs/plans/sprint20-phase4/context-pack-common.md
2. crates/monopsony-client/src/proxy.rs tests module (harness style —
   router-over-loopback + injected upstream pattern from streaming proxy work)
3. crates/monopsony-client/src/ledger.rs + index_parse.rs (real APIs)

## File set
- crates/monopsony-client/tests/ledger_e2e.rs (NEW integration test file)

## Tests to add
1. `index_downgrade_reject_503s`: local upstream serves a pacman .db (fixture
   from slice 3 helpers) with bash 5.2.38; proxy with ledger reject policy;
   first fetch → 200 (records high-water). Second fetch serves bash 5.2.37 →
   503, body contains "rewound", tamper-event JSONL gained a "downgrade:" line.
2. `index_downgrade_warn_headers`: same fixture, warn policy → 200 +
   X-Monopsony-Downgrade header present; ledger row still holds 5.2.38.
3. `garbage_index_reject`: reject policy + unparseable .db bytes → 503 +
   "index-parse-error:" event.
4. `disabled_ledger_inert`: ledger=None → all fetches pass, no events, no
   headers (proves the default-deploy inert path).
5. `stale_index_header_only`: max_index_age_secs=1 + inject repo_seen(now-3600)
   via direct Ledger API → 200 + X-Monopsony-Index-Stale header.
6. If time remains: payload cross-check test (pacman-style payload URL basename
   parsing) — else report not_done.

## Fleet verification brief (separate deliverable, this dir)
Write `fleet-verification-brief.md` modeled on sprint20-phase3's: deploy steps
(.58 server untouched by phase 4 — this is client-side; clients .20 pacman +
.159 apk primary, .62 inert no-ledger control), matrix items (downgrade
injection via a doctored upstream index? NO — use ledger CLI: pre-seed a
high-water above current on .20, then `pacman -Syu` must trip policy; warn vs
reject config flip; inert proof on .62), restore steps, failure table, hard
rails (no commits; config backups), reporting contract.

## Do NOT
- Do not touch production code (test helpers only).
- Do not hit the real fleet from this slice — the brief is a spec for the bench.

## Verify (ONCE)
`cargo test -p monopsony-client --test ledger_e2e -- --nocapture`
