#!/bin/sh
# make-shadow-spec-commits.sh — stage + sign the "bring shadow packages up to spec" workstream.
# Repo auto-signs via gpg.isSignCommit=true; the OPERATOR runs this in a TTY (pinentry).
# The agent never signs. Idempotent-ish: aborts if the tree drifts from the whitelisted state.
#
# Usage:  sh make-shadow-spec-commits.sh          # stage + commit both groups
#         DRY_RUN=1 sh make-shadow-spec-commits.sh  # show plan, stage nothing
#
# Commits:
#   1. feat(shadow): align schema to §5 — enum naming, downgrade gate, template engine, service templates
#   2. feat(tracker): persist + serve recipe description and homepage
# Leave-behinds (NOT staged): "-" junk file, make-pairing-commits.sh, packages/*, packages/staging/,
#   target-musl/, tmp/ — handled by their own sign-off scripts.

set -u

C1_PATHS="
crates/monopsony-admin/src/help.rs
crates/monopsony-admin/src/recipe.rs
crates/monopsony-client/src/detect.rs
crates/monopsony-client/src/main.rs
crates/monopsony-client/src/shadow_cli.rs
crates/monopsony-client/src/shadow_executor.rs
crates/monopsony-client/src/shadow_service.rs
crates/monopsony-client/src/templates/monopsony.service.tmpl
crates/monopsony-client/src/templates/monopsony.openrc.tmpl
crates/monopsony-common/src/lib.rs
crates/monopsony-common/src/shadow_template.rs
crates/monopsony-common/src/shadow_types.rs
crates/monopsony-shadow/src/preloaded.rs
crates/monopsony-shadow/src/shadow.rs
crates/monopsony-shadow/src/shadow_git_mirror.rs
crates/monopsony-shadow/src/shadow_recipe.rs
crates/monopsony-shadow/src/shadow_source.rs
recipes/shadow/i2pd.toml
"

C2_PATHS="
crates/monopsony-tracker/src/recipes.rs
"

C3_PATHS="
make-shadow-spec-commits.sh
"

LEAVE_BEHINDS="
-
make-pairing-commits.sh
packages/arch/monopsony-client-1.0.0-rc2
packages/arch/monopsony-client-1.0.0_rc2
packages/arch/monopsony-client-1.0.0_rc2-1-x86_64.pkg.tar.zst
packages/build/
packages/staging/
target-musl/
tmp/
crates/monopsony-client/src/templates/
"

C1_MSG='feat(shadow): align shadow schema to §5 — enum naming, downgrade gate, template engine, service templates

Bring the shadow package system up to the evolved design spec
(docs/design/shadow-packages.md §3/§5). Four sub-changes, one commit because
main.rs carries hunks of each and per-commit compilability requires them to
travel together:

- source_type enum: GitBuild -> GitRelease across 13 files to match the §5
  value list (direct_url / git_release / github_release); CLI help, test
  names, and the i2pd recipe updated with the rename. No BinarySha256
  variant: §5 lists it, but nothing uses it as a source type yet.
- downgrade_unsafe enforcement (§3 invariant #5): execute_update now refuses
  a downgrade when the recipe sets downgrade_unsafe, the update is
  non-interactive, and no health_check makes success verifiable. Refusal
  happens at plan stage (nothing to roll back). is_downgrade() compares via
  semver (rc1 < rc2 < release) and fails open on unparseable versions.
  Gate inputs bundled into DowngradeContext at the execute_inner boundary.
- provisioning template engine (§3 invariant #1): monopsony-common::
  shadow_template::render_step — {{var}} substitution + simple
  {{#if var == "literal"}} conditionals, zero new dependencies, fail-loud on
  unknown variables. Engine + tests only; [build].steps has no consumer
  until the executor gains build support (deliberate, minimal-subset).
- init-system service scripts (§3 invariant #2): monopsony-client::
  shadow_service — os-release detection (systemd default, OpenRC explicit),
  embedded systemd + OpenRC templates, fail-loud renderer, blank
  no-package generation with REPLACE_ME markers. Library layer only;
  executor/CLI wiring and quadlets are later phases.

Also fixes a pre-existing needless_borrow in a shadow_types test.

LLM-drafted implementation, reviewed and verified in-session.
Tested on: bare-metal Arch workstation; cargo test --workspace 1165 passed /
0 failed (1121 baseline pre-session); clippy 0 new warnings on
monopsony-common, monopsony-client, monopsony-tracker. NOT live-deployed.'

C2_MSG='feat(tracker): persist + serve recipe description and homepage

Recipe description and homepage were parsed and signed but not served:
the shadow_recipes table had a description column yet search/get SELECTs
did not return both fields, and homepage was not stored at all.

- shadow_recipes gains a homepage TEXT column via an idempotent
  migration guard (PRAGMA table_info check + ALTER), so fresh test DBs
  and existing server DBs both work; the migration runs as real Rust
  after the execute_batch block rather than inside the SQL string.
- publish stores homepage (INSERT + ON CONFLICT DO UPDATE); search and
  detail SELECTs return description + homepage with aligned row indices.
- golden round-trip test: publish with both fields, GET /recipes/:id,
  assert both survive.

grpc.rs register/detect core is deliberately untouched — these are
informational catalog fields, not provisioning semantics (§4 gate).

LLM-drafted implementation, reviewed and verified in-session.
Tested on: bare-metal Arch workstation; cargo test -p monopsony-tracker
50 passed / 0 failed (46 baseline); clippy clean. NOT live-deployed.'

C3_MSG='chore(scripts): add shadow-spec sign-off staging script

Guarded staging script for the two shadow-spec commits: whitelist guard
aborts on any tree drift, per-group nothing-staged guards, DRY_RUN mode.
Follows the established make-commits.sh pattern; operator signs in TTY.'

# ---------- guards ----------

DIRTY=$(git status --porcelain | sed 's/^"//; s/"$//; s/^...//' | sort -u)
# untracked dirs show as "dir/" in porcelain; normalize for comparison
ABORT=0
for p in $DIRTY; do
  found=0
  for w in $C1_PATHS $C2_PATHS $C3_PATHS $LEAVE_BEHINDS; do
    if [ "$p" = "$w" ] || [ "$p" = "${w%/}" ] || [ "${p#/}" = "$w" ]; then found=1; break; fi
    case "$w" in
      */) case "$p" in "$w"*) found=1; break;; esac;;
    esac
  done
  if [ "$found" = "0" ]; then
    echo "GUARD FAIL: dirty path not whitelisted: $p"
    ABORT=1
  fi
done
if [ "$ABORT" = "1" ]; then
  echo "Tree drifted from the expected state — fix the whitelist or clean up, then re-run."
  exit 1
fi
echo "Guard OK: every dirty path is whitelisted."

commit_group() {
  name="$1"; msg="$2"; shift 2
  if [ "${DRY_RUN:-0}" = "1" ]; then
    echo "DRY-RUN commit [$name]: would stage:"
    for f in "$@"; do echo "    $f"; done
    return 0
  fi
  git add -- "$@" || { echo "STAGE FAIL [$name]"; exit 1; }
  if git diff --cached --quiet; then
    echo "NOTHING STAGED for [$name] — pathspec bug? Aborting."
    exit 1
  fi
  git commit -m "$msg" || { echo "COMMIT FAIL [$name]"; exit 1; }
  echo "Committed [$name]: $(git diff --cached --name-only | wc -l) files (post-commit log below)"
  git log --oneline -1
}

echo "=============================================="
commit_group "schema-alignment" "$C1_MSG" $C1_PATHS
echo "=============================================="
commit_group "tracker-homepage" "$C2_MSG" $C2_PATHS
echo "=============================================="
commit_group "staging-script" "$C3_MSG" $C3_PATHS
echo "=============================================="
if [ "${DRY_RUN:-0}" = "1" ]; then
  echo "DRY-RUN complete — nothing staged, nothing committed."
else
  echo "Both commits landed. Tree should now hold only leave-behinds:"
  git status --short
fi
