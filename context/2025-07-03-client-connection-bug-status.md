# Client Connection Bug Status — July 3, 2026 (Updated)

**Session:** WebUI 74ab59e31910 (glm-5.2)
**Base commit:** `0a531d1` — "fix: client registration bugs (partial WIP)"
**Working tree:** Uncommitted changes — Bug B fix + port fix + accessor + dev-deps cleanup
**Test server:** 10.10.20.58 (monopsony-server, fresh CA generated this session)
**Test client:** 10.10.20.20 (pacmantest, Arch Linux, monopsony-client deployed)

---

## Bug B: Client daemon gRPC TLS UnknownIssuer — ✅ FIXED

### Root Cause

NOT a certificate generation issue. The bug was in `grpc_client.rs:67`:

```rust
// BROKEN — calls MonopsonyControlClient::connect(endpoint)
let client = MonopsonyControlClient::connect(endpoint).await?;
```

`MonopsonyControlClient::connect()` is tonic-generated code that internally calls `Endpoint::new(dst)`. Tonic's `Endpoint::new()` (tonic-0.12.3 `endpoint.rs:53-54`) detects the HTTPS URI scheme and **silently overwrites** any custom TLS config with `ClientTlsConfig::new().with_enabled_roots()` — which uses only the system's native/webpki root certificates. Our private CA is not in those system roots, so webpki reports `UnknownIssuer`.

### The Fix

In `crates/monopsony-client/src/grpc_client.rs`, `MonopsonyGrpcClient::connect()`:

```rust
// FIXED — connect the channel directly, bypass tonic's Endpoint::new() overwrite
let channel = endpoint.connect().await?;
let client = MonopsonyControlClient::new(channel);
```

This preserves the custom CA trust anchor in the TLS config.

### Why It Was Hard to Find

- OpenSSL validated the chain fine (it doesn't use webpki)
- A local rustls/webpki unit test passed (it bypassed tonic's Endpoint::new)
- A standalone tonic test binary passed (it called `endpoint.connect()` directly)
- Only the daemon binary failed — because it was the only code path going through `MonopsonyControlClient::connect(endpoint)`
- The error message (`UnknownIssuer`) pointed at cert chain issues, not at tonic's internal config overwrite

### Verification (ad-hoc, live)

Deployed fresh client binary to 10.10.20.20. Daemon output:
```
gRPC connection established
Connected to server
Heartbeat sent
```
No `UnknownIssuer` error. Both initial connection and heartbeat loop reconnect succeed.

---

## Bug A: Pairing cancel doesn't abort listener task — ✅ FIXED

**Root cause:** `handle_pair_cancel` marked the session `Expired` but never aborted the spawned tokio task holding port 50052 in `accept()`.

**Fix:** Added `pairing_task: Arc<Mutex<Option<JoinHandle<()>>>>` field to `AdminState`. `handle_pair_init` stores the `JoinHandle` after spawning. `handle_pair_cancel` calls `handle.abort()` to release the port.

**Verification (ad-hoc, live):** Cancel + immediate re-init succeeds. Server log confirms `Pairing listener task aborted` followed by new session binding port 50052 cleanly.

---

## Bug C: Client pair address port-doubling ✅ FIXED (uncommitted)

Fixed in `grpc_client.rs` — checks if address already ends with `:50052` before appending.

---

## Files Changed (current working tree)

| File | Change |
|------|--------|
| `crates/monopsony-client/src/grpc_client.rs` | **Bug B fix**: `endpoint.connect()` + `MonopsonyControlClient::new(channel)` instead of `connect(endpoint)`. Also Bug C port-doubling fix. |
| `crates/monopsony-admin/src/pairing.rs` | **Bug A fix**: `handle_pair_init` stores `JoinHandle`, `handle_pair_cancel` calls `handle.abort()`. |
| `crates/monopsony-admin/src/lib.rs` | Added `pairing_task` field to `AdminState`. |
| `crates/monopsony-server/src/main.rs` | Wire `pairing_task` field in `AdminState` constructor. |
| `crates/monopsony-control/src/tls.rs` | Added `ca_cert_der_bytes()` public accessor. |

---

## Current Test Environment State

| Host | IP | OS | Role | Status |
|------|-----|-----|------|--------|
| Server | 10.10.20.58 | Arch | monopsony-server | **Running** (fresh CA), ports 3128,50051,3129,3130,9090 |
| Client | 10.10.20.20 | Arch | monopsony-client | **Connected** ✅ Paired, daemon works, heartbeats flowing |
| Client | 10.10.20.62 | Ubuntu | Not yet deployed | Ready |
| Client | 10.10.20.235 | Rocky 9 | Needs musl build | Ready |
| Client | 10.10.20.242 | Debian 12 | Needs musl build | Ready |
| Client | 10.10.20.159 | Alpine | Needs musl build | Ready |

---

**Last updated:** July 3, 2026 — Bug A and Bug B both fixed and verified live. Client connects, heartbeats flow, cancel+re-init works.
