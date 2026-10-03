# B1 fleet gate — shared context pack (all lanes)

Read this fully before your first command. You know nothing else about this
session; this pack + your lane brief are the complete contract.

## Mission

Fleet-verify Sprint 26 Phase 1 (Homebrew caching, "B1") per
`docs/plans/sprint26-phase1-homebrew/bench-gate-checklist.md` (9 items).
Every item ends with a verdict in the report table: **proven / stale / blocked**
+ which binary proved it (sha256 prefix) + one-line evidence (log line, header,
count — not narrative).

## Hosts (verified 2026-10-02)

| Host | Role | Address | SSH user |
|---|---|---|---|
| .58 | Monopsony staging server | `10.10.20.58` | `monopsony@` (key auth works; BatchMode ok) |
| m2mini | macOS client under test | `10.10.2.127` (LAN IP) | `martin-ci@` (key auth works) |
| .20 | Arch Linux client (item 8) | `10.10.20.20` | see Linux lane brief step 0 |

- **m2mini: DO NOT REBOOT.** No tailscale tun on it; only the LAN IP works.
  The Mac tailscale app has no `tailscale status` CLI — do not try.
- **.58 staging server:** binary `~monopsony/monopsony-staging-server`, sha256
  prefix `f692b12d210f2127` (= fresh build of commit a7dfab6 — ALREADY DEPLOYED
  AND RUNNING; do not redeploy). Started with:
  `--config /etc/monopsony/server.toml --database-url postgres://monopsony:@localhost/monopsony`,
  log at `/tmp/monopsony-server.log`, proxy :3128, dashboard :3130
  (admin/testpass123), health `curl http://10.10.20.58:3128/health` → `ok`.
- Brew shim routes verified live: `GET /brew/api/formula/jq.json` → 200.

## m2mini facts

- macOS 26.3.1, brew 7.0.7 at `/opt/homebrew/bin/brew` (ABSOLUTE PATH — not on
  non-interactive ssh PATH). `/opt/homebrew` owned by `martin` — run brew as
  martin-ci (prefix owner user is `martin`, but martin-ci owns the checkout and
  the spike proved brew works as martin-ci).
- Stale `~/.homebrew/brew.env` was already removed — clean slate.
- LuLu firewall is deny-by-default outbound with GUI-only approval. If an
  sshd-spawned curl/brew gets `http=000` / connect-timeout, the fix is a prompt
  at the Mac's console (user must approve) — rules persist after. If this hits
  you: record the item BLOCKED-lulu, continue with other items, report exactly
  which binary needed approval.
- m2mini has a source tree at `~/projects/monopsony` that is NOT a git repo
  (leftover from the spike). Staging will replace it via rsync before your
  first step (hub does this, or your step 0 does — see lane brief).

## Server config state (.58) — /etc/monopsony/server.toml

- `budget_bytes = 1048576` (1 MiB — deliberate standing value post-Phase-F).
- `scrub_interval_secs = 60`, `scrub_min_age_secs = 0` (bench: scrub every pass),
  `stale_part_sweep_secs = 60` (janitor cadence 1 min).
- Retention evicts **LangArtifact (brew bottles) ONLY** — metadata (the 15.6 MB
  JWS API bundle, formula JSON) is NEVER an eviction candidate. Distros are not
  candidates either. So 1 MiB standing budget does not threaten item 1, but WILL
  evict bottles once a few accumulate → **your lane brief tells you when to bump
  `budget_bytes` and to what. Any edit: `cp /etc/monopsony/server.toml
  /tmp/server.toml.bak` FIRST, and restore + restart before you finish.**

## Server restart recipe (only where your brief says)

```bash
ssh monopsony@10.10.20.58 'PID=$(ps -o pid= -C monopsony-staging-server | head -1 | tr -d " "); kill $PID; sleep 2; cd ~ && setsid nohup ./monopsony-staging-server --config /etc/monopsony/server.toml --database-url postgres://monopsony:@localhost/monopsony > /tmp/monopsony-server.log 2>&1 < /dev/null & sleep 5; curl -s -o /dev/null -w "health=%{http_code}\n" http://127.0.0.1:3128/health'
```
Then verify the NEW daemon: fresh `/tmp/monopsony-server.log` first lines
(`Loaded config from ...`) + health ok + `sha256sum ~/monopsony-staging-server |
cut -c1-16` still `f692b12d210f2127`. A failed kill leaves the OLD daemon
serving — every later test then exercises stale code.

If two lanes run at once and a request suddenly gets connection refused for
~30 s, the other lane is restarting the server — wait 60 s, retry once.

## Binding traps (from fleet-verification history — all have bitten real runs)

1. **404 stubs lie.** Brew silently falls back to origin on mirror 404 — a
   "passing" install proves nothing. Evidence must be server-side (log lines,
   X-Cache headers, cache_entries rows), never brew's output alone.
2. **Fresh-download evidence** = changed hash prefix under
   `~/Library/Caches/Homebrew` + the .58 log/hit counters — not brew's output.
3. **Verify the DEPLOYED binary contains the shim** before gate items: sha256
   vs `f692b12d210f2127`. A green /health proves nothing about which code runs.
4. **Never leave a corrupted blob behind** (scrub item): purge blob + sidecars
   immediately after the check — verify-on-serve skips young blobs, so poison
   keeps serving and every later MISS through that URL eats it.
5. **Never leave a forced budget behind**: after retention item, restore
   `budget_bytes = 1048576` + restart + health check.
6. Evicting one entry = blob AND sidecar trees (`.cache-meta/<key>` AND
   `.meta/<key>` under the cache root). Enumerate with `find <root> -path
   "*<key>*"` before deleting; don't derive paths by convention.
7. **Headers are the X-Cache source of truth**: `curl -sD - -o /dev/null <url>`
   shows `x-cache: HIT|MISS|STALE`. HIT/STALE from cache; the DB
   `cache_entries.cached_at` is miss-time-only — never use it as freshness
   evidence.
8. **Kill/restart discipline:** never `pkill -f` a pattern that appears in the
   same composite command (it kills your own session mid-script). Split kill
   and relaunch into separate ssh calls. Backgrounded daemon + sleep in one ssh
   dies at tool timeout — the setsid'd daemon survives; verify through a FRESH
   ssh (pgrep + log tail), never through that command's exit code.
9. **Never interleave grep inside `&&` build/deploy chains** — grep exit 1
   (no matches) halts the rest silently. Run test and build as separate calls.
10. If a tool approval gate BLOCKS a command twice, stop issuing variants of
    it — note BLOCKED-gate in the report and move on. Never loop retries.
11. Cache root on .58: `/var/cache/monopsony`. Disk: 119 G free — no limits.

## Cache-key shapes (brew)

- Metadata: `brew/api/...` (class lang_metadata — TTL-revalidated, exempt from
  retention eviction AND from scrubber hashing? NO — scrubber hashes
  lang_artifact only; metadata is parse-candidate = None).
- Bottles: `brew/bottles/<name>/<version>-<rebuild>/<name>-<version>.<platform>.bottle.<rebuild>.tar.gz`
  shape (flat-ish; retention `parse_brew_path` reads segments
  `bottles/<name>/<version>/<rest>` → name+version → semver major for floors).

## Reporting

Final report = per-item verdict table (item #, verdict, evidence, which host +
binary sha proved it), then: leftover-state checklist (env file removed? budget
restored? corrupted blobs purged? seeds cleaned?), then open blockers. Report
FACTS from tool output — a child summary claiming success without evidence will
be re-verified by the hub anyway; save us both the pass.
