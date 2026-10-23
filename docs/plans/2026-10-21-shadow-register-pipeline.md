# Shadow Register Pipeline — detection depth, health monitoring, fleet reuse

**Date:** 2026-10-21 · **Status:** RESOLVED — ready for implementation handoff · **Owner:** martin
**Scope:** client register/detect pipeline + recipe schema/executor + (Phase 4) proto/server

## Context (all verified 2026-10-21, live on ct/1952 Sonarr)

- `Register` (main.rs:2884–2961) builds a minimal template when `--recipe` is omitted:
  no `[version]`, no service, no config_paths, no health check. Every bare register
  stores a **blank version** (`detect_current_version` always returns None — the
  template has no detect_command; shadow_executor.rs:257–265).
- The real binary-inspection machinery exists — `detect.rs` (Sprint 22 Phase B) — but is
  wired only into the read-only `Detect` subcommand (main.rs:3224–3259). Register never
  calls it.
- Live Sonarr (`/opt/Sonarr/Sonarr`) results: service match **high** (execstart-path →
  `sonarr.service`); version **4.0.16** probed from the binary; config_paths
  **false positive** (`/root/.config/Sonarr` via conventional exists-check — real dir is
  `/var/lib/sonarr/`, visible in ExecStart `-data=` arg); `dpkg -S` → unowned (not
  PM-managed); `--source` upgrades `source_type` → `github_release` via a duplicated
  URL-chain (detect.rs:610–615 vs shadow_cli.rs:8–17).
- Server schema (`shadow_packages`, db.rs:354): has name/service_name/install_paths/
  config_paths/source_url — **no binary sha256 column**.
- `Adopt` (main.rs:3278+) already implements catalog→trust-verify→local-scan→register;
  peer-deploy generalizes it.

## Design invariants

1. `Detect` stays a read-only preview; `Register` is the action. Register without
   `--recipe` must produce **draft-quality** output automatically.
2. Recipe trust path unchanged (`verify_recipe_trust`); peer-derived recipes must pass
   the same gate. Never weaken the operator-signature model.
3. Rollback stays the default; exceptions are explicit per-recipe (`rollback = "never"`).
4. `systemctl show` properties only (machine-readable) — never parse `status` output.
5. No `cargo fmt` (merge-time pass realigns). Plain single commands; no Cargo.toml edits
   unless a phase explicitly calls for one.

---

## P0 — Renderer correctness (client-only, detect.rs)

- [ ] **T0.1** Share `detect_source_type()`: render_draft_toml calls the shadow_cli.rs
      function; delete the duplicated chain (detect.rs:610–615).
- [ ] **T0.2** Confidence fixes: `source_type` tag follows the source confidence
      (currently always "guess", detect.rs:631); `name` confidence gets a real basis —
      basename case-insensitively matches the source repo basename → medium, else guess
      (kills the arbitrary `source_url.is_some()` proxy, detect.rs:621–625).
- [ ] **T0.3** When a service was matched, emit *suggestion comments* for lifecycle glue:
      `# pre_update = "systemctl stop <unit>"`, `# post_update = "systemctl start <unit>"`,
      and a health_check hint. Suggestions only — never uncommented.
- [ ] **T0.4** `REPLACE_ME` policy: draft keeps the placeholder; add header comment
      "registration requires --source while source_url is REPLACE_ME". Register bails
      with that exact message if the rendered recipe still contains REPLACE_ME.

## P1 — Detection depth (client-only, detect.rs)

- [ ] **T1.1** PM-ownership probe: `dpkg -S` / `rpm -qf` / `pacman -Qo` on the resolved
      path → `DetectReport.pm_managed: Option<bool>` (high confidence on a definitive
      answer; None when no PM found). Draft emits `# pm-managed: no (unowned by dpkg) —
      shadow candidate`.
- [ ] **T1.2** Implement `ConfigSource::UnitArg` (currently dead code, detect.rs:86–87):
      parse ExecStart args for `--data=` / `-data=` / `--config=` / `--datadir=` /
      `--confdir=` → ConfigFinding{source: UnitArg, confidence: high}. For Sonarr this
      finds `/var/lib/sonarr/` and outranks the conventional guess.
- [ ] **T1.3** User=-aware search space: parse `User=` from the unit; conventional
      candidates become `/var/lib/<name>`, `<home>/<user>/.config/<Name>`; **deprioritize
      `/root/*` when User != root** (fixes the false positive class).
- [ ] **T1.4** Structured unit facts on DetectReport: `exec_args: Vec<String>`,
      `unit_user: Option<String>` (consumed by T1.2/T1.3 and later by health checks).
- Tests: sonarr-shaped unit fixture + graded corpus (unit-arg hit / conventional hit /
      both / neither) as golden regression — pre-registered, no relaxation after runs.

## P2 — Register↔Detect wiring (client-only, main.rs)

- [ ] **T2.1** Register no-`--recipe` path: run `detect::detect(path)`; recipe =
      `render_draft_toml(report, name, source)`; bail with the T0.4 message when source
      absent (opt-out later if needed).
- [ ] **T2.2** `detected_version` = `report.version` — blank-version registers die;
      the "registering blank" branch (main.rs:2931–2937) becomes the fallback only when
      the probe genuinely fails.
- [ ] **T2.3** `--recipe` path unchanged: operator-authored recipes bypass detection
      entirely.
- [ ] **T2.4** `Detect` subcommand = same pipeline minus the server call (semantics
      unchanged; it remains the preview of what Register would do).
- **Live verification:** real register of ct/1952 Sonarr — expect service/config/version
  populated, version 4.0.16 non-blank, config_paths = ["/var/lib/sonarr"].

## P3 — Health monitoring + rollback policy (schema + executor)

- [ ] **T3.1** Schema (resolved Q3/Q4): `[install] failure_policy = "rollback" | "hold"`
      (default `rollback`; `hold` = Authentik-class — do NOT restore, wait for the
      operator). Stability is a **time-or-condition choice**, not a fixed figure:
      `[install.health_check.stable]` with `mode = "time" | "restarts" | "log_quiet"` +
      `secs` (time/log_quiet) or `count` (max restarts inside the window). Default
      `time:30`.
- [ ] **T3.2** systemd health check: pre-update baseline via
      `systemctl show -p ActiveState,SubState,NRestarts <unit>`; post-update the unit
      must reach active within timeout_secs, then satisfy the stable criterion. Breach →
      failure path, gated by failure_policy.
- [ ] **T3.3** Success monitor (resolved Q3 — lengthy migrations): optional
      `[install.success_monitor]` window (`window_secs`, any check type) during which
      the update is **provisional** — Authentik's DB migration may run minutes before
      the app is healthy. Failure inside the window → failure_policy; window expires
      clean → update recorded as final. Provisional state surfaces in `package update`
      output and the dashboard.
- [ ] **T3.4** `failure_policy = "hold"` recipes marked `downgrade_unsafe = true` at
      catalog level so adopt/check warn before offering updates.
- [ ] **T3.5** Failure diagnostics: capture `journalctl -u <unit> -n 50` into the update
      log on health failure.
- Tests: injectable command runner (ScanPaths pattern) with a fake systemctl; edge cases:
      restart at stable_secs−1, NRestarts already >0 at baseline.

## P4 — Server reach-out → similarity → peer deploy (proto + server + client)

- [ ] **T4.1** Register sends `binary_sha256`; db migration adds
      `shadow_packages.binary_sha256` (CREATE TABLE IF NOT EXISTS won't add a column —
      needs an ALTER migration).
- [ ] **T4.2** New RPC `QueryShadowRegistry {binary_sha256, name, service_name,
      target_dir, source_url}` → matches with recipe name/version/signed-flag + fleet
      installs (hostname, version, install_paths, last_seen).
- [ ] **T4.3** Client similarity scoring at register time. **Pre-registered weights:**
      sha256 exact +5 · name (ci) +2 · service unit +2 · source repo +1 · target_dir +1.
      Score ≥4 → **interactive choice prompt only** (resolved Q2 — no automation flags
      until real demand plus a safe architecture for them exists); 2–3 → draft
      annotation only; <2 → silent.
- [ ] **T4.4** Confidence cross-check against the fleet recipe: local config finding
      agrees with proven recipe → boost to high; disagrees → downgrade + review comment
      in the draft. Field-tested detect_command from the fleet recipe preferred when the
      local probe failed.
- [ ] **T4.5** Adopt-from-peer: high-score match → offer deploy-instead-of-register;
      fetch recipe (verify signature via verify_recipe_trust), **mutate**
      target_dir/config_paths/service to the locally detected values, register with the
      local detected_version. Resolved Q1 scope: peer-adopt ships for
      **private/tailnet servers**; the public-server policy is the future trust track
      below.
- [ ] **T4.6** Golden fixtures for the scorer: exact-match / near-match / no-match
      synthetic pairs shipped as tests; thresholds are the pre-registered ones above.
- [ ] **T4.7** Update-time similarity is **lenient** (resolved Q5) with one hard gate:
      if an update's source/asset URL changes the base repo (host or repo path) vs the
      registered recipe, require operator/owner **re-approval** before applying —
      supply-chain protection until automated update scanning exists. Re-approvals are
      recorded in shadow_update_history.
- **Pre-verification (blocks T4.5):** are client-registered recipes operator-signed
  server-side? Check the register_shadow_package handler (grpc.rs:1010) signing path.
  If unsigned, peer-adopt requires operator re-sign first.

## Verification plan

1. P2 exit: live Sonarr register on ct/1952 (real target: unowned, systemd service,
   non-standard dirs).
2. P3 exit: forced-failure update on a test unit → rollback within stable window;
   rollback=never unit → no restore, loud outcome.
3. P4 exit: second client adopts the Sonarr recipe with mutated dirs; server shows one
   recipe, two managed installs.
4. `cargo clippy` clean; full test suite green.

## Future track — identity-bound recipe trust (public servers / monetization)

Resolved direction for Q1; explicitly OUT of scope for P0–P4, which ship on the
private/tailnet trust model:

- On any non-private server, client-registered recipes must be **signed**, with the
  signature bound to a **history-bearing identity** (Twitter/Bluesky/Facebook/Reddit/
  NOSTR) — an un-monetized operator's shame/exclusion mechanism against abuse.
- Public visibility of a client-submitted recipe is **operator-reviewed**; a non-admin
  submitter may still deploy their own recipe to machines whose client is logged in as
  the same user (self-ownership deploy — same spirit as the remote-assignment
  ownership gate).
- Monetized servers: optional **stake** ("skin in the game") — submitters stake crypto
  claimable by the operator when they publish malware recipes.
- Long-term: **OAuth binding** (GitHub first) — repo contributors/maintainers define
  which package/commit signing keys are valid for that project's recipes, proving
  official-project provenance and creating an audit trail for abusers.

## Resolved decisions (operator answers, 2026-10-21)

1. **Peer trust** → future track above; P4 ships private/tailnet-only peer-adopt.
2. **Prompt semantics** → interactive only, for now; no automation flags.
3. **Failure posture** → `failure_policy` recipe component + `success_monitor`
   provisional window (lengthy-migration apps like Authentik).
4. **Stable criterion** → time-or-condition choice (`time` / `restarts` / `log_quiet`),
   default `time:30`; not a fixed global figure.
5. **Similarity scope** → both register and update; update-time is lenient except the
   repo-change re-approval gate (supply-chain protection).
