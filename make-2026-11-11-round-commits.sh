#!/usr/bin/env bash
# make-2026-11-11-round-commits.sh — stage + sign the 2026-11-11 round: 9 lanes -> 9 commits.
#
# Repo signs via commit.gpgsign=true + explicit -S; the OPERATOR runs this in a TTY
# (pinentry-curses renders in any TTY, including a phone SSH client). The agent never
# signs. Rehearsed end-to-end 2026-11-11 with --no-gpg-sign + `git reset --mixed HEAD~9`
# + byte-untouched porcelain proof before handoff.
#
# Usage:  bash make-2026-11-11-round-commits.sh            # guards + 9 signed commits
#         DRY_RUN=1 bash make-2026-11-11-round-commits.sh  # guards + preflights only
#         RESUME=1 bash make-2026-11-11-round-commits.sh   # after a mid-train sign failure
#
# Commits (in order — dependency-ordered, shared files whole-file per hotspot rule):
#   G1 feat(grpc):    report update rollout outcomes over the wire (Sprint 34 P4a)
#   G2 feat(server):  guarded self-apply + rollout admin REST (Sprint 34 P4a)
#   G3 feat(dashboard): updates panel, rollout creator, fleet version skew (Sprint 34 P4b)
#   G4 docs(plans):   non-systemd detection + container adoption options (Sprint 31)
#   G5 feat(client):  OpenRC detection tier + container recipe adoption (Sprint 31)
#   G6 fix(client):   shadow_scorer config_agreement path canonicalization
#   G7 docs(plans):   store-source spikes - Accrescent, Aurora, GrapheneOS, gate summary
#   G8 docs(agents):  pairing-v2 plan status - P0-P7 landed, uncommitted line was stale
#   G9 chore(scripts): this script (house practice: train scripts are tracked; e324708)
#
# NEVER stages docs/sprint-roadmap.md (gitignored; a runtime assert aborts if it ever
# appears in a staging list). context/kanban-progress-*.md are intentionally left
# UNSTAGED for operator review (the orchestrator clears them after review).
#
# Stray cleanup, recorded here per round card: the junk files '-' (empty, Oct 21) and
# '.git-commit-message.tmp' (stale Oct 27 commit-message draft, long since landed as
# a0a26a9) are DELETED at the start of a real run.
#
# Leave-behinds (never staged): AGENTS-sanctioned-diff-*.patch x3, packages/ (+arch,
# +build, +staging), target-musl/, tmp/ (spike corpus, prepare_corpus.py,
# finish-m2mini-launchd.sh), scripts/invalidate-checksums.sh (parked).
#
# This script does NOT push. Review with `git log --stat -9`; pushing is yours.

set -u

BASE=5675f0b93e805c4589cc1ffe7b55aa0d7230ac51
DRY_RUN=${DRY_RUN:-0}
RESUME=${RESUME:-0}
SELF=make-2026-11-11-round-commits.sh

die() { printf 'ABORT: %s\n' "$*" >&2; exit 1; }

REPO=$(git -C "$(dirname "${BASH_SOURCE[0]}")" rev-parse --show-toplevel) || die "cannot resolve repo root from script location"
cd "$REPO" || die "cannot cd to $REPO"

MSGDIR=$(mktemp -d) || die "mktemp failed"
trap 'rm -rf "$MSGDIR"' EXIT

# ---------------------------------------------------------------- messages ---

cat > "$MSGDIR/g1.msg" <<'EOF'
feat(grpc): report update rollout outcomes over the wire (Sprint 34 P4a)

Rollout 'applied' normally resolves server-side via the heartbeat
version funnel (a client whose reported monopsony_version reaches its
assigned target is applied without any explicit call); refusals are
only observable client-side, so they now travel on their own RPC.

- proto: rpc ReportUpdateOutcome + messages — the client's verdict on
  one rollout assignment (outcome "applied"|"refused", from_version,
  detail); response.resolved is true iff an open assignment row was
  found and transitioned.
- control/grpc.rs: handler resolves the open (pending|delivered) row
  for (client_id, version) to applied|refused; adds pub
  contract_version_cmp, the release-contract comparator shared with
  the admin self-apply guard (converged can never disagree with
  offered).
- db.rs: list_open_update_assignments / list_update_assignments /
  list_client_monopsony_versions.
- client grpc_client.rs: report_update_outcome call site + update_mock
  stand-in arm. Shared-file hotspot (whole-file add, itemized here so
  no later group touches this file): also carries the S31 lane's
  removal of a vestigial #[allow(dead_code)] on list_shadow_packages
  (already called from main.rs at this train's base — no warning at
  any intermediate tree) and a rustfmt reflow of one test call.

Gates at round close: cargo test --workspace --locked 1570 passed /
0 failed / 3 ignored; clippy exactly the 15 pre-existing locations.

LLM-drafted: yes (Hermes Agent). Tested on: the round tree's gate run
above; the wire change rides the same binaries bench-verified on
ct/1952 (Debian/systemd), ct/1904 (Alpine/OpenRC) and m2mini
(macOS/launchd), where applied-outcomes resolved via the heartbeat
funnel as designed. NOT live-deployed beyond the bench.
EOF

cat > "$MSGDIR/g2.msg" <<'EOF'
feat(server): guarded self-apply + rollout admin REST (Sprint 34 P4a)

Server half of P4a: staged self-apply for the server binary itself,
plus the rollout bookkeeping the dashboard drives. Admin gains a
monopsony-client path dep; Cargo.toml and the one-line Cargo.lock
entry travel together (locked builds must not drift).

- server main.rs: hidden self-update subcommands Finalize
  {--command-file} (phase 2 of a staged self-apply, spawned detached
  by the admin endpoint) and Rollback (restore newest backup).
- server self_update.rs: the Finalize arm is PID-guarded
  (acquire_finalize_guard), reads the command file, swaps the staged
  binary, restarts via detected init, health-gates, and rolls back on
  failure — delegating to the client crate's P3 machinery
  (read_finalize_command / run_finalize / rollback_latest_backup).
- admin: POST /api/v1/updates/rollout (create or reset per-client
  assignments; explicit ids or all), GET /api/v1/updates/rollouts
  (assignments + ack state), POST /api/v1/updates/self-apply behind
  evaluate_self_apply_guard — a client whose reported version is below
  the target blocks with 409 + per-client conflict rows unless
  override_guard is set; an unparseable report counts as lagging
  (fail-safe: garbage must never read as converged); clients with no
  reported version (legacy daemons) are listed as unknown and never
  block; empty fleet is not blocked; comparison uses the same
  contract_version_cmp as the distribution gate.
- guard tests: converged/lagging/unknown classification, prerelease
  ordering vs the release contract, unparseable-as-lagging, clear and
  empty fleet not blocked, request override-alias deserialization.

Gates at round close: workspace 1570/0/3; admin crate 87/0/0; clippy
exactly the 15 pre-existing locations.

LLM-drafted: yes (Hermes Agent). Tested on: bench server (Debian,
systemd) swapped to the P4a binary — guard ladder fired live: 400
empty version, 200 idempotent no-op at the current target
(already_running_target_or_newer, staged:false), 404 unknown release;
the 409 convergence rung is unit-tested only (no stored release above
rc2 exists to point at). NOT live-deployed beyond the bench.
EOF

cat > "$MSGDIR/g3.msg" <<'EOF'
feat(dashboard): updates panel, rollout creator, fleet version skew (Sprint 34 P4b)

- lib/utils/version.ts (new): TypeScript mirror of the
  release-contract comparator (contract_version_precedence /
  ContractVersion Ord / prerelease rules) so the fleet version column
  and skew badge count releases exactly the way the server's ack
  funnel and self-apply guard do; kept in sync with
  crates/monopsony-control/src/grpc.rs by comment contract.
- routes/updates/+page.svelte (new): latest release + per-platform
  asset completeness, "Check upstream now", rollout creator with
  per-client multiselect, ack-state table (pending/delivered/applied/
  refused), and the guarded self-apply panel — pre-submit guard
  warning, controls disabled until a release exists, structured 409
  conflict rows readable at the call site.
- routes/clients/+page.svelte: Version column (reported version, or
  "legacy" when the daemon never reported — never the raw 0) and
  Updates column ("0 security / 0 other" for reporting daemons, an
  em dash otherwise).
- +layout.svelte: Updates nav entry. api/client.ts: updates.{list,
  check, rollouts, createRollout, selfApply}; request errors now carry
  status + parsed body so guard conflicts surface. api/types.ts: the
  update / rollout / self-apply / guard-conflict types.

Gates at lane close: svelte-check 0 errors / 0 warnings; npm run build
green; workspace 1551/0/3.

LLM-drafted: yes (Hermes Agent). Tested on: live against the bench
server — /updates renders the rollout multiselect with fleet versions
(authentik and sonarr at 1.0.0-rc2, every non-upgraded client as
legacy) and the self-apply guard warning; /clients shows the version
skew (rc2 x2 vs legacy x14) and the Updates columns. NOT
live-deployed beyond the bench.
EOF

cat > "$MSGDIR/g4.msg" <<'EOF'
docs(plans): non-systemd detection + container adoption options, operator-decided (Sprint 31)

docs/plans/2026-11-11-s31-nonsystemd-detection-options.md — options
doc for the Sprint 31 rows 6-7 re-scope: service-detection designs
(1A OpenRC tier among them), container recipe-adoption designs
(2A/2B), and the Q5 content-hash scope question. Carries the operator
Decision section verbatim: "Choices 1A and 2B approved to continue",
"Q5 = ELF only." Plan-only; no code changed by that card.
EOF

cat > "$MSGDIR/g5.msg" <<'EOF'
feat(client): OpenRC detection tier + container recipe adoption (Sprint 31)

Implements the operator-picked design from
docs/plans/2026-11-11-s31-nonsystemd-detection-options.md (1A + 2B,
Q5 ELF-only).

- detect.rs: OpenRC detection tier — init.d scripts classify by
  shebang and depend() blocks, /etc/runlevels enablement lifts
  confidence, plain-sh scripts stay SysV, and a busybox-symlink
  target never claims a service (honest "none" for e.g. crond).
  Tier-2 container cgroup markers: podman/libpod/docker scopes
  resolve to a container short-id + engine hint; systemd slices still
  claim their unit; without either, proc/exe claims nothing (no more
  empty service rows). Draft TOML renderer follows the detected
  lifecycle: OpenRC hints carry rc-update/rc-service, container hints
  are placeholders (never systemctl), and no lifecycle hint is
  emitted with an empty unit_name. +11 tests.
- main.rs: item-7 adoption metadata — adopt and remote-assigned
  registration now carry the previous recipe plus an ELF-only content
  hash for the config_paths shape (elf_only_sha256, the Q5 decision);
  previous_recipe_from_rows surfaces the prior recipe in drafts and
  tolerates unparsable stored bytes as None, not an error. +6 tests.
- recipes/shadow/authentik-style-example.toml: synthetic
  container-style fixture (script install method, bind-root
  config_paths) for the adopt exercise.

Gates at lane close: monopsony-client 628 -> 645 passed / 0 failed;
workspace 1568/0/3 at lane exit (1570/0/3 at round close after the
scorer fix); clippy exactly the pre-existing set.

LLM-drafted: yes (Hermes Agent). Tested on: bench fleet — OpenRC tier
verified live on ct/1904 (Alpine/OpenRC): monopsony-client matches
its own service via the openrc tier, podman-compose attributes to the
authentik wrapper service, busybox crond stays honest "none"; sonarr
draft byte-stable pre/post apart from the documented transforms;
fixture registered live on the bench server. NOT live-deployed beyond
the bench.
EOF

cat > "$MSGDIR/g6.msg" <<'EOF'
fix(client): shadow_scorer config_agreement path canonicalization

config_agreement compared peer and local config paths as exact
strings, so a trailing-slash spelling difference (/var/lib/sonarr vs
/var/lib/sonarr/) produced a false disagreement vote between peers
describing the same layout.

Add a private normalize_config_path (strip one trailing '/', an
all-slash path collapses to root) and apply it to BOTH sides of the
peer-vs-local comparison only — signature, return shape, comment
strings, and every other scoring rule unchanged. Two regression
tests: trailing-slash pairs now agree; genuinely different paths
still disagree after canonicalization.

Gates: monopsony-client 647 passed / 0 failed (645 + the two new);
clippy exactly the 7 pre-existing client-crate locations, zero from
this file; the two hunks this change wrote are rustfmt-clean (the
file is otherwise deliberately left unformatted to keep the diff
minimal).

Tested on: cargo test -p monopsony-client on the round tree (this
workstation); no runtime behavior beyond the comparison inputs
changed.
EOF

cat > "$MSGDIR/g7.msg" <<'EOF'
docs(plans): store-source spikes - Accrescent, Aurora, GrapheneOS, gate summary

- store-spike-accrescent.md: PARTIAL / MEDIUM — individually cacheable
  developer-signed APKs at best (DirectUrl); the project rejects
  third-party repos by design and publishes no index or wire format,
  so there is nothing store-level to mirror.
- store-spike-aurora.md: NOT-FEASIBLE / MEDIUM-HIGH — download URLs
  are minted inside a credential-gated reverse-engineered session;
  anonymous mode rides dispensers pooling real Google accounts and
  fleet aggregation triggers Google-side invalidations.
- store-spike-grapheneos.md: NOT-FEASIBLE / MEDIUM-HIGH — the cleanest
  wire contract of the five (static HTTPS, signify-signed metadata,
  fs-verity end-to-end verification) but the OS-integrated client is
  hard-wired to the origin: no redirect path, no mirror program.
- store-spike-gate-summary.md: five-store verdict table (F-Droid
  FEASIBLE, Obtainium/Accrescent PARTIAL, Aurora/GrapheneOS
  NOT-FEASIBLE) + gate arithmetic: 2 banked pre-card + 3 evening
  spikes = 5 of 5; kickoff remains gated on the VpnService policy
  review and the PWA-vs-native management-app call.

Doc-only; no code.
EOF

cat > "$MSGDIR/g8.msg" <<'EOF'
docs(agents): pairing-v2 plan status - P0-P7 landed, uncommitted line was stale

The pairing-v2 security-notes header still described the plan as
"P0-P7 complete, uncommitted". Supersession was since proven via git
log: P0 1fc18fa, P1 1bc95d0, P2/P4/P5 b823d80, P3/P5/P6 183d2aa,
P7 f4ce1fc, follow-ups 8361c51. The status line now records the
landed hashes instead.

Parent-owned correction, staged verbatim by the round commit train;
the train itself does not edit this file.
EOF

cat > "$MSGDIR/g9.msg" <<'EOF'
chore(scripts): add 2026-11-11 round commit-train script

Guarded 9-group train consolidating the round's lanes into one
operator-signed sequence: P4a wire contract, P4a server self-apply +
admin REST, P4b dashboard, S31 design doc, S31 client impl, scorer
path fix, store-spike docs, the AGENTS.md pairing-v2 status
correction, and this script. Exact modified-set guard + untracked
allowlist (aborts on tree drift), per-group symbol preflights (a
whitelist proves file dirtiness, not hunk presence), staged-set
equality after every add, RESUME=1 via per-group OPEN flags (never an
empty-dirty-set), DRY_RUN mode, hard runtime assert that
docs/sprint-roadmap.md is never staged, and context/kanban-progress-*
left unstaged for operator review. Rehearsed end-to-end with
--no-gpg-sign + git reset --mixed + byte-untouched porcelain proof
before handoff. Follows the make-shadow-spec-commits.sh pattern
(e324708).
EOF

# ------------------------------------------------------------ group lists ---

G1_SUBJ='feat(grpc): report update rollout outcomes over the wire (Sprint 34 P4a)'
G1_T=(proto/monopsony.proto crates/monopsony-db/src/db.rs crates/monopsony-control/src/grpc.rs crates/monopsony-client/src/grpc_client.rs)
G1_N=()

G2_SUBJ='feat(server): guarded self-apply + rollout admin REST (Sprint 34 P4a)'
G2_T=(crates/monopsony-server/src/main.rs crates/monopsony-server/src/self_update.rs crates/monopsony-admin/Cargo.toml crates/monopsony-admin/src/lib.rs crates/monopsony-admin/src/updates.rs Cargo.lock)
G2_N=()

G3_SUBJ='feat(dashboard): updates panel, rollout creator, fleet version skew (Sprint 34 P4b)'
G3_T=(dashboard/src/lib/api/types.ts dashboard/src/lib/api/client.ts dashboard/src/routes/+layout.svelte dashboard/src/routes/clients/+page.svelte)
G3_N=(dashboard/src/lib/utils/version.ts dashboard/src/routes/updates/+page.svelte)

G4_SUBJ='docs(plans): non-systemd detection + container adoption options, operator-decided (Sprint 31)'
G4_T=()
G4_N=(docs/plans/2026-11-11-s31-nonsystemd-detection-options.md)

G5_SUBJ='feat(client): OpenRC detection tier + container recipe adoption (Sprint 31)'
G5_T=(crates/monopsony-client/src/detect.rs crates/monopsony-client/src/main.rs)
G5_N=(recipes/shadow/authentik-style-example.toml)

G6_SUBJ='fix(client): shadow_scorer config_agreement path canonicalization'
G6_T=(crates/monopsony-client/src/shadow_scorer.rs)
G6_N=()

G7_SUBJ='docs(plans): store-source spikes - Accrescent, Aurora, GrapheneOS, gate summary'
G7_T=()
G7_N=(docs/plans/2026-11-11-store-spike-accrescent.md docs/plans/2026-11-11-store-spike-aurora.md docs/plans/2026-11-11-store-spike-grapheneos.md docs/plans/2026-11-11-store-spike-gate-summary.md)

G8_SUBJ='docs(agents): pairing-v2 plan status - P0-P7 landed, uncommitted line was stale'
G8_T=(AGENTS.md)
G8_N=()

G9_SUBJ='chore(scripts): add 2026-11-11 round commit-train script'
G9_T=()
G9_N=(make-2026-11-11-round-commits.sh)

NGROUPS=9

# Static assert: the roadmap file must never appear in any staging list.
for _i in $(seq 1 $NGROUPS); do
  declare -n _t="G${_i}_T" _n="G${_i}_N"
  for _p in "${_t[@]}" "${_n[@]}"; do
    if [ "$_p" = "docs/sprint-roadmap.md" ]; then
      die "internal: docs/sprint-roadmap.md listed in group $_i"
    fi
  done
done

# ------------------------------------------------------------- helpers ---

in_head() { git cat-file -e "HEAD:$1" 2>/dev/null; }

file_clean() { git diff --quiet HEAD -- "$1" 2>/dev/null; }

require_new() {
  [ -f "$1" ] || die "preflight: new file missing on disk: $1"
  if in_head "$1"; then die "preflight: expected NEW (absent from HEAD) but HEAD has it: $1"; fi
}

grep_at_least() { # $1=min_lines $2=pattern $3=file
  local got
  got=$(grep -c -- "$2" "$3" 2>/dev/null)
  got=${got:-0}
  if [ "$got" -lt "$1" ]; then
    die "preflight: '$2' found ${got}x (need >= $1) in $3 — hunk missing, not just dirty"
  fi
}

preflight() { # $1 = group index; runs only for OPEN groups
  case $1 in
    1)
      grep_at_least 3 'ReportUpdateOutcome' proto/monopsony.proto
      grep_at_least 1 'report_update_outcome' crates/monopsony-control/src/grpc.rs
      grep_at_least 1 'report_update_outcome' crates/monopsony-client/src/grpc_client.rs
      grep_at_least 1 'list_open_update_assignments' crates/monopsony-db/src/db.rs
      grep_at_least 1 'list_shadow_packages' crates/monopsony-client/src/grpc_client.rs
      ;;
    2)
      grep_at_least 1 'Finalize' crates/monopsony-server/src/main.rs
      grep_at_least 1 'run_finalize' crates/monopsony-server/src/self_update.rs
      grep_at_least 1 'updates/self-apply' crates/monopsony-admin/src/lib.rs
      grep_at_least 2 'evaluate_self_apply_guard' crates/monopsony-admin/src/updates.rs
      grep_at_least 1 'monopsony-client' crates/monopsony-admin/Cargo.toml
      git diff HEAD -- Cargo.lock | grep -q '^+.*"monopsony-client"' \
        || die "preflight: Cargo.lock one-line admin/monopsony-client addition absent"
      ;;
    3)
      require_new dashboard/src/lib/utils/version.ts
      require_new dashboard/src/routes/updates/+page.svelte
      grep_at_least 1 'contract_version' dashboard/src/lib/utils/version.ts
      grep_at_least 1 'selfApply' dashboard/src/lib/api/client.ts
      grep_at_least 1 'SelfApplyResponse' dashboard/src/lib/api/types.ts
      grep_at_least 1 "href: 'updates'" dashboard/src/routes/+layout.svelte
      grep_at_least 1 'legacy' dashboard/src/routes/clients/+page.svelte
      ;;
    4)
      require_new docs/plans/2026-11-11-s31-nonsystemd-detection-options.md
      grep_at_least 1 '## Decision' docs/plans/2026-11-11-s31-nonsystemd-detection-options.md
      ;;
    5)
      grep_at_least 2 'is_openrc_script' crates/monopsony-client/src/detect.rs
      grep_at_least 2 'elf_only_sha256' crates/monopsony-client/src/main.rs
      require_new recipes/shadow/authentik-style-example.toml
      grep_at_least 1 'method = "script"' recipes/shadow/authentik-style-example.toml
      ;;
    6)
      grep_at_least 3 'normalize_config_path' crates/monopsony-client/src/shadow_scorer.rs
      grep_at_least 1 'config_agreement_agrees_ignoring_trailing_slash' crates/monopsony-client/src/shadow_scorer.rs
      ;;
    7)
      require_new docs/plans/2026-11-11-store-spike-accrescent.md
      require_new docs/plans/2026-11-11-store-spike-aurora.md
      require_new docs/plans/2026-11-11-store-spike-grapheneos.md
      require_new docs/plans/2026-11-11-store-spike-gate-summary.md
      grep_at_least 1 'Accrescent' docs/plans/2026-11-11-store-spike-accrescent.md
      grep_at_least 1 'Aurora Store' docs/plans/2026-11-11-store-spike-aurora.md
      grep_at_least 1 'GrapheneOS' docs/plans/2026-11-11-store-spike-grapheneos.md
      grep_at_least 1 'five-store verdict' docs/plans/2026-11-11-store-spike-gate-summary.md
      ;;
    8)
      grep_at_least 1 'supersession proven via git log' AGENTS.md
      ;;
    9)
      require_new "$SELF"
      ;;
  esac
}

# Group state: 0 = landed (all files in HEAD and clean), 1 = open, dies on mixed.
group_state() { # $1 = group index
  local i=$1 f state_t=ok state_n=ok
  local -n T="G${i}_T" N="G${i}_N"
  for f in "${T[@]}"; do
    if ! in_head "$f"; then die "group $i: tracked file not in HEAD: $f"; fi
    if ! file_clean "$f"; then state_t=dirty; fi
  done
  for f in "${N[@]}"; do
    if in_head "$f"; then
      if ! file_clean "$f"; then die "group $i: new file already in HEAD but modified: $f"; fi
      state_n=head
    else
      [ -f "$f" ] || die "group $i: new file missing on disk: $f"
      state_n=disk
    fi
  done
  # landed = no dirty tracked + every new file in HEAD and clean (vacuous when list empty)
  if [ "$state_t" = "ok" ] && { [ "$state_n" = "ok" ] || [ "$state_n" = "head" ]; }; then
    return 0
  fi
  # open = every tracked file dirty (vacuous when none) + every new file on-disk-only
  local t_open=0 n_open=0
  if [ "$state_t" = "dirty" ] || [ "${#T[@]}" -eq 0 ]; then t_open=1; fi
  if [ "$state_n" = "disk" ] || [ "${#N[@]}" -eq 0 ]; then n_open=1; fi
  if [ "$t_open" = "1" ] && [ "$n_open" = "1" ]; then
    return 1
  fi
  die "group $i: mixed state (tracked=$state_t new=$state_n) — not a clean train state; inspect git status"
}

subject_of() { # $1 = group index -> echoes subject
  local -n s="G${1}_SUBJ"
  printf '%s\n' "$s"
}

allowed_untracked() { # $1 = porcelain untracked path (may end in /)
  local p=$1 f d
  p=$1
  case $p in
    AGENTS-sanctioned-diff-*.patch) return 0 ;;
    -|.git-commit-message.tmp) return 0 ;;
    scripts/invalidate-checksums.sh) return 0 ;;
    packages/*|packages/|target-musl/*|target-musl/|tmp/*|tmp/) return 0 ;;
  esac
  if [ "$p" = "$SELF" ]; then return 0; fi
  for f in "${OPEN_N[@]}"; do
    if [ "$p" = "$f" ]; then return 0; fi
    d=$(dirname "$f")
    if [ "$p" = "$d" ] || [ "$p" = "$d/" ]; then return 0; fi
    case $p in "$f"*) return 0 ;; esac
  done
  return 1
}

# --------------------------------------------------------------- banner ---

echo "make-2026-11-11-round-commits.sh — 9-group round train"
echo "HEAD:   $(git rev-parse HEAD)"
echo "BASE:   $BASE"
echo "MODE:   DRY_RUN=$DRY_RUN RESUME=$RESUME"
echo

# ------------------------------------------------------------ group scan ---

if [ "$RESUME" != "1" ]; then
  if [ "$(git rev-parse HEAD)" != "$BASE" ]; then
    die "HEAD is not the train base $BASE — if groups already landed, re-run with RESUME=1"
  fi
fi

declare -a STATE=() OPEN_IDX=() CLOSED_IDX=()
for i in $(seq 1 $NGROUPS); do
  if group_state "$i"; then
    STATE[$i]=landed
    CLOSED_IDX+=("$i")
  else
    STATE[$i]=open
    OPEN_IDX+=("$i")
  fi
done

# All-landed detection via the OPEN-flag sum (never an empty-dirty-set).
if [ "${#OPEN_IDX[@]}" -eq 0 ]; then
  echo "All $NGROUPS groups already landed — nothing to do. Verifying history subjects:"
  ok=1
  mapfile -t HIST < <(git log --format=%s -$NGROUPS)
  for i in $(seq 1 $NGROUPS); do
    want=$(subject_of "$i")
    have=${HIST[$((NGROUPS - i))]:-}
    if [ "$have" != "$want" ]; then
      echo "  MISMATCH at G$i: history='$have' expected='$want'"
      ok=0
    else
      echo "  ok G$i: $have"
    fi
  done
  if [ "$ok" != "1" ]; then die "history does not match this train's subjects"; fi
  echo "Train complete. This script does NOT push."
  exit 0
fi

FIRST_OPEN=${OPEN_IDX[0]}
# Prefix rule: landed groups must be exactly G1..G(FIRST_OPEN-1), open ones the rest.
for i in $(seq 1 $NGROUPS); do
  if [ "$i" -lt "$FIRST_OPEN" ]; then
    if [ "${STATE[$i]}" != "landed" ]; then die "internal: gap before first open group at G$i"; fi
  else
    if [ "${STATE[$i]}" != "open" ]; then die "group G$i landed while G$FIRST_OPEN still open — not a prefix train state"; fi
  fi
done

# On resume, verify each landed group's files were committed under this train's subject.
if [ "$RESUME" = "1" ] && [ "${#CLOSED_IDX[@]}" -gt 0 ]; then
  k=${#CLOSED_IDX[@]}
  mapfile -t HIST < <(git log --format=%s -$k)
  idx=0
  for i in $(seq $k -1 1); do
    want=$(subject_of "$i")
    have=${HIST[$idx]:-}
    if [ "$have" != "$want" ]; then
      die "resume: history at -$((idx + 1)) is '$have', expected '$want' — HEAD does not look like this train's prefix"
    fi
    idx=$((idx + 1))
  done
  # Belt: every landed group's last-touching commit carries its subject.
  for i in "${CLOSED_IDX[@]}"; do
    want=$(subject_of "$i")
    declare -n T="G${i}_T" N="G${i}_N"
    for f in "${T[@]}" "${N[@]}"; do
      got=$(git log --format=%s -1 -- "$f")
      if [ "$got" != "$want" ]; then
        die "resume: $f last landed under '$got', expected '$want' — refusing to guess"
      fi
    done
  done
fi

# ------------------------------------------------------------ dirty sets ---

declare -a ALL_T=() ALL_N=() OPEN_T=() OPEN_N=()
for i in $(seq 1 $NGROUPS); do
  declare -n T="G${i}_T" N="G${i}_N"
  ALL_T+=("${T[@]}"); ALL_N+=("${N[@]}")
  if [ "${STATE[$i]}" = "open" ]; then
    OPEN_T+=("${T[@]}"); OPEN_N+=("${N[@]}")
  fi
done

MODIFIED=$(git diff --name-only HEAD | sort)
EXPECTED_ALL=$(printf '%s\n' "${ALL_T[@]}" | sort)
EXPECTED_OPEN=$(printf '%s\n' "${OPEN_T[@]}" | sort)

if [ "$RESUME" != "1" ]; then
  if [ "$MODIFIED" != "$EXPECTED_ALL" ]; then
    echo "GUARD FAIL: modified tracked set drifted from the 18-file round whitelist."
    diff <(printf '%s\n' "$MODIFIED") <(printf '%s\n' "$EXPECTED_ALL") || true
    die "fix the tree or the whitelist; never stage around it"
  fi
else
  bad=$(printf '%s\n' "$MODIFIED" | while IFS= read -r f; do
    printf '%s\n' "$EXPECTED_OPEN" | grep -Fxq -- "$f" || printf '%s\n' "$f"
  done)
  if [ -n "$bad" ]; then
    echo "GUARD FAIL: modified files outside the open groups:"
    printf '%s\n' "$bad"
    die "tree drifted mid-train"
  fi
fi

# Untracked guard (porcelain ?? lines only).
git status --porcelain=v1 | grep -q '^R' && die "renames present in tree — not a recognized state"
UNTRACKED=$(git status --porcelain=v1 | grep '^??' | cut -c4-)
if [ -n "$UNTRACKED" ]; then
  while IFS= read -r p; do
    [ -n "$p" ] || continue
    if ! allowed_untracked "$p"; then
      die "GUARD FAIL: untracked path not in the allowlist: $p"
    fi
  done <<EOF
$UNTRACKED
EOF
fi

# Index guard.
if git diff --cached --quiet; then
  INDEX_EMPTY=1
else
  INDEX_EMPTY=0
fi
if [ "$RESUME" != "1" ]; then
  if [ "$INDEX_EMPTY" != "1" ]; then
    die "index not clean at start — commit or unstage it first (never fold stray staged content into a group)"
  fi
else
  if [ "$INDEX_EMPTY" != "1" ]; then
    STAGED=$(git diff --cached --name-only)
    bad=$(printf '%s\n' "$STAGED" | while IFS= read -r f; do
      found=0
      for g in "${OPEN_T[@]}" "${OPEN_N[@]}"; do
        if [ "$f" = "$g" ]; then found=1; break; fi
      done
      if [ "$found" = "0" ]; then printf '%s\n' "$f"; fi
    done)
    if [ -n "$bad" ]; then
      echo "GUARD FAIL: staged files outside the open groups:"
      printf '%s\n' "$bad"
      die "unexpected staged content"
    fi
    echo "Resume: folding in-flight staged set (belongs to this train):"
    printf '%s\n' "$STAGED"
  fi
fi

echo "Guard OK: modified set exact, untracked within allowlist, index ${INDEX_EMPTY:+clean}."
echo "Groups: landed=${#CLOSED_IDX[@]} open=${#OPEN_IDX[@]} (first open: G$FIRST_OPEN)"
echo

# ------------------------------------------------------------- strays ---

if [ "$DRY_RUN" = "1" ]; then
  echo "DRY-RUN: would delete declared strays: '-' and '.git-commit-message.tmp'"
else
  rm -f -- '-' .git-commit-message.tmp \
    && echo "Deleted declared strays: '-' (empty, Oct 21) and '.git-commit-message.tmp' (stale Oct 27 draft; landed long ago as a0a26a9)."
fi
echo

# ------------------------------------------------------------- run ---

COMMITTED=0
for i in $(seq $FIRST_OPEN $NGROUPS); do
  declare -n T="G${i}_T" N="G${i}_N"
  subj=$(subject_of "$i")
  echo "=============================================="
  echo "G$i: $subj"
  preflight "$i"
  echo "  preflight OK ($((${#T[@]} + ${#N[@]})) files: ${#T[@]} tracked + ${#N[@]} new)"
  if [ "$DRY_RUN" = "1" ]; then
    for f in "${T[@]}" "${N[@]}"; do echo "    would stage: $f"; done
    continue
  fi
  for p in "${T[@]}" "${N[@]}"; do
    if [ "$p" = "docs/sprint-roadmap.md" ]; then
      die "runtime assert: refusing to stage docs/sprint-roadmap.md"
    fi
  done
  git add -- "${T[@]}" "${N[@]}" || die "G$i: git add failed"
  if git diff --cached --quiet; then
    die "G$i: nothing staged after add — pathspec bug"
  fi
  actual=$(git diff --cached --name-only | sort)
  expected=$(printf '%s\n' "${T[@]}" "${N[@]}" | sort)
  if [ "$actual" != "$expected" ]; then
    echo "G$i: staged-set drift — got:"
    printf '%s\n' "$actual"
    echo "expected:"
    printf '%s\n' "$expected"
    die "staged set does not equal the group list (index left staged for inspection)"
  fi
  git commit -S -F "$MSGDIR/g$i.msg" \
    || die "G$i: commit failed (pinentry cancelled?) — index left staged; fix and re-run with RESUME=1"
  got=$(git log -1 --format=%s)
  if [ "$got" != "$subj" ]; then
    die "G$i: post-commit subject mismatch: '$got' != '$subj'"
  fi
  echo "  committed G$i"
  COMMITTED=$((COMMITTED + 1))
done

echo "=============================================="
if [ "$DRY_RUN" = "1" ]; then
  echo "DRY-RUN complete — all guards and preflights passed; nothing staged, nothing committed, strays kept."
else
  echo "Train landed: $COMMITTED commit(s) this run."
  echo
  git log --oneline -$COMMITTED
  echo
  echo "Remaining working tree (leave-behinds only is healthy):"
  git status --porcelain=v1
  echo
  echo "Review: git log --stat -$COMMITTED   — this script does NOT push; pushing is yours."
fi
exit 0
