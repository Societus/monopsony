# Lane 1 — m2mini brew chain (items 1, 2, 3, 4, 5, 7, 6, 9 in THIS order)

Shared background: read `bench-gate-context-pack.md` in this directory FIRST.
Your lane owns the m2mini client + the brew-cache-dependent server items.
Strictly sequential — every item depends on state from the previous one.
A second lane (Linux regression, item 8) runs in parallel on a DIFFERENT host;
it never touches server config. If .58 briefly refuses connections, it's the
other lane probing — wait 30 s, retry once.

## Step 0 — toolchain staging on m2mini — ✅ DONE BY HUB (2026-10-02 09:4x)

- Repo rsynced to `martin-ci@m2mini:~/projects/monopsony/` (a7dfab6, no target/).
- Client BUILT on m2mini: `target/release/monopsony-client` — Mach-O arm64,
  sha256 prefix `ce45a223a84964b9`, `proxy status --pm brew` runs (exit 0).
- protoc installed at `~/.local/bin/protoc` (36.2) — only needed if you rebuild.
- DO NOT rsync again (a hub re-rsync would clobber nothing now, but don't risk
  it); DO NOT rebuild unless you change code — if you must, use:
  `PROTOC=$HOME/.local/bin/protoc cargo build --release --offline -p monopsony-client`
  (LuLu blocks crates.io; registry cache is synced for offline builds).

Start at Step 1. Your binary provenance for the report: client sha
`ce45a223a84964b9` (m2mini), server sha `f692b12d210f2127` (.58).

## Step 1 — setup (checklist setup §1-3)

```bash
ssh martin-ci@m2mini 'cd ~/projects/monopsony && ./target/release/monopsony-client proxy on --pm brew --server 10.10.20.58:3128'
ssh martin-ci@m2mini 'cat ~/.homebrew/brew.env'   # exactly 2 lines, both http://10.10.20.58:3128/brew/...
ssh martin-ci@m2mini '/opt/homebrew/bin/brew config | grep -i domain'
```

Item 9 (off-state) comes LAST — do not run `proxy off` now.

## Step 2 — item 1: formula metadata MISS→HIT

1. Clear brew's local api cache (user-run on the Mac? NO — it's under
   `~/Library/Caches/Homebrew` of martin-ci, martin-ci owns it):
   `ssh martin-ci@m2mini 'rm -rf ~/Library/Caches/Homebrew/api/*'`
2. `ssh martin-ci@m2mini '/opt/homebrew/bin/brew info jq'`
3. On .58: `grep -iE 'formulae.brew.sh|jq' /tmp/monopsony-server.log | tail -5`
   — expect the upstream fetch (MISS).
4. Re-run `brew info jq`, then probe the route directly:
   `curl -sD - -o /dev/null http://10.10.20.58:3128/brew/api/formula/jq.json`
   → expect `x-cache: HIT`.
5. DB row: `ssh monopsony@10.10.20.58 'PGPASSWORD="" psql -h localhost -U monopsony monopsony -c "SELECT url, cache_class FROM cache_entries WHERE url LIKE '"'"'%brew/api%'"'"' ORDER BY cached_at DESC LIMIT 5"'`
   (passwordless local peer auth; if psql absent use
   `sudo -u postgres psql monopsony -c ...` — try plain first).
   Expect class = lang_metadata.
6. Verdict: MISS logged + second fetch HIT + row lang_metadata.

## Step 3 — item 2: bottle full chain

1. Pick a small NOT-installed formula (jq if absent: `brew list --formula |
   grep -x jq || use jq`). Force fresh: `brew uninstall jq 2>/dev/null; rm -rf
   ~/Library/Caches/Homebrew/downloads/*`.
2. `brew install jq` — must succeed.
3. Evidence on .58 log: formula JSON → ghcr manifest fetch → blob fetch
   (307-following reqwest path). grep the log for `ghcr` / `bottles/jq`.
4. sha256 proof: brew verified it (no "SHA256 mismatch" / fallback Warning in
   brew output) — capture brew's tail. Plus blob row in cache_entries
   (class lang_artifact, url LIKE '%bottles/jq%').
5. Cache-side: `find /var/cache/monopsony -path '*jq*' | head` shows blob +
   BOTH sidecar trees.

## Step 4 — item 3: second-install HIT

1. `ssh martin-ci@m2mini 'brew uninstall jq; rm -rf ~/Library/Caches/Homebrew/downloads/*'`
2. `brew install jq` again → succeeds.
3. Probe: `curl -sD - -o /dev/null http://10.10.20.58:3128/brew/bottles/jq/<ver>-<rebuild>/...tar.gz`
   (copy the exact path from step 3's cache find) → x-cache: HIT. And .58 log
   shows NO new ghcr upstream fetch this round.

## Step 5 — item 4: upgrade-from-cache

1. Find an outdated formula: `brew outdated` (if none, `brew pin` something
   then... simpler: pick any formula whose installed version is behind:
   `brew info <f>` vs `brew list --versions <f>`. If nothing is outdated,
   install an older-versioned formula from a versioned tap/name if trivially
   available, else mark item 4 PARTIAL with reason "no outdated formula on
   bench" — do NOT fabricate an upgrade).
2. `brew upgrade <f>` → succeeds; .58 log shows bottle served from cache
   (no upstream fetch for that blob), brew sha256 passes.

## Step 6 — item 5: enablement toggle

1. Dashboard API (admin:testpass123 on :3130):
   `curl -s -u admin:testpass123 http://10.10.20.58:3130/api/v1/admin/language-pms`
   → brew row present (source=auto after step 3's first request).
2. PATCH brew → disabled (same endpoint, PATCH with
   `{"pm":"brew","enabled":false}` — check exact shape via GET first).
3. `brew uninstall jq; rm -rf ~/Library/Caches/Homebrew/downloads/*; brew install jq`
   → still SUCCEEDS (silent origin fallback) but .58 log shows the 404 on the
   blob/manifest route.
4. PATCH back → enabled. Re-probe route → HIT/MISS normal.

## Step 7 — item 7: scrubber quarantine (before retention — needs a live blob)

1. Pick a THROWAWAY cache key (re-installable, e.g. jq's blob — it will be
   re-cached on demand later). Corrupt the BLOB file in place
   (`find /var/cache/monopsony -path '*jq*' -name '*.tar.gz'` — write garbage
   over it: `printf 'corrupt' > <blob>`).
2. Scrub cadence is 60 s with min_age 0 — but min_age 0 may also mean young-blob
   skip is OFF; wait for a scrub pass: check log for `scrub` lines
   (`grep -i scrub /tmp/monopsony-server.log | tail`), up to 90 s.
3. Verify: tamper_alerts row (`SELECT url, expected_sha256, actual_sha256 FROM
   tamper_alerts ORDER BY observed_at DESC LIMIT 3`), metric
   scrub_quarantined_total incremented
   (`curl -s http://10.10.20.58:3128/metrics | grep scrub_quarantined`).
4. **PURGE IMMEDIATELY**: find ALL jq blob + sidecar paths
   (`find /var/cache/monopsony -path '*jq*'`), rm them, plus delete the
   tamper_alerts row is NOT needed (audit trail is fine to keep). Verify the
   route now 404s (quarantine gone with the files) — re-install jq via brew to
   re-cache cleanly (expect MISS) if later items need it.

## Step 8 — item 6: retention eviction

The tricky one. Seeds must be SAME formula name, MULTIPLE majors. The brew API
only serves the latest version, so build seeds DIRECTLY in the cache:

1. Ensure jq's real blob + sidecars exist (from step 3/7 re-cache). Read the
   real sidecars:
   `.cache-meta/<key>` and `.meta/<key>` (find exact paths first).
2. Create 2 synthetic OLD-major entries by copying the real blob file to
   sibling paths that parse as older versions. Key/path shape follows the real
   jq blob path with version swapped (e.g. real `2.1.1` → seed `1.6.0` and
   `0.20.0`). Copy BOTH sidecar files for each seed, editing the JSON fields
   that carry size/checksum to match the copied blob (size must match the
   real file you copied; keep class lang_artifact). If the sidecar schema has
   a version/url field, update it too — make each seed self-consistent.
3. Force the budget: backup config, set `budget_bytes = 1048576` (1 MiB —
   jq real bottle ~0.4-1 MB + seeds > budget), restart server (recipe in
   context pack), health-check + sha-verify binary.
4. Wait one janitor cadence (60 s). Expect log line `Retention: evicting`
   naming the OLD-major seeds while the CURRENT jq major survives (floor
   protects the newest major line).
5. Restore: `cp /tmp/server.toml.bak /etc/monopsony/server.toml`, restart,
   health-check. Verify budget back to 1048576 standing value.
6. If eviction fires but ALSO evicts the current major → STOP, record FAIL
   with the evict log lines (floor logic broken) — do not "fix" in place.

## Step 9 — item 9: off-state

1. `ssh martin-ci@m2mini 'cd ~/projects/monopsony && ./target/release/monopsony-client proxy off --pm brew'`
2. `cat ~/.homebrew/brew.env` → gone. `brew config | grep -i domain` → stock
   domains (ghcr.io / formulae.brew.sh).

## Report

Per-item verdict table (1,2,3,4,5,7,6,9 — report in checklist order), each
with evidence + which binary proved it (server sha f692b12d…, client sha from
`shasum -a 256 target/release/monopsony-client | cut -c1-16` on m2mini). Then
leftover-state checklist: budget restored? corrupted blobs purged? synthetic
seeds gone (evicted)? brew.env removed? Then blockers (LuLu, gate timeouts).
FACTS from tool output only.
