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
osascript -e 'do shell script "/Users/martin/.local/bin/monopsony-client setup mac --enable" with administrator privileges with prompt "Monopsony: enable Content Caching"'
```

> **2026-10-31 correction:** the original sample used `~/.local/bin/monopsony-client` —
> `~` inside `do shell script … with administrator privileges` expands in ROOT's context
> (`/var/root`, verified via `dscl`) and would fail file-not-found. Absolute paths only.

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

## Verdict record (2026-10-31 agent-assisted pass — nobody at console; details in
## notes/2026-10-31-b4-cc-toggle-prototype.md + notes/2026-10-31-b4-scan-traffic-verdict.md)

| Item | Verdict | Evidence (one line) |
|---|---|---|
| 2 status --quiet | pass | over ssh: `Server: 10.10.20.58:50051` from the real pairing file; HIT-ratio line absent (counters zero — daemon never ran) |
| 3 HIT ratio | not run | server .58 DOWN (3130/9090/50051 refused) + daemon crash-loops on missing certs (`~/.config/monopsony`) — rerun after server is up |
| 4 menu app | not run | needs GUI at console |
| 5 CC toggle action | WIRE (one console approval pending) | ssh osascript admin fails instantly (-60007, no prompt); GUI-domain LaunchAgent presents the dialog; `~` pitfall found — use absolute paths |
| 6 scan button | CUT (keep CLI verb) | routing PROVEN via system proxy (CONNECT swscan/gdmf/xp captured on 127.0.0.1:3129) but uncached tunnels only; `--background` throttle-gated (frequent no-op) |

## Verdict record — completion pass (2026-10-31 18:05–18:25 MST; .58 back in service)

Reattempt of the items the earlier pass could not run (server down). Method: fresh
dashboard `pair/init` code → `pair --cert-dir ~/.monopsony-b4-certs` (dedup-replace,
same client_id d1e8834e…) → `daemon --cert-dir`, traffic through 127.0.0.1:3129, server
metrics delta on :9090. Full detail: review-spec §11.2 + the two notes files.

| Item | Verdict | Evidence (one line) |
|---|---|---|
| 2 status --quiet | PASS | `Server: 10.10.20.58:50051` + `Cache HIT ratio: 50.0%` after 1 MISS + 1 HIT through the daemon |
| 3 HIT ratio | PASS — moves correctly | `hello_2.10-3_arm64.deb`: `x-cache: MISS` → server journal `Cached: … (52308 bytes, sha256 f7b20d304db0)` → refetch `x-cache: HIT` (+ server `Joining in-flight fetch`); counters `{"hits":1,"misses":1,"stale":0}` → ratio 50.0% |
| 6 scan button | CUT (strengthened) | both `softwareupdate -l` scans bypassed the just-enabled scutil proxy (0 CONNECTs at the daemon, counter flat) while Chrome tunnelled ~4 MB through the same chain (connect_bytes 5,372 → 4,016,228) — SUS proxy pickup is session-dependent; cache value still zero |
| server-side CONNECT counters | CONFIRMED observable | controlled CONNECT swscan.apple.com:443 moved `monopsony_proxy_connect_bytes_total` 0 → 5372; journal logs per-target dial failures (`CONNECT upstream dial ingest:8006 failed: dial to 100.119.165.62:8006 timed out`) |
| 4 menu app | not run (GUI) | still needs the console; the Oct 14 bench client must be redeployed first (gRPC transport dies vs Oct 30 server — TLS verify 0 (ok), post-handshake failure) |

New findings (review-spec §11.2): root-owned bench state dirs (`~/.config/monopsony/`,
`~/.local/state/monopsony/` — Oct 15 elevated run) freeze the HIT counters and block the
LaunchAgent daemon; one-line console fix `sudo chown -R martin:staff ~/.config/monopsony
~/.local/state/monopsony`; `monopsony-server pair` subcommand 401s against its own admin
API while manual login→pair/init works (follow-up card).

## Verdict record — close-out (2026-10-31 ~12:35–12:45 MST; rsync + chown landed, agent over ssh)

Fresh-client redeploy per review-spec §11.3 (full detail there). The source sync
included `clients/` this time; the console chown was applied before this pass — the
LaunchAgent daemon has been crash-free since 12:32:45.

| Item | Verdict | Evidence (one line) |
|---|---|---|
| 1 fresh client deploy | DONE | bench build (runbook env) 60 s clean; md5 `c34d2d6cfb70178106a7ab5060ce316f` replaces Oct 14 `701d71aa…`; KeepAlive relaunched daemon PID 72872 on the new binary |
| gRPC transport failure | ROOT-CAUSED: version skew (closed) | same certs + server: Oct 14 binary dies ~11 ms post-TLS; fresh build connects instantly — heartbeat gauge `d1e8834e…` cycling 0→60 (was frozen), `failed_updates 0`, 0 new err-log lines since restart |
| canonical certs | refreshed to Oct 31 pairing | copied from `~/.monopsony-b4-certs` (same client_id d1e8834e…, cert the server minted today); old set → `~/.config/monopsony.oct15.bak`; Cert Expiry 2027-01-29, renewal not-due |
| 2 status --quiet | PASS | `Server: 10.10.20.58:50051` + Cert Expiry + **`Cache HIT ratio: 100.0%`** after 2× `x-cache: HIT` on hello_2.10-3_arm64.deb; counters `{"hits":2,"misses":0,"stale":0}` persisted (mtime advancing) |
| chown fix | CONFIRMED effective | daemon healthy since 12:32:45 (was crash-looping on the root-owned 600 key); persist() writes the canonical state dir |
| menu app build | DONE (staged for item 4) | `swift build` complete 43.64 s on synced tree; binary at `/tmp/monopsony-build/clients/macos/.build/debug/MonopsonyMenuBar` |
| 4 menu app GUI + 5 CC approval | STILL OPEN (console) | the only items left; need martin at the m2mini console |

Bench state left: daemon running healthy (steady state), 3129 listening, system
proxy OFF (`HTTPEnable : 0`), CC unchanged, no reboots.

## Restore / cleanup

- If CC was enabled for step 5/6 and shouldn't stay: `~/.local/bin/monopsony-client setup mac --disable`
  (or `sudo defaults delete /Library/Preferences/com.apple.AssetCache AllowPersonalCaching` + reloadSettings).
- System proxy: restore via the existing Tier-1 removal (`networksetup` cleanup — client
  `--system off` path).
- No server-side bench seeds are created by this run (HIT counters are client-local state);
  nothing to delete on .58.
- Gate: record verdicts back into `docs/plans/sprint26-phase4-b4-review-spec.md` §10 table +
  §11 notes so Phase 4 can close.
