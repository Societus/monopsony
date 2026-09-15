# Slice 3 — Client: verify-before-handoff + tamper event spool

## Read first (ONLY these)
1. docs/plans/sprint20-phase3/context-pack-common.md
2. crates/monopsony-client/src/proxy.rs (whole file — it is 676 lines)
3. crates/monopsony-client/src/lib.rs (module list)

## File set (touch NOTHING else)
- crates/monopsony-client/src/proxy.rs
- crates/monopsony-client/src/pin_verify.rs (NEW)

## Design to implement
### A. New module pin_verify.rs
```rust
pub struct PinState { /* your fields */ }
impl PinState {
    /// key_bytes: 32 raw Ed25519 bytes from PairResponse/RegisterResponse;
    /// empty/zeroed slice = pinning not provisioned (all checks inert-pass).
    pub fn new(verify_key: Option<[u8; 32]>) -> Self
    /// Returns Ok(verify_outcome_string) or Err(mismatch_details).
    /// policy "flag": mismatch is returned as Ok with a "tamper" outcome? NO —
    /// see B: mismatches ALWAYS produce Err(PinError) and the caller decides
    /// via policy whether to truncate.
    pub fn verify_attestation_json(&self, pin_json: &str, url: &str) -> Result<(), String>
    pub fn provisioned(&self) -> bool
}
```
Verify = parse monopsony_common::monopsony_common::pin_store::from_json + `.verify(&vk)` + url equality.
monopsony-common is already a dep of this crate (line 12 of Cargo.toml). Hash computation lives in the stream wrapper (B).
### B. Hash-checking body wrapper in client proxy.rs
New pub struct `PinCheckBody` wrapping the streamed server response body, implementing
`http_body::Body` (the crate is available transitively — check what proxy.rs already
imports; hyper 1.x re-exports via `hyper::body`; if you need the direct dep it is NOT in
Cargo.toml — then implement the wrapper as an async stream adapter instead using
`futures_core::Stream<Item=Result<Bytes, io::Error>>` and `axum::body::Body::from_stream`,
which the crate already uses elsewhere). Behavior per chunk:
1. Feed sha256 (sha2 is already a dep of this crate — verify).
2. On end-of-stream: compare final hex against pin.expected_sha256 from the
   X-Monopsony-Pin header (captured before the body is consumed).
   - Match → append `X-Monopsony-Verified: pin-ok` to the response headers ALREADY SENT?
     Headers are sent before the body — so instead attach a TRAILER? axum Body::from_stream
     does not support trailers cleanly. DECISION: verification outcome is NOT a header.
     It goes in the ProxyState-shared event spool (C) + a warn!/info! log + the returned
     response is UNCHANGED on success. Add the `X-Monopsony-Verified` header ONLY in the
     header phase when the pin is ABSENT or origin=unverified (i.e. the decided outcome is
     known before streaming: `pin-missing` / `pin-unverified` / `inert`). The pin-ok/pin-fail
     outcome arrives after streaming and is logged + spooled only.
   - Mismatch → spool a TamperEvent (C), error! log with expected/actual hex, and TRUNCATE:
     return an error through the body stream (axum converts stream errors to an aborted
     body — the PM sees a failed download, which is the reject policy). Also, if the PM
     would retry transparently that is fine; the cache remains poisoned server-side until
     quarantine lands.
3. Response flows through the EXISTING retry loop untouched — wrap at the final return in
   proxy_handler's `Ok(Ok(mut response))` arm (and only there; CONNECT path unaffected).
### C. Event spool + drain
`TamperEvent` serde struct in pin_verify.rs {url, expected_sha256, actual_sha256, detail,
observed_at}. Spool: append-only JSONL at `<state_dir>/tamper-events.jsonl` (state_dir comes
from where cert_dir/state paths resolve in main — for the lib side accept a PathBuf in
PinState::new; main.rs wiring is slice 4). Drain: in the client daemon's heartbeat loop
(find it in grpc_client.rs — READING grpc_client.rs is allowed for reference but EDITS there
are NOT yours; expose `PinState::drain_events(path) -> Vec<TamperEvent>` and a free fn
`encode_tamper_alert(e) -> monopsony_common::proto::monopsony::TamperAlert`, and the wiring
call site goes in slice 4's brief), constructing the proto TamperAlert via
`monopsony_common::proto::monopsony::TamperAlert` — generated type name follows the proto.
If the generated type is not yet in scope after hub rebuild, report it; the hub regenerates.
### D. ClientProxyState
Add `pub pin: Arc<PinState>` (or RwLock-wrapped if the daemon provisions the key AFTER state
construction — check how the daemon builds ClientProxyState in main.rs... main.rs reading is
slice-4 territory; to stay safe make PinState interior-mutable:
`pub struct PinState { inner: std::sync::RwLock<Option<VerifyingKey>>, events_path: PathBuf }`
with `pub fn set_key(&self, bytes: [u8; 32])`). Default: no key (inert).

## Tests (in pin_verify.rs #[cfg(test)])
- verify_ok_roundtrip (sign with test key, verify ok)
- verify_rejects_wrong_key / rejects_tampered_url (as in pin_store tests)
- inert_when_unprovisioned (zeroed key → provisioned()==false, verify is Ok)
- event spool roundtrip (write 2 events to tempdir JSONL, drain returns both and leaves
  file empty/removed — pick one semantic: drain truncates)

## Do NOT
- Do not touch grpc_client.rs, main.rs, config structs, or server files.
- Do not add Cargo dependencies (sha2, prost, serde, monopsony-proxy must already be there;
  verify with a grep before starting and STOP+report if any is missing).

## Verify (ONCE, from /home/societus/projects/monopsony)
`cargo check -p monopsony-client && cargo test -p monopsony-client pin_verify -- --nocapture`
