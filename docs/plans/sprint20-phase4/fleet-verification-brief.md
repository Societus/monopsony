# Sprint 20 Phase 4 — fleet verification brief (bench ops)

## Background (all facts up front — discover nothing)
- Server: 10.10.20.58 (Arch VM). **Phase 4 touches NOTHING on the server** —
  the ledger, downgrade policy, index parsing, and override CLI are all
  client-side. The server stays UNTOUCHED and running as-is; do not edit
  `/etc/monopsony/server.toml` or restart it for this brief.
- Clients (all root@ ssh + sudo, paired, current CA):
  - .20 Arch/pacman — PRIMARY (gnu target): M1, M2, M4 run here.
  - .159 Alpine/apk — PRIMARY (musl target): index-parsing smoke on APKINDEX.
  - .62 Ubuntu/apt — INERT CONTROL: ledger disabled in client config; M3.
  - Client binary `/usr/local/bin/monopsony-client`. Client daemon is NOT
    systemd-managed on bench hosts — start via
    `setsid nohup monopsony-client daemon ...`.
- GOTCHA (unchanged): stray `monopsony-server` processes can grab :3129 on
  client hosts — before starting any client daemon: `pgrep -af monopsony` and
  kill strays (match on comm, not pgrep -f args, to avoid self-matches).
- Build + deploy (client binary only):
  ```
  export PROTOC=/usr/bin/protoc
  cargo build --release -p monopsony-client
  sha256sum target/release/monopsony-client   # record BEFORE deploy: ____________
  scp target/release/monopsony-client root@10.10.20.20:/usr/local/bin/monopsony-client.new
  ssh root@10.10.20.20 'cp -a /usr/local/bin/monopsony-client /usr/local/bin/monopsony-client.bak-phase4 && mv /usr/local/bin/monopsony-client.new /usr/local/bin/monopsony-client && chmod +x /usr/local/bin/monopsony-client && sha256sum /usr/local/bin/monopsony-client'
  ```
  Repeat for root@10.10.20.159 and root@10.10.20.62. Record sha256 AFTER
  deploy per host: .20 ____________ .159 ____________ .62 ____________
  (before/after must match).
- Config: `/etc/monopsony/client.toml`. Phase 4 section:
  ```
  [ledger]
  enabled = true
  policy = "warn"          # "warn" | "reject"
  max_index_age_secs = 0   # 0 = staleness check disabled
  ```
  ALWAYS `cp -a /etc/monopsony/client.toml /etc/monopsony/client.toml.bak-phase4`
  before editing. Ledger DB lives at `$HOME/.local/state/monopsony/ledger.sqlite3`
  (root: /root/.local/state/monopsony/ledger.sqlite3); tamper events at
  `/root/.local/state/monopsony/tamper-events.jsonl`.
- CLI surface used below: `monopsony-client override add|list` (signed
  exception records, slice 6) and the ledger's high-water via the override/
  ledger CLI — NEVER by doctoring upstream indexes.

## Tasks (linear)
A. Build + deploy the new client binary to .20, .159, .62 (keep
   `.bak-phase4` copies; sha256sum before/after — slots above).
B. Config: add the `[ledger]` block above on .20 and .159 (backup first).
   On .62 add `[ledger] enabled = false` (or omit entirely — both inert).
   Restart each client daemon (kill PID, re-run the setsid line; check for
   :3129 squatters first). Confirm each daemon log opens the ledger
   (log line naming the ledger path) on .20/.159 and does NOT on .62.
C. Verification matrix (below).

## Verification matrix (each item: run → compare → PASS/FAIL)
- M1 (.20) downgrade policy trips:
  1. Seed a high-water ABOVE current repo state (forces a downgrade on the
     next real refresh). NOTE: there is no `ledger seed` CLI subcommand —
     seed via sqlite3 directly (order_key format is
     `{:016}:{}:{:06}` of epoch/version/release from version_ord::order_key):
     ```
     sqlite3 /root/.local/state/monopsony/ledger.sqlite3 \
       "INSERT OR REPLACE INTO high_water (package_key, version, version_ord, source, seen_at) \
        VALUES ('pacman:bash', '9.9.9-1', '0000000000000000:9.9.9:000001', 'bench-seed', strftime('%s','now'));"
     ```
  2. policy = "warn": `curl -v -x http://127.0.0.1:3129 \
     http://mirror.osbeck.com/repo/os/x86_64/core.db -o /tmp/core.db`
     → HTTP 200 with `X-Monopsony-Downgrade:` header present. Also
     `pacman -Sy` through the proxy → completes (warn passes) with a
     downgrade warning in the client log.
  3. Flip policy to "reject" (M2 step, below), repeat the same curl →
     HTTP 503, body contains "rewound", and
     `/root/.local/state/monopsony/tamper-events.jsonl` gains a line
     containing `downgrade:`.
- M2 (.20) policy flip changes behavior: backup config, edit
  `/etc/monopsony/client.toml` `[ledger] policy` warn → reject, restart the
  client daemon (pkill monopsony-clien; re-run the setsid line — a broken
  kill loop means the OLD daemon keeps :3129 and every later step silently
  tests stale code), repeat the M1 fetch → behavior must change from
  200+header to 503. Restore policy to "warn" after M4.
- M3 (.62) inert control: `[ledger] enabled = false` must be EXPLICIT —
  the section's serde default is enabled=true, so omitting [ledger]
  ENABLES the ledger (derive-Default trap, opposite of the usual inert
  default). With the explicit false: `apt update` fetches flow but show no
  ledger behavior and tamper-events.jsonl does not grow. Known pre-existing
  (out of phase-4 scope): .62's monopsony-mirror.list points
  /mirror/ubuntu at the server's gRPC port 50051, so apt errors with
  "not signed"/403 regardless of the ledger.
- M4 (.20) override exception: with policy "reject" and M1's seed still
  in place (fetches 503ing). Real flags are `--to` and `--ttl-secs`
  (NOT --to-version/--ttl):
  ```
  monopsony-client override add --package pacman:bash --to 5.2.36-1 \
    --reason "phase4 M4 bench: vendor-verified downgrade window" --ttl-secs 120
  monopsony-client override list          # record shows, with expiry
  curl -sS -D - -o /dev/null -x http://127.0.0.1:3129 .../core.db   # → 200 now
  sleep 121 && monopsony-client override list    # record dropped (expired)
  curl -sS -D - -o /dev/null -x http://127.0.0.1:3129 .../core.db   # → 503 again
  ```
  Then restore policy "warn", restart daemon.
- M5 (.159) apk smoke (musl): fetch an APKINDEX through .159's proxy —
  use the HTTP form of the URL (https goes through the CONNECT tunnel,
  which is opaque by design and never parsed):
  ```
  curl -sS -D - -o /tmp/APKINDEX.tar.gz -x http://127.0.0.1:3129 \
    http://dl-cdn.alpinelinux.org/alpine/v3.22/main/x86_64/APKINDEX.tar.gz
  ```
  → 200; on a second fetch after seeding `apk:busybox` above current state
  (same sqlite3 INSERT pattern as M1, order_key
  `0000000000000000:9.9.9:000001`) → `X-Monopsony-Downgrade` header (warn).
  Confirms the APKINDEX parser path on musl.

## Failure table (symptom → bounded action)
- Client daemon won't start after config edit → TOML typo; `tomlcheck`
  /etc/monopsony/client.toml; restore `.bak-phase4`.
- M1 fetch does NOT trip after seed → check the daemon log for the ledger
  open line and the ledger DB existing at
  `/root/.local/state/monopsony/ledger.sqlite3`; check `[ledger] enabled` in
  the config the running daemon actually loaded (restart after ANY edit).
- 503 on EVERYTHING through :3129 → stray :3129 squatter (kill it) or the
  upstream :3128 chain is down (server side out of scope here — check
  connectivity, do not touch .58).
- Override add/list: command not found → client binary predates slice 6;
  redeploy the wave-2 binary (task A) before M4.
- Events file not growing but headers present → the warn path only spools
  on downgrade/parse-error; that is correct behavior, not a failure.
- Disk/ENOSPC on the WORKSTATION during builds → `du -sh /tmp/.tmp*` after
  any killed cargo run (tmpfs, 7.7G).
- Two failures or any auth anomaly → STOP and report. Never iterate past two.

## Hard rails
- NO commits (owner signs manually); no destructive git operations.
- `cp -a` config AND binary backups before every replacement.
- NEVER weaken auth or SSRF guards to make a step pass.
- Restore any https→http repo rewrites after testing, per the standard
  fleet procedure (pacman mirrorlist / sources.list edits are test-only).
- .58 server stays UNTOUCHED for the whole brief.

## Reporting contract
Per matrix item: exact command + PASS/FAIL + one-line evidence (header line,
HTTP status, or jsonl excerpt). Include binary sha256s (slots above), config
diffs, and an explicit "not verified" list. One-paragraph rollback recipe at
the end: restore `.bak-phase4` binaries, restore `.bak-phase4` configs,
restart daemons, confirm ledger lines gone from .62 and warn-mode restored
on .20/.159.

## Coverage note
This brief covers matrix items M1–M5 (downgrade policy trip, policy flip,
inert control, override TTL window, apk/musl smoke). Not covered here and
explicitly OUT of scope for this bench session: payload-vs-index hash
cross-checks (phase 4 L4 — needs pinned infrastructure), apt Valid-Until
native enforcement (wave 2 slice 5 follow-up).
