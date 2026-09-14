# Fleet Verification Brief — Sprint 19 Phase 1 CONNECT pass-through

You are a verification agent. A feature is code-complete on the dev machine;
your job is to deploy it to the test bench, run a fixed verification matrix,
document a config example, and stage (NOT make) a commit. Work linearly,
report honestly, never fabricate output. If a step fails, follow the failure
table; if it fails twice, STOP and report — do not improvise workarounds.

════════════════════════════════════════════════════════════════════
## 1. Background (read once, no action)
════════════════════════════════════════════════════════════════════

Repo: `/home/societus/projects/monopsony` (Rust workspace). The working tree has
UNCOMMITTED changes (9 modified files + untracked `docs/plans/sprint19-phase1/`
and `context/` files) implementing HTTP CONNECT pass-through: https URLs now
tunnel through client proxy (:3129) → server proxy (:3128) → upstream, opaque,
SSRF-guarded, per-tunnel rate-limited. All 509 workspace tests pass locally.

Rules for the tree:
- NEVER run destructive git commands (stash/reset/checkout --/clean). The
  uncommitted work is the only copy of several sessions' effort.
- Do not commit. The owner signs commits manually.

════════════════════════════════════════════════════════════════════
## 2. Test bench facts
════════════════════════════════════════════════════════════════════

- **Server**: 10.10.20.58 — Arch VM, 4 GB. SSH `monopsony@10.10.20.58`
  (passwordless sudo). PostgreSQL `monopsony/monopsony`. Server config
  `/etc/monopsony/server.toml`. **NO systemd on this host** — the server runs
  as `setsid nohup /home/monopsony/monopsony-staging-server … &`.
- **Client**: 10.10.20.20 — Arch LXC. SSH root. Client proxy :3129,
  config `/etc/monopsony/client.toml` (⚠ schema is stale on this host —
  if parsing fails, fix minimally, note it in your report).
- Ports on .58: 3128 (proxy), 50051 (gRPC), 9090 (metrics), 3130 (dashboard,
  admin/testpass123), 3129 (mirror router).
- ⚠ Stray `monopsony-server` processes sometimes squat ports on client hosts.
  Before starting the client daemon on .20: `pgrep -af monopsony` and kill
  anything unexpected (match by name via `ps -eo pid,comm,args`, not pgrep -f).
- Bench binaries are glibc (both hosts are Arch x86_64).

════════════════════════════════════════════════════════════════════
## 3. Task A — Build and deploy (do this first)
════════════════════════════════════════════════════════════════════

1. On the dev machine, from `/home/societus/projects/monopsony`:
   `cargo build --release -p monopsony-server -p monopsony-client`
   (If protoc errors: `export PROTOC=/home/societus/.local/bin/python-grpc-tools-protoc`.)
   Verify both binaries' mtimes are fresh before proceeding.
2. Back up + deploy server to .58:
   - `ssh monopsony@10.10.20.58 'cp -a ~/monopsony-staging-server ~/monopsony-staging-server.bak-$(date +%Y%m%d-%H%M%S)'`
   - `scp target/release/monopsony-server monopsony@10.10.20.58:~/monopsony-staging-server.new`
   - Swap: on .58, `mv` the .new over the live path ONLY after stopping the old
     process (Task B step 1). Keep the .bak files for rollback.
3. Deploy client to .20:
   - `scp target/release/monopsony-client root@10.10.20.20:/usr/local/bin/monopsony-client.new`
     then swap atomically (mv). Back up the old binary first the same way.
4. Enable CONNECT on the server: on .58, append to `/etc/monopsony/server.toml`:

   ```toml
   [proxy]
   # HTTP CONNECT tunneling (https pass-through). Opaque tunnels, no caching.
   connect_enabled = true
   # SSRF guard stays ON (omit allow_private_upstreams — default false).
   # Optional deny-list example: connect_deny = ["internal.corp"]
   # Timeouts (defaults shown): connect_timeout_secs = 10, tunnel_max_lifetime_secs = 3600
   ```

════════════════════════════════════════════════════════════════════
## 4. Task B — Verification matrix (run in order, capture every output)
════════════════════════════════════════════════════════════════════

**B0. Restart server with new binary + config.** On .58: find old PID
(`ps -eo pid,args | grep monopsony-staging` — avoid grep self-match by using
ps), `kill` it, wait for exit, then start:
`cd ~ && setsid nohup ./monopsony-staging-server --config /etc/monopsony/server.toml > /tmp/monopsony-server.log 2>&1 &`
(Use the SAME CLI args the old process had — read them from `ps` output BEFORE
killing; if unsure, check `/tmp` log headers or report and ask.)
Verify: `curl -s http://10.10.20.58:3128/health` → `ok`, and dashboard
:3130 answers.

**B1. Baseline regression — the existing http cache path must not have
regressed.** On .20 (ensure client daemon running with the NEW binary):
fetch any small package file twice through the chain and confirm the second
is `X-Cache: HIT` (inspect headers via `curl -v` or check cache stats).
Also confirm `pacman -Sy` still succeeds through the proxy.

**B2. CONNECT e2e (the headline test).** On .20:
`curl -sS -o /dev/null -w '%{http_code} %{time_total}s\n' -x http://127.0.0.1:3129 https://api.github.com`
Expect `200`. Then repeat against a real https package mirror (e.g.
`https://geo.mirror.pkgbuild.com/core/os/x86_64/core.db`) — expect 200 and
note that CONNECT tunnels are uncached BY DESIGN (no X-Cache header is
correct).

**B3. Server-down → clean 503, no fallback.** On .58 stop the server (kill
PID). On .20: same curl as B2 with `--max-time 30`. Expect HTTP 503 with a
body containing `Server proxy connect failed` or the CONNECT-failed marker.
Then RESTART the server exactly as B0 and re-verify B2 passes (leave the
bench running).

**B4. SSRF reject.** On .20:
`curl -s -o /dev/null -w '%{http_code}\n' -x http://127.0.0.1:3129 https://10.10.20.1/`
Expect `403` (private target, allow_private_upstreams=false).

**B5. Metrics.** `curl -s http://10.10.20.58:9090/metrics | grep connect_bytes`
→ `monopsony_proxy_connect_bytes_total` present and > 0 after B2 traffic.

**Failure table:**
| Symptom | Action |
|---|---|
| Server won't start | Check `/tmp/monopsony-server.log`; config parse error → fix TOML syntax; port taken → find PID via `ss -tlnp`, kill, retry once |
| B2 fails, connection closed instantly | Check server log for `CONNECT` lines; confirm `[proxy]` section actually in the config TOML the process was started with |
| B2 fails, 503 | Client can't reach :3128 — firewall/routing; `nc -zv 10.10.20.58 3128` from .20 |
| B4 returns 200 | SSRF guard broken — STOP, report; do not weaken the guard |
| Any cargo/ssh auth issue | STOP and report; do not modify SSH config |

════════════════════════════════════════════════════════════════════
## 5. Task C — Config example documentation
════════════════════════════════════════════════════════════════════

In the repo, edit `dist/server.toml.example`: add a fully-commented `[proxy]`
section (same fields/knobs as Task A step 4, with prose explaining
connect_enabled, the SSRF default, connect_deny suffix semantics, and both
timeout knobs). Match the file's existing comment style. Also add one line to
the section header comment: CONNECT tunnels are opaque/uncached by design.

════════════════════════════════════════════════════════════════════
## 6. Task D — Stage the commit (do NOT commit)
════════════════════════════════════════════════════════════════════

Draft the commit message to `/home/societus/projects/monopsony/.git-commit-message.tmp`
(an UNTRACKED file — verify with `git status --porcelain` that it stays
untracked; never `git add` it). Message requirements:
- Summary line: `Sprint 19 Phase 1: HTTP CONNECT pass-through (https tunnels) + hardening`
- Body: what landed (server+client CONNECT, serve-loop Router bypass and the
  axum Content-Length root cause, SSRF/deny/timeouts/NODELAY, slot-lifetime
  fix, resolve-once anti-rebinding, 9 streaming tests incl. chain e2e),
  verification status (509 workspace tests; live fleet results from Task B),
  and a factual LLM-disclosure line: the implementation was LLM-drafted and
  human-reviewed across multiple sessions.
Do NOT run `git commit`. The owner runs the signed commit themselves.

════════════════════════════════════════════════════════════════════
## 7. Report format (final message)
════════════════════════════════════════════════════════════════════

For each matrix item B0–B5: PASS/FAIL + the actual command output (verbatim,
trimmed). Deployment record: binary sha256s (dev + both bench hosts), old
binary backups kept, config diff made on .58. Task C diff summary. Commit
message path. Any deviations from this brief. Anything left unverified and
why. Rollback instructions (one paragraph: restore .bak binaries, revert
server.toml, restart per B0).
