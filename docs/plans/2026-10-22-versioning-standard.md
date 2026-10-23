# Sprint 33 — Versioning standardization

**Date:** 2026-10-22 · **Status:** plan only, not implemented

## Goal

Adopt semver 2.0 with pre-release identifiers and build metadata as the single
versioning convention across all projects. Fix the lying-version bug in
`self_update.rs`. No per-build version churn — traceability comes from build
metadata, not version bumps.

## Background

The repo currently tags `v1.0.0-rc2`. `Cargo.toml` workspace version is the
single source of truth via `env!("CARGO_PKG_VERSION")`. `parse_version()` in
`self_update.rs` strips `-rcN` and compares only the `(major, minor, patch)`
tuple, so `1.0.0-rc1` and `1.0.0-rc2` compare as equal — the lying-version bug
that bit during the shadow registry fleet test.

## Version format

```
MAJOR.MINOR.PATCH[-PRERELEASE][+BUILDMETA]
```

Pre-release levels in order: `alpha.1` → `alpha.2` → ... → `beta.1` → ... →
`rc.1` → ...

Build metadata: `+git.abc123` (git short hash). CI may append `.ci.N`.
Build metadata never affects version ordering.

## Lifecycle

```
Sprint starts, features incomplete
  → 1.1.0-alpha.1+git.abc123

Features complete, internal testing
  → 1.1.0-beta.1+git.abc123

Fleet test round 1
  → 1.1.0-rc.1+git.abc123
  → 1.1.0-rc.1+git.abc123.ci.42   (same code, CI rebuild)

Regression found, fix committed
  → 1.1.0-rc.2+git.def456

Fleet clean, approved
  → 1.1.0                         (tagged, drops pre-release + metadata)
```

The version only changes when the *content* changes. Rebuilding the same
commit for CI produces the same version with different build metadata.

## LTS

LTS is a release channel, not a version component. Lightweight git tags
(`v1.0.0-LTS` pointing at the same commit as `v1.0.0`) mark the channel.
The binary version is unchanged.

## Distro packages

Platform goes in the filename, not the version string:

```
Release tag:  v1.0.0-rc.2
Debian:       monopsony-server_1.0.0-rc.2-1_amd64.deb
Arch:         monopsony-server-1.0.0_rc2-1-x86_64.pkg.tar.zst
RPM:          monopsony-server-1.0.0.rc2-1.x86_64.rpm
```

Build metadata (`+...`) may be appended for distro packages if needed for
traceability, but it must not affect version ordering.

## Files

| File | Change |
|---|---|
| `Cargo.toml` | Add `semver = "1"` to `[workspace.dependencies]` |
| `crates/monopsony-client/Cargo.toml` | Add `semver` dep |
| `crates/monopsony-server/Cargo.toml` | Add `semver` dep (re-exports client self_update) |
| `crates/monopsony-client/src/self_update.rs` | Replace `parse_version()` + `compare_versions()` with `semver::Version`; update tests |
| `docs/version-convention.md` | **NEW** — canonical reference |
| `scripts/bump-version.sh` | **NEW** — version level bumper |
| `CHANGELOG.md` | Add `1.0.0-rc2` entry; note convention change |
| `dist/install.sh` | Update `VERSION_FALLBACK` and `resolve_version()` |
| `.github/workflows/release.yml` | Strip `+...` metadata from tag when packaging |
| `docs/sprint-roadmap.md` | Add Sprint 33 entry |
| `AGENTS.md` | Update version status line |

## Bump script

Fires on version *level* changes, not per build:

```bash
./scripts/bump-version.sh alpha 1   # → 1.0.0-alpha.1
./scripts/bump-version.sh beta 1    # → 1.0.0-beta.1
./scripts/bump-version.sh rc 1      # → 1.0.0-rc.1
./scripts/bump-version.sh release   # → 1.0.0 (drops pre-release, tags)
./scripts/bump-version.sh patch     # → 1.0.1
./scripts/bump-version.sh minor     # → 1.1.0
./scripts/bump-version.sh major     # → 2.0.0
```

Each command reads `Cargo.toml`, computes the next version, writes it back,
and optionally tags. Build metadata is added by CI at build time.

## Tests

| Test | What it covers |
|---|---|
| `test_parse_version_rc_ordering` | `1.0.0-rc1 < 1.0.0-rc2 < 1.0.0` |
| `test_parse_version_pre_release_levels` | `alpha.1 < beta.1 < rc.1 < 1.0.0` |
| `test_parse_version_build_metadata_ignored` | `1.0.0+git.abc == 1.0.0+git.def` |
| `test_parse_version_v_prefix` | `v1.0.0 == 1.0.0` |
| `test_parse_version_invalid` | garbage → error, not panic |
| `bump-version.sh --dry-run` | each subcommand computes the right next version |

## Verify

```bash
cargo check --workspace --locked
cargo test -p monopsony-client self_update -- --nocapture
bash -n scripts/bump-version.sh
bash scripts/bump-version.sh --dry-run alpha 1
bash scripts/bump-version.sh --dry-run release
bash scripts/bump-version.sh --dry-run patch
bash scripts/bump-version.sh --dry-run minor
```

## Do NOT

- Do not bump versions per build — traceability is `+git.abc123`, not version churn
- Do not put LTS in the version string — it's a channel tag, not a version component
- Do not put platform/arch in the version string — it goes in filenames
- Do not conflate `version_ord.rs` (distro version normalizer) with semver — they serve different purposes
- Do not touch any feature code outside `self_update.rs`, `Cargo.toml` files, and the files listed above