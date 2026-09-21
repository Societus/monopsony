# Shadow Recipe Detection & Fleet Propagation — Sprint 22 Plan

**Date:** 2026-09-18
**Status:** Planned (scheduled as Sprint 22; see `docs/sprint-roadmap.md`)
**Prerequisite sprints:** 20 (integrity core), 21 (sourcedb oracle — version normalizer reuse)

---

## Provenance

This mechanism was part of the original shadow-package planning discussion (sprint 7–9 era,
May 2026). The original plan docs were symlinks into `~/projects/monopsony-plans/`, which
does not exist on the current dev host — the repo only stores the link stubs, so the links
were dead. **Recovered 2026-09-18:** the original plan directory still exists on
`martin@dev-environment` at `~/projects/monopsony-plans/` (sprints 5–18 + the Jun 3
roadmap). Reviewed against the user's recollection:

- Sprint 7 planned `generate_registration_template(name, source_url, source_type,
  detected_arch, detected_version, available_assets)` — auto-detected fields for
  interactive registration, and `validate_recipe_full` including "service name is valid
  (exists in systemd)". This is the **source-driven** half of the remembered design.
- Sprint 8 landed `detect_source_type()` + register flags (the CLI grammar that survives
  today).
- The old roadmap's "Sprint 7c — wire 8 shadow RPCs" was the ancestor of Phase A below,
  still unwired in the current tree.
- **Not found anywhere** (old repo, all plan files, or the old roadmap): pointing at an
  installed **binary path** to generate a recipe, the server-global recipe catalog, fleet
  scan of other clients, or either adoption path (adopt-by-name, dashboard assign). Those
  existed only in discussion, never in writing.

The surviving artifact of the original intent is the `package register --path` flag
("Path to the installed binary or directory") in `crates/monopsony-client/src/shadow_cli.rs`.
This document re-captures the full design from the author's intent (2026-09-18) and is now
the authoritative spec for it.

## Design intent (as originally conceived)

1. A client registers a new shadow package by pointing at its binary. The client
   **detects** the installation's shape (service units, config file locations, version
   detection method) and creates a shadow recipe, which is sent to the server.
2. All clients connected to the server see every existing shadow recipe (a server-global
   recipe catalog) and scan their local install locations to determine whether the same
   item is installed on them as well.
3. Update-management takeover then has **two adoption paths**:
   - **Path 1 (client-initiated):** the other client adopts by recipe/package name alone —
     no need to define the binary path.
   - **Path 2 (server-initiated):** the server GUI clients page lets the operator select a
     client, list its installed packages, and "assign recipe" from the server's catalog.
     For security, this path is behind a configurable toggle and should not be enabled on
     a public server unless the server is provably owned by the client's user and carries
     extra hardening: hardware-security-key 2FA for **all** admin accounts — including
     passwordless login via OAuth SSO + FIDO2/YubiKey.

## Current state (verified 2026-09-18)

| Piece | State |
|---|---|
| CLI grammar `package register/add/unregister/list/inspect/check/update/rollback/recipe` | Exists (`shadow_cli.rs`); handler `run_shadow_package()` (main.rs:1515) is print-only stubs |
| Shadow gRPC RPCs (`ListShadowPackages`, `RegisterShadowPackage`, `GetShadowRecipe`, …) | Server-side **implemented** — Sprint 7c completed 2026-06-22 (`SPRINT_7C_IMPLEMENTATION.md`): real handlers in `crates/monopsony-control/src/grpc.rs`, `ShadowPackageManager` constructed in server main.rs and injected. Client side unwired: the daemon never calls any shadow RPC, and `run_shadow_package()` CLI handlers are print stubs. (AGENTS.md's "not yet wired to gRPC handlers" note was stale — server side only — corrected 2026-09-18.) |
| Recipe schema (`ShadowRecipe` in `monopsony-common/src/shadow_types.rs`) | Supports everything detection must emit: `[install] service`, `config_paths`, pre/post hooks, backups; `[version] detect_command/detect_pattern` (command/file/HTTP-header); `[asset_selector]`; `[health_check]` (http/port/process) |
| Recipe signing | `shadow_signing.rs` (Ed25519 keygen/sign/verify); client `[shadow_trust]` policy + trusted author/operator keys; quarantine gates exist |
| DB | `shadow_packages` is **per-client** (`client_id` column + index) — no global recipe catalog concept |
| Feature flags | `FeatureFlags` in `server_settings` JSONB (`monopsony-admin/src/features.rs`) — `shadow_enabled` exists; no remote-assignment toggle |
| Server→client channel | `ServerMessage.Command` on the ControlStream exists |
| Dashboard | `/clients` page exists (no package list / assignment UI); `/shadow` recipe pages exist |
| Install detection in the field | None. No code scans systemd units, init.d, or config locations for an arbitrary binary |

## Phases

### Phase A — Wire the client to the shadow gRPC surface (prerequisite chunk)

Server side is already done (Sprint 7c, 2026-06-22): all 8 RPCs have real handlers and
`ShadowPackageManager` is constructed in server main.rs. What's missing is the client
half: the daemon never calls the shadow RPCs, and `run_shadow_package()` in the client
is print-only stubs while `monopsony-client/src/shadow.rs` (1.3k lines: detect_version,
backup/restore/rollback, extract/install, health checks) sits dead. Wire the daemon's
gRPC client + real CLI handlers so `package list/inspect/check/update/rollback` work
against a connected server.

**Est. 1.5–2 d.**

### Phase B — `package detect <path>` (local discovery → draft recipe)

```
monopsony-client package detect /opt/bin/thingamajig [--source URL] [--name X] [--out FILE]
```

Detection steps (all local, read-only; nothing is modified):

1. **Binary identity:** resolve real path (`readlink -f`), record sha256, size, ELF
   target (os/arch/libc via existing build-target detection helpers).
2. **Version probe:** try `--version`, `-V`, `version`, `-h` (bounded timeout); capture
   output; propose `detect_command` + `detect_pattern` regex (semver-ish extraction).
   If the binary won't run (foreign arch, needs daemon ctx), fall back to file-based
   detection and mark the field `# TODO: verify`.
3. **Service units:** match the binary path against unit `ExecStart` (resolve via
   `/proc/<pid>/exe` for running instances; scan `/etc/systemd/system`,
   `/usr/lib/systemd/system`, `~/.config/systemd/user`). SysV fallback: grep
   `/etc/init.d/*` for the path (Alpine). Extract unit name → `[install] service`.
4. **Config locations:** unit `Environment=`/`ExecStart` args referencing files, plus
   conventional dirs (`/etc/<name>`, `~/.<name>`, `~/.config/<name>`). Populate
   `config_paths`; everything not confirmed is emitted as a commented suggestion.
5. **Emit a draft TOML** annotated with confidence notes per field. Operator reviews
   (`package recipe validate`, `$EDITOR`) — detection never auto-registers.

`package register` then sends `recipe_toml` + `install_paths`/`config_paths`/
`service_name` via the existing `RegisterShadowPackageRequest` wire format.

**Est. 2–3 d.**

### Phase C — Server-global recipe catalog + fleet scan

**Design directive (user decision 2026-09-18):** build the catalog edges deliberately
tracker-shaped from day one — recipes published to the tracker tier the way mirrors
announce today, with reputation gating on the receive side. This keeps Phase 6
cross-server recipe federation a thin adapter over Phase C instead of a migration.
Costs nothing now; it constrains only the exchange-format edges, not the local schema.

Schema change: recipes become server-global; per-client rows become assignments.

- New table `shadow_recipes` (name, current signed recipe blob + sig, author/operator
  key ids, provenance client_id, versions) with `shadow_packages` refactoring into
  per-client assignments referencing it (migration required; keep per-client columns
  that are genuinely per-client: version, masked_from_pm, last_checked_at).
- On `RegisterShadowPackage` from any client: recipe is validated, stored in the
  catalog, and signed with the server's recipe key (existing `shadow_signing`), subject
  to the existing trust-policy/approval flow.
- Connected clients pull the catalog on daemon start and on change (piggyback on
  inventory/heartbeat cadence or a catalog hash — same pattern as sync hash verification).
- For each catalog recipe with no local assignment, the client runs a **scan**: check
  `install_paths` presence + `detect_command`/`detect_pattern`. Findings are reported
  to the server (installed yes/no + detected version) so the dashboard shows coverage.
- Scans are read-only and rate-limited; a recipe's detect failure must never degrade
  the daemon.

**Est. 3 d.**

### Phase D — Adoption path 1: adopt by name (client-initiated)

```
monopsony-client package adopt <name>
```

Fetches the signed recipe by name (`GetShadowRecipe`), verifies the client's
`shadow_trust` policy (author/operator keys) **before** anything executes, records the
assignment, and runs the same detect pass to confirm the local install and current
version. No binary path required — the recipe's detect fields locate the install.

**Est. 1 d.**

### Phase E — Adoption path 2: dashboard "Assign Recipe" (server-initiated)

- `/clients` page: select client → installed-packages list (from inventory + scan
  findings) → "Assign Recipe" picker fed by the server catalog.
- Server pushes the assignment to the client daemon over the existing
  `ServerMessage.Command` channel; the client applies the same adopt path locally
  (trust verification included — the server cannot bypass client-side trust).
- **Dual gate, both required:**
  - Server feature flag `remote_assignment_enabled` in `FeatureFlags` (default **false**).
  - Client opt-in `[shadow_trust] accept_remote_assignment` (default **false**).
- Assignment command carries the full signed recipe so verification is end-to-end.

**Est. 2 d.**

### Phase F — Public-server hardening gate

`remote_assignment_enabled` must refuse to enable when the server detects public
exposure (reuse the tiered-auth exposure heuristics: non-private bind / funnel mode).
Documented requirements for a public server to qualify:

1. Provable ownership of the server by the client's user (operator attestation flow).
2. Hardware-security-key 2FA enforced for **all** admin accounts — WebAuthn/FIDO2 or
   YubiKey; passwordless WebAuthn, or OAuth SSO + FIDO2 second factor, are both
   acceptable shapes.
3. Audit logging of every remote assignment.

WebAuthn support in dashboard auth is a separate work item (backlog); until it lands,
the flag stays locked to non-public deployments. This matches the existing production
security model (pair over public internet, then lock to private ranges).

**Est. 1 d for the gate + docs; WebAuthn tracked separately.**

## Security model summary

- Recipes are signed by the server recipe key (Ed25519, existing); clients verify
  author/operator trust policy before executing anything (existing `shadow_trust`).
- Server-initiated assignment is dual-gated (server flag + client opt-in) and can never
  bypass client-side trust enforcement.
- Detection is read-only and local; drafts require operator review before registration.
- Public servers: remote assignment off unless ownership proven + hardware-key 2FA for
  all admins + audit log.

## Exit criteria

1. `package detect` on a hand-installed binary (e.g. i2pd's layout) produces a valid,
   operator-approvable recipe; `recipe validate` passes.
2. Register on client A → client B's daemon sees the catalog entry and its scan
   correctly reports installed/not-installed + version.
3. Client B adopts by name only; an update then flows through the normal signed-recipe
   path (verify → backup → swap → health check → rollback on failure).
4. Dashboard assign-recipe flow works with both gates on; with either gate off, the
   assignment is refused server- or client-side respectively.
5. Flag refuses to enable under detected public exposure.
6. Zero regressions: pinning, ledger, and inventory paths unaffected (full workspace
   green + fleet spot-check).

## Open questions

- Migration shape for `shadow_packages` → assignments (in-place column moves vs. new
  table + backfill); decide at slice-spec time.
- Whether scan findings live in the existing inventory report or a dedicated
  `shadow_scan_results` table (leaning: dedicated, avoids bloating inventory).
- Recipe catalog distribution to offline clients: defer to tracker/federation tier
  (Phase 6) — this sprint only covers same-server clients.
