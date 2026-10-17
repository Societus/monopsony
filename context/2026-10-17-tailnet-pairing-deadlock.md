# 2026-10-17 — Tailnet pairing hang (CT 1933 → 100.124.155.69)

First tailnet deployment. `monopsony-client pair --server 100.124.155.69:50052 --code 5MUQQT`
connects, prints the INFO line, then hangs forever. No certs saved. Server shows the client
registered. Re-runs also hang despite `nc` reachability.

## Root cause chain (verified from live evidence)

1. **Stale server binary.** `/usr/local/bin/monopsony-server` on 10.10.20.58 is dated
   **Jul 4 23:17** — two months older than commit `9aaf9ae` (Sep 13), which fixed the
   `start_sync_task` tokio RwLock self-deadlock ("froze pairing at cert generation").
   Version string `1.0.0-rc1` is just the Cargo package version — it says nothing about
   build freshness. Remote has no tags; the rc1 install path delivered the July artifact.
2. **Startup self-deadlock.** Server booted 15:50:50 UTC, logged `Loaded 3 mirror
   configuration(s) from database` (archlinux `sync_enabled=true`), then `monopsony_mirror`
   never logged again — no `Starting initial sync for mirror`. Pre-fix `start_sync_task`
   holds the `configs` read guard while awaiting its own `write()` ("mark syncing") →
   eternal pending writer.
3. **Reader starvation.** tokio RwLock is fair: every later `configs.read()` queues behind
   the pending writer.
4. **Pairing wedges.** 17:20:03 — `register_client` + `record_pairing` succeed (no lock
   needed; last server log: `Client registered and paired in DB`), then
   `ensure_mirrors_for_package_managers("apt")` → `get_config("debian")` →
   `configs.read().await` queues forever. No `Auto-creating mirror config` log, no
   `Pairing certificates sent`, no error. pg_stat_activity clean (not a DB hang).
5. **Client symptoms explained.** Client blocks in `read_exact` on the PairResponse —
   `pair()` had no timeout. The stuck pairing task keeps the listener bound, so re-runs
   land in the kernel backlog: `ss -tn` shows ESTAB/CLOSE-WAIT sockets on :50052 with
   ~41 unread request bytes and zero owner progress.

## Evidence highlights

- journal: session started 17:16:52, client connected 17:20:03.279, code validated
  .319, registered .357 — then silence.
- `ss -tn | grep 50052`: original socket `ESTAB Recv-Q 0 Send-Q 0`; later attempts
  queued with unread bytes.
- psql: no waiting/active queries; `mirror_configs.archlinux.sync_status='syncing'`
  stale since 2026-07-04 (the July deadlock, persisted in the DB row).
- `GET /api/v1/mirrors` responds in 1.5 ms — it reads the DB, so it does NOT prove the
  in-memory lock is healthy.

## Fixes

Already in HEAD before this session: `9aaf9ae` (Sep 13) — start_sync_task drops the read
guard before syncing.

Added this session (defense in depth — a stall of any kind must error, not hang):

- `monopsony-admin/src/pairing.rs`: request read + response write bounded by
  `PAIRING_IO_TIMEOUT` (30 s); mirror auto-creation bounded by `MIRROR_ENSURE_TIMEOUT`
  (5 s, warn + continue pairing). Frame parsing extracted to `read_pair_request` with
  the 64 KiB cap.
- `monopsony-client/src/grpc_client.rs`: `pair()` connect timeout (10 s), response
  timeout (30 s) with explicit "server accepted but never completed" message, response
  length cap 64 KiB (`read_pair_response`).
- Tests: frame helper tests in both crates (valid / oversized / truncated via
  `tokio::io::duplex`); `mirror.rs` regression test pinning that
  `ensure_mirrors_for_package_managers` stalls while the config lock is held.
- Workspace version bumped `1.0.0-rc1` → `1.0.0-rc2`; cut rc2 from HEAD so the
  installer delivers current code.

## Verification

- `cargo test -p monopsony-admin -p monopsony-mirror -p monopsony-client`: all pass
  (incl. new tests).
- `cargo clippy` on the three crates: clean.

## Live-box recovery (user-executed)

1. Build from HEAD (rc2) and replace `/usr/local/bin/monopsony-server` on 10.10.20.58
   (or install via the rc2 release once tagged).
2. `sudo systemctl restart monopsony-server` — confirm the startup log now shows
   `Starting initial sync for mirror: archlinux` (fingerprint of the fixed sync task).
3. Fresh pairing code, re-pair CT 1933 against 100.124.155.69:50052; verify certs in
   the cert dir and `monopsony-client daemon` connecting over mTLS :50051.
