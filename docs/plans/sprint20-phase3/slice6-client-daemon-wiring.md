# Slice 6 (wave 2) — Client daemon: key provisioning + tamper alert drain

## Read first (ONLY these)
1. docs/plans/sprint20-phase3/context-pack-common.md
2. crates/monopsony-client/src/pin_verify.rs (landed in wave 1 — consume its real API)
3. crates/monopsony-client/src/grpc_client.rs (whole file)
4. crates/monopsony-client/src/main.rs — regions: run_pair, daemon command start, where
   ClientProxyState is constructed, where the daemon heartbeat loop runs.

## File set
- crates/monopsony-client/src/grpc_client.rs
- crates/monopsony-client/src/main.rs

## Design
### A. Key capture
1. In grpc_client.rs `pair()`: PairResponse now carries `mirror_verify_key` (bytes). Return
   it — extend the success tuple or a small struct (match the file's style; keep the change
   minimal and mechanical at the 1-2 call sites in main.rs).
2. In the Register RPC path (daemon register): RegisterResponse.mirror_verify_key likewise.
3. Persist: write the 32 raw bytes to `<cert_dir>/mirror-verify.key` (cert_dir is available
   in main.rs scope where pairing/registration completes). On daemon startup, if the file
   exists, load it and `PinState::set_key(...)` BEFORE the proxy starts serving.
4. Zeroed/empty bytes → do not write the file, leave inert.
### B. Event drain (heartbeat loop)
The landed API (wave 1, in crates/monopsony-client/src/pin_verify.rs — CONSUME AS-IS):
- `PinState::new(Option<[u8; 32]>, PathBuf)` — path is the JSONL spool file
- `PinState::set_key([u8; 32])` — provision/rotate after pairing (interior-mutable)
- `PinState::drain_events() -> Vec<TamperEvent>` — no arg; truncates the file
- `encode_tamper_alert(&TamperEvent) -> proto TamperAlert` (free fn)
- `ClientProxyState::with_pin(Arc<PinState>)` builder (client proxy.rs)
- Default events path (proxy.rs `dirs_events_path`): `$HOME/.local/state/monopsony/tamper-events.jsonl`

In the daemon heartbeat loop: after a successful heartbeat, `drain_events()`; for each
event, send a `ClientMessage { tamper_alert: encode_tamper_alert(e) }` over the existing
ControlStream (find how inventory/heartbeat messages are sent — reuse that exact send
path). One drain per beat. On send failure: re-append the events to the spool file
(`record_event` per event) so alarms are not lost.
### C. Wiring
- main.rs: construct the shared `Arc<PinState>` (events path under the client state dir —
  prefer cert_dir's parent or the existing state dir convention over the HOME default);
  pass it to the proxy builder via `with_pin(...)` AND hold a clone for the daemon loop.
- Key provisioning: daemon startup — if `<cert_dir>/mirror-verify.key` exists, read 32 raw
  bytes and `set_key(...)` BEFORE the proxy starts serving. On pair/register success,
  write the received `mirror_verify_key` bytes to that file (skip when empty/zeroed).

## Do NOT
- Do not change server behavior, proto, or config structs.
- Do not implement pin policies beyond what wave-1 landed.

## Verify (ONCE)
`cargo check -p monopsony-client && cargo test -p monopsony-client -- --nocapture`
(report full test tail; wave-1 pin_verify tests must still pass)
