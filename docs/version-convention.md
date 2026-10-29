# Version convention

Canonical repo-level reference for versioning Monopsony: version format, pre-release
levels, build metadata, release lifecycle, LTS channel, distro packaging, the version
bump script, `self_update.rs` comparison, release CI, and `CHANGELOG.md` entries.

## Version format

```
MAJOR.MINOR.PATCH[-PRERELEASE][+BUILDMETA]
```

This is semver 2.0 §2 and §9. Every tool in the Rust ecosystem handles it natively.

## Pre-release levels

Three levels, in order:

| Level | Meaning | When it appears |
|-------|---------|-----------------|
| `-alpha.N` | Early development, incomplete features | Code compiles, major pieces not wired |
| `-beta.N` | Feature-complete, internal testing | All features present, not fleet-tested |
| `-rc.N` | Release candidate, fleet-tested | Proven in fleet, ready for release decision |

Ordering: `alpha.N < beta.N < rc.N < stable`. The `N` counter resets to 1 whenever the
`MAJOR.MINOR.PATCH` tuple changes.

## Build metadata

`+git.<short-hash>` (git short hash). CI may append `.ci.N` for rebuild counts. The
`+` part is invisible to version ordering — `1.0.0-rc.2+git.abc123` and
`1.0.0-rc.2+git.def456` are the *same version* for comparison purposes; they differ
only in which build produced the binary.

Build metadata is added by CI at build time, never by hand and never by the bump
script.

## Lifecycle

| Stage | Version |
|-------|---------|
| Sprint starts, features incomplete | `1.1.0-alpha.1+git.abc123` |
| Features complete, internal testing | `1.1.0-beta.1+git.abc123` |
| Fleet test round 1 | `1.1.0-rc.1+git.abc123` |
| Same code rebuilt by CI | `1.1.0-rc.1+git.abc123.ci.42` — same version, only build metadata changes |
| Regression found, fix committed | `1.1.0-rc.2+git.def456` |
| Fleet clean, approved | `1.1.0` — tagged, drops pre-release and metadata |

The version only changes when the *content* changes. Rebuilding the same commit for
CI produces the same version with different build metadata.

## LTS

LTS is a **release channel**, not a version component. A lightweight git tag
(`v1.0.0-LTS`, pointing at the same commit as `v1.0.0`) marks the channel. The
binary's version string is unchanged. Nothing LTS-related ever appears in
`MAJOR.MINOR.PATCH[-PRERELEASE][+BUILDMETA]`.

## Distro packages

Platform and architecture go in the **filename**, never the version string:

```
Release tag:  v1.0.0-rc.2
Debian:       monopsony-server_1.0.0-rc.2-1_amd64.deb
Arch:         monopsony-server-1.0.0_rc2-1-x86_64.pkg.tar.zst
RPM:          monopsony-server-1.0.0.rc2-1.x86_64.rpm
```

## Bump script contract

`scripts/bump-version.sh` fires on version **level** changes only — never per build:

```bash
./scripts/bump-version.sh alpha 1   # → 1.0.0-alpha.1
./scripts/bump-version.sh beta 1    # → 1.0.0-beta.1
./scripts/bump-version.sh rc 1      # → 1.0.0-rc.1
./scripts/bump-version.sh release   # → 1.0.0 (drops pre-release, tags)
./scripts/bump-version.sh patch     # → 1.0.1
./scripts/bump-version.sh minor     # → 1.1.0
./scripts/bump-version.sh major     # → 2.0.0
```

Each subcommand reads the root `Cargo.toml`, computes the next version, writes it
back, and optionally tags. The script must **not** add build metadata — CI does that
at build time.

## The lying-version bug (fixed)

`parse_version` at `crates/monopsony-client/src/self_update.rs:438` previously
stripped the pre-release suffix and compared only the `(major, minor, patch)` tuple,
so `1.0.0-rc1` and `1.0.0-rc2` compared as equal — self-update reported "already at
latest" while a newer rc was deployed. This bug motivated the versioning convention.

**Fix (landed):** `parse_version` now returns `Option<semver::Version>` via
`semver::Version::parse()` (the `semver = "1"` workspace dependency is inherited by
`monopsony-client` and `monopsony-server`), which correctly orders
`1.0.0-rc1 < 1.0.0-rc2 < 1.0.0`. The old tests that encoded the broken behavior were
replaced with six tests:

| Test | What it asserts |
|------|-----------------|
| `test_parse_version` | `(major, minor, patch)` + `pre`/`build` fields parsed correctly |
| `test_parse_version_rc_ordering` | `rc1 < rc2 < stable` |
| `test_parse_version_pre_release_levels` | `alpha.1 < beta.1 < rc.1 < stable` |
| `test_parse_version_build_metadata_ignored` | same core version, different build metadata compares equal for ordering |
| `test_parse_version_v_prefix` | `v1.0.0 == 1.0.0` |
| `test_parse_version_invalid` | garbage input → `None`, no panic |

### semver crate caveat

`semver::Version`'s `PartialEq` and `Ord` **include** build metadata — a deviation
from semver spec §11, where build metadata is ignored. Equality checks on two
versions that differ only in `+build` must compare the ordering-relevant fields
`(major, minor, patch, pre)` directly; do not use `assert_eq!` on two
`semver::Version` values that differ only in build metadata. (The `check_for_update`
comparison uses `current.cmp(&latest)` and is unaffected, since release tags carry
no build metadata.)

## Release CI

`.github/workflows/release.yml` derives the packaging `VERSION` from the git tag by
stripping the leading `v` **and** dropping any `+...` build-metadata suffix, so
assets stay clean:

```
Tag:     v1.0.0-rc.2+git.abc123
Version: 1.0.0-rc.2
Asset:   monopsony-server-v1.0.0-rc.2-linux-x86_64.tar.gz
```

Platform goes in filenames only; build metadata must never leak into an asset name.

## CHANGELOG

One entry per release (stable or rc), never per build — build metadata lives in CI
logs, not the changelog. Follow the Keep a Changelog format.

## Do NOT

- Do not bump versions per build — traceability comes from `+git.<short-hash>`, not
  version churn.
- Do not put LTS in the version string — it is a channel tag, not a version
  component.
- Do not put platform/arch in the version string — it goes in filenames.
- Do not conflate `version_ord.rs` (distro version normalizer) with semver — they
  serve different purposes.
- Do not touch feature code outside the versioning files (`self_update.rs`,
  `Cargo.toml` files, and the doc/script/CI files this convention governs).
