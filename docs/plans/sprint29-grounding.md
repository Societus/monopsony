# Sprint 29 grounding — Centralized Updates & OS Release Readiness

**Plan:** `docs/plans/2026-10-16-centralized-updates-and-os-release-readiness.md` (read VERBATIM
before implementing each phase — never from a summary).
**Roadmap:** `docs/sprint-roadmap.md` §"Sprint 29 — PLAN only, draft 2026-10-16".
**Status at grounding:** greenfield — prior-art audit found ZERO occurrences of
`UpdateReport` / `OsReleaseUpgrade` / `GuidedRecipe` / `readiness` / `upgradable` in
`crates/`, `proto/`, or `dashboard/` (verified 2026-10-29 at HEAD `c27c79b`).
**Trust boundary (plan §Proven-failure + §Trust model, load-bearing):** server renders
instructions, NEVER executes; advisory-only through Phase 4; no auto-push. Vendor tooling
does readiness — we parse WARN/FAIL, never re-implement cluster/space detection.

## Repo state + baselines (recorded 2026-10-29, HEAD `c27c79b`, tree clean)

Branch `master`, 1 ahead / 0 behind origin (c27c79b = Sprint 25 S2, signed, unpushed —
do not touch). Untracked leftovers (`make-*.sh`, `packages/`, `notes/`, `tmp/`,
`.git-commit-message.tmp`) are spent artifacts — never stage them.

Aggregated baselines (`grep '^test result' | awk` sum, crate scope):

| Crate | passed/failed/ignored |
|---|---|
| monopsony-common | 190/0/0 |
| monopsony-db | 0/0/0 (suite requires live PG; count is real) |
| monopsony-proxy | 227/0/2 |
| monopsony-control | 26/0/0 |
| monopsony-mirror | 44/0/0 |
| monopsony-shadow | 50/0/0 |
| monopsony-server | 79/0/0 |
| monopsony-client | 514/0/0 |
| monopsony-tracker | 50/0/0 |

Workspace total ≈ 1180/0/2. Re-run and reconcile before declaring green; record SCOPE
with every count.

## Source anchors (verified this session)

| Anchor | Location |
|---|---|
| Backend trait (add `list_upgradable` near) | `crates/monopsony-common/src/package_manager/mod.rs:306` (`async fn list_installed`) |
| Client heartbeat send | `crates/monopsony-client/src/grpc_client.rs:554` (`heartbeat`), `:675` (`report_inventory`) |
| gRPC server trait impl / shadow update RPCs (pattern reference) | `crates/monopsony-control/src/grpc.rs:1231` (`check_shadow_updates`), `:1297` (`apply_shadow_update`) |
| Dashboard REST client list / inventory | `crates/monopsony-admin/src/clients.rs:13,39,65` |
| Dashboard frontend components | `dashboard/src/lib/components/` (SvelteKit 2 + runes; a11y warnings fixed proactively) |
| Runtime-editable settings pattern | `server_settings` table, `db.rs` `get_setting`/`set_setting` (skill §Runtime-Editable Settings) |
| Proto (single file, tonic-build) | `proto/monopsony.proto` — PROTOC may need `~/.local/bin/python-grpc-tools-protoc` (see AGENTS.md) |

## Resolved decisions (change only with a recorded reason)

- **D1 — persistence:** update counts live as new columns on the `clients` table via an
  append-only migration (per-client, queried on every dashboard list). NOT server_settings
  (that is for global flags, not per-client rows).
- **D2 — release catalog:** the curated `(distro → available majors)` map is a
  `server_settings` key `os_release_catalog` (JSONB, runtime-editable, dashboard-editable
  later) with a code-default seed for debian/ubuntu. Server merges client-reported
  distro/version against it; unknown majors never adopt.
- **D3 — delivery channel:** `UpdateReport` rides the periodic heartbeat as an optional
  field (counts change without a full inventory rescan). Client computes counts at
  inventory time (or on a local TTL) — `apt list --upgradable` is slow; do not run it
  every heartbeat tick. Cache client-side, attach when fresh.
- **D4 — backend coverage:** trait method with a default "unsupported" answer; real
  `upgradable` parsing for apt/dnf/pacman/apk first-class (fleet-verifiable: ct/1952 =
  Debian/apt, ct/1904 = Alpine/apk); zypper/xbps/emerge best-effort same pattern;
  language PMs + winget/choco/scoop/brew report `unsupported` with a note this phase.
  Security-vs-other split: distro security channel where the PM exposes one
  (apt: `apt-get -s upgrade` + security origin check or `-security` suite membership),
  else all counts land in "other" with a note.
- **D5 — preflight matrix (Phase 3):** Debian → no vendor dry-run tool in base
  (`do-release-upgrade` is Ubuntu-only): report `ready` + explicit note. Ubuntu →
  `do-release-upgrade --dry-run` parse. PVE → `pve8to9 --parse`/output parse
  (WARN→review, FAIL→not-ready). Absence of a tool NEVER blocks — report ready + note.
- **D6 — guided recipes (Phase 4):** authored per distro as data (TOML reusing the
  shadow `RecipeScript` parser, display-only repurpose per plan decision #4); rendered
  server-side into `GuidedRecipe` steps; variables filled from the client's verdict.
  No execution path may exist — dashboard renders text only.

## Batch plan (serial — one implementer at a time; shared files: proto, grpc.rs, admin/clients.rs, dashboard)

- **B1 = Plan Phase 1 + Phase 2 (vertical slice):** proto messages
  (`UpdateReport`, `OsReleaseUpgrade`) + heartbeat wiring; client distro/version detect +
  `list_upgradable` per D4; server persist (D1) + release catalog (D2) + merge;
  admin REST exposes counts/readiness on client rows; dashboard renders
  "X security / Y other" + release row. Gates: workspace check (trait + proto are
  shared) + per-crate suites + clippy clean on edited code.
- **B2 = Plan Phase 3 (preflight parsing):** D5 matrix, readiness verdict + notes,
  dashboard annotations. Ubuntu parse tested against REAL captured output fixture.
- **B3 = Plan Phase 4 (guided recipes):** D6 authored recipes + renderer + dashboard
  checklist. Verification: rendered guide matches safe ordering for a PVE + Ubuntu
  example; grep-proof that no server-side execution path exists.

## Standing constraints (every batch)

NO git commit/push/fmt by subagents — work lands uncommitted; dispatcher verifies gates
itself, then builds ONE staging script at the end (sign-flow: operator signs). Plain
terminal commands; scripts via `write_file` + run by path; no heredocs in foreground.
Shared-struct changes (common schema, proto) gate on `cargo test --workspace`.
a11y fixes proactive in Svelte changes. LLM-disclosure note in eventual commit bodies
per public-content convention.

## Fleet verification (dispatcher-only — never subagents) — AFTER B1–B3 land

Deploy: rebuild both binaries, `scp` → `sudo install -m 755` → `sudo systemctl restart`
on `monopsony@10.10.20.58` (passwordless sudo; dashboard admin/testpass123 :3130);
client into ct/1952 (Debian/systemd, Sonarr — never force-fail, never restart without
approval) and ct/1904 (Alpine/OpenRC, authentik podman). Verify by md5, never `--version`.

- **S29 exit:** counts correct on apt + apk hosts; dashboard rows render; heartbeats
  carry UpdateReport (server log/wire check); Debian release row appears for ct/1952;
  synthetic unsupported major does not; preflight notes present; rendered guide displays
  with safe ordering; zero server-side execution paths.
- **S32 exit (same deploy pass):** provisioning steps pull-if-missing + render on the
  fleet; systemd unit (ct/1952) + OpenRC script (ct/1904) deploy with location
  substitution; update adding a required flag/container surfaces a warning, not a silent
  no-op; `downgrade_unsafe` blocks a non-interactive dist-upgrade attempt (synthetic
  unit on ct/1952 — NOT Sonarr).
- Clean up all bench seeds at session end (fleet-bench-verification rules).
