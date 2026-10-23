# Handoff — Shadow Register Pipeline implementation (Sprint 30)

**Date:** 2026-10-21 · **Executor:** subagent dispatch (subagent-driven-development workflow) · **Dispatcher:** Hermes session
**Source of truth:** `docs/plans/2026-10-21-shadow-register-pipeline.md` (RESOLVED — read it first; this handoff adds execution grounding, not new design)

## Mission

Implement the four resolved phases of the shadow register pipeline: P0 renderer correctness,
P1 detection depth, P2 Register↔Detect wiring, P3 health monitoring + rollback policy,
P4 server reach-out → similarity → peer deploy. Every task T-number below refers to that plan.

## Grounding (pre-dispatch, verified 2026-10-21)

### Repo state
- Branch `master`, **6 ahead / 0 behind** `origin/master`. Subagents operate ahead of origin;
  this is expected.
- Working tree carries **uncommitted pairing-v2 work** (staged session ledger: admin/pairing.rs,
  client main.rs + grpc_client.rs + trust.rs + Cargo.toml, dashboard pairing UI,
  `crates/monopsony-client/src/shadow_executor.rs` is **untracked but active** — P3 edits it).
  **Do not revert, reformat, or "clean up" any of it. Do not `git add`/`git commit`/`git push` —
  the operator signs commits manually in a later pass.** Subagents report changed files; the
  dispatcher tracks them.
- `main.rs` (client) is 3,804 lines / ~150 KB — edit via small anchored patches only.

### Build/test baselines (recorded so regressions are attributable)
- `cargo clippy --workspace --all-targets`: **clean** (exit 0). Pre-existing noise, not yours:
  5 warnings in `monopsony-admin` (lib test target) + `sqlx-postgres 0.8.0` future-incompat note.
- `cargo test -p monopsony-client`: **405 passed / 0 failed / 0 ignored**.
- Gate for every phase: clippy clean on edited code, full `cargo test -p monopsony-client`
  green (and workspace test for P4), **no `cargo fmt`** (merge-time pass realigns).

### Source anchors (verified this session)
- Register arm: `crates/monopsony-client/src/main.rs:2884–2961` — minimal-template build at
  2916–2923; version detect call 2931–2937; gRPC call 2940–2955 (sends libc `""`,
  `config_paths=[]`, `service_name=""`, `mask_from_pm=false`).
- Detect arm (read-only preview): `main.rs:3224–3259` → `detect::detect()` +
  `detect::render_draft_toml()`.
- Renderer: `crates/monopsony-client/src/detect.rs:587–735`. Duplicated source-type chain
  610–615; name-confidence proxy 621–625; source_type always-"guess" 631; config gating
  673–703; `[version]` escaping 706–732. Dead `ConfigSource::UnitArg` at 86–87. Confidence
  enum 55–72. Tests live in the same file (`mod tests`, 737+).
- `detect_source_type()` canonical home: `crates/monopsony-client/src/shadow_cli.rs:8–17`;
  `PackageCommand` enum at 19–43 (`Register` 22, `Detect` 103).
- Version detection: `crates/monopsony-client/src/shadow_executor.rs:257–265`
  (`detect_current_version`); executor pipeline `execute_update` at 269+.
- Server: `shadow_packages` schema `crates/monopsony-db/src/db.rs:354–386` (no sha256 column);
  register handler `crates/monopsony-control/src/grpc.rs:1010`; recipes/scan/assignment tables
  db.rs:535–591.

### Known blackholes / out-of-scope traps
- `PackageCommand::Add` (main.rs:2962–2977) is a **stub that prints** — it is NOT in scope;
  do not "fix" it while nearby.
- `render_draft_toml` writes `REPLACE_ME` when no source — T0.4's bail policy is the handling;
  do not invent an upstream-guessing heuristic.
- **Blocks T4.5 (verify before P4 client work):** whether client-registered recipes are
  operator-signed server-side — read the `register_shadow_package` handler (grpc.rs:1010)
  and the recipe-signing path. Record the finding in the dispatch report either way.
- Dashboard surfacing of P3's provisional state may **trail** as a follow-up slice; the P3
  exit gate is CLI output only.

## Environment & workflow rules (binding on every subagent)

1. **No git commits, no pushes.** Operator signs manually. Report changed files.
2. **No `cargo fmt`.** Match surrounding style; the merge pass realigns.
3. **Terminal discipline:** single plain commands; no multi-line heredocs; no
   `setsid`/`nohup` in foreground terminal calls (guard blocks them — use
   `background=true` when a detached launch is genuinely needed). Write scripts via
   write_file, run by path.
4. **Live test target:** ct/1952 on hypervisor `ingest` — `ssh root@ingest "pct exec 1952 -- sh -c '<single line>'"`.
   Full pattern + pitfalls: skill `monopsony-client-onboarding`, reference
   `references/pitfalls-deploy-dns.md`. **Do not kill the running `monopsony-client` daemon**
   (freshly paired today) and **do not restart Sonarr's service without dispatcher approval**.
5. **Server side (P4):** test server 10.10.20.58, staging binary
   `/home/monopsony/monopsony-staging-server` (no systemd), dashboard admin/testpass123 :3130,
   `ssh monopsony@10.10.20.58`. Server restarts during P4 verification need dispatcher approval.
6. **Scope lock:** only files named in the phase spec (+ their tests). No drive-by refactors,
   no workspace-wide edits, no new dependencies without dispatcher approval.

## Execution order (serial — shared files; one implementer at a time)

| # | Batch | Plan tasks | Primary files | Exit gate |
|---|---|---|---|---|
| 1 | P0 renderer fixes | T0.1–T0.4 | detect.rs, shadow_cli.rs | golden draft test: shared source_type, fixed confidence tags, lifecycle suggestion comments, REPLACE_ME header |
| 2 | P1 detection depth | T1.1–T1.4 | detect.rs | graded-corpus unit tests (unit-arg / conventional / both / neither); sonarr-shaped fixture |
| 3 | P2 wiring | T2.1–T2.4 | main.rs | **live:** real register of ct/1952 Sonarr → service/config/version populated, version 4.0.16 non-blank |
| 4 | P3 health + policy | T3.1–T3.5 | monopsony-common shadow_types.rs, shadow_executor.rs, catalog | forced-failure on a **synthetic test unit** → rollback in window; `hold` unit → no restore + loud outcome; fake-systemctl injectable tests |
| 5 | P4a server | T4.1–T4.2 | proto (common), db.rs migration, grpc.rs | sha256 round-trips; QueryShadowRegistry returns seeded matches |
| 6 | P4b client | T4.3–T4.7 | main.rs, detect.rs, new scorer module | golden scorer fixtures (exact/near/no-match); repo-change gate unit test; live second-client adopt |

**Dependencies:** 1→2 (same file) → 3; 4 independent of 2/3 but before 5 (schema fields land
first); 5→6. Batches 1–3 may be one implementer each or combined 1+2; never parallel.

## Per-task review protocol (from subagent-driven-development)

For every batch: implementer → **spec-compliance reviewer** (against the plan task text,
verbatim) → **code-quality reviewer** → fix loop → then next batch. After all batches: one
final integration reviewer (consistency, full test suite, clippy). Reviewers get the plan
task text + file list, not the whole plan.

## Live-verification protocol

- P2: dispatcher (not subagent) runs the Sonarr register on ct/1952 after the implementer
  reports green — subagents stage, dispatcher executes live steps.
- P3: synthetic unit (e.g. `/etc/systemd/system/mono-health-test.service` running a sleep
  binary) on ct/1952; forced failure by pointing detect_command at a failing probe. Never
  force-fail Sonarr itself.
- P4: second paired client (any of .20/.62/.235/.242/.159 per roadmap test-env section) for
  the adopt path; server-side seeds via existing recipe admin routes.
- All live output pasted into the batch report; "should work" is not a gate.

## Report format per batch

Changed files (paths) · test counts before/after · clippy status · spec-review verdict ·
quality-review verdict · live-gate output (when applicable) · blackhole findings (T4.5
signing-path answer lands in batch 5's report).
