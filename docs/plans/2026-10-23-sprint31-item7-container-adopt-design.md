# Sprint 31 Item 7 — Container recipe adoption: adopt-on-fresh-host exercise + minimal update-surfacing seam

**Status:** design (no code changes yet)
**Date:** 2026-10-23
**Ground truth:** verified at dirty HEAD (e324708 + uncommitted Sprint-31 items I1–I5)
**Roadmap entry:** `docs/sprint-roadmap.md:415` — "Container recipe adoption — authentik-style recipe
(script method, config_paths = bind roots) → exercise adopt-on-fresh-host path; updates should surface
new flags/env/template changes vs the registered recipe (needs template diffing) | new detect/adopt work"
**Scope guard:** `docs/sprint-roadmap.md:437` (Sprint 32 Phase 3, update diffing, 2–3 d) — this item must
NOT pre-build P3. §5 below draws the line and writes the handoff note P3 consumes.

---

## 1. Current adopt-path map (verified anchors)

### 1.1 The `Adopt` command — `crates/monopsony-client/src/main.rs:3535–3628`

| Step | Anchor | What happens |
|---|---|---|
| Trust gate | :3538–3554 | `load_verified_recipe` fetches the catalog recipe; a trust failure is a **hard abort** ("we never adopt (register + manage) an unverified recipe", :3540). |
| Lenient parse | :3556–3567 | `source_url` pulled from `[package]` via `toml::Value` (catalog recipes may carry extra keys the strict `ShadowRecipe` struct rejects). |
| Install fields | :3568 | `shadow_cli::extract_install_fields(&recipe_toml)` → `(install_paths, service_name)`. Implementation: `shadow_cli.rs:140–167` (lenient, `[install] install_paths` + `[install] service`). |
| Local-install confirmation | :3570–3592 | `catalog_scan::scan_entry(&name, &recipe_toml)` (`catalog_scan.rs:62–133`): `installed = any install_path exists on disk`; if a `[version] detect_command` exists and runs, `detected_version` is probed. `installed == false` → adopt refused (:3573–3578). |
| Register | :3597–3614 | `register_shadow_package(...)` with `"adopted"` install-source, the full `recipe_toml`, and — the crux — the metadata arguments at :3604–3612. |

### 1.2 The metadata arguments at :3604–3612 (exact state at dirty HEAD)

```rust
"",            // libc: unknown from CLI                                   :3604
"adopted",     // install source                                           :3605
&source_url,   // from [package].source_url                                :3606
&recipe_toml,  // full verified recipe persisted server-side               :3607
&install_paths,// DERIVED from recipe [install].install_paths  (:3568)     :3608
&[],           // config_paths: NOT adopted from recipe install config_paths :3609
&service_name, // DERIVED from recipe [install].service        (:3568)     :3610
"",            // binary_sha256: NOT probed on the adopt path               :3611
false,         // mask_from_pm: opt-in only                                :3612
```

### 1.3 Stale-premise correction

The tasking said adopt "registers with EMPTY metadata" (:3605–3611). **That is now only half true at
dirty HEAD.** Adopt already derives two of the four row-metadata fields from the verified recipe TOML:

- `install_paths` — derived (:3568 → :3608). ✅ populated
- `service` — derived (:3568 → :3610). ✅ populated
- `config_paths` — hard-coded `&[]` with the explicit comment at :3609. ❌ empty
- `binary_sha256` — hard-coded `""` with the explicit comment at :3611. ❌ empty

So the I2-era rationale ("recipe TOML is the trusted source for row metadata once signature
verification passed") **already won on the register path for install_paths/service** — I2 wired
`extract_install_fields`, `extract_config_paths`, and a recipe-derived sha256 into the `--recipe`
register branch (`main.rs:2966–2974` install fields, `:3129–3168` config_paths/service/sha256
selection, comment at :3129–3132 spells out the recipe-vs-detect split). The adopt path is the one
place still carrying the old T4.5 posture. The clash is therefore narrower than "empty metadata":
it is exactly **config_paths + sha256**.

### 1.4 Why the old T4.5 choice existed, and why it now clashes

T4.5's adopt flow was written when row metadata was understood as *detect-derived facts about this
host* — copying `config_paths` out of a recipe felt like trusting a document over the machine. Two
things changed:

1. **I2 set the precedent on the register path** (`main.rs:3129–3168`): after a trusted recipe is in
   hand, recipe-derived row metadata is the norm, and the register path even *hashes* from recipe
   context (`reg_sha256` at :3158–3162 hashes the file at the CLI `path` argument).
2. **The trust gate moved upstream of adoption** (:3538–3554): adopt never runs against an unverified
   recipe. By the time :3609 executes, the TOML has passed the same verification the register path's
   `--recipe` file has. There is no remaining trust asymmetry that justifies dropping recipe-derived
   `config_paths` — only a stale comment.

Consequence if left unfixed: an adopted container-style row (authentik-style: config_paths = bind
roots) has **empty config_paths**, so downstream consumers that read the row (similarity scoring's
config overlap inputs, future P3 diffing baselines, the config_paths hygiene tooling from roadmap
:413) see a package with no configuration surface. The row and its own persisted `recipe_toml`
disagree — a data inconsistency I2 was written to eliminate.

---

## 2. Decision proposal — what adopt should register post-I2

**Recommendation:** adopt derives row metadata from the **verified recipe TOML exactly the way the
`--recipe` register path now does**, with one divergence on sha256.

| Field | Proposal | Rationale |
|---|---|---|
| `config_paths` | `shadow_cli::extract_config_paths(&recipe_toml)` (exists, `shadow_cli.rs:171+`; already used by the register path at `main.rs:3133–3134`) | Same trusted source, zero new parsing code, kills the :3609 inconsistency. For container recipes these are the bind roots — precisely what the roadmap says they should be (:415). |
| `service` | Keep as-is (`extract_install_fields`, already wired :3568→:3610) | Already recipe-derived. For script/container recipes this is typically empty — see §3.3 and Open Question A. |
| `binary_sha256` | **Probe the confirmed local install, not the recipe.** Concretely: if any `install_paths` entry is an existing regular *file*, hash it (reuse `detect::sha256_hex`, same semantics as `main.rs:3158–3162`); otherwise (entries are directories — bind roots, target dirs — or nothing readable) leave `""` | See the for/against below. |
| `libc` | Leave `""` (out of scope; register path also sends `""`, :3094/:3177) | Uniform with both existing paths; not an adopt-specific gap. |

### sha256 on adopt: for / against

**For probing (recommended):**
- The row's sha256 feeds `query_shadow_registry` matching on the register path (`main.rs:2996`) and
  is the strongest similarity weight (+5, `shadow_scorer.rs:10`). An adopted row with `""` can never
  be matched by sha256 by a *later* peer registering the same package — the adopt path silently
  degrades the fleet-wide registry's match quality.
- After :3570–3578 we *know* the install exists; hashing an existing file is one `fs::read` + a hex
  pass, no new failure modes (empty string on unreadable, matching register-path semantics).
- Symmetry: post-change, all three registration flows (detect, `--recipe`, adopt) populate sha256
  when a probeable artifact exists.

**Against probing (the counter-case, taken seriously):**
- A sha256 is only meaningful for a *pinned binary*. Adopted installs are heterogeneous and can be
  updated out-of-band by the operator's own tooling, so the hash can silently go stale — a stale hash
  is arguably worse than an honest empty (it asserts a fact that may be false at query time).
- For container packages there is no single binary at all (see §3), so the probe mostly no-ops there.

**Verdict:** the staleness objection is real but applies equally to the register path's `reg_sha256`
(:3158–3162), which I2 already accepted. Consistency wins; both paths should eventually get the same
staleness treatment (e.g. re-hash at update-check time — P3 territory, noted in the §5 handoff).
Probe files, fall back to `""` for directories/nothing — never hash a directory handle or guess an
executable inside a bind root.

**Explicitly rejected:** deriving `config_paths` from local *detection* on the adopt path (the way the
T4.5 adopt-from-peer branch does at `main.rs:3070–3075`, filtering `report.config_findings` to
existing paths). Adopt has no detect report — it runs `scan_entry`, which returns no config findings.
Fabricating a detect run inside adopt would duplicate the detect pipeline inside a second command for
no gain; the recipe's list is the operator-approved superset and the row already persists the recipe
TOML for reconciliation.

---

## 3. Reference container recipe fixture (authentik-style)

### 3.1 Design constraints from the real fleet observation

The roadmap's referent is the authentik instance observed on the fleet (ct/1904, Alpine/musl/OpenRC):
installed via upstream `docker-compose` script method, all state in **bind-mounted roots** on the
host, **no native init-system unit** (the containers are supervised by the container runtime, not
OpenRC — roadmap :414 records the resulting row had empty service). The fixture must be honest about
that shape while being clearly synthetic (it is a *style* exemplar, not a supported recipe — the
in-repo precedent `recipes/shadow/technitium-dns.toml` shows the documentation-heavy header style).

North-star hooks honored: `downgrade_unsafe = true` (Authentik is the canonical forward-only-migration
example in `docs/design/shadow-packages.md` §5 point 5, which *names* Authentik); `[script] update` is
the existing `RecipeScript { update }` field (`shadow_types.rs:331–334`); health_check reuses the
existing `[install.health_check]` machinery (same shape as technitium's port check).

### 3.2 Proposed fixture — `recipes/shadow/authentik-style-example.toml`

```toml
# Shadow package recipe: authentik-style-example
# SYNTHETIC fixture for the Sprint-31 item-7 adopt exercise. Exemplifies the
# "container-style" recipe shape observed on the fleet (Alpine ct/1904):
#   * [install] method = "script" — the package is deployed by a compose
#     script, not by monopsony's extract/build machinery.
#   * config_paths = the host-side BIND ROOTS (compose mounts), not files —
#     this is the roadmap's "config_paths = bind roots" (:415).
#   * NO native init-system service: the container runtime supervises the
#     processes; [install].service is therefore absent. Downstream code must
#     treat empty `service` as legitimate, not as missing data.
#   * downgrade_unsafe = true — authentik-style apps run forward-only DB
#     migrations; a downgrade bricks the instance (north-star §5.5).
#
# Updates: `docker compose pull && docker compose up -d` inside the deploy
# dir. New upstream env/flag surface lands in .env / compose.yaml — diffing
# that is Sprint 32 Phase 3 and deliberately NOT handled by this recipe.

[package]
name = "authentik-style-example"
description = "Container-style adopt fixture: compose-deployed SSO with bind-root config"
homepage = "https://goauthentik.io"
source_type = "github_release"
source_url = "https://github.com/goauthentik/authentik"
downgrade_unsafe = true

[install]
method = "script"
target_dir = "/opt/authentik-example"
# Host-visible single source of truth for the deployment (compose file).
install_paths = ["/opt/authentik-example/docker-compose.yml"]
# Bind roots mounted into the containers — the package's real config surface.
config_paths = [
  "/opt/authentik-example/.env",
  "/opt/authentik-example/media",
  "/opt/authentik-example/custom-templates",
]
backup_old = false          # containers own their rollback; host files are configs, not artifacts
# No `service` key: container-managed, no systemd/OpenRC unit exists.

# Post-update sanity: the compose-deployed web tier must answer.
[install.health_check]
type = "port"
port = 9000
timeout_secs = 120          # image pull + container start can be slow on first update

[script]
update = "cd /opt/authentik-example && docker compose pull && docker compose up -d"

[version]
# The running container reports its version; no host binary to probe.
detect_command = "docker compose -f /opt/authentik-example/docker-compose.yml exec -T server ak version 2>/dev/null"
detect_pattern = '(\\d+\\.\\d+\\.\\d+)'

[notifications]
changelog_url = "https://github.com/goauthentik/authentik/releases/tag/version%2F{version}"
notes = "Fixture only. Updates run compose pull/up; forward-only migrations make downgrades unsafe (downgrade_unsafe = true)."
```

### 3.3 What does `service` hold for a script-method recipe? → **empty (absent key)**

Options considered:

1. **Absent/empty (recommended for this item).** Matches observed fleet truth (roadmap :414: the real
   authentik row registered with empty service); requires zero code; `extract_install_fields` already
   returns `""` when the key is missing (`shadow_cli.rs` test `extract_install_fields_empty_when_missing`).
   The row is not "missing data" — the package genuinely has no init-system service. Downstream code
   that assumed non-empty service must treat `""` as *container/script-managed*.
2. **Container marker string** (e.g. `service = "container:authentik-style-example"` or
   `compose:authentik-style-example`). Pro: gives P2 (quadlet/service templates,
   `sprint-roadmap.md:436`) a name to key on; makes "this row is container-managed" machine-readable
   in the row without re-parsing recipe TOML. Con: invents a naming convention *before* P2 defines one
   — if P2 lands different vocabulary (quadlet unit names at `/etc/containers/systemd`), the fixture's
   marker becomes debt. **Defer to P2; listed as Open Question A.**
3. **New optional `[install].runtime` field.** Cleanest long-term, but it's a schema change
   (`RecipeInstall`, `shadow_types.rs:162–174`) — out of scope for a design-only item and exactly the
   kind of broadening the north-star's minimal-subset rule says to postpone until a real consumer exists.

Empty it is; the fixture documents the semantics in its header so the choice is self-explanatory.

### 3.4 Fixture placement: in-repo (recommended)

`recipes/shadow/authentik-style-example.toml` ships in-repo next to `technitium-dns.toml`.
In-repo means: unit tests can parse the *actual shipped bytes* (not a copy that rots), reviewers can
see the container shape, and the fresh-host exercise (§4) has a stable artifact to sign and publish.
It is never auto-published — catalog publication stays an operator-signed act, same as technitium's.
Risk of "users think it's a supported recipe" is mitigated by the SYNTHETIC header and the
`-style-example` name.

---

## 4. Adopt-on-fresh-host exercise (end-to-end)

**Fleet surface: ct/1904** (musl/Alpine/OpenRC) — the surface class the roadmap premise is grounded in
(:414–415: authentik observed there; Alpine/OpenRC is the *hard* case because it also exercises
non-systemd absence, not just container absence). ct/1952 (glibc/systemd) runs the negative controls
(§6) to prove adopt still behaves on the primary surface.

Prerequisite ordering: **I1–I5 land first** (operator sign + commit). The §2 change rides on I2's
`extract_config_paths` register wiring; doing adopt work before I2 lands would mean rebasing the same
lines twice.

### Flow

1. **Publish (server side, operator):** sign `authentik-style-example.toml` with the operator key and
   publish to the catalog (same path technitium took; trust policy requires operator signature).
2. **Stage the host (ct/1904):** create `/opt/authentik-example/` with a minimal compose file + `.env`
   + `media/` + `custom-templates/` (bind roots), and a stub `ak version`-capable container OR — for a
   pure no-docker exercise — a `detect_command` shim on PATH. The point under test is *adopt*, not
   compose; the stub keeps the exercise deterministic. (Variant B, full real compose deploy, is the
   optional stretch run recorded in §6.)
3. **Adopt (fresh host, no prior row):**
   `monopsony-client shadow adopt authentik-style-example`
   - :3542 trust check must PASS (signed recipe) — exercise proves the hard-abort path does *not* fire.
   - :3572 `scan_entry` → `install_paths[0]` exists → `installed = true`; `detect_command` yields a
     version → `method = "detect_command"`.
   - :3597 register → row created with `source = "adopted"`.
4. **Inspect the row** (server-side `shadow_packages`):
   - `install_paths = ["/opt/authentik-example/docker-compose.yml"]`
   - `config_paths = ["/opt/authentik-example/.env", ".../media", ".../custom-templates"]` ← the §2
     change; **empty before the change — this is the item's headline assertion**
   - `service = ""` (legitimately; §3.3)
   - `binary_sha256 = ""` (install_paths is a TOML file, not a binary — the §2 file-only probe rule
     correctly no-ops; asserts we didn't hash a directory)
   - `recipe_toml` = full fixture text; `version` = detected; `arch/os` = ct/1904's.
5. **What proves it (acceptance):**
   a. Row contents as in step 4 — specifically config_paths non-empty and byte-equal to the recipe's
      `[install].config_paths`.
   b. **Refusal path:** re-run adopt on ct/1952 *without* staging → exits non-zero with the
      :3574 "not detected on this host; adopt refused" message, no row created.
   c. **Trust path:** publish an unsigned/tampered variant → :3548 "Adopt aborted: recipe trust could
      not be verified", no row, no local writes.
   d. Idempotence/parity: registering the same package via `--recipe` on a third host produces a row
      whose config_paths/service match the adopted row (modulo version/arch) — proving the two paths
      now agree, which is the whole point of resolving the §1.3 clash.

---

## 5. Update-surfacing: minimal seam vs Sprint 32 Phase 3

### What the roadmap asks item 7 to deliver

":415 … updates should surface new flags/env/template changes vs the registered recipe (needs
template diffing)". Read strictly, the second half *is* P3 (:437: "Diff requirements across versions
— env vars, flags, containers, service-unit names…"). Building that here would duplicate P3's 2–3 day
phase. The north-star's own discipline (minimal subset, broaden only when a real consumer exists)
applies to process as much as schema.

### What item 7 delivers — a drift *detector seam*, not a diff engine

**Deliverable: recipe-TOML drift notice.** At update-check time (`check_shadow_updates` path), the
client compares the **recipe TOML registered in the row** (the adopted baseline, persisted at
:3607) against the **current catalog recipe** for that name (already fetched and trust-verified by the
existing check/adopt machinery). Comparison is a byte/sha256 inequality of the two TOMLs — no
parsing, no field-level diffing. On mismatch: surface
`"recipe for <name> drifted from the adopted version — review before updating"`
(interactive) and record it in the check output (non-interactive). That is the honest minimal version
of "surface changes vs the registered recipe": it tells the operator *that* something changed (new
env vars, new flags, edited bind roots — whatever it was), and defers *what exactly* to P3.

Why this seam and not nothing:
- It is implementable with machinery that exists today (both TOMLs are already in hand on both paths;
  the only new code is a comparison + a warning line).
- It closes the actual item-7 risk on the fleet: an adopted container recipe whose catalog copy gains a
  new bind root or env requirement would otherwise update silently against stale assumptions.
- It creates the exact hook P3 replaces: the comparison site. P3 swaps "bytes differ → warn" for
  "field-level diff → structured warnings" without touching any caller.

### What item 7 explicitly defers to P3 (and says so in code comments/doc)

- Field-level diffing of env vars, flags, compose service lists, unit names (:437).
- Any templating/`[build].steps` awareness (P3 rides P1's templating engine, :435).
- Structured "warn when an update needs a recipe edit" classification and the
  failure/rollback-signal join (:437, reuses health-check machinery).
- Stale-sha256 refresh at update-check time (the §2 counter-case; naturally P3 work since P3 already
  re-reads rows at check time).
- Adopt-path auto-migration to the new recipe version (adoption stays a point-in-time act; drift is
  *surfaced*, never auto-applied — consistent with §7's server-gated distribution posture).

### Handoff note P3 will consume (verbatim-ready)

> Adopt-registered rows (post item 7) carry `config_paths` and `service` derived from the verified
> recipe TOML and persist the adopted `recipe_toml` server-side (:3607). The update-check drift seam
> (item 7) marks the single comparison site — byte-inequality of row recipe_toml vs current catalog
> recipe. P3 replaces the byte comparison at that site with field-level diffing over the *same two
> artifacts*; no new data source is needed. Container rows legitimately have `service = ""` (no init
> unit; §3.3) — diff logic must not flag empty service as a regression, and "containers" in P3's
> diff list should key on `[install].install_paths`/compose artifacts for script-method recipes.
> `binary_sha256` on container rows is expected empty (file-only probe rule); P3's staleness
> re-hash should skip directory-only rows rather than treat empty as drift.

---

## 6. Test plan + fleet verification

### Unit tests (client crate; land with the implementation, not this doc)

1. `extract_config_paths` / `extract_install_fields` against the **shipped fixture bytes**
   (`include_str!("../../../recipes/shadow/authentik-style-example.toml")`): bind roots extracted in
   order; service empty; method parses as `Script`. (Guards the fixture against rot — it *is* the
   regression corpus for the container shape.)
2. Adopt-row derivation parity: given the fixture TOML, the §2 derivation yields
   `config_paths = bind_roots`, `sha256 = ""` (no file probeable → no directory hashing) — and given a
   technitium-style TOML with a real file in install_paths, `sha256 = sha256_hex(file)`.
3. `scan_entry` on the fixture: `installed` gates on the compose file's existence; version extraction
   from the `detect_command` pattern.
4. Drift-seam test: registered TOML ≠ catalog TOML → warning emitted / flag set; equal → silent.
   Both with the fixture and a mutated copy.
5. Existing baselines must not move: client 480/0/0, control 26/0/0 at dirty HEAD; clippy delta zero.

### Fleet verification checklist

| Surface | Step | Expected |
|---|---|---|
| ct/1904 (musl/Alpine/OpenRC) | Sign + publish fixture; stage bind roots; `shadow adopt authentik-style-example` | ✓ adopted; row matches §4 step 4 exactly |
| ct/1904 | Mutate catalog recipe (add a config_path), re-sign, run update-check | drift warning fires; update itself still gated by `downgrade_unsafe = true` (no silent non-interactive update) |
| ct/1904 | (stretch, optional) real compose deploy + real `ak version` | same row shape with real version string |
| ct/1952 (glibc/systemd) | Adopt without staging | refused :3574, exit ≠ 0, no row |
| ct/1952 | Adopt with tampered signature | aborted :3548, no row |
| ct/1952 | Regression: adopt `technitium-dns` on a staged host | row identical to pre-change behavior except sha256 now populated (file probe) — the only intended delta |
| both | `cargo test -p monopsony-client`, clippy | baselines hold |

### Size estimate

| Piece | Est. |
|---|---|
| §2 adopt metadata change (config_paths + file-probe sha256 in `main.rs` Adopt arm) | 0.25–0.5 d incl. tests 2 |
| Fixture + tests 1/3 | 0.25 d |
| Drift seam (§5) incl. test 4 | 0.5 d |
| Fleet exercise ct/1904 + ct/1952 controls, doc updates | 0.5–0.75 d |
| **Total** | **~1.5–2 d** |

---

## 7. Open questions (each with a recommendation)

1. **Adopt-path sha256: probe or empty?** — Recommendation: probe **regular files only** among
   install_paths, `""` otherwise (§2). Container rows stay honestly empty; binary rows match the
   register path's semantics and keep similarity scoring's +5 sha256 weight reachable. Revisit
   staleness re-hash at P3.
2. **`service` for script-method/container recipes: empty vs marker (`compose:…`)?** —
   Recommendation: **empty for item 7**; matches fleet truth and adds no invented vocabulary. P2
   (quadlet/service templates, :436) should own the naming convention; revisit there (its outcome
   supersedes this choice).
3. **Fixture in-repo vs fleet-side only?** — Recommendation: **in-repo** at
   `recipes/shadow/authentik-style-example.toml`, clearly marked SYNTHETIC (§3.4). In-repo lets tests
   pin the shipped bytes; fleet-side-only would leave unit tests testing a copy. Rejected alternative:
   committing a redacted dump of the *real* ct/1904 authentik recipe — leaks infrastructure detail and
   still isn't reproducible for tests.
4. **Does the drift seam belong in `check_shadow_updates` (control-side, :1226+) or client-side at
   update time?** — Recommendation: client-side at the update/adopt-check point where both TOMLs are
   already fetched, avoiding the ids-filter bug noted at roadmap :412 and keeping the control crate
   out of Sprint 31's file-conflict set. Flag for the operator to confirm given the :412 fix may land
   in the same sprint.
5. **Should adopt refuse when the recipe has no `detect_command` (version unknowable)?** — Current
   code tolerates it (:3579–3583 renders `<unknown>`). Recommendation: keep tolerating (the register
   path's blank-version rule at :2957–2962 is explicitly a no-recipe/detect-path rule; forcing it on
   adopt would block legitimate container rows whose version probe needs the runtime running).
6. **Publish the fixture to the *production* catalog for the exercise, or a scratch catalog?** —
   Recommendation: scratch/staging catalog on the tailnet server for steps 1–3, production only after
   the negative controls pass; the fixture is synthetic and shouldn't sit in the production catalog
   longer than the exercise.

## 8. Summary of what this item changes vs. what it doesn't

- **Changes:** adopt registers recipe-derived `config_paths` (+ file-probed sha256); container-style
  fixture recipe added; fresh-host adopt exercised on ct/1904 with ct/1952 controls; byte-level
  recipe-drift warning at update-check.
- **Doesn't change:** no template diffing, no field-level update diffing (P3, :437), no service
  vocabulary for containers (P2, :436), no recipe schema changes (`RecipeInstall` untouched), no
  control-crate changes.
- **Premise correction recorded:** adopt was never fully "empty metadata" — install_paths and service
  were already recipe-derived at dirty HEAD (§1.3); the actual gap is config_paths + sha256, and this
  design closes exactly that gap.
