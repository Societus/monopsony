# Slice 5 (wave 2) — Server: quarantine enforcement on cache hits
# (Pairing key delivery ALREADY LANDED by the hub in pairing.rs — do not touch it.)

## Read first (ONLY these)
1. docs/plans/sprint20-phase3/context-pack-common.md
2. crates/monopsony-proxy/src/proxy.rs — serve_cache_hit region + the wave-1 pin helpers

## File set
- crates/monopsony-proxy/src/proxy.rs (wave-1 slice-1 edits are landed; do not undo them)

## Design
### Quarantine gate in serve_cache_hit
Before streaming a HIT's bytes: if `state.db` is Some and
`db.has_tamper_alert(&normalize_url_scheme(url)).await` returns Ok(true) → return 403
FORBIDDEN (reuse the scan-policy Block response shape), header `X-Monopsony-Quarantine: 1`,
NO body stream. Fail-open on DB error (log warn, serve) — proxy availability must not hinge
on the alerts table. Place the check BEFORE the pin-header attachment (quarantined objects
get 403, not headers). Structure as `async fn quarantined(state, url) -> bool` returning
false when db is None; unit-test THAT (db=None → false). The DB-true path is verified live
on the bench, not in CI. slice-2's `has_tamper_alert` is landed in monopsony-db — consume it.

## Do NOT
- Do not touch crates/monopsony-admin/src/pairing.rs (hub-landed, done).
- Do not touch client files (slice 6 runs in parallel and owns them).

## Verify (ONCE)
`cargo check -p monopsony-proxy && cargo test -p monopsony-proxy quarantin -- --nocapture`
