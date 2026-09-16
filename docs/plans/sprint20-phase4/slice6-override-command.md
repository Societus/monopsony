# Slice 6 (wave 2) — override command (signed downgrade exceptions)

## Read first (ONLY these)
1. docs/plans/sprint20-phase4/context-pack-common.md
2. crates/monopsony-client/src/ledger.rs (exceptions table API from slice 2)
3. crates/monopsony-client/src/main.rs — CLI subcommand enum + run_pair region
   (style reference for a new subcommand)

## File set
- crates/monopsony-client/src/main.rs (new `Override` subcommand)
- crates/monopsony-client/src/ledger.rs (only additive: exception signing if
  slice 2 didn't land it — coordinate via this spec)

## Design
```
monopsony-client override --package pacman:bash --to 5.2.37-1 \
  --reason "downgrading for kernel compat" [--ttl-secs 604800]
```
- Validates package_key shape (`<pm>:<name>`), non-empty reason.
- Writes an exception record: created_at, expires_at = now + ttl (default 7d).
- **Signed:** Ed25519 over canonical record JSON, key = dedicated
  `<state_dir>/ledger-override.key` (load-or-generate, chmod 0600 — NOT the
  pin key, NOT any server key; this is the operator's local authority).
  Store signature + public key hex in the exceptions row (additive columns).
- check_and_update already consults active_exception (slice 2); extend the
  match to verify the signature when present (unsigned rows from before =
  still honored, log deprecation warn — migration path).
- `monopsony-client override --list` prints active exceptions with expiry.
- Footgun UX: the command prints what will be allowed and requires the reason
  to be non-empty; no interactive confirm needed (deliberate flag is the gate).

## Verify (ONCE)
`cargo test -p monopsony-client ledger -- --nocapture && cargo run -p monopsony-client -- override --help`
