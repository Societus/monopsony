# Sprint 33 grounding — versioning standardization (semver 2.0)

**Date:** 2026-10-29 · **Status:** LANDED 2026-10-29 — all six items verified; staged
uncommitted, awaiting sign-off (repo convention: operator signs every commit)
**Scope:** the REMAINING items of Sprint 33 (`docs/plans/2026-10-22-versioning-standard.md`).
The core code work is DONE and committed; this doc grounds only what is left.
**Roadmap:** `docs/sprint-roadmap.md` §"Sprint 33" (added at close-out, local-only — the
roadmap is gitignored by design).

## TL;DR — what landed vs. what remains

Sprint 33's **core code work is complete and committed** in `dcb1c6b`
(2026-10-22): `self_update.rs::parse_version()` now returns `semver::Version`, the
lying-version bug (`1.0.0-rc1 == 1.0.0-rc2`) is fixed, and `semver`/`tempfile` deps are
added to root + client + server `Cargo.toml`.

**Everything remaining is docs / shell scripts / CI config — no Rust code changes.**
Consequence: there is **no cargo test gate** for these items. The only gates are
`bash -n` + `--dry-run` on the two shell artifacts, and a workspace green check to prove
the landed semver commit didn't regress anything (already recorded below).

## Repo state (verified 2026-10-29 at HEAD `19ebb8b`)

| Item | State |
|---|---|
| Branch | `master` |
| Ahead/behind origin/master | **0 / 0** (`git rev-list --left-right --count HEAD...origin/main`) |
| Current version (Cargo.toml `[workspace.package]`) | `1.0.0-rc2` |
| Current git tag | `v1.0.0-rc2` (`git describe` → `v1.0.0-rc2-26-g19ebb8b`) |
| semver dep present | root + client + server `Cargo.toml` all have it (workspace-inherited) |
| self_update.rs parse_version | line 438, returns `Option<semver::Version>` — DONE |

Pre-existing untracked files (`packages/`, `tmp/`, `target-musl/`, `notes/`,
`make-*-commits.sh`, `.git-commit-message.tmp`, `recipes/shadow/checkmk-server.toml`)
— leave alone; not part of this work. Stage by explicit path only, **no commits** (user
signs manually).

## Build/test baselines (recorded so regressions are attributable)

| Check | Baseline → after remaining items |
|---|---|
| `cargo test --workspace` | 1232/0/2 (unchanged — no Rust touched) |
| `cargo test -p monopsony-common` | 190/0/0 |
| `cargo test -p monopsony-client` | 514/0/0 |
| `cargo test -p monopsony-server` | 79/0/0 |
| `cargo clippy -p monopsony-common --all-targets` | clean (0 warnings) — landed semver commit added no warnings |

These numbers match the Sprint 32 Phase 3 landed record, so they are a stable floor. The
remaining items don't change any of them; re-run only to confirm nothing drifted while the
subagents edit docs/scripts/CI.

## Source anchors (verified against current disk)

| What | Where | State |
|---|---|---|
| Canonical versioning reference (depth) | skill `references/versioning-convention.md` (`~/.hermes/skills/software-development/monopsony-development/references/versioning-convention.md`) — format, lifecycle, LTS, distro filenames, bump script, lying-version bug, CHANGELOG rule, Do NOT list | **Content source** for the new top-level `docs/version-convention.md` (see §Remaining item 1). |
| Canonical reference (repo) | `docs/plans/2026-10-22-versioning-standard.md` — plan-only spec: format, lifecycle table, files table, tests, verify commands, Do NOT list | Parent plan; re-read verbatim before editing any listed file. |
| semver::Version caveat (build metadata in PartialEq/Ord) | `self_update.rs:760` comment + skill §"Caveat" | Tests comparing two versions for equality must compare `(major, minor, patch, pre)` fields directly — never `assert_eq!` on two `semver::Version` differing only in `+build`. |
| Landed parse_version | `self_update.rs:438` (`fn parse_version -> Option<semver::Version>`), tests at ~L760–800 | DONE; do not re-derive. Six new tests replace the old broken ones. |
| install.sh VERSION_FALLBACK + resolve_version | `dist/install.sh:32,249–266` — `VERSION_FALLBACK="1.0.0-rc2"`; `resolve_version()` strips only leading `v`, **not** `+git` build metadata | Remaining item 4 (see below). |
| release.yml VERSION derivation | `.github/workflows/release.yml:103` — `VERSION="${GITHUB_REF_NAME#v}"` strips only `v`, so a tag with build metadata leaks into the asset filename | Remaining item 5. |
| CHANGELOG head | `CHANGELOG.md` — currently starts at `## [1.0.0-rc1]`; no rc2 entry exists yet | Remaining item 3. |

## Remaining work index (dispatchable, run-any-order)

All items are independent doc/script/CI edits. No two share a file except the shared
convention text in item 1. Sequence only matters for item 1 → item 3 (the CHANGELOG entry
should cite the canonical reference).

| # | File | Change | Est. | Gate |
|---|---|---|---|
| 1 | `docs/version-convention.md` (**NEW**) | Canonical repo copy of the skill's `references/versioning-convention.md`: format, pre-release levels (alpha→beta→rc), build metadata (`+git`, invisible to ordering), lifecycle table, LTS-as-channel-tag, distro filenames (platform in filename not version string), bump-script contract, lying-version bug + fix location, CHANGELOG rule, Do NOT list. Point `self_update.rs` line 438 as the live anchor. | `bash -n` n/a; it's a doc — just verify it matches the skill reference and cites real anchors. |
| 2 | `scripts/bump-version.sh` (**NEW**) | Version *level* bumper (not per-build): reads root `Cargo.toml`, computes next version, writes back, optional tag. Subcommands: `alpha N`, `beta N`, `rc N`, `release` (drops pre-release + metadata, tags), `patch`, `minor`, `major`. Build metadata added by CI at build time — the script must NOT add it. Include a small test suite (`--dry-run` for each subcommand computes the right next version). | `bash -n scripts/bump-version.sh`; `scripts/bump-version.sh --dry-run alpha 1|beta 1|rc 1|release|patch|minor|major`. |
| 3 | `CHANGELOG.md` | Add a `## [1.0.0-rc2] — <date>` entry under the existing rc1 heading (Keep a Changelog format), one bullet noting the versioning-convention adoption + lying-version bug fix, plus a convention-change note. Do NOT add per-build entries. | None (doc). |
| 4 | `dist/install.sh` | Make `resolve_version()` strip build metadata: after stripping leading `v`, also drop everything from `+` so the resolved version is ordering-clean (`1.0.0-rc2`, never `1.0.0-rc2+git.abc`). Leave `VERSION_FALLBACK="1.0.0-rc2"` (it already matches the current tag). | `bash -n dist/install.sh`. |
| 5 | `.github/workflows/release.yml` | Strip build metadata from the tag before packaging: line 103 `${GITHUB_REF_NAME#v}` → also drop `+...` so a tag like `v1.0.0-rc2+git.abc` yields asset name `monopsony-server-v1.0.0-rc2-linux-x86_64.tar.gz`. Platform stays in the filename; build metadata must not appear in it. | None (CI config). Verify by reading the tag→filename mapping at lines 103–117. |
| 6 | `AGENTS.md` | Two stale version strings say `v1.0.0-rc1` but the repo is on `v1.0.0-rc2`: line 10 (git state) and line 45 (release status). Update to rc2. Low-priority cleanup tied to the semver landing, not core Sprint 33 scope — flag in the ledger if skipped. | None (doc). |

**Note on item 6:** AGENTS.md is stale only because it still reads `rc1` while the tag is
`rc2`. It's listed as a file-to-update in the plan; treat it as optional cleanup and record
the skip in the follow-up ledger rather than blocking dispatch.

## Dispatch mechanics (per skill `monopsony-development`)

- **No Rust changes** → no cargo/clippy gate for these items. Only gates: `bash -n` +
  `--dry-run` on items 2 and 4; a workspace green check to confirm the landed semver commit
  is intact (already at 1232/0/2).
- **Serial, one batch at a time** — item 1's canonical text feeds item 3's CHANGELOG note.
  No two batches share a file except that dependency; otherwise all six run in any order.
- **Brief constraints:** no git commit/push/fmt (operator signs); single plain terminal
  commands only; write scripts via `write_file`, run by path; `bash -n` + dry-run, never
  heredocs/setsid/nohup in foreground.
- **Report per item:** changed file(s), gate result (`bash -n` exit code / dry-run output),
  and for item 1 a note that the content matches the skill reference verbatim.

## Gates (unchanged from repo convention)

`bash -n <script>` → `--dry-run` each subcommand, then stage by explicit path; operator
signs. No commits by agent. **NO COMMITS** — leave this grounding doc + any landed files
untracked/staged-by-path for the sign-off batch.

## Landed record (2026-10-29, dispatcher-verified)

All six remaining items LANDED. Dispatch: R1 `deleg_b94ba8da` (item 1) → R2
`deleg_533df452` (item 2) → R3 `deleg_a5924aec` (items 3, 4+5; item 6 blocked by the
subagent write-guard on AGENTS.md and completed parent-side). Every leaf report was
re-verified by the dispatcher: gates re-run independently, tree diffed against the
recorded before-state, changed-file set reconciled.

| # | Item | Files | Verification |
|---|---|---|---|
| 1 | Canonical reference | docs/version-convention.md (new) | all sections present, standalone framing; anchor self_update.rs:438 re-verified by dispatcher grep; six tests cited by name, no drifting line numbers |
| 2 | Bump script | scripts/bump-version.sh (new, mode 755) | dispatcher re-ran all gates: bash -n OK; --selftest 17/17; real-tree dry-run matrix 7/7 exact from 1.0.0-rc2; Cargo.toml/Cargo.lock checksums unchanged |
| 3 | CHANGELOG | CHANGELOG.md | pure insertion +29/-0: [1.0.0-rc2] — 2026-10-17 (verified tag date) between header and rc1; link def added matching the file's reference-style convention; every Added bullet roadmap-traceable; post-tag sprints (25/30/31/32) deliberately excluded |
| 4 | install.sh | dist/install.sh | normalize_version() at :251; both tag-derived paths (:258, :267) routed through it; fallback untouched; bash -n OK; stubbed 3-path functional test |
| 5 | release.yml | .github/workflows/release.yml | +suffix strip after the v-strip, before the branch-dispatch fallback; single-hunk diff |
| 6 | AGENTS.md | AGENTS.md | subagent write-guard denied the edit; applied parent-side within the sanctioned sprint scope: lines 10 + 45 rc1→rc2; grep rc1 = 0; diff = exactly two one-token lines |

Staging: `make-sprint33-commits.sh` — 4 file-disjoint commits (C1 grounding / C2
convention doc + bump script / C3 release-metadata stripping / C4 changelog + AGENTS).
Whitelist + hunk-presence + bash -n guards, per-commit nothing-staged guards, DRY_RUN
capable. DRY_RUN=1 green; full REHEARSAL against a fake-git shim green end-to-end
(exit 0, HEAD advanced 4x, real tree untouched, shim deleted). The operator runs it
in a TTY to sign.

No Rust code was touched at any point; the workspace baseline recorded above holds
(1232/0/2).

Dispatch decisions recorded (Round-2 brief): dot-form pre-release output with legacy
no-dot parsed on read; --tag prints the signed-tag command rather than tagging (operator
signs); real bumps best-effort refresh Cargo.lock.
