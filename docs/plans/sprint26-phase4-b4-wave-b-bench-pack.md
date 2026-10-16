# Sprint 26 Phase 4 — B4 Wave B Bench Pack (turnkey console run)

**Date:** 2026-10-16 · **Status:** STAGED — everything below runs in one console session at m2mini
**Source of truth:** review spec §11 (banked runbook, commit 9fa787b) + §10 results table.
**Cross-ref:** LuLu path-keying RCA `context/2026-10-15-macos-codesigning-lulu-compat.md`.

Everything here needs **martin at the m2mini console** (sudo + GUI prompts). Agent-side
read-only ssh is fine for pre-flight; the run itself is human-operated.

## Pre-flight facts (verified 2026-10-16 over read-only ssh)

- Toolchains present: `1.94.1-aarch64-apple-darwin` (the runbook's pinned path — still valid)
  AND `stable-aarch64-apple-darwin`. **Use the stable dir** unless comparing against an older
  bench build; memory records stable = 1.98.1. If the runbook PATH is kept verbatim, it works.
- `protoc` at `/Users/martin-ci/.local/bin/protoc` → libprotoc 36.2. ✓
- Wave B files are committed (per §11 header). Confirm on the dev host before syncing:
  `git status --short` should show no `clients/macos/` or client-crate dirt.
- **LuLu note (why the deploy step is shaped the way it is):** unsigned binaries are keyed by
  PATH only in LuLu's rules.plist (2026-10-15 inspection) — `cp` over `~/.local/bin/monopsony-client`
  keeps the existing approval valid. NEVER deploy to a versioned path on this box: the client
  would silently lose outbound (TCP connects, payload blackholed, no prompt on a headless node).

## Run checklist

### 1. Deploy client binary (bench-build on the Mac)

From the dev host, then on the bench (§11 runbook verbatim):

```sh
rsync -a --exclude target/ --exclude .git/ --exclude dashboard/ --exclude clients/ --exclude docs/ --exclude dist/ \
  ./ m2mini:/tmp/monopsony-build/
ssh m2mini 'mkdir -p /tmp/monopsony-build/dashboard/build'
ssh m2mini 'export PATH="/Users/martin-ci/.rustup/toolchains/1.94.1-aarch64-apple-darwin/bin:/Users/martin/.local/bin:/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin"
  export PROTOC=/Users/martin-ci/.local/bin/protoc
  cd /tmp/monopsony-build && cargo build --release -p monopsony-client \
  && cp target/release/monopsony-client ~/.local/bin/monopsony-client'
```

Gate: `~/.local/bin/monopsony-client --version` prints the new build; `ls -la` mtime is today.

### 2. `status --quiet` live check (slice 5 data source)

As martin: `~/.local/bin/monopsony-client status --quiet`
Expect: `Server: <grpc_addr-or-ws_url>` from the real pairing file; `Cache HIT ratio:` line
appears after traffic. **Verdict:** ☐ pass ☐ fail (ratio never moves → slice 5 data source
wrong in the field — record below).

### 3. HIT-ratio meaning experiment (slice 5 gate)

Cold fetch through the daemon (e.g. `brew install` of something not cached, or a fresh npm
package) → ratio drops; refetch → climbs. **Verdict:** ☐ moves correctly ☐ wrong.

### 4. Menu app connect (GUI, at console)

Launch the built app. Check: icon states (bolt green / slash / warning), per-service rows,
dashboard opens, scan button fires `softwareupdate --background` **without prompting**.
**Verdict:** ☐ pass ☐ fail (which element).

### 5. sudo elevation prototype (slice 1 action half — keep/cut)

At the console, try:

```sh
osascript -e 'do shell script "PATH=/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:$PATH ~/.local/bin/monopsony-client setup mac --enable" with administrator privileges with prompt "Monopsony: enable Content Caching"'
```

- Expected: ONE GUI password prompt at the console; `setup mac --enable` runs; CC activates
  (check `AssetCacheManagerUtil canActivate` / status row flips).
- If reliable → wire the menu button (Core.swift osascript wrapper + AppState action).
- If prompts land wrong, double-prompt, or fail on headless → slice 1 stays status-only with a
  "run in Terminal" hint. **Verdict:** ☐ wire the button ☐ status-only.

### 6. Scan-traffic experiment (slice 2 gate — keep/cut the scan button)

With system proxy ON (and/or CC active), trigger `monopsony-client update-scan`, then check
whether SUS traffic is observable through Monopsony. Observables, any one suffices:

- **Server side (.58):** `curl -s http://10.10.20.58:9090/metrics | grep -i connect` before/during —
  CONNECT counter delta to `swscan`, `mesu`, `gdmf`, or `cacheserve` Apple hosts; or tail proxy logs.
- **Mac side:** `AssetCacheManagerUtil statistics` before/after — bytes-served delta proves
  CC-routed SUS traffic; `assetcachelocatorutil` shows which cache the Mac selected.
- `log stream --info --predicate 'process IN {"softwareupdated","nsurlsessiond"}'` — URLs
  visible while the scan runs.

**No observable routing → cut the button as cosmetic** (keep/cut rule from the review spec).
**Verdict:** ☐ observable (keep) ☐ cosmetic (cut) — evidence line recorded below.

## Verdict record (fill during the run, then paste into review-spec §10/§11)

| Item | Verdict | Evidence (one line) |
|---|---|---|
| 2 status --quiet | | |
| 3 HIT ratio | | |
| 4 menu app | | |
| 5 CC toggle action | | |
| 6 scan button | | |

## Restore / cleanup

- If CC was enabled for step 5/6 and shouldn't stay: `~/.local/bin/monopsony-client setup mac --disable`
  (or `sudo defaults delete /Library/Preferences/com.apple.AssetCache AllowPersonalCaching` + reloadSettings).
- System proxy: restore via the existing Tier-1 removal (`networksetup` cleanup — client
  `--system off` path).
- No server-side bench seeds are created by this run (HIT counters are client-local state);
  nothing to delete on .58.
- Gate: record verdicts back into `docs/plans/sprint26-phase4-b4-review-spec.md` §10 table +
  §11 notes so Phase 4 can close.
