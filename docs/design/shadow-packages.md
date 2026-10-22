# Shadow Package System — Design Intent

> North-star reference for the shadow package system. Read this first when designing,
> extending, or debugging anything in the shadow recipe pipeline so implementation does
> not drift back toward "just point at a binary."

## 1. Purpose (what it is FOR)

The shadow package system lets Monopsony **orchestrate the install and update of software
that has no native way to be installed or updated on this host** — either because there is no
package-manager binary for it, or because the distro's packaged version lags far behind a
maintainer-run source (a git repo, an upstream tarball, a "build from source" release).

It is **not** primarily a package cache/proxy. The proxy subsystem handles fetching and
caching file bytes; shadow packages handle *managing the lifecycle* of a specific software
install — getting it onto a host correctly, keeping it updated, rolling back on failure,
and wiring its service into the host's init system.

The surviving artifact of the original intent is `package register --path`: point the client
at where the binary lives and Monopsony records enough to manage that install fleet-wide.

## 2. The model (declarative recipes)

A shadow package recipe is a **declarative description of an entire install + update
lifecycle**, not a pointer at a single binary. Recipes are:

- **Uploaded to the server** on registration (`register_shadow_package`), signed with the
  shadow manager's Ed25519 key, and published to a reputation-gated, server-global catalog.
- **Validated/approved by the server** before they can be distributed — the server marks a
  recipe safe, quarantines or rejects unsafe ones.
- **Assigned fleet-wide**: any connected client scans for local installs matching an assigned
  recipe and adopts it (register-by-name, or "Assign Recipe" from the dashboard clients page).

The catalog is tracker-shaped on purpose: keeping exchange-format edges makes later cross-server
federation a thin adapter rather than a migration.

## 3. Design invariants (do not regress these)

1. **Provisioning is first-class.** A recipe's `[build].steps` can run templated install steps
   that use `--source` for any file that must exist and *pull it from the source location if
   missing* (e.g. find `.env` / `docker-compose.yml`, download from `<repo>` when absent). This
   This is core — not an afterthought. The current schema stores steps as plain strings; a templating
   engine scoped to this subset (variable substitution + simple conditionals) is the expansion,
   **not** a full Jinja/Ansible dependency swap.

   **Minimal-subset discipline.** Implement only what is adequate for today's scenarios; do not build
   a broad template engine up front. When a scenario later requires more capability, add it in a way
   that stays backwards-compatible with older recipe versions (newer versions may emit fields older
   versions ignore). A minimal engine that can be extended beats an over-built one that has to be
   rewritten.

2. **Service integration is first-class.** Recipes describe how the software runs: its init-system
   service script (`os_family` detection → **systemd** and **OpenRC** for v1; all other init systems —
   SysV/init.d, rc6, runit, s6 — deferred to a later phase), container path via quadlets at
   `/etc/containers/systemd` (quadlets is systemd-only, so non-systemd container persistence differs),
   and runtime persistence semantics (on-demand, unless-stopped). A default "no package" template set
   ships in the client binary so operators can generate a blank service file per init system. Location
   substitution into these scripts is part of rendering an install.

**Init-system scope.** Long-term goal is ALL init systems, but v1 targets systemd + OpenRC — the two
primary ones on this fleet. Add others only when a real target requires them; each new init system is
its own small phase (new template + renderer), never a wholesale rewrite of the existing two.

3. **Updates must diff and warn.** Applying an update runs the recipe's hooks (pre_update → snapshot
   → atomic swap → health_check → rollback) but it must also *diff requirements across versions* —
   new required flags/variables, added/split containers, changed service unit names — and warn when
   those changes require a recipe edit or caused a client to fail/rollback. A silent no-op update is
   a defect.

4. **Server-gated distribution.** The server validates/signs/approves before distribution; the
   catalog is reputation-gated on entry; assignment is dual-gated (server flag `remote_assignment_enabled`
   + client opt-in) and refused under public exposure unless ownership is proven. All admin accounts
   enforce hardware-key 2FA (WebAuthn/FIDO2/YubiKey).

5. **Rollback is real, not cosmetic.** Health checks drive rollback via the managed pipeline;
   `failure_policy` (rollback / hold) is enforced. A recipe may set `downgrade_unsafe = true` to mark
   an app as dangerous or impossible to downgrade — then the client must **not** apply a non-interactive
   update: it requires either health-check-verifiable success or interactive confirmation, otherwise
   it refuses/aborts. This prevents blind dist-upgrade-style updates (e.g. `apt dist-upgrade -y`) from
   bricking a service. Forward-only-migration apps (e.g. Authentik) additionally use an auto/never-rollback
   setting with journalctl capture on failure — this is a deliberate exception, not a regression of the invariant.

## 4. What it currently is vs. what it should be

The early implementation settled into **binary-shadowing**: `[method]=extract/build/script`,
`binaries[]`, `target_dir`, asset selection by os/arch/libc. That is a valid *subset* and still
works — but provisioning steps, service scripts, and update-diff warnings are the parts that make
the system do what it was designed for, not just "shadow an existing binary."

When extending this area: start from the declarative lifecycle model in §2/§3. If a change only
makes sense as "point at a binary," ask whether provisioning/service/update-diff is required to
fulfill the purpose — if yes, it belongs here now; if no, it is out of scope for shadow packages.

## 5. Schema (authoritative field map)

From `crates/monopsony-common/src/shadow_types.rs` (`ShadowRecipe`):

- `[package]` (`RecipePackage`) — `name`, `description`, `homepage`, `source_type`
  (`direct_url` / `git_release` / `github_release` / `binary_sha256`), `source_url`.
- `[install]` (`RecipeInstall`) — `method` (extract/build/script), `target_dir`, `chmod[]`,
  `service` (unit name, optional), `config_paths[]`, `backup_old`, `max_backups`,
  `binaries[]` (per-binary extract map), and the update hooks: `pre_update`, `post_update`,
  `on_failure`; plus `health_check {type, port/…, timeout_secs}` and `failure_policy`
  (`rollback` / `hold`).
- `[build]` (`RecipeBuild`) — `steps[]` (provisioning), `requires[]`, `allow_network`. This is the
  section provisioning steps live in; today it stores plain strings, so a templating engine scoped
  to this subset is the expansion.
- `[script]` (`RecipeScript`) — `update` (a single command string).
- `[version]` (`RecipeVersion`) — `detect_command`, `detect_pattern`.
- `[asset_selector]` — `tag_pattern` plus `rules[] {os, arch, libc, pattern}` for os/arch/libc asset selection.
- recipe flags — `downgrade_unsafe: bool` (per-recipe; when set, client refuses non-interactive updates unless success is health-verifiable or interactive).

## 6. Related documents

- Sprint 22 spec (detection + fleet propagation): `docs/plans/2026-09-18-shadow-recipe-detection-and-fleet-propagation.md`
- Register pipeline plan: `docs/plans/2026-10-21-shadow-register-pipeline.md`
- Pairing security model (ownership proof, 2FA): `docs/pairing-security.md`

## 7. Open decisions & scope locks (recorded 2026-10-22)

These are **decisions made but not yet implemented** — recorded here so future work does not drift
off the intended shape. The shadow package redesign is scheduled as **Sprint 32** in
`docs/sprint-roadmap.md`; this doc is its north-star reference.

1. **Jinja subset stays minimal, with a broadening path.** Implement only what's adequate for today's
   scenarios; do not build a broad template engine up front. When capability is later required, add it
   backwards-compatibly (newer versions may emit fields older ones ignore). A minimal engine that can
   be extended beats an over-built one that has to be rewritten.

2. **Init systems: systemd + OpenRC for v1; all others deferred.** Long-term goal is ALL init systems,
   but the primary two on this fleet (systemd, OpenRC) are the v1 targets. Each additional init system
   is its own small phase (new template + renderer), never a wholesale rewrite of the existing two.

3. **`downgrade_unsafe = true` gates non-interactive updates.** When set, the client must not apply an
   update without either health-check-verifiable success or interactive confirmation — otherwise it
   refuses/aborts. This prevents blind dist-upgrade-style cron jobs (e.g. `apt dist-upgrade -y`) from
   bricking a service. *Status: field exists; enforcement in the executor is open work.*

**Out of scope for shadow packages:** OS-release upgrades and push-triggered updates are separate
feature areas (atomic-rollback recipe vs. no-fallback kernel migration); do not conflate them.
