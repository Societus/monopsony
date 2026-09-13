# CA Lifecycle Guard — Session Status (2026-09-13)

Fail-closed CA generation + backup/auto-restore ("plan B") implemented, tested, and
live-verified on 10.10.20.58. **Uncommitted** — awaiting user GPG signing.
**Test bench left mid-test** — see "Current .58 state" before touching the server.

## Motivation

Bug C (Sep 12 checkpoint) root cause: cache-dir wipe silently regenerated the CA while
clients were paired → every client UnknownIssuer until re-pair. This makes that
failure mode impossible to hit silently.

## What was built

- `crates/monopsony-server/src/ca_guard.rs` (NEW):
  - `decide()` — pure decision fn: LoadExisting / RestoreFromBackup / GenerateFresh /
    AbortMissingWithClients. DB-unreachable + missing CA fails closed; explicit
    `--no-db` counts as clientless (safe to generate).
  - `backup_ca()` — mirrors ca.crt+ca.key (key chmod 0600) + `ca-fingerprint.txt` to
    `--ca-backup-dir` (default `/var/lib/monopsony/ca-backup`, OUTSIDE the cache dir
    so a cache wipe can't destroy both copies). Idempotent; detects drift.
  - `restore_ca()` — auto-restore (plan B); rejects partial backups.
  - `init_ca()` — wires it up. **Backup failures are best-effort (warn + continue)** —
    initial version aborted startup when backup dir unwritable, which broke the 3
    smoke_tests (non-root test process can't write /var/lib). Fail-closed only fires
    when the CA itself is missing with clients present.
- `main.rs`: DB connect moved BEFORE CA init (guard needs client count); new flags
  `--ca-backup-dir`, `--regenerate-ca` (deliberate rotation override).

## Verification

- Unit: 6 new ca_guard tests; workspace total **503 passed / 0 failed / 2 ignored**.
- Live on .58 (5 paired clients in DB):
  1. Normal boot → `CA backed up to /var/lib/monopsony/ca-backup`, fingerprint matches.
  2. Cache-wipe sim (tls dir moved away) → `CA restored from backup`, SAME fingerprint
     `c4:48:87:1c:...:84:24` → the Jul 4 incident is now self-healing.
  3. Total-loss sim (tls + backup both hidden) → server **aborted** with recovery
     instructions; generated nothing. Server NOT restarted after this test (see below).

## Current .58 state (MUST restore before any client testing)

- `monopsony-staging-server`: **STOPPED** (test 3 abort was the expected result).
- `/var/cache/monopsony/tls` → hidden at `/var/cache/monopsony/tls.hidden-test`
  (plus stale `tls.wiped-test` copy).
- `/var/lib/monopsony/ca-backup` → hidden at `/var/lib/monopsony/ca-backup.hidden-test`.
- Deployed binary already contains the ca_guard build (scp'd + swapped 15:43).

Restore procedure (was blocked on approval, never ran):
```
sudo rm -rf /var/cache/monopsony/tls.hidden-test /var/cache/monopsony/tls.wiped-test
sudo mv /var/lib/monopsony/ca-backup.hidden-test /var/lib/monopsony/ca-backup
sudo -u monopsony setsid /home/monopsony/monopsony-staging-server \
  --config /etc/monopsony/server.toml >> /home/monopsony/server-run.log 2>&1 &
# expect 5 ports (3128/50051/9090/3129/3130) + fingerprint c4:48:...:84:24
```

## Commit pending

Stage: `crates/monopsony-server/src/ca_guard.rs` (new) +
`crates/monopsony-server/src/main.rs`. Message draft ready for user review/signing
(`.git-commit-message.tmp` workflow, LLM-disclosure line included).

## Gotchas

- Nested ssh from ops box into .58 fails BatchMode host-key verification — run
  remote state checks directly from the ops box, scp scripts over.
- `sudo mv` of the tls dir is the clean way to simulate CA loss (no rm needed).
- Backup-dir permission failure = warn+continue, NOT abort (smoke-test lesson).
