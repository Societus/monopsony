# Phase 3 grounding — update diffing: expected warnings

**Source of truth:** `docs/design/shadow-packages.md` §3 invariant #3 — updates must
*diff requirements across versions* and warn when a change requires a recipe edit or
caused a client to fail/rollback. **A silent no-op update is a defect.** Reuse the
existing health-check machinery for the failure signal (journalctl capture on
health_check failure already exists in the executor, T3.5).

Fixtures: `app-v1.0.0.toml` (baseline) → `app-v2.0.0.toml` (churn), and
`app-v1.0.0.toml` → `app-v1.0.1.toml` (negative case).

## What is diffable at recipe level (schema fields that exist today)

`install.service`, hooks (`pre_update` / `post_update` / `on_failure`),
`install.config_paths`, `install.health_check`, `build.requires`, `build.steps`,
`script.update`, `[version]` detect pair. Env vars and flags are NOT first-class
schema — they live inside step/hook strings and the rendered service script
(Phase 2 output). The diff engine therefore has two inputs:

1. **Recipe-struct diff** — field-level, mechanical, warning-classifiable.
2. **Rendered-service-script diff** (Phase 2 units) — where `ExecStart` flags and
   `Environment=` changes surface. Fixture step 5 below is only detectable there.

## Expected warnings: v1.0.0 → v2.0.0 (exactly these, order not load-bearing)

| # | Field changed | v1.0.0 | v2.0.0 | Required warning |
|---|---|---|---|---|
| 1 | `install.service` (and hooks) | `app.service` | `appd.service` | service unit renamed — recipe edit required; hooks referencing the old unit shipped with the OLD recipe will no-op |
| 2 | `build.requires` | gcc, make | gcc, make, **openssl** | new build dependency `openssl` — which-checked; build aborts on hosts lacking it |
| 3 | `build.steps` | 3 steps | 5 steps (**env file**, **quadlet**) | new provisioning requirements: required env `APP_TOKEN` (operator must supply; update cannot complete silently) + sidecar container `app-metrics` (quadlet, systemd hosts only) |
| 4 | `install.health_check.port` | 8080 | 8443 | health check moved — a v1-era port check proves nothing on v2; flag so operators update external monitors |
| 5 | rendered unit (ExecStart) | plain http | TLS via env | new required flag/env in the service definition — recipe edit required (detected in the Phase 2 rendered-script diff, not the struct diff) |

## Negative case: v1.0.0 → v1.0.1 — ZERO warnings

Patch release, byte-identical requirements. Warning here is noise that trains
operators to ignore real warnings; this pair is the regression gate.

## Failure-signal coupling (reuse, don't reinvent)

When a v1→v2 update fails `health_check`, the warning must name the requirement diff
that most plausibly caused it (e.g. "failed health check on 8443; this update added
required env APP_TOKEN and moved the check port") — the executor already captures
journalctl on health failure (T3.5, `shadow_executor.rs:257`); the diff engine feeds
its findings into that same outcome log rather than a parallel channel.
