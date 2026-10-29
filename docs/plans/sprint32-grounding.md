# Sprint 32 grounding — declarative recipes, Phases 1–3

**Date:** 2026-10-28 · **Status:** GROUNDING COMPLETE — staged uncommitted, awaiting
sign-off (repo convention: operator signs every commit)
**Scope:** grounding data + fixtures + verification tests for Sprint 32 **Phases 1–3**
(provisioning engine, service scripts, update diffing). Phase 4 (downgrade gate) is NOT
in scope — its core (`is_downgrade` + the `downgrade_unsafe` refuse path) already
landed with Sprint 27/30 work in `shadow_executor.rs:309,678`.
**Roadmap:** `docs/sprint-roadmap.md` §"Sprint 32" · **North star:**
`docs/design/shadow-packages.md` (§3 invariants, §7 scope locks — read before
implementing; the three scope locks bound every phase here).

## Grounding (verified 2026-10-28 at HEAD `45b3305`)

### Repo state
- Branch `master`, **0 ahead / 0 behind** `origin/master`.
- Pre-existing untracked files (`packages/`, `tmp/`, `target-musl/`, `notes/`,
  `make-*-commits.sh`, `.git-commit-message.tmp`, `recipes/shadow/checkmk-server.toml`)
  — leave alone; they are not part of this work.
- No tracked changes were made. New files are listed under "Grounding-data index" —
  stage by explicit path only, **no commits** (user signs manually).

### Build/test baselines (recorded so regressions are attributable)
| Check | Baseline → after grounding data |
|---|---|
| `cargo test -p monopsony-common` | 167/0/0 → **167/0/0 lib + 5/0/0** `grounding_fixtures` (new integration target) |
| `cargo test -p monopsony-client` | 480/0/0 → unchanged (no client code touched) |
| `cargo clippy -p monopsony-common --all-targets` | clean → clean (0 warnings) |

## Source anchors (verified this session)

| What | Where | State |
|---|---|---|
| Template engine (`render_step`, `StepContext`, `TemplateError`) | `crates/monopsony-common/src/shadow_template.rs` | Complete, 18 unit tests green, **ORPHANED** — no call site outside its own tests + `lib.rs:22` re-export. Wiring it is Phase 1 work. |
| Service-script renderer (Phase 2 core — **ALREADY LANDED**) | `crates/monopsony-client/src/shadow_service.rs` (commit `77ee254`) | `detect_init_system` (os-release content, systemd default), `ServiceSpec{unit_name,description,exec_start,working_dir}`, `render_service_file`, `blank_service_file`; templates embedded via `include_str!` (`src/templates/monopsony.{service,openrc}.tmpl`). **Library-only** (`#![allow(dead_code)]`): executor/CLI wiring is the remaining Phase 2 work. Deliberately a SEPARATE renderer from the step engine — recipes run services, they don't build them; see D3. |
| Step execution | `crates/monopsony-client/src/shadow_executor.rs` `execute_inner()` | `InstallMethod::Build` rejected outright ("does not support git_release yet") — `[build]` steps never run client-side today. |
| Asset-template rendering (SINGLE-brace `{version}`) | `shadow_executor.rs:322` `render_asset_template` | Distinct engine from Phase 1's double-brace — see Open Decision D1. |
| Legacy single-brace in the wild | `recipes/shadow/i2pd.toml` (`{target_dir}` in build steps) | Must be translated or migrated when wiring. |
| Downgrade gate (Phase 4, out of scope) | `shadow_executor.rs:309` (`is_downgrade`), `:678` (refuse path) | Exists; tested. |
| Health-failure journalctl capture (Phase 3 reuse point) | `shadow_executor.rs:257` `capture_journalctl` | Exists (T3.5); diff findings feed the same outcome log. |
| os_family detection prior design | `docs/plans/2026-10-23-sprint31-item6-nonsystemd-detect-design.md` | Sprint 31 item 6 designed detection; Phase 2 implements the two v1 renderers only. |
| Real recipe fixtures (grounding corpus) | `recipes/shadow/{technitium-dns,i2pd,checkmk-server}.toml` | Real fleet apps: systemd / user-mode build / omd-script. |

## Grounding-data index (new files, all staged)

```
recipes/shadow/fixtures/
├── phase1-provisioning-steps.toml      # [build].steps exercising the Jinja subset;
│                                       # expected-render block VERIFIED by tests (2)
├── phase1-source-location.md           # --source / pull-if-missing contract (NEW
│                                       # capability; author syntax + runtime rules)
├── phase2-service-scripts/
│   ├── README.md                       # os_family→template mapping, substitution
│   │                                   # contract, persistence semantics, bench rule
│   ├── systemd/technitium-dns.service.template   # ground truth: real upstream shape
│   ├── systemd/example-app-user.service.template # i2pd user-mode precedent (+linger)
│   ├── openrc/example-app.openrc.template        # Alpine ct/1904 target
│   └── quadlet/example-app.container.template    # systemd-only container persistence
└── phase3-update-diffing/
    ├── app-v1.0.0.toml                 # baseline
    ├── app-v2.0.0.toml                 # churn: unit rename, +openssl dep, env file,
    │                                   # quadlet sidecar, health-check port move
    ├── app-v1.0.1.toml                 # NEGATIVE case: zero-churn patch release
    └── README.md                       # expected-warnings table + failure coupling
crates/monopsony-common/tests/grounding_fixtures.rs   # 5 tests: parse-as-ShadowRecipe,
                                                      # render-truth, churn+negative
docs/plans/sprint32-grounding.md                      # this document
```

## What each phase's grounding pins down

**Phase 1 — provisioning engine.** The fixture's expected-render block is now
machine-verified against the real engine (`phase1_expected_render_block_is_true_*`):
`{{var}}` substitution, `{{#if os/libc == "..."}}` inclusion AND off-platform drop,
author-composed tag prefix (`v{{version}}` — StepContext.version is bare semver).
The `--source` contract doc defines exists→use / missing→pull /
never-overwrite-operator-edits (config_paths invariant extends to provisioning) with
the two design-doc-named shapes (.env, docker-compose.yml).

**Phase 2 — service scripts.** Templates are grounded on real fleet shapes:
technitium's upstream unit (WorkingDirectory/ExecStart dotnet), i2pd's user-mode
layout (`%h`, `WantedBy=default.target`, linger caveat), standard Alpine openrc-run
(command/command_background/pidfile), quadlet with restart-policy mapping for the
design doc's on-demand / unless-stopped semantics. Same render engine + variable
vocabulary as Phase 1 (one substitution path, one code path).

**Phase 3 — update diffing.** The v1→v2 pair encodes five requirement changes through
REAL schema fields (unit rename in `install.service`, `openssl` in
`build.requires`, env-file + quadlet steps in `build.steps`, health port move), with
the expected-warnings table and the struct-vs-rendered-script diff split. The
v1.0.1 negative pair is machine-checked field-identical (`phase3_negative_case_*`) —
the zero-warnings regression gate. Failure coupling reuses `capture_journalctl` /
the existing outcome log, not a parallel channel.

## Landed slices (dispatcher-verified)

**Phase 1 — provisioning engine: LANDED 2026-10-28, verified.** Client 505/0/0
(+25), common 172/0/0 untouched, clippy clean (7 pre-existing only). Files:
`shadow_provision.rs` (new: legacy-brace translation, --source resolution,
step runner), `shadow_executor.rs` (Build pipeline: which-checked requires
stage before any host change, render→provision→execute per-step fail-fast,
git_release gated to method=build), `main.rs` (Build bypasses asset
resolution), `i2pd.toml` (double-brace migration). Spec-review PASS against
the phase-1 contract; decisions + gaps recorded below.

Phase 1 follow-up ledger (recorded, not scheduled):
- git_build source acquisition: the executor runs steps with inherited cwd;
  recipes whose steps assume a pre-populated source tree (i2pd) need a
  clone-at-tag stage before their first step. New recipes should self-clone
  (the phase-1 fixture pattern) until that stage lands.
- --source fallbacks: http(s) URLs only this slice; repo-relative fallbacks
  abort with "unsupported fallback form".
- resolve_asset_url still hard-errors for git_release; every caller must
  special-case Build (the update arm does).
- Tilde expansion covers `~` and `~/` only; `~user` stays literal (D2).

**Phase 2 — service scripts: LANDED 2026-10-28, verified.** Client 511/0/0
(+6), common 174/0/0 (lib 167→169, +2 exec_start parse/default tests;
grounding_fixtures untouched and green), workspace 1213/0/0, clippy: the same
7 pre-existing client warnings only (grpc_client x4, trust x1,
shadow_executor x2). Files: `shadow_types.rs` (optional `exec_start` on
`RecipeInstall`), `shadow_executor.rs` (`install_service_script`: detect init
via os-release content → render ServiceSpec → write to the init-specific path
→ enable; plan-stage gate: service set + build/script + no exec_start = hard
plan error; extract defaults exec_start to `target_dir/package.name`; never
starts the service — that stays post_update's job; OpenRC strips a trailing
`.service` from the script/rc-update name), `shadow.rs`/`shadow_signing.rs`
(struct-literal `exec_start: None`), `docs/design/shadow-packages.md` §5
install bullet. Hermetic seams: injectable install root (default `/etc`) +
os-release content override; enable runs through the existing generic
`run_systemd_cmd` seam. Written unit/init script is NOT rolled back
(idempotent overwrite re-render). Deferred per brief: quadlet generation,
user-mode units (D4), CLI blank-file arm, on-demand persistence knob.
Recorded deviation: the in-binary systemd template carries
`Restart=on-failure`, not the README's unless-stopped mapping of
`Restart=always` — persistence default still lands (Restart + `[Install]`
WantedBy=multi-user.target); changing the template is an operator-visible
decision, flagged not made.

Dispatcher verification 2026-10-28: tests re-run independently (client
511/0/0, common 174/0/0, grounding_fixtures green), clippy locations
confirmed = the 7 pre-existing only, `cargo check --workspace` clean (only
the pre-existing sqlx-postgres future-incompat note). Reporting correction:
the child's changed_files list omitted `crates/monopsony-client/src/shadow.rs`
and `crates/monopsony-shadow/src/shadow_signing.rs` — both carry a single
forced `exec_start: None` struct-literal addition from the schema field
(necessary fallout, verified minimal). Spec-review PASS.

**Phase 3 — update diffing: LANDED 2026-10-28, verified.** Workspace
1232/0/2 (+19: 16 shadow_diff + 3 executor; the 2 ignores are pre-existing),
common clippy 0, client clippy the same 7 pre-existing only. Files:
`shadow_diff.rs` (new, pure, in common: 9 diff classes, empty-on-identical
negative gate, sorted-compare for order-insensitive collections, ordered
compare for steps, `recipe_edit_required` true for all classes except
VersionDetect), `lib.rs` (mod + re-export), `shadow_executor.rs`
(`execute_update_with_diff` wrapper — warnings prepend to the same outcome
log; no signature churn), `main.rs` (update arm on the wrapper). Spec-review
PASS against the phase-3 README table (churn pair re-expresses as 6
warnings: service unit + 2 hooks + health + requires + steps; negative pair
silent). Recorded gap (per brief's fallback): the client has NO local
previously-registered-recipe store — Sprint 30 persistence is server-side —
so `previous = None` today and the update arm carries the gap comment;
switching is one line when an accessor lands. Rendered-script ExecStart
diff remains deferred (templates are binary-baked, not per-recipe).

## Open decisions for implementers (resolved HERE; change only with a recorded reason)

- **D1 — brace style.** Phase 1 engine is `{{var}}`; i2pd's shipped build steps use
  `{target_dir}`. Resolve by translating the legacy single-brace names at the
  executor boundary OR migrating the i2pd fixture. No silent dual support inside one
  step (a literal `{version}` in a URL would be eaten by the wrong engine).
- **D2 — tilde paths.** `~` in target_dir stays literal through rendering; expansion
  is executor-side (i2pd's documented "proposed" behavior). Fixtures encode this.
- **D3 — service rendering context (REVISED after tree audit).** The tree already
  landed a dedicated service renderer (`shadow_service.rs`, `ServiceSpec` with
  `unit_name`/`description`/`exec_start`/`working_dir`) instead of extending
  `StepContext` — recipes run services, they do not build them. Do **NOT** extend
  `StepContext`; wire the landed `ServiceSpec` renderer into the executor instead.
  The fixture `.template` files remain grounded examples of rendered output, not
  the in-binary defaults (those are `src/templates/monopsony.{service,openrc}.tmpl`).
- **D4 — user units.** Whether systemd user units are v1 or fast-follow is a scoping
  call; the template + linger caveat are in the grounding either way.
- **D5 — quadlet bench rule.** Validate the quadlet fixture on a live podman host
  (group-merging behavior confirmed, not assumed) before baking the template set
  into the client binary. Fleet-bench rules apply (`references/fleet-bench-verification.md`).

## Gates (unchanged from repo convention)

`cargo check -p <crate>` → `cargo test -p <crate>` → `cargo clippy -p <crate>
--all-targets`, zero NEW warnings; shared-struct changes gate on
`cargo test --workspace`. Live verification of rendered units happens on ct/1952
(systemd) and ct/1904 (Alpine/OpenRC) per the roadmap exit criteria — never force-fail
Sonarr (ct/1952), never restart without approval. **NO COMMITS** — stage by explicit
path; the operator signs.
