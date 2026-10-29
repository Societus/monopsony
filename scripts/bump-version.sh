#!/usr/bin/env bash
# bump-version.sh — workspace version level bumper (Sprint 33 item 2).
#
# Fires on version LEVEL changes only (alpha/beta/rc/release/patch/minor/major),
# never per build. Build metadata (+git.<hash>) is added by CI at build time;
# this script never adds it and always drops it on write.
#
# Contract: docs/version-convention.md §"Bump script contract" and
# docs/plans/2026-10-22-versioning-standard.md §"Bump script".
#
# Usage:
#   scripts/bump-version.sh alpha N | beta N | rc N | release | patch | minor | major [--dry-run] [--tag]
#   scripts/bump-version.sh --selftest
#
# Notes:
#   - Pre-release is emitted in canonical dot form (1.0.0-rc.3). Input tolerates
#     the legacy no-dot form (1.0.0-rc2) and a leading v. Liberal on read,
#     canonical on write.
#   - --tag only PRINTS the signed-tag command; the operator signs everything
#     (pinentry needs a TTY — never bypass with --no-gpg-sign).
#   - After a REAL write, best-effort `cargo metadata` refresh keeps Cargo.lock
#     in sync. --dry-run writes nothing, including the lock.

set -euo pipefail

SCRIPT_PATH="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/$(basename "${BASH_SOURCE[0]}")"
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# Overridable so --selftest can run against temp-dir fixtures, never the real file.
TARGET_TOML="${BUMP_VERSION_TOML:-$REPO_ROOT/Cargo.toml}"

PROG="$(basename "$0")"

die() {
  echo "$PROG: error: $*" >&2
  exit 1
}

usage() {
  printf 'usage: %s <subcommand> [N] [--dry-run] [--tag]\n' "$PROG"
  printf '\n'
  printf 'subcommands:\n'
  printf '  alpha N | beta N | rc N   set pre-release alpha.N / beta.N / rc.N on the current tuple\n'
  printf '  release                   drop pre-release and build metadata (bare tuple)\n'
  printf '  patch | minor | major     bump that component; drops pre-release + metadata\n'
  printf '\n'
  printf 'flags:\n'
  printf '  --dry-run    print the computed version only; write nothing\n'
  printf '  --tag        print (never run) the git tag -s command for the new version\n'
  printf '  --selftest   run the internal test suite against temp-dir fixtures\n'
}

# ---------------------------------------------------------------------------
# Version parsing / computation
# ---------------------------------------------------------------------------

# Globals set by parse_version: P_MAJOR P_MINOR P_PATCH P_LEVEL P_N
P_MAJOR="" P_MINOR="" P_PATCH="" P_LEVEL="" P_N=""

# parse_version <string>
# Liberal on read: leading v, legacy no-dot pre-release (rc2), build metadata.
# Returns non-zero for anything unparseable; sets the P_* globals on success.
parse_version() {
  local raw="$1"
  local v="$raw"
  P_LEVEL=""
  P_N=""

  # tolerate a leading v
  if [[ "$v" == v* ]]; then
    v="${v#v}"
  fi

  # build metadata is invisible to ordering; dropped on write
  if [[ "$v" == *"+"* ]]; then
    v="${v%%+*}"
  fi

  # split off the pre-release
  local pre=""
  if [[ "$v" == *"-"* ]]; then
    pre="${v#*-}"
    v="${v%%-*}"
    if [[ -z "$pre" ]]; then
      return 1
    fi
  fi

  # core tuple
  local major="" minor="" patch="" extra=""
  IFS='.' read -r major minor patch extra <<< "$v"
  if [[ -z "$major" || -z "$minor" || -z "$patch" || -n "$extra" ]]; then
    return 1
  fi
  if [[ ! "$major" =~ ^[0-9]+$ || ! "$minor" =~ ^[0-9]+$ || ! "$patch" =~ ^[0-9]+$ ]]; then
    return 1
  fi

  # pre-release: accept alpha.N / beta.N / rc.N and the legacy alphaN forms
  if [[ -n "$pre" ]]; then
    local level="" num=""
    if [[ "$pre" == *"."* ]]; then
      level="${pre%%.*}"
      num="${pre#*.}"
      if [[ "$num" == *"."* ]]; then
        return 1
      fi
    else
      if [[ "$pre" =~ ^([a-z]+)([0-9]+)$ ]]; then
        level="${BASH_REMATCH[1]}"
        num="${BASH_REMATCH[2]}"
      else
        return 1
      fi
    fi
    case "$level" in
      alpha|beta|rc) : ;;  # known levels fall through to N validation
      *) return 1 ;;
    esac
    if [[ ! "$num" =~ ^[0-9]+$ ]]; then
      return 1
    fi
    if [[ "$num" -lt 1 ]]; then
      return 1
    fi
    P_LEVEL="$level"
    P_N="$num"
  fi

  P_MAJOR="$major"
  P_MINOR="$minor"
  P_PATCH="$patch"
  return 0
}

# Globals: C_MAJOR C_MINOR C_PATCH (current tuple), NEXT_VERSION (result)
C_MAJOR=0 C_MINOR=0 C_PATCH=0
NEXT_VERSION=""

compute_next() {
  local sub="$1" n="${2:-}"
  case "$sub" in
    alpha|beta|rc)
      NEXT_VERSION="$C_MAJOR.$C_MINOR.$C_PATCH-$sub.$n"
      ;;
    release)
      NEXT_VERSION="$C_MAJOR.$C_MINOR.$C_PATCH"
      ;;
    patch)
      NEXT_VERSION="$C_MAJOR.$C_MINOR.$((C_PATCH + 1))"
      ;;
    minor)
      NEXT_VERSION="$C_MAJOR.$((C_MINOR + 1)).0"
      ;;
    major)
      NEXT_VERSION="$((C_MAJOR + 1)).0.0"
      ;;
    *)
      die "unknown subcommand '$sub'"
      ;;
  esac
}

# ---------------------------------------------------------------------------
# Cargo.toml I/O — strictly scoped to the [workspace.package] table
# ---------------------------------------------------------------------------

# Print the current workspace version. Fails (non-zero) unless exactly one
# version line exists inside [workspace.package].
read_workspace_version() {
  local file="$1"
  if [[ ! -f "$file" ]]; then
    die "target file not found: $file"
  fi
  local ver=""
  ver="$(awk '
    /^[[:space:]]*\[workspace\.package\][[:space:]]*$/ { inws = 1; next }
    /^[[:space:]]*\[/ { inws = 0 }
    inws && /^[[:space:]]*version[[:space:]]*=/ {
      count++
      sub(/^[[:space:]]*version[[:space:]]*=[[:space:]]*"/, "")
      sub(/".*$/, "")
      print
    }
    END { if (count != 1) exit 1 }
  ' "$file")" || die "could not read exactly one version line from [workspace.package] in $file"
  echo "$ver"
}

# Rewrite the version line inside [workspace.package]. Aborts without writing
# unless the table exists and contains exactly one version line. Never touches
# dependency version strings elsewhere in the file.
set_version_in_toml() {
  local file="$1" newver="$2"
  local tmp="" rc=0 mode=""
  mode="$(stat -c %a "$file" 2>/dev/null || echo 644)"
  tmp="$(mktemp "$(dirname "$file")/.bump-version.XXXXXX")"
  awk -v newver="$newver" '
    /^[[:space:]]*\[workspace\.package\][[:space:]]*$/ { inws = 1; seen = 1; print; next }
    /^[[:space:]]*\[/ { inws = 0 }
    inws && /^[[:space:]]*version[[:space:]]*=/ {
      matches++
      if (matches == 1) {
        printf "version = \"%s\"\n", newver
        next
      }
    }
    { print }
    END {
      if (!seen) exit 3
      if (matches == 0) exit 4
      if (matches > 1) exit 5
    }
  ' "$file" > "$tmp" || rc=$?
  if [[ "$rc" -ne 0 ]]; then
    rm -f "$tmp"
    case "$rc" in
      3) die "no [workspace.package] table found in $file" ;;
      4) die "no version line found inside [workspace.package] in $file" ;;
      5) die "multiple version lines inside [workspace.package] in $file; refusing to guess" ;;
      *) die "failed to rewrite $file (awk exit $rc)" ;;
    esac
  fi
  mv "$tmp" "$file"
  chmod "$mode" "$file"
}

# ---------------------------------------------------------------------------
# Main flow
# ---------------------------------------------------------------------------

main() {
  local dry_run=0 want_tag=0 selftest=0
  local sub="" n=""

  while [[ $# -gt 0 ]]; do
    case "$1" in
      --dry-run) dry_run=1 ;;
      --tag) want_tag=1 ;;
      --selftest) selftest=1 ;;
      -h|--help) usage; exit 0 ;;
      -*)
        die "unknown flag: $1"
        ;;
      *)
        if [[ -z "$sub" ]]; then
          sub="$1"
        elif [[ -z "$n" ]]; then
          n="$1"
        else
          die "unexpected argument: $1"
        fi
        ;;
    esac
    shift
  done

  if [[ "$selftest" -eq 1 ]]; then
    run_selftest
    exit 0  # not reached; run_selftest exits
  fi

  if [[ -z "$sub" ]]; then
    usage >&2
    die "no subcommand given"
  fi

  case "$sub" in
    alpha|beta|rc)
      if [[ -z "$n" ]]; then
        die "subcommand '$sub' requires a positive integer N (e.g. $sub 1)"
      fi
      if [[ ! "$n" =~ ^[0-9]+$ ]] || [[ "$n" -lt 1 ]]; then
        die "N must be a positive integer, got '$n'"
      fi
      ;;
    release|patch|minor|major)
      if [[ -n "$n" ]]; then
        die "subcommand '$sub' takes no counter argument"
      fi
      ;;
    *)
      die "unknown subcommand '$sub' (expected: alpha|beta|rc|release|patch|minor|major)"
      ;;
  esac

  CURRENT="$(read_workspace_version "$TARGET_TOML")"

  if ! parse_version "$CURRENT"; then
    die "current version '$CURRENT' in $TARGET_TOML is not a parseable semver MAJOR.MINOR.PATCH[-PRE][+META]"
  fi
  C_MAJOR="$P_MAJOR"
  C_MINOR="$P_MINOR"
  C_PATCH="$P_PATCH"

  compute_next "$sub" "$n"

  if [[ "$want_tag" -eq 1 ]]; then
    echo "# operator: create the signed tag yourself (script never tags):" >&2
    echo "git tag -s v$NEXT_VERSION -m \"v$NEXT_VERSION\"" >&2
  fi

  if [[ "$dry_run" -eq 1 ]]; then
    echo "$NEXT_VERSION"
    exit 0
  fi

  set_version_in_toml "$TARGET_TOML" "$NEXT_VERSION"
  echo "$CURRENT -> $NEXT_VERSION"

  # Best-effort Cargo.lock refresh — only for the real workspace root, and
  # never abort the bump if it fails.
  if [[ "$TARGET_TOML" == "$REPO_ROOT/Cargo.toml" ]]; then
    if command -v cargo >/dev/null 2>&1; then
      if ! cargo metadata --format-version 1 >/dev/null 2>&1; then
        echo "$PROG: warning: 'cargo metadata' failed; Cargo.lock may still hold the old workspace version" >&2
      fi
    else
      echo "$PROG: warning: cargo not found; Cargo.lock not refreshed" >&2
    fi
  fi
  exit 0
}

# ---------------------------------------------------------------------------
# Selftest — runs against temp-dir fixture copies, never the real Cargo.toml
# ---------------------------------------------------------------------------

SELFTEST_DIR=""
PASS_COUNT=0
FAIL_COUNT=0

cleanup_selftest() {
  if [[ -n "$SELFTEST_DIR" && -d "$SELFTEST_DIR" ]]; then
    rm -rf "$SELFTEST_DIR"
  fi
}

checksum() {
  cksum "$1" | cut -d' ' -f1-2
}

make_fixture() {
  # $1 = file, $2 = version, $3 = "deps" | "nodeps"
  local file="$1" ver="$2" with_deps="$3"
  {
    printf '[package]\n'
    printf 'name = "fixture"\n'
    printf '\n'
    printf '[workspace.package]\n'
    printf 'edition = "2021"\n'
    printf 'version = "%s"\n' "$ver"
    printf 'license = "MIT"\n'
    printf '\n'
    printf '[workspace.dependencies]\n'
    if [[ "$with_deps" == "deps" ]]; then
      printf 'serde = { version = "1.0.219", features = ["derive"] }\n'
      printf 'semver = "1"\n'
      printf 'axum = "0.8"\n'
    fi
  } > "$file"
}

pass_case() {
  PASS_COUNT=$((PASS_COUNT + 1))
  echo "PASS: $1"
}

fail_case() {
  FAIL_COUNT=$((FAIL_COUNT + 1))
  echo "FAIL: $1"
}

# Dry-run case: computed version must match expected AND the fixture must stay
# byte-identical (checksum before/after).
case_dry_run() {
  # $1 name, $2 fixture version, $3 deps, $4 sub, $5 n, $6 expected
  local name="$1" ver="$2" deps="$3" sub="$4" n="$5" expected="$6"
  local f="$SELFTEST_DIR/$name.toml"
  make_fixture "$f" "$ver" "$deps"
  local before="" after="" out="" rc=0
  before="$(checksum "$f")"
  out="$(BUMP_VERSION_TOML="$f" "$SCRIPT_PATH" --dry-run $sub $n 2>&1)" && rc=0 || rc=$?
  after="$(checksum "$f")"
  if [[ "$rc" -ne 0 ]]; then
    fail_case "$name — exit $rc, wanted computed version $expected (output: $out)"
    return 0
  fi
  if [[ "$out" != "$expected" ]]; then
    fail_case "$name — computed '$out', wanted '$expected'"
    return 0
  fi
  if [[ "$before" != "$after" ]]; then
    fail_case "$name — --dry-run modified the fixture"
    return 0
  fi
  pass_case "$name — dry-run computes $expected, fixture untouched"
}

# Real-write case: expect_rc 0 verifies the rewrite + dep scoping; non-zero
# verifies the error path left the fixture byte-identical.
case_write() {
  # $1 name, $2 fixture version, $3 deps, $4 sub, $5 n, $6 expected version, $7 expect rc
  local name="$1" ver="$2" deps="$3" sub="$4" n="$5" expected="$6" expect_rc="$7"
  local f="$SELFTEST_DIR/$name.toml"
  make_fixture "$f" "$ver" "$deps"
  local before="" after="" out="" rc=0
  before="$(checksum "$f")"
  out="$(BUMP_VERSION_TOML="$f" "$SCRIPT_PATH" $sub $n 2>&1)" && rc=0 || rc=$?
  after="$(checksum "$f")"
  if [[ "$rc" -ne "$expect_rc" ]]; then
    fail_case "$name — exit $rc, wanted $expect_rc (output: $out)"
    return 0
  fi
  if [[ "$expect_rc" -eq 0 ]]; then
    local ws=""
    ws="$(awk '
      /^[[:space:]]*\[workspace\.package\][[:space:]]*$/ { inws = 1; next }
      /^[[:space:]]*\[/ { inws = 0 }
      inws && /^[[:space:]]*version[[:space:]]*=/ { print }
    ' "$f")"
    if [[ "$ws" != "version = \"$expected\"" ]]; then
      fail_case "$name — workspace line is '$ws', wanted 'version = \"$expected\"'"
      return 0
    fi
    if [[ "$before" == "$after" ]]; then
      fail_case "$name — file unchanged after real write"
      return 0
    fi
    if [[ "$deps" == "deps" ]]; then
      if ! grep -q 'version = "1.0.219"' "$f"; then
        fail_case "$name — dependency version line was touched"
        return 0
      fi
      if ! grep -q 'semver = "1"' "$f"; then
        fail_case "$name — semver dependency line was touched"
        return 0
      fi
    fi
    pass_case "$name — workspace line rewritten to $expected, deps untouched"
  else
    if [[ "$before" != "$after" ]]; then
      fail_case "$name — error path modified the fixture"
      return 0
    fi
    pass_case "$name — rejected with exit $expect_rc, fixture untouched"
  fi
}

run_selftest() {
  SELFTEST_DIR="$(mktemp -d)"
  trap cleanup_selftest EXIT
  PASS_COUNT=0
  FAIL_COUNT=0

  echo "bump-version.sh selftest (fixtures in $SELFTEST_DIR)"
  echo "----"

  # -- dry-run compute cases (each also proves --dry-run is byte-identical)
  case_dry_run "alpha-from-legacy-prerelease" "1.0.0-rc2"             nodeps alpha 1   "1.0.0-alpha.1"
  case_dry_run "beta-from-dot-prerelease"     "1.0.0-rc.1"            nodeps beta 2    "1.0.0-beta.2"
  case_dry_run "rc-from-legacy-no-dot"        "1.0.0-rc2"             nodeps rc 3      "1.0.0-rc.3"
  case_dry_run "rc-from-stable"               "0.9.0"                 nodeps rc 1      "0.9.0-rc.1"
  case_dry_run "release-drops-pre-and-meta"   "1.0.0-rc.2+git.abc123" nodeps release "" "1.0.0"
  case_dry_run "patch-drops-prerelease"       "1.0.0-rc2"             nodeps patch ""  "1.0.1"
  case_dry_run "minor-bump"                   "1.0.0"                 nodeps minor ""  "1.1.0"
  case_dry_run "major-bump"                   "1.0.0"                 nodeps major ""  "2.0.0"
  case_dry_run "meta-dropped-on-rc"           "1.2.3+git.abc"         nodeps rc 1      "1.2.3-rc.1"
  case_dry_run "meta-dropped-on-patch"        "1.0.0+git.abc"         nodeps patch ""  "1.0.1"
  case_dry_run "v-prefix-tolerated"           "v1.0.0"                nodeps minor ""  "1.1.0"

  # -- real-write cases against fixtures
  case_write "scoping-only-workspace-line" "1.0.0-rc2" deps release "" "1.0.0"      0
  case_write "real-write-rc-dot-form"      "1.0.0-rc2" deps rc 3      "1.0.0-rc.3" 0

  # -- error cases: non-zero exit, file untouched
  case_write "invalid-current-version" "banana"    nodeps release   ""  "" 1
  case_write "rc-missing-N"            "1.0.0-rc2" nodeps rc        ""  "" 1
  case_write "rc-zero-N"               "1.0.0"     nodeps rc        0   "" 1
  case_write "unknown-subcommand"      "1.0.0"     nodeps downgrade ""  "" 1

  echo "----"
  echo "selftest: $PASS_COUNT passed, $FAIL_COUNT failed"
  if [[ "$FAIL_COUNT" -gt 0 ]]; then
    exit 1
  fi
  exit 0
}

main "$@"
