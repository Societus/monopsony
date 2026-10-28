# Sprint 31 Item 6 — Non-systemd service detection (OpenRC + container seam): Design

**Status:** design only — no code changes.
**Scope guard:** `docs/design/shadow-packages.md` §7 item 2 (recorded 2026-10-22): *"Init systems: systemd + OpenRC for v1; all others deferred. Each additional init system is its own small phase (new template + renderer), never a wholesale rewrite of the existing two."* Container persistence via quadlets is **Sprint 32 Phase 2** scope for systemd hosts (roadmap :436); this doc only carves the **detect-side seam** for containers.
**Fleet:** ct/1952 = glibc/systemd · ct/1904 = musl/Alpine/OpenRC (real authentik finding: registered with empty service).
**Baseline at dirty HEAD (I1–I5 uncommitted):** `cargo test -p monopsony-client` 480/0/0, control 26/0/0, clippy delta zero. This item must NOT land before I1–I5 is signed+committed.

---

## 1. Current-state map — every systemd-specific assumption in `crates/monopsony-client/src/detect.rs`

| # | Anchor | What it assumes | Non-systemd consequence |
|---|--------|-----------------|-------------------------|
| A1 | `ScanPaths::system()` :32–54 | `system_units` = `/etc/systemd/system`, `/usr/lib/systemd/system` (retained only if dirs exist); `init_d` = `/etc/init.d` if it exists | On Alpine, `/etc/init.d` exists (OpenRC scripts), so the SysV fallback is armed everywhere — see A5. No concept of runlevels/enablement. |
| A2 | `ServiceMatch` struct :116–127; doc comment :121–122 says methods are `"execstart-path"`, `"execstart-basename"`, `"proc-exe"`, `"sysv"` | Method universe is systemd-centric. **Doc comment is stale:** actual emitted string is `"execstart"` (:461), not the two split variants the comment claims. | Adding `"openrc"` (and a container marker) widens an enum-as-string that callers treat opaquely — safe, but the comment must be fixed in the same change. |
| A3 | Tier 1 `match_service` :412–468 — scans `.service` files for `ExecStart=`, path/basename match, emits `method="execstart"` :461 | Service = a systemd unit file. | Silently no-ops on OpenRC (correct behavior — no false positives), then falls through to worse tiers. |
| A4 | Tier 2 `/proc/*/exe` :470–503 — emits `ServiceMatch` with `unit_name = cgroup_unit(...).unwrap_or_default()` :493–500, `method="proc-exe"`, `unit_path="/proc/<pid>/exe"` | A running process *is* a systemd unit; `cgroup_unit` :544–557 only extracts names after `system.slice/`. | On Alpine/LXC the cgroup line has no `system.slice/` (e.g. `0::/init.scope` or the lxc payload path) → `unit_name` is **empty string**, `confidence: Medium`. This is the authentik bug: a `ServiceMatch` is *claimed* with an empty name, tier 3 never runs, and the register row shows `service = ""`. Worse: `render_draft_toml` :853 then emits `# pre_update = "systemctl stop "` (empty unit) — a dangerous commented hint. |
| A5 | Tier 3 SysV fallback :505–537 — greps `/etc/init.d/*` for path/basename, emits `method="sysv"`, `unit_name` = script filename :525–534 | Any script in `/etc/init.d` is a SysV init script. | **On Alpine `/etc/init.d/*` ARE OpenRC scripts.** The fallback "accidentally half-works": it finds e.g. `sshd`, labels it `"sysv"`, and the renderer then proposes `systemctl` commands that don't exist on the host. The roadmap premise ("SysV scan exists but unexercised") is **half-stale**: it is exercised on Alpine today — it just mislabels and mis-renders. No shebang check, no `rc-update` enablement check, no `rc-service` awareness. |
| A6 | T1.4 extraction :685–695 — reads `svc.unit_path` as text, runs `extract_execstart_args` / `extract_unit_user` | `unit_path` is a systemd unit file with `ExecStart=`/`User=` keys. | For `proc-exe`, `unit_path` is `/proc/<pid>/exe` (a symlink; read fails → defaults). For SysV/OpenRC it reads the init script but finds no `ExecStart=` → `exec_args`/`unit_user` come back empty. Silent systemd-only data loss; OpenRC scripts use `command=`/`command_args=`/`command_user=`. |
| A7 | `cgroup_unit` :544–557 | Cgroup v2 unified layout with `system.slice/<unit>` | No container marker: a binary inside a podman/docker container shows cgroup `.../libpod-<id>.scope` or `.../docker-<id>.scope`; on Alpine-LXC hosts the guest has no slice at all. The scope path is *right there* in `/proc/<pid>/cgroup` — this is the cheap container-detection seam (§3). |
| A8 | `render_draft_toml` :853–874 — unconditional `systemctl stop/start {unit}` hint comments :861–869 | Matched service is a systemd unit; `systemctl` exists on the host. | Emits `rc-service`-incompatible and empty-unit (`systemctl stop `) hints. Also :855 emits the raw method string into the draft comment (fine), so the renderer already *has* the info it needs to be init-aware. |
| A9 | `discover_configs` signature :572+ consumes `Option<&ServiceMatch>` for conventional-path seeding | Same systemd framing, but it only reads `unit_name`/`unit_path` shape — low risk, verify when touching. |

**Roadmap premise check (explicit):** the sprint entry (roadmap :414) says "SysV scan exists but unexercised". Correction: the scan **is exercised on Alpine** (ct/1904 ships `/etc/init.d/*` OpenRC scripts) — it just (a) mislabels OpenRC scripts as `"sysv"`, (b) never checks enablement, and (c) gets short-circuited by tier 2's empty-name `proc-exe` match for running processes. The rest of the premise (no OpenRC logic, authentik empty-service row) is accurate.

---

## 2. OpenRC design (small phase — no rewrite of tiers 1–3)

### 2.1 New method: `"openrc"`

Insert a new tier **2.5 in `match_service`**, between `/proc` (:503) and the SysV fallback (:505). Rationale: if the binary is running, tier 2 wins (it's the truth); OpenRC script scan is next-cheapest; SysV fallback becomes last-resort only. Order stays: unit files → running process → **OpenRC scripts** → SysV scripts → None.

### 2.2 Distinguishing OpenRC from SysV scripts (no subprocess in the hot path)

For each `/etc/init.d/<name>` script that already matched by path/basename (reuse the A5 grep, don't duplicate it):

1. **Shebang check** (primary, read from the first line already in memory):
   - `#!/sbin/openrc-run` or `#!/usr/sbin/openrc-run` → OpenRC.
   - `#!/bin/sh` / `#!/bin/bash` with no `. /etc/init.d/functions` or `#!/sbin/runscript` → SysV. (`#!/sbin/runscript` is legacy Gentoo/OpenRC — classify OpenRC too.)
   - Unknown/no shebang → treat as SysV (today's behavior; never *lose* a match the current code finds).
2. **OpenRC dependency line** (tiebreaker, same read): presence of `depend() {` is a strong OpenRC signature and is what real-world init scripts (sshd, nginx on Alpine) always carry.

Decision rule: `shebang contains "openrc-run"` **OR** `shebang is runscript` **OR** (`depend() {` present AND `/sbin/openrc-run` exists on host) → `method="openrc"`; else `method="sysv"` (unchanged).

### 2.3 Enablement via `rc-update show`

A found script is only worth registering if it's actually in a runlevel. Design:

- Check the **filesystem first, no subprocess**: `/etc/runlevels/<rl>/<name>` symlinks (boot, default, sysinit, shutdown…). If any exists → `Confidence::Medium` → present in draft as live.
- If no runlevel symlink exists → still report, but with a draft comment `# openrc script present but not in any runlevel (rc-update add <name> default to enable)` and confidence downgraded to `Guess`. Never `rc_update`-shell-out during detection (keeps detection pure-read, testable, and root-optional).
- `rc-service <name> status` is **not** used at detect time (subprocess + needs the OpenRC daemon up); it belongs to health_check/executor in Sprint 32.

### 2.4 `unit_name` format

**Recommendation: bare service name, no suffix** — e.g. `sshd`, `nginx`. OpenRC service names are already suffix-free (`rc-service sshd restart`); `.service` is a systemd-only suffix. `unit_path` carries `/etc/init.d/sshd` exactly as the SysV path does today (:530), so the full-file information is preserved and T1.4-style parsing (:686–695) can later learn `command=` keys without re-scanning.

Field reuse (no struct change): `unit_name = "sshd"`, `unit_path = "/etc/init.d/sshd"`, `method = "openrc"`, `confidence = Medium|Guess` (per 2.3), `pid = None`.

---

## 3. Container story (detect-side seam only — quadlet is Sprint 32 P2)

### 3.1 What is feasible client-side, ranked by cost

| Feasible? | Signal | Where | Cost |
|-----------|--------|-------|------|
| ✅ Cheap | Binary runs inside a container: `/proc/<pid>/cgroup` path contains `libpod-`/`docker-`/`containerd-`/`crio-` + `.scope`, or `/proc/<pid>/cgroup` has no `system.slice/` **and** `/.dockerenv` exists / `/run/.containerenv` exists | Already read in tier 2 (`cgroup_unit` :544–557 reads this exact file) | ~zero — one extra string check |
| ✅ Cheap | Which engine: `/run/.containerenv` → podman; `/.dockerenv` → docker | stat two files | ~zero |
| ⚠️ Medium | *Host-side* quadlet that wraps the binary: `/etc/containers/systemd/*.kube\|.container` containing the image/name — only reachable when the client runs on the container host, not inside | new dir scan | new scan path + parsing |
| ⚠️ Medium | *Compose* unit wrapping it: host-side `.service` unit with `ExecStart=/usr/bin/podman ... compose ...` or `docker compose` | tier 1 already reads ExecStart lines; add a marker match | parse existing data |
| ❌ OUT of reach | A service unit *inside* the container (the container's own OpenRC/systemd service for the binary) — requires executing into the container (`podman exec <ctr> rc-service ...`), root, and engine coupling | — | deferred |
| ❌ OUT of reach | Auto-deriving the container *name* that a quadlet will later manage | needs engine socket | Sprint 32 P2 |

### 3.2 What registers instead (the authentik case)

Today (A4): tier 2 claims `ServiceMatch` with `unit_name: ""`, `method: "proc-exe"`, `confidence: Medium` → register row `service = ""`.

Proposed seam, still inside the existing `ServiceMatch` struct:

- **Fix the false claim:** in tier 2, if `cgroup_unit` returns `None` **and** the container-cgroup heuristics (3.1 row 1) match → emit `method = "container"`, `unit_name` = best-effort container id/name (short container id from the cgroup scope name, e.g. `libpod-<12 hex>`) and `confidence = Low` → represent as `Confidence::Guess` (the existing enum has no Low; Guess is the honest floor). If the process is running but *neither* a unit nor a container marker is found → **do not return a ServiceMatch at all** (fix the empty-name claim; the current `unwrap_or_default()` :495 is the actual bug behind the authentik row).
- **Draft rendering:** renderer (§4) emits for `method = "container"`:
  ```
  service = "libpod-9f3c…" # confidence: guess (matched via container)
  # pre_update  = "podman restart 9f3c…"    # container-managed: verify engine + wrapper
  # post_update = "<engine start equivalent>"
  # container <id>: managed by a host-side unit (quadlet/compose) — Sprint 32 P2 will
  # discover the wrapper; until then update hooks above are placeholders.
  ```
- **Register row:** keep `service` populated with the container id marker (never empty string again) — the DB gets *something* greppable, and the method string distinguishes it from a real unit. No schema change.

Out of scope here by §7: host-side quadlet/compose discovery, mapping container → wrapper unit, `podman`-aware hooks. The only contract this item commits to is: **`method = "container"` + best-effort id in `unit_name` + honest Guess confidence + no empty-name claims**.

---

## 4. Renderer fix — init-aware pre/post_update hints

Single switch in `render_draft_toml` (:853–874) on `svc.method`:

| method | pre_update comment | post_update comment |
|--------|--------------------|---------------------|
| `"execstart"` (systemd, incl. cgroup-derived) | `systemctl stop {unit}` (unchanged, :861) | `systemctl start {unit}` (unchanged, :866) |
| `"openrc"` | `rc-service {unit} stop` | `rc-service {unit} start` |
| `"sysv"` | `/etc/init.d/{unit} stop` | `/etc/init.d/{unit} start` — with comment `# SysV: no supervisor-managed restart; verify manually` |
| `"container"` | `podman restart {id}` placeholder per §3.2 | same |
| other/unknown | `# pre_update = "<unknown init: no template for method '{method}'>"` | same shape |

**Unknown-init communication:** the `# (matched via {method})` suffix already emitted at :857–859 *is* the machine-readable channel — the draft parser and the operator both see the method. Add nothing new. The safety property this item guarantees: **the renderer never emits a `systemctl` hint for a non-`execstart` method** (today it can emit `systemctl stop ` with an empty unit — A4/A8).

---

## 5. Test plan (fixture-based, no root) + fleet verification

### Unit/integration (all via existing `ScanPaths` injection — :19–54 is already test-shaped; tests at :1012+ show the pattern)

1. `openrc_shebang_classified`: fixture `/etc/init.d/foo` (tempdir) with `#!/sbin/openrc-run` + `command="/opt/bin/foo"` + `depend() {` → expect `method="openrc"`, `unit_name="foo"`.
2. `sysv_script_still_sysv`: same dir, `#!/bin/sh` + `. /etc/init.d/functions` + `DAEMON=/opt/bin/foo` → `method="sysv"` (no regression).
3. `openrc_runlevel_enablement`: add tempdir `/etc/runlevels/default/foo` symlink → `confidence Medium` + no "not in any runlevel" comment; absent → `Guess` + comment present.
4. `proc_match_without_unit_does_not_claim_service`: `proc_dir` fixture with a fake pid whose `cgroup` has no `system.slice/` and no container markers → `match_service` returns `None` (the authentik-bug regression test).
5. `container_cgroup_marked`: fake pid cgroup `0::/system.slice/libpod-abc123….scope` (or `docker-…`) → `method="container"`, `unit_name` contains short id.
6. Renderer table-driven: for each method → assert the emitted pre/post comment strings (guards A8 forever).
7. Golden draft: `render_draft_toml` on an OpenRC-matched report → no `systemctl` substring anywhere in output.
8. Stale doc comment fix (:121–122) + new methods added to the doc list.

No test requires `rc-service`/`rc-update` binaries or root — enablement is read from `/etc/runlevels` (injectable via a new `ScanPaths.runlevels: Vec<PathBuf>` field, default `[("/etc/runlevels")]` retained-if-dir).

### Fleet verification — ct/1904 (Alpine/OpenRC, musl)

1. Ship build → `ssh ct1904` → run `monopsony-client detect --path <authentik-binary>` (or the actual recipe path) → expect: either `openrc` match on the real init script or `container`/None per the authentik layout — **never empty `service`**.
2. `detect --path /usr/sbin/sshd` → expect `method="openrc"`, `unit_name="sshd"`, `# pre_update = "rc-service sshd stop"`.
3. Negative: a non-service binary (e.g. `/usr/bin/ls`) → no service claim, no hints.
4. ct/1952 (systemd) re-run sonarr detect → byte-identical draft to pre-change (regression: systemd path untouched).
5. Register both recipes from drafts; confirm catalog rows show non-empty service + correct method labels.

---

## 6. OPEN QUESTIONS (each with a recommendation — operator arbitrates)

1. **`unit_name` format for OpenRC** — bare name (`sshd`) vs. keep filename (`foo` is already suffix-free) vs. path-qualified. → **Recommend bare name** (§2.4); it round-trips into `rc-service`/`rc-update` commands verbatim, and `unit_path` preserves the file location.
2. **Do unenabled OpenRC scripts (no runlevel symlink) count as matches?** Register with `Guess` + comment vs. skip entirely. → **Recommend register-as-Guess**: the operator sees the script and decides; skipping hides real files (matches how nonexistent conventional config paths still surface as suggestion comments today).
3. **Container method marker naming** — `"container"` vs. `"podman"`/`"docker"` split vs. `"proc-cgroup"`. → **Recommend `"container"`** with the engine discoverable from `unit_path` (e.g. `podman:<short-id>`), one method string, no schema churn; per-engine splitting is trivially addable later and is renderer-level.
4. **Fix tier-2 empty-name claims now or with the OpenRC change?** The `unwrap_or_default()` (:495) empty-`ServiceMatch` is a standalone bug fixable in one line. → **Recommend same PR**: it's the authentik root cause and the OpenRC tier's ordering depends on it.
5. **Enablement check source** — filesystem `/etc/runlevels/*` scan vs. shelling to `rc-update show`. → **Recommend filesystem scan** (pure-read, injectable, no root, no OpenRC-on-CI requirement); `rc-update` belongs to executor/health-check territory (Sprint 32).
6. **SysV mislabel on Alpine (A5) — reclassify existing `"sysv"` matches as `"openrc"` when shebang says so, even on hosts where systemd also exists?** → **Recommend yes**, shebang is authoritative; systemd+`/etc/init.d` coexistence (Gentoo-era, rare on this fleet) is handled by tier order (systemd units win first).
7. **Is `unit_name` for containers allowed to hold a non-service id (container short-id)?** It bends the field's semantic. → **Recommend yes for now**, documented in the doc comment; Sprint 32 P2 will replace it with the wrapper unit name once quadlet discovery exists.
8. **Should `render_draft_toml` emit hints for `"sysv"` at all**, given no supervisor restart semantics? → **Recommend emit with explicit "verify manually" comment** (§4) — better than silence, still honest.

---

## 7. Estimated size & 'small phase' fit

| Piece | Est. |
|-------|------|
| Tier 2 container cgroup markers + no-empty-claim fix (detect.rs :470–503, :544–557) | ~30 LOC + tests |
| OpenRC tier (new match_service section) + runlevel scan field | ~60 LOC + tests |
| SysV/OpenRC shebang classifier | ~25 LOC (pure fn, heavily tested) |
| Renderer switch (:853–874) | ~20 LOC + table test |
| Doc comment fixes (:116–127) | trivial |
| **Total implementation** | **~135 LOC production + ~150 LOC tests — one PR, one crate (`monopsony-client`), zero new dependencies, zero schema/proto changes** |

**Fits the §7 discipline:** it adds one init system (OpenRC) and one marker (container) to the *existing* tier pipeline — no new template, no renderer rewrite (the renderer change is a 4-arm match on data the report already carries), systemd path untouched (ct/1952 byte-identical regression test is the guard). The genuinely deferred work (host-side quadlet discovery, `command=` parsing for OpenRC `exec_args`/`unit_user` equivalents, rc-service health checks) is explicitly punted to Sprint 32 Phase 2 and listed as such.
