# Slice 4 — Server main: key custody + proto delivery + gRPC tamper handling

## Read first (ONLY these)
1. docs/plans/sprint20-phase3/context-pack-common.md
2. crates/monopsony-server/src/main.rs — regions ONLY: the mirror/shadow manager init block
   (~line 505-545, where `signing.key` is loaded), the `[proxy]` config-read block
   (~line 439-476), the run_proxy call (~line 473-490), the AdminState literal (~line 786),
   the grpc::start call (~line 582-591), and the imports at the top.
3. crates/monopsony-control/src/grpc.rs — regions ONLY: `struct MonopsonyGrpc` (~line 32-70),
   `handle_register` tail where RegisterResponse is built (~line 105-120), the
   `dispatch_client_message` match (~line 301-345), and the two `MonopsonyGrpc::new` call
   sites (~line 399, ~line 895) plus `pub async fn start` signature (~line 886).

## File set (touch NOTHING else)
- crates/monopsony-server/src/main.rs
- crates/monopsony-control/src/grpc.rs

## Design to implement
### A. Pin key custody + config load (main.rs)
1. Load the pin signing key ONCE, next to the shadow key init:
   `let pin_key_path = shadow_dir.join("pin-signing.key");` →
   `monopsony_shadow::shadow_signing::load_or_generate_signing_key(Some(&pin_key_path))`.
   Keep it out of the shadow manager — it is owned by main and cloned where needed.
2. Read `[integrity]` from args.config the same way `[proxy]` is read (small wrapper struct +
   toml::from_str, `#[serde(default)]`), then pass
   `(Some(pin_key.clone()), integrity_cfg)` into the run_proxy call REPLACING the hub's
   `None + default` placeholders.
3. Gate generation on pinning being enabled: if `!integrity_cfg.pinning_enabled`, still
   LOAD-if-exists (verification of old pins must work) but DO NOT generate a new key — pass
   `None` as the signing key. Log the decision line:
   `info!("Integrity pinning: enabled/disabled (policy={})", ...)`.
### B. AdminState field (main.rs, one line in the literal)
`mirror_verify_key: *pin_key.verifying_key().as_bytes()` — when key exists; when disabled
pass `[0u8; 32]` (pack rule: zeroed = not provisioned). Clone the SigningKey BEFORE the
run_proxy move block if ownership collides (SigningKey is Clone).
### C. Pairing delivery (admin/pairing.rs is NOT yours — the hub already added the field to
PairResponse in proto and to AdminState; pairing.rs read+edit is HUB-owned and happens after
your slice; skip it)
### D. gRPC (grpc.rs)
1. `MonopsonyGrpc` gets `mirror_verify_key: [u8; 32]` field + builder
   `.with_mirror_verify_key([u8; 32])` (default zeroed, like the existing builders).
2. `handle_register`: set `mirror_verify_key: Vec<u8>` in the RegisterResponse from the new
   field.
3. Tamper handling: in `dispatch_client_message`, add the match arm
   `client_message::Msg::TamperAlert(alert)` →
   - `db.record_tamper_alert(...)` (method lands via slice 2 in monopsony-db; call it
     THROUGH the db handle you already have) with client_id: None (the stream does not
     currently carry authenticated client identity into dispatch — pass alert.client info
     via the `detail` field? NO: pass client_id: None and add a `// client identity: future`
     comment), observed_at from the alert.
   - Log at ERROR level: url, expected vs actual hash. This is the alarm surface.
   - Return `ServerMessage { msg: Some(SyncRequest{}) }` ack (same as inventory).
4. Thread the verify key: `pub async fn start(...)` gains a `mirror_verify_key: [u8; 32]`
   param BEFORE `additional_tls_sans`; update the internal MonopsonyGrpc::new chain; update
   the ControlStream construction site (~line 399) to use `self.mirror_verify_key` (the
   outer struct builds per-message `grpc` handles — copy the field into that construction).
5. main.rs grpc::start call site: pass the key ([0u8;32] when disabled).

## Do NOT
- Do not touch crates/monopsony-admin/src/pairing.rs (hub-owned).
- Do not change proto, config.rs, db.rs signatures (consume slice-2's record_tamper_alert;
  if it is not visible yet in your check, write the call as specified — slice 2 lands in
  parallel; your final cargo check may show that one missing-method error: report it
  verbatim instead of stubbing it).
- Do not change ProxyState or proxy.rs.

## Verify (ONCE, from /home/societus/projects/monopsony)
`cargo check -p monopsony-server -p monopsony-control` (expect green EXCEPT possibly one
missing `record_tamper_alert` error from the parallel slice-2 — report verbatim if so)
