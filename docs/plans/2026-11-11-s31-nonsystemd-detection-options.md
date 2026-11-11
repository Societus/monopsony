# Sprint 31 rows 6–7 (re-scoped) — non-systemd service detection + container recipe adoption: OPTIONS

**Date:** 2026-11-11 · **Status:** DECIDED — operator picked **1A + 2B**, **Q5 = ELF-only** (card
comments 2026-11-10); see `## Decision` at the end. **No code changed by this card.**
**Consolidates:** `2026-10-23-sprint31-item6-nonsystemd-detect-design.md` and
`2026-10-23-sprint31-item7-container-adopt-design.md` — both design-only, their open questions were
never answered. Those remain the deep dives; THIS doc is the decision surface.
**Grounding:** static read only, at the current working tree (post Sprint 32/34 lanes). All anchors
below were re-verified today; line drift from the October docs is corrected inline.

---

## 0. Code-state deltas since the 2026-10-23 designs (why this re-scope exists)

| # | Delta | Verified anchor | Consequence for the options |
|---|-------|-----------------|-----------------------------|
| D1 | Sprint 32 Phase 2 (service templates) **landed**: OpenRC rendering + enablement exists on the write side | `shadow_service.rs:37` `InitSystem{Systemd,OpenRC}`; consumed at `shadow_executor.rs:674–744` — `.service` suffix stripped for OpenRC (`:685`), init.d write (`:706`), chmod 755 (`:716–728`), `rc-update add <name> default` (`:741`) | Only DETECT lacks an OpenRC story. Item 6 stays detect-side; its `unit_name` convention must match the write side (bare name, suffix-free). |
| D2 | Sprint 32 Phase 3 (requirement diffing) **landed but dead-wired** | `execute_update_with_diff(recipe, previous: Option<&ShadowRecipe>, …)` `shadow_executor.rs:890`; called with `None` at `main.rs:4141–4144`; gap comment `main.rs:4134–4140`: *"client has NO local store of the previously registered/adopted recipe … previous is always None today — zero diff warnings"* | Item 7's "template diffing" need has an ENGINE with no INPUT. The item-7 doc's byte-compare drift seam predates this engine and is now the weaker option at nearly the same wiring cost. |
| D3 | The "previous recipe" accessor **already exists over gRPC** | `ShadowPackageInfo.recipe_toml` (`proto/monopsony.proto:423`) returned by `list_shadow_packages` (`grpc_client.rs:1001`, currently `#[allow(dead_code)]`); server persists the registered bytes at adopt (`main.rs:4361`) | The D2 gap is closable client-side, zero proto/server changes. |
| D4 | A **third** registration site cloned the adopt arm's empty-metadata shape | remote-assigned flow `main.rs:1660–1690`: `&[]` config_paths (`:1686`), `""` sha256 (`:1688`) | Any item-7 metadata fix must cover Adopt (`main.rs:4363`/`:4365`) AND remote-assigned (`:1686`/`:1688`), or the row inconsistency survives on the heartbeat path. |

Also verified: I5 hygiene IS present in detect.rs (`normalize_config_path` `:561–569`; renderer
`/root/*` demotion `:879–911`); the register `--recipe` path derives row metadata since I2
(`main.rs:3699–3726`, selection `:3871–3889`). The adopt path does not — the stale comments at
`:4363`/`:4365` are verbatim from the October doc.

**Premise correction (carried over from item-6 doc §1, still true at today's anchors):** the
roadmap's "SysV scan exists but unexercised" is half-stale. On Alpine the `/etc/init.d` scan IS
exercised and mislabels OpenRC scripts as `"sysv"` (`detect.rs:505–537`, no shebang check), and for
running processes tier 2 short-circuits it with an EMPTY-name match — `cgroup_unit` (`:545–557`)
parses only `system.slice/`, and `unwrap_or_default()` (`:495`) turns `None` into `unit_name: ""`.
That empty name is the authentik empty-service root cause, and `render_draft_toml` then emits
`systemctl stop ` hints with an empty unit (`:853–874`, unconditional systemctl).

---

## 1. Item 6 — non-systemd service detection

### Current detection surface (verified, `crates/monopsony-client/src/detect.rs`)

- Tier 1 systemd units: `match_service` `:412–468` → `method="execstart"` (emitted at `:461`).
- Tier 2 `/proc/*/exe`: `:470–503` → `method="proc-exe"`, `unit_name = cgroup_unit(..).unwrap_or_default()`.
- Tier 3 SysV `/etc/init.d` grep: `:505–537` → `method="sysv"`.
- `ScanPaths` `:19–56`: system_units / user_units / init_d / proc_dir / pm_probe — no runlevels, no container awareness. Injection pattern for tests at `:1012+`.
- `ServiceMatch.method` doc comment `:121–122` is stale (claims `"execstart-path"`/`"execstart-basename"`; code emits `"execstart"`).
- Init awareness elsewhere in the SAME crate, shared by nothing in detect.rs: `self_update.rs:1089` `InitSystem{Systemd,OpenRc,Launchd}` (runtime markers `/run/systemd/system`, `/run/openrc/softlevel`, `rc-service`, `launchctl` at `:1118–1126`; `restart_argv` → `rc-service <unit> restart` `:1168–1171`) and `shadow_service.rs:37` (D1).

### Option 1A — OpenRC tier + container marker inside the existing `ServiceMatch` — RECOMMENDED

**(a) Mechanism** (item-6 doc §2–4, re-anchored; four pieces, all in detect.rs):
1. **Tier 2.5** in `match_service`, between `/proc` (`:503`) and SysV (`:505`): for an `/etc/init.d/<name>`
   hit (reuse the existing grep), classify by content already in memory: shebang
   `openrc-run`/`runscript` OR a `depend() {` line → `method="openrc"`; else stays `"sysv"` (never
   lose a match the current code finds). Enablement read from the FILESYSTEM: any
   `/etc/runlevels/<rl>/<name>` entry → `Confidence::Medium`; none → `Confidence::Guess` + draft
   comment (`rc-update add <name> default to enable`). New injectable
   `ScanPaths.runlevels: Vec<PathBuf>` (default `/etc/runlevels`, retained-if-dir — same pattern as
   `init_d` at `:45–46`). `unit_name` = **bare name, suffix-free** — matches what
   `shadow_service.rs:685` renders on the write side; `unit_path` keeps the full script path.
2. **Tier 2 container marker**: cgroup path contains `libpod-`/`docker-`/`containerd-`/`crio-` scope →
   `method="container"`, `unit_name` = short id from the scope name, `Confidence::Guess` (the honest
   floor). Engine hint: `/run/.containerenv` → podman, `/.dockerenv` → docker.
3. **Empty-claim fix**: tier 2 with no `system.slice/` unit AND no container marker → return `None`
   (one-line fix at `:493–500`); tiers 2.5/3 then actually run. This is the authentik regression fix.
4. **Renderer switch** on `svc.method` in `render_draft_toml` `:853–874`: `execstart` → `systemctl`
   (unchanged); `openrc` → `rc-service <unit> stop|start`; `sysv` → `/etc/init.d/<unit> stop|start` +
   "no supervisor restart; verify manually"; `container` → `podman restart <id>` placeholder +
   "wrapper discovery is Sprint 32 P2" note. Never emit a hint with an empty unit name; never emit
   `systemctl` for a non-execstart method. Fix the stale `:121–122` doc comment in the same change.

**(b) Files:** `crates/monopsony-client/src/detect.rs` only (+ its test module). No schema, proto,
server, or dependency changes.

**(c) Testability:** bench-free via the existing `ScanPaths` tempdir-fixture pattern (`:1012+`):
openrc-shebang classification; sysv-stays-sysv regression; runlevel symlink → Medium / absent →
Guess+comment; fake-pid cgroup with no slice and no container markers → `None` (authentik
regression); `libpod-` cgroup → container+short-id; table-driven renderer test per method; golden
draft for an openrc match asserting no `systemctl` substring anywhere. **Fleet re-verify:** ct/1904 —
re-detect the authentik process (never empty service; expected `container` or `None` per layout),
`/usr/sbin/sshd` → `openrc` + `rc-service sshd stop` hint; ct/1952 — sonarr detect draft
byte-identical pre/post (systemd untouched); register both, confirm non-empty service + method labels.

**(d) Risks:** shebang-less scripts default to sysv (today's behavior — safe direction); container-id
extraction depends on engine cgroup naming (A3 below); `unit_name` semantics bent to hold a container
id (documented in the doc comment; Sprint 32 P2 replaces it with the wrapper unit name).

**(e) Rec:** SHIP — closes the roadmap row at ~135 LOC + tests (item-6 §7 estimate), no cross-crate
motion, aligned with the already-landed Sprint 32 P2 write-side conventions.

### Option 1B — empty-claim fix + host-level init labeling only (minimal slice)

**(a) Mechanism:** fix `:495` (return `None` instead of an empty `ServiceMatch`), and label
`/etc/init.d` hits using the HOST init system (reuse/extract the `self_update.rs:1118–1126` marker
detection) so Alpine scripts render `rc-service` hints. No new tier, no runlevels field, no container
marker.
**(b) Files:** detect.rs (+ ~15-LOC helper shared with or duplicated from self_update.rs).
**(c) Testability:** same fixture pattern; fleet check = authentik row no longer empty + sshd labeled openrc.
**(d) Risks:** leaves the container story entirely unanswered (authentik-class rows stay
service-less — half the roadmap row); host-level labeling is wrong on systemd+init.d coexistence
hosts unless the shebang is also consulted — at which point half of 1A is rebuilt anyway.
**(e) Rec:** only if the operator wants the smallest possible slice this sprint.

### Option 1C — unify all init handling (shared ServiceKind in monopsony-common)

**(a) Mechanism:** one common enum consumed by detect.rs, `shadow_service.rs`, `self_update.rs`;
single hint/command renderer.
**(b) Files:** monopsony-common + three client modules — cross-crate churn.
**(c) Testability:** per-module tests exist, but the blast radius is the just-landed Sprint 32 P2 surface.
**(d) Risks:** violates the north-star discipline quoted in the item-6 doc ("each init system is its
own small phase — new template + renderer, never a wholesale rewrite"); the three enums serve
different jobs (observe vs write-target vs self-restart) and are not actually redundant.
**(e) Rec:** REJECT for this sprint; revisit only when a third init system arrives (launchd is the
likely next one, via the macOS lane).

---

## 2. Item 7 — container recipe adoption (authentik-style: script method, bind roots)

### Current adopt/assignment state (verified)

- **Adopt arm** `main.rs:4289–4382`: trust gate (hard abort) `:4296–4308` → `extract_install_fields`
  (`shadow_cli.rs:157`) `:4322` → `catalog_scan::scan_entry` installed-gate `:4326–4332`
  (`catalog_scan.rs:62`; `ScanFinding` `:53–58`, methods `install_path`/`detect_command`) →
  register `:4351–4368` with `&[]` config_paths (`:4363`) and `""` sha256 (`:4365`).
- **Remote-assigned flow** `:1660–1690` — same two empties (`:1686`, `:1688`).
- **Register `--recipe` path** derives all four fields since I2 (`:3699–3726`, `:3871–3889`);
  `extract_config_paths` exists (`shadow_cli.rs:188`).
- **Update dispatch** `:4110–4144`: T4.7 repo-change gate wired; `execute_update_with_diff` called
  with `previous = None` (D2/D3).

### Option 2A — row-metadata fix only

**(a) Mechanism:** Adopt + remote-assigned derive `config_paths = extract_config_paths(&recipe_toml)`
and sha256 per the probe rule (see Q5) from `install_paths` — mirroring the I2 register-path
precedent exactly. Kills the row-vs-recipe inconsistency the October doc §1.3/§2 documented.
**(b) Files:** `main.rs` (two call sites) + tests.
**(c) Testability:** bench-free — derivation unit tests against fixture TOML bytes; fleet = re-adopt
on a staged host shows a populated row.
**(d) Risks:** the roadmap's second half ("updates should surface new flags/env/template changes")
stays dead — `previous` remains `None`, the P3 engine stays silent.
**(e) Rec:** necessary but not sufficient.

### Option 2B — 2A + in-repo fixture + close the previous-recipe gap — RECOMMENDED

**(a) Mechanism:** = 2A, plus:
1. **Fixture** `recipes/shadow/authentik-style-example.toml` (October doc §3.2 verbatim shape:
   `method="script"`, `config_paths` = bind roots, NO `service` key — header documents that empty
   service is legitimate for container/script recipes, `downgrade_unsafe = true`, port health check,
   compose-pull `[script] update`). Tests pin the shipped bytes via `include_str!`.
2. **Previous-recipe wiring** at the update dispatch site (`main.rs:4136–4144`): call
   `list_shadow_packages(client_id)` (un-`#[allow(dead_code)]` `grpc_client.rs:1001`), find the row
   by package name, strict-parse its `recipe_toml` field (lenient fetch → strict `ShadowRecipe`;
   unparsable → `None` + warn, never block the update), pass `Some(&previous)` into
   `execute_update_with_diff`. Field-level diff warnings (env/flags/steps — the landed P3 engine)
   then land in the update log for every registered row, adopted ones included. The October doc's
   §5 byte-compare seam is SUPERSEDED: same two artifacts, better engine, same wiring cost.
**(b) Files:** `main.rs` (adopt + remote-assigned + update dispatch), `grpc_client.rs` (attr only),
`recipes/shadow/authentik-style-example.toml`, tests.
**(c) Testability:** bench-free — fixture-byte tests (bind roots extracted in order, service empty,
Script method); 2A derivation tests; row→`Option<ShadowRecipe>` mapping tests (absent row → None,
present → parsed, garbage → None+warn). **Fleet:** publish fixture to a scratch catalog → stage
ct/1904 bind roots + version shim → `shadow adopt` → row assertions (config_paths byte-equal to
recipe, service empty, sha256 per Q5 rule) → mutate + re-sign the catalog recipe (add a config_path)
→ update shows diff warnings; ct/1952 negative controls: unstaged → refused (`:4328`), tampered
signature → aborted (`:4302`), technitium adopt regression (only sha256 may differ).
**(d) Risks:** row `recipe_toml` round-trip fidelity (A5 below — static read shows the field flows
both ways; fleet check confirms bytes); `list_shadow_packages` returns all rows (client-side filter,
cheap); diff-engine behavior itself is Sprint 32 P3's gate, not this card's.
**(e) Rec:** SHIP — this is the full roadmap row: adopt exercised on a fresh host AND updates
surface changes vs the registered recipe, using the engine that already exists.

### Option 2C — server-side drift flag at check time

**(a) Mechanism:** `check_shadow_updates` (control grpc.rs) compares each row's `recipe_toml` against
the current catalog copy and flags drift in check output.
**(b) Files:** monopsony-control grpc.rs (+proto if a wire field is needed) — cross-crate.
**(c) Testability:** control-crate tests; fleet = check output on ct/1904.
**(d) Risks:** control-crate churn in a handler with a recent bug class (the I4 ids filter); surfaces
THAT something changed, but the update-time engine still needs `previous` for field-level warnings —
you end up building both.
**(e) Rec:** REJECT — more moving parts than 2B for less value.

---

## 3. Combined recommendation

**1A + 2B.** Both land in monopsony-client only (2B additionally touches a dead-code attribute in
`grpc_client.rs`); no schema/proto/server changes; both fully fixture-testable without bench; both
cite landed Sprint 32 P2/P3 conventions instead of inventing new vocabulary. Estimates: 1A ≈ 1 day,
2B ≈ 1.5–2 days including the fleet exercise (October docs' estimates, still plausible).

## 4. Operator decisions (recommendations marked)

**Item 6:**
- **Q1 — option pick:** 1A (recommended) / 1B minimal / 1C rejected.
- **Q2 — OpenRC `unit_name` format:** bare suffix-free name (recommended — round-trips into
  `rc-service` verbatim and matches `shadow_service.rs:685`) vs filename-as-is vs path-qualified.
- **Q3 — unenabled scripts** (no runlevel entry): register as Guess + comment (recommended — mirrors
  how nonexistent config paths still surface as suggestions) vs skip entirely.
- **Q4 — container method marker:** single `"container"` method string with engine in `unit_path`
  (recommended) vs per-engine methods.
- **Q5 (item 7, sha256 probe rule — NEW, resolves an October-doc contradiction):** the October doc
  says "hash any existing regular file in install_paths" but then expects `""` for the fixture whose
  install_paths IS a regular file (docker-compose.yml). Both cannot hold. Choices:
  (i) **ELF-only** — hash only when `parse_elf_header` (detect.rs:179, already pub) accepts the file;
  compose/scripts/binds stay honestly empty and `binary_sha256` means what it says (RECOMMENDED);
  (ii) any-regular-file — compose files get hashed (defensible for similarity matching, but the field
  is named binary_sha256); (iii) always empty on adopt/remote-assigned (weakest — keeps the
  similarity +5 weight unreachable).
- **Q6 — reclassify existing `"sysv"` hits as `"openrc"` by shebang even where systemd coexists:**
  yes (recommended — shebang is authoritative; tier order already prefers systemd units).

**Item 7:**
- **Q7 — option pick:** 2B (recommended) / 2A metadata-only / 2C rejected.
- **Q8 — `service` for script/container recipes:** empty/absent key (recommended — matches fleet
  truth; Sprint 32 P2 owns any future vocabulary) vs marker string like `compose:<name>`.
- **Q9 — fixture placement:** in-repo `recipes/shadow/authentik-style-example.toml`, SYNTHETIC header
  (recommended — tests pin shipped bytes) vs fleet-side only.
- **Q10 — exercise catalog + adopt tolerance:** scratch/staging catalog first, production only after
  negative controls (recommended); adopt keeps tolerating a missing `detect_command` (recommended —
  container version probes need the runtime up; the register-path blank-version rule is deliberately
  not imposed on adopt, `main.rs:4333–4346`).

## 5. ASSUMPTION-UNVERIFIED register (each with the fleet check that must run)

- **A1 OpenRC runscript layout on Alpine:** shebang `#!/sbin/openrc-run` and `depend() {` blocks in
  real ct/1904 scripts. Check: `head -3 /etc/init.d/sshd` + `grep -l 'depend()' /etc/init.d/* | head`.
- **A2 Runlevel layout:** `/etc/runlevels/<boot|default>/<name>` entries point at init.d scripts.
  Check: `ls -l /etc/runlevels/default/` on ct/1904.
- **A3 Container cgroup naming:** podman ≥4 on cgroup v2 exposes `…/libpod-<id>.scope` in
  `/proc/<pid>/cgroup`; docker → `docker-<id>.scope`. Check: read `/proc/<pid>/cgroup` for the
  authentik server process on ct/1904.
- **A4 Engine marker files:** `/run/.containerenv` (podman) vs `/.dockerenv` (docker). Check: stat
  both inside the CT (and on the container host if the client would run there).
- **A5 Row recipe_toml round-trip:** the server stores and returns the exact registered bytes via
  `list_shadow_packages`. Static read says yes (proto field 9, register sends full TOML `main.rs:4361`);
  check at exercise time: adopt fixture → list → byte-compare.
- **A6 Fixture detect_command** (`docker compose … exec -T server ak version`) works headless —
  exercise-time check only; the deterministic path uses a PATH shim instead (October doc §4 step 2).
- **A7 Real authentik row shape:** bench facts say script method + bind-root config_paths, but the
  exact `install_paths` value on the ct/1904 row is UNVERIFIED. Check before finalizing the fixture:
  inspect the row + `ls` the deploy dir.

**Explicitly NOT assumed/used:** `podman inspect` fields and Quadlet unit parsing — both Sprint 32
P2 territory; 1A reads only `/proc/<pid>/cgroup` + marker files, so no dependency on them.

## 6. What a fleet re-verify needs (summary)

ct/1904 (Alpine/OpenRC): detect authentik process + `/usr/sbin/sshd`; adopt fixture on staged bind
roots; mutated-catalog update shows diff warnings. ct/1952 (Debian/systemd): sonarr detect draft
byte-identical (1A regression); adopt negative controls (unstaged / tampered signature); technitium
adopt regression. Server: row assertions via dashboard/SQL (config_paths byte-equal, service empty,
sha256 per Q5 rule); `list_shadow_packages` byte round-trip (A5).

---

## Decision

**Operator picks — verbatim from card comments on t_440d1030 (author `webui`, 2026-11-10):**

> Choices 1A and 2B approved to continue
>
> *(22:48)*

> Q5 = ELF only.
>
> *(22:56)*

**Resolved by those picks:**

- **Q1 (Item 6 option): 1A** — OpenRC tier 2.5 (shebang/`depend()` classify, runlevel enablement from
  `/etc/runlevels`, bare suffix-free `unit_name`) + tier-2 container cgroup marker + no-empty-claims
  fix + init-aware renderer hints; `detect.rs` only (§1 Option 1A).
- **Q7 (Item 7 option): 2B** — 2A row-metadata fix (recipe-derived `config_paths` + sha256 per the Q5
  rule, applied at BOTH adopt `main.rs:4363/:4365` and remote-assigned `:1686/:1688`) + in-repo
  `recipes/shadow/authentik-style-example.toml` fixture + wire `previous` from the
  `list_shadow_packages` row `recipe_toml` into `execute_update_with_diff` (§2 Option 2B).
- **Q5 (sha256 probe rule): (i) ELF-only** — hash an `install_paths` entry only when
  `parse_elf_header` (detect.rs:179) accepts the file; compose files/scripts/bind roots stay honestly
  `""`. This supersedes the October item-7 doc's "any existing regular file" wording: a `""`
  `binary_sha256` on a compose-file install_paths is VALID, not a mismatch or drift signal.

**Adopted by default (the §4 recommended values; the operator approved the combined 1A+2B
recommendation without individually overriding these — the implementing card treats them as decided
unless the operator reopens them):**

- **Q2** — OpenRC `unit_name`: bare suffix-free name (round-trips into `rc-service` verbatim;
  matches the write side at `shadow_service.rs:685`).
- **Q3** — unenabled scripts (no runlevel entry): register as `Confidence::Guess` + draft comment
  (`rc-update add <name> default to enable`); never silently skipped.
- **Q4** — container marker: single `method="container"` string, engine identity carried in
  `unit_path`; no per-engine method strings.
- **Q6** — reclassify existing `sysv` hits as `openrc` by shebang even where systemd coexists: yes
  (shebang is authoritative; tier order already prefers systemd units).
- **Q8** — `service` for script/container recipes: empty/absent key (matches fleet truth on
  ct/1904); no marker string like `compose:<name>`.
- **Q9** — fixture placement: in-repo `recipes/shadow/authentik-style-example.toml` with SYNTHETIC
  header; tests pin the shipped bytes via `include_str!`.
- **Q10** — exercise via a scratch/staging catalog first, production only after the negative
  controls; adopt keeps tolerating a missing `detect_command`.

**Still open (NOT resolved by this decision):** ASSUMPTION-UNVERIFIED A1–A7 (§5) — each carries its
own fleet check that the implementing card's re-verify (§6) must run; none may be assumed true.
Diff output stays advisory/display-only (surfacing, never auto-apply), per the standing rule.

**Implementing-card scope:** 1A + 2B exactly as specified in §1/§2 — `crates/monopsony-client/src/detect.rs`
(+ test module), `main.rs` (adopt + remote-assigned + update-dispatch sites), the
`#[allow(dead_code)]` attribute on `grpc_client.rs:1001` only, and the new fixture file. No schema,
proto, or server changes.
