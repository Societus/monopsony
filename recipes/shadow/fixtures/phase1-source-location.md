# Phase 1 contract — source-location awareness (`--source` / pull-if-missing)

**Source of truth:** `docs/design/shadow-packages.md` §3 invariant #1 (provisioning is
first-class) and §7 scope lock #1 (minimal subset, extend backwards-compatibly).

> A recipe's `[build].steps` can run templated install steps that use `--source` for any
> file that must exist and *pull it from the source location if missing* (e.g. find
> `.env` / `docker-compose.yml`, download from `<repo>` when absent).

This capability does **not exist in code yet** (`shadow_template.rs` renders variables +
conditionals only; nothing knows about `--source`). This document is the contract its
implementer must satisfy; the grounding fixture is
`phase1-provisioning-steps.toml` (same directory).

## Status of related code (verified 2026-10-28 at HEAD `45b3305`)

- `crates/monopsony-common/src/shadow_template.rs` — `render_step()` + `StepContext`
  exist, 18 unit tests green, but the module is **orphaned**: no call site outside its
  own tests and the `lib.rs` re-export. Wiring it into step execution is Phase 1 work.
- `crates/monopsony-client/src/shadow_executor.rs` — `execute_inner()` rejects
  `InstallMethod::Build` outright ("does not support git_release yet"), so `[build]`
  steps never run on the client today.
- **Brace-style conflict (resolve before wiring):** build steps in the wild use
  single-brace `{target_dir}` (see `recipes/shadow/i2pd.toml`, matching the
  asset-template style of `render_asset_template`), while the Phase 1 engine is
  double-brace `{{target_dir}}`. The executor must either translate the legacy single
  form for known names or migrate the i2pd fixture; silent dual support in the same
  step is not acceptable (a literal `{version}` in a URL would be eaten by the wrong
  engine). Decision recorded in the sprint-32 grounding handoff.

## Author-facing syntax

The flag appears inside a step string, before the file path it guards:

```
--source <path>            # operator-managed: use as-is if present
--source <path> <fallback> # pull-if-missing: fetch <fallback> when absent
```

`<fallback>` is a URL (raw file from the recipe's source repo) or a repo-relative path
resolvable against `package.source_url`. Steps stay plain strings in the schema — no new
TOML types; this is deliberately the same minimal-subset discipline as the templating
engine (§7 scope lock #1).

## Runtime contract

1. **Exists → use as-is.** The executor checks the path before running the step; when
   present, the step runs unchanged. An operator-tuned `.env` is never overwritten.
2. **Missing → pull, then run.** The executor fetches `<fallback>` to `<path>` first.
   Fetch failure aborts the step with a clear error naming the file and source — never
   a silent empty-file substitute.
3. **`config_paths` still wins.** Any path listed in `install.config_paths` is
   operator-managed: `--source` may seed it on first install (when absent) but an
   update must never overwrite it. This is the executor's standing invariant
   ("config_paths are NEVER touched") extended to provisioning.
4. **Idempotent re-runs.** Pull-if-missing twice leaves the second run a no-op (file
   now exists); verify with a run-twice test.
5. **Render order.** Templates render first (`render_step`), then `--source` resolution
   sees the final literal paths — a `{{target_dir}}/.env` guard must work.

## Grounding examples (the two cases the design doc names)

```toml
[build]
steps = [
    # .env: operator-managed after first seed; pulled from the repo only if missing.
    "monopsony-provision --source {{target_dir}}/.env https://raw.githubusercontent.com/example/app/v{{version}}/.env.example",
    # compose file: same contract, operator edits after seed are preserved.
    "monopsony-provision --source /etc/example-app/docker-compose.yml https://raw.githubusercontent.com/example/app/v{{version}}/deploy/docker-compose.yml",
]
```

The `monopsony-provision` helper name is illustrative — the implementer may resolve
`--source` inside the step runner instead of a shipped binary; what is load-bearing is
the exists→use / missing→pull / never-overwrite-operator-edits behavior, tested for
both the `.env` and compose-file shapes above.
