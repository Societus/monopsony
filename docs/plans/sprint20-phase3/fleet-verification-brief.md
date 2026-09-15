# Sprint 20 Phase 3 — fleet verification brief (bench ops)

## Background (all facts up front — discover nothing)
- Server: 10.10.20.58 (Arch VM). NO systemd — the staging server runs via
  `setsid nohup /home/monopsony/monopsony-staging-server`; restart = kill PID, then re-run
  that line as user monopsony. Config `/etc/monopsony/server.toml` (edit via sudo).
  Ports: 3128 proxy, 50051 gRPC, 9090 metrics, 3129 mirror, 3130 dashboard (admin/testpass123).
  PostgreSQL: monopsony/monopsony locally; `psql -U monopsony monopsony`.
- Clients (all root@ ssh + sudo, paired, current CA): .20 Arch/pacman, .62 Ubuntu/apt,
  .235 Rocky/dnf, .242 Debian/apt, .159 Alpine/apk. Client binary
  `/usr/local/bin/monopsony-client`. Client daemon is NOT systemd-managed on bench hosts —
  start via `setsid nohup monopsony-client daemon ...`.
- GOTCHA: stray `monopsony-server` processes can grab :3129 on client hosts — before
  starting any client daemon: `pgrep -af monopsony` and kill strays (match on comm, not
  pgrep -f args, to avoid self-matches).
- Binaries: build locally (`cargo build --release -p monopsony-server -p monopsony-client`
  with `export PROTOC=/usr/bin/protoc`), scp to hosts, keep `.bak` of the replaced binary.
- KEY ROTATION FACT: already-paired clients never re-register (client_id file exists), so
  they will NOT receive `mirror_verify_key` from Register. For a test client to get the
  pin key: `rm <cert_dir>/client_id` and restart the daemon (forces re-register), or re-pair.
  Use .20 as the pinning client; leave .62/.159 unpinned to prove inert behavior.

## Tasks (linear)
A. Build + deploy server and client binaries to .58 and .20 (keep .bak copies; sha256sum
   both before/after).
B. Server config: add to `/etc/monopsony/server.toml` on .58 (backup first):
   `[integrity]` / `pinning_enabled = true`. Restart the staging server. Confirm log line
   "Integrity pinning: enabled" and that `<shadow_dir>/pin-signing.key` was created.
C. Client .20: delete client_id, restart daemon, confirm log "Mirror verify key saved"
   (register path) and "Integrity pin verification enabled".
D. Verification matrix (each item: run, compare, PASS/FAIL):
   1. Pin capture: through .20's proxy chain, fetch a package URL the cache has NOT seen
      (`pacman -Sw <small-pkg>` after clearing that URL, or curl an Arch mirror .db via
      the chain). Then refetch → response must carry `X-Monopsony-Pin` header; on the
      server `<cache_dir>/pins/<key>.pin` exists. [Index URLs may be pin origin
      unverified — that is correct for Phase 3; a GitHub-release asset with a .sha256
      sidecar should show origin sidecar.]
   2. Tamper injection: on .58, flip a byte in the cached object file for the URL from
      step 1 (`printf '\x00' | dd of=<cachefile> bs=1 seek=<N> conv=notrunc`). Refetch
      through .20 → download must FAIL (client hash-check stream truncates the body);
      `.20:<cert_dir>/tamper-events.jsonl` contains a mismatch event within one heartbeat
      (≤60s); server log shows the TamperAlert ERROR line; `psql` shows a tamper_alerts
      row; a third fetch gets 403 with `X-Monopsony-Quarantine: 1`.
   3. Unpinned client stays green: same tampered URL fetched via .62 (pinning not
      provisioned) → serves fine, no events. Proves the feature is inert without a key.
   4. No false positives: `pacman -Syu` on .20 completes green end-to-end with pinning on.
   5. Restore: undo the tampered byte (restore from the .bak of the object or delete the
      cache entry), remove the tamper_alerts row is NOT needed (history is fine), keep
      `[integrity]` enabled on .58.
E. Docs: record results (PASS/FAIL per item + server/client log excerpts) in
   `context/2026-09-14-phase3-pinning-fleet-verification.md`. NO commits — owner signs.

## Failure table
- Refetch after tamper NOT rejected → check .20 log for "Integrity pin verification
  enabled"; check the pin file exists on .58; if missing, the commit path silently failed
  (parent-dir bug class) — capture `ls -la <cache_dir>/pins/`.
- No tamper_alerts row but client rejected → heartbeat drain needs one beat; wait 60s.
- Server won't boot after config edit → TOML typo; `tomlcheck` the file; restore .bak.
- Client daemon loops on connect → stale CA (ca_guard handles; check server log) or stray
  :3129 squatter (kill it).
- Disk/ENOSPC weirdness on the WORKSTATION during builds → `du -sh /tmp/.tmp*` after any
  killed cargo run (tmpfs, 7.7G).

## Hard rails
- No destructive git operations; NO commits (owner signs manually).
- Config/binary backups before every replacement; restore after matrix item 5.
- Never weaken auth or SSRF guards to make a step pass; two failures or any security-guard
  anomaly → STOP and report.

## Reporting contract
Per matrix item: exact command + PASS/FAIL + one-line evidence (log excerpt or psql row).
Include binary sha256s, config diff, and an explicit "not verified" list. One-paragraph
rollback recipe at the end.
