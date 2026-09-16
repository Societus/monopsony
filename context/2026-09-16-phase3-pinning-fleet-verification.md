# Sprint 20 Phase 3 — pinning fleet verification results

**Executed:** 2026-09-16, bench ops via SSH from thequietbox.
**Brief:** `docs/plans/sprint20-phase3/fleet-verification-brief.md`
**Verdict: MATRIX STOPPED at item 2 — two real bugs found (brief's two-failure rule).**
Items 1–5 partially verified; items 3–4 not run (blocked by item 2 failure).

## Binaries deployed (Task A — PASS)

| Host | Path | sha256 | Backup |
|---|---|---|---|
| .58 server | `/home/monopsony/monopsony-staging-server` | `2520c31460bebd6e3858b18365a023e6b0f304b6623dd868c3bb67ab70fa4bc7` | `.bak-20260916-181753` (97b93dc8…) |
| .20 client | `/usr/local/bin/monopsony-client` | `bb58745ab3c39e3d82b4c8471cd5913d2d959c433be032ba92ca7e4f28d468a2` | `.bak-20260916-181753` |

Built locally from `b18a110` (clean tree, cargo check green, warnings only).

## Task B — server config (PASS)

- `/etc/monopsony/server.toml` backed up (`server.toml.bak-*`), appended:
  ```toml
  [integrity]
  pinning_enabled = true
  unverified_policy = "flag"
  ```
- Restarted via `setsid nohup ./monopsony-staging-server --config /etc/monopsony/server.toml`.
- Log: `Integrity pinning: enabled (policy=flag), key: /var/lib/monopsony/shadow/pin-signing.key`
- `/var/lib/monopsony/shadow/pin-signing.key` created (32 bytes).
- `cache_pins` + `tamper_alerts` tables migrated (already existed from earlier runs).

NOTE: pkill with the binary name in the SSH command's own args self-matches and
kills the launching shell — kill by PID (`ps -eo pid,args | awk`), never `pkill -f`.

## Task C — client .20 re-register (PASS, with Bug 1 caveat)

- REAL cert dir on .20 is `/root/.config/monopsony/` (client.toml `[tls]`), NOT
  `/var/lib/monopsony/certs/` (that dir holds stale July certs).
- Moved `client_id` → `client_id.pre-phase3test`, restarted daemon:
  - `Registering with server hostname="pacmantest" … received_cert=true`
  - `Mirror verify key saved for pin verification`
  - `Registered with server, client ID: 0ac5ebad-365f-4549-9431-c7ffd7ab3724` (same ID re-issued)
- **BUT** "Integrity pin verification enabled" did NOT appear on that boot → Bug 1.

## Matrix item 1 — pin capture (PASS on file+header; FINDING on DB row)

Evicted `tree-2.3.2-1-x86_64.pkg.tar.zst` (files + `cache_entries` row) then:

1. MISS through .20 chain: `x-cache: MISS`, 44562 bytes fetched, 200 OK.
2. HIT refetch: `x-cache: HIT` + full signed header:
   ```
   x-monopsony-pin: {"url":"https://geo.mirror.pkgbuild.com/extra/os/x86_64/tree-2.3.2-1-x86_64.pkg.tar.zst","expected_sha256":"8a6230468cc31a2c984a41c092035dd16bf97e737ec3241490724a5419903739","origin":"unverified","key_id":"2b6121f781d8e1b8",...,"signature":"37e5cfc8…e04"}
   ```
3. Pin file: `/var/cache/monopsony/pins/pacman/extra/os/x86_64/tree-2.3.2-1-x86_64.pkg.tar.zst.pin` exists (full nested parent chain — the slash-key bug did not regress).
4. **FINDING (non-blocking):** `cache_pins` table stayed at 0 rows —
   `db.record_pin()` exists in monopsony-db (db.rs:770) but has NO production
   call site; proxy.rs:1092 still says `// DB upsert: slice-2 wires db.record_pin()`.
   Slice 2's DB layer was never wired.

## Matrix item 2 — tamper injection (FAIL → STOP)

Setup: flipped byte @20000 in the cached object (sha `8a623046…` → `c10804e9…`).

Run 1 (fresh-registered daemon): 200 OK, full 44562 bytes served, sha of
download = tampered `c10804e9…`, no `x-monopsony-verified` header, no tamper
event, no server alert. → Bug 1 (below): verification not provisioned on that boot.

Run 2 (after daemon restart — key now loaded from disk): 200 OK, full body,
`x-monopsony-verified: pending` present (wrap ENGAGED), but download sha STILL
tampered `c10804e9…`, no "TAMPER" or "Pin verified" log line, empty
`tamper-events.jsonl`, no `tamper_alerts` row. → Bug 2 (below).

Quarantine 403 check: unreachable (no alert row can exist).

## The two bugs (root causes, code-verified)

**Bug 1 — provisioning-order: pin verification inert on the key-delivery boot.**
`crates/monopsony-client/src/main.rs`: `PinState` is constructed at ~line 813
from `load_mirror_verify_key()` BEFORE the register block; when register
delivers the key, `store_mirror_verify_key()` (line ~862) writes it to disk but
never calls `pin_state.set_key()` — `set_key` is dead code (build warning
confirmed). Verification engages only on the NEXT daemon restart. Every
first-provisioning client is unprotected for one daemon lifetime.
**Fix:** after `store_mirror_verify_key`, call `pin_state.set_key(&mirror_key)`.

**Bug 2 — Content-Length suppresses the end-of-stream hash check.**
`crates/monopsony-client/src/pin_verify.rs` `PinCheckStream::poll_next` only
computes/compares the digest at `Ready(None)` (end of inner stream). The HIT
response carries `Content-Length: 44562`; once hyper's encoder has written that
many body bytes it stops polling the stream — the terminal poll never runs, so
mismatch is never detected (and even if it were, the Err chunk arrives after a
length-complete body, so curl/pacman have already accepted it).
The ledger e2e tests pass because their raw-socket fixtures close the
connection, forcing the terminal poll.
**Fix direction:** strip/replace `content-length` with `transfer-encoding:
chunked` when wrapping with `pin_check_stream` (the wrapper already appends a
synthetic Err chunk on mismatch — chunked framing is what makes truncation
observable to the PM). Also see skill pitfall: same class as the latch rule.

**Secondary finding:** drained `tamper_alerts` rows (from earlier ledger tests)
have EMPTY `url` and `client_id` columns — the drain path loses both fields.
Data-quality bug in the TamperAlert gRPC arm.

## Items not run (blocked by item 2)

- Item 3 (unpinned client .62 stays green) — meaningless until 2 works.
- Item 4 (`pacman -Syu` no false positives) — same.
- Windows bench verification — already deferred (user decision 2026-09-14).

## Item 5 — restore (DONE)

- Pristine object restored on .58 (scp from .20's MISS-fetch copy):
  sha256 = `8a6230468cc31a2c984a41c092035dd16bf97e737ec3241490724a5419903739` ✓
- `client_id` on .20 restored by re-register (same UUID re-issued; the
  `.pre-phase3test` rename files left in place for forensics).
- `[integrity]` left ENABLED on .58 per brief. tamper_alerts history kept.

## Rollback recipe

1. .58: restore binary `cp monopsony-staging-server.bak-20260916-181753
   monopsony-staging-server`; restore config `cp /etc/monopsony/server.toml.bak-*
   server.toml`; kill PID + `setsid nohup ./monopsony-staging-server --config
   /etc/monopsony/server.toml > /tmp/monopsony-server.log 2>&1 &`.
2. .20: restore binary from `.bak-20260916-181753`, restart daemon same pattern.
3. No DB rollback needed (DELETE 1 cache_entries row for tree was re-INSERTed
   by the MISS fetch; 6 pre-existing tamper_alerts rows are test history).

## Next actions (owner decision)

1. Fix Bug 1 (one-line `set_key` call) and Bug 2 (chunked wrap) in a follow-up
   slice; re-run matrix items 2–4 after redeploy.
2. Wire `db.record_pin()` at proxy.rs:1092 (or drop the table from the schema
   until wired — pick one truth).
3. Fix TamperAlert drain to populate url + client_id.
4. Then re-run this matrix + items 3/4 in one session.

## FIX SESSION (2026-09-16, same day)

Both bugs fixed in-tree (uncommitted; owner signs):

**Bug 1 fix** — `crates/monopsony-client/src/main.rs` (register block): after
`store_mirror_verify_key()`, the delivered key now also provisions the
in-memory state via `pin_state.set_key()` (zeroed delivery → unprovision,
matching server-off semantics). Log line added:
"Integrity pin verification enabled (mirror key delivered at register)".
The `set_key` dead-code warning is gone.

**Bug 2 fix** — `crates/monopsony-client/src/proxy.rs` `wrap_with_pin_check`:
strips `content-length` + `transfer-encoding` from the wrapped response so
hyper re-frames the body chunked; the end-of-stream hash verdict now actually
executes and a mismatch aborts the transfer (reset or error frame mid-body —
both are failed downloads to the PM). Inert path (no key / no pin header)
keeps original framing untouched.

**Regression tests:**
- Unit (`proxy.rs` tests): `pin_wrap_strips_content_length` (provisions AFTER
  construction — the Bug 1 path — and asserts CL/TE stripped, verified header
  set), `pin_wrap_inert_without_key_keeps_framing` (inert keeps CL).
- E2E (`tests/pin_e2e.rs`, NEW — the missing coverage class): keep-alive +
  Content-Length upstream (production framing; the ledger harness's
  `Connection: close` masked the bug). 3 tests:
  `tampered_body_aborts_under_content_length_framing` (observed live: TAMPER
  fires ~12ms, download does NOT complete cleanly, event spooled with both
  hashes), `clean_body_serves_clean_under_framing` (chunked re-frame,
  byte-identical body, zero events), `unprovisioned_client_is_inert` (200 +
  original framing, zero events). All 3 green.
- Note: on mismatch the h1 encoder may abort the connection with the head
  un-flushed (reset, zero response bytes). Legal h1 behavior, still a failed
  download; the e2e accepts either abort shape.

Fleet re-verification of matrix items 2–4 with the fixed binary: PENDING.

## FLEET RE-VERIFICATION (2026-09-16 20:28–20:31, fixed client 91b0952a…)

Deployed fixed client to .20 (`/usr/local/bin/monopsony-client`
91b0952ad82383785b23bc389cf22eb6d28d03554a06b8fd222eb37bfa70a9f1;
previous bb58745a… kept as server-side .bak copies from the morning run).
Daemon restart logged `Integrity pin verification enabled (mirror key
provisioned)` on the boot itself — Bug 1 fix confirmed live.

**Item 2 — tamper injection: PASS (all four assertions)**
1. Byte @20000 flipped (sha 8a623046… → c10804e9…). Refetch through .20:
   `curl rc=18` (transfer closed, outstanding data) — download FAILED;
   23,949 of 44,562 bytes delivered. Client log:
   `TAMPER: pin mismatch … — truncating body`.
2. Event spooled to tamper-events.jsonl (url + both hashes) within seconds.
3. Heartbeat drain ≤60s: server log `TAMPER ALERT: client-reported cache
   integrity mismatch url=…` (ERROR) + `Tamper alert recorded: id=7` —
   tamper_alerts row WITH url + detail populated.
4. Third fetch: `403 Forbidden` + `x-monopsony-quarantine: 1` — quarantine
   gate live.

**Item 3 — unpinned client inert: PASS**
.62 (no mirror-verify.key): served cleanly through its chain after restore
(200, 44562B, pristine sha), no client-side pin/tamper log lines, no spool.
Note: while the test quarantine row existed, .62 ALSO received the 403 —
that is the server-side quarantine gate (by design: an alert row makes the
URL un-servable for everyone; forensic-evidence contract). Client-side
inertness is what item 3 verifies and it held.

**Item 4 — no false positives (pacman -Syu): PASS**
Full `pacman -Syu --noconfirm` on .20 with pinning on: rc=0, core.db (8.47M)
synced through the chain, `Pin verified … (sha256 ok)` logged for the tree
package, ZERO false TAMPER events. (Two 404s from pacman's mirror-status
probes passed through untouched — pre-existing upstream behavior, unrelated.)

**Item 5 — restore: DONE**
Pristine object restored on .58 (sha 8a623046… verified); test alert row
id=7 removed (tamper_alerts back to the 6 pre-existing ledger-test rows);
both daemons healthy; `[integrity]` left enabled per brief.

## VERDICT

Matrix items 1–5 ALL PASS with the fixed client. Phase 3 pinning is
live-proven end-to-end on the bench: pin capture at commit, signed delivery
on HIT, client stream verification with real truncation, tamper-event spool,
heartbeat drain to server, tamper_alerts row, and quarantine 403.

Remaining known gaps (unchanged, non-blocking): `db.record_pin()` unwired
(cache_pins table stays empty; pin file + header are the working source of
truth), TamperAlert client_id not populated (documented future work).
