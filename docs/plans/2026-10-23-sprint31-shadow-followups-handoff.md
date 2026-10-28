# Handoff — Sprint 31 shadow pipeline follow-ups (items 1–5, mechanical dispatch)

**Date:** 2026-10-23 · **Executor:** local model (mechanical implementation only) · **Dispatcher:** Hermes session
**Self-contained:** this document is the executor's complete spec — it needs no other file. The
roadmap section (Sprint 31, `docs/sprint-roadmap.md`) is the source of the item list; this handoff
adds verified anchors, exact changes, and gates.

## Mission

Implement Sprint 31 items I1–I5 exactly as specified. Each is small, mechanical, and lands with
its own tests where this spec names one. Items 6–7 of the roadmap section (non-systemd service
detection, container recipe adoption) are DESIGN work — **explicitly OUT OF SCOPE**.

## Grounding (verified 2026-10-23 at HEAD `e324708`)

### Repo state
- Branch `master`, **15 ahead / 0 behind** `origin/master`. Expected; do NOT push.
- No tracked changes in the working tree. Untracked (`packages/`, `tmp/`, `target-musl/`,
  `make-pairing-commits.sh`) — leave alone.
- All Sprint-30 work is COMMITTED (`936ae2c` register pipeline, `77ee254` schema §5 alignment).
  The roadmap's "uncommitted awaiting sign" note is stale; the Sprint-31 dependency is CLEARED.
- `crates/monopsony-client/src/main.rs` is 4,032 lines — edit via small anchored patches only.

### Build/test baselines (regressions are attributable against these)
| Check | Baseline |
|---|---|
| `cargo test -p monopsony-client` | 473 passed / 0 failed / 0 ignored |
| `cargo test -p monopsony-control` | 23 passed / 0 failed / 0 ignored |
| `cargo test -p monopsony-common` | 167 passed / 0 failed / 0 ignored |
| `cargo clippy -p monopsony-client --all-targets` | exit 0; 9 pre-existing warning lines |
| `cargo clippy -p monopsony-control --all-targets` | exit 0; 5 pre-existing warning lines |

NOTE: the Sprint-30 record's "control 293" disagrees with the fresh 23 — different scope; ignore
it. These fresh numbers are the only baseline. Gate everywhere: suite green vs baseline, clippy
exit 0, **no NEW warnings from your edits**.

### Execution order (strictly serial — I1 → I2 → I3 → I4 → I5)
One item at a time; run the item's gate before starting the next. I2 and I3 both touch `main.rs`
(disjoint regions) and I2/I5 share metadata semantics — serial keeps attribution clean.

## Environment & workflow rules (BINDING)

1. **No `git add` / `git commit` / `git push`. No `cargo fmt`.** The operator signs and formats
   at merge time. Match surrounding style.
2. **Terminal discipline:** single plain commands; no multi-line heredocs; no `setsid`/`nohup`.
   Write any scratch script with the file tool and run it by path.
3. **Edit discipline:** read each region before patching; anchor patches on unique multi-line
   context **from the current file** — line numbers below were true at `e324708` but may have
   drifted by the time you edit; when they have, locate by the quoted content. Strip the `NNN|`
   prefix that file reads glue onto lines. Keep each patch call small (<~8K tokens).
4. **`cargo check` does NOT compile test code:** after any struct change (I3), run
   `cargo test -p monopsony-client --no-run` FIRST, then the full suite.
5. **Scope lock:** only files named per item. No drive-by refactors, no new dependencies, no
   signature changes to existing `pub fn`s unless the item explicitly says so.
6. **Do not touch:** the `PackageCommand::Add` stub (`main.rs` ~:3168–3183 — prints only, NOT in
   scope); the no-recipe register path's behavior; the no-shadow-manager branch of
   `check_shadow_updates` (`grpc.rs` ~:1283–1286); `dist/init/` templates.
7. **Live/fleet verification** (ct/1952, ct/1904, server 10.10.20.58) is dispatcher work — NOT
   yours. Stop when items are green; report.

---

## I1 — installer PATH (`dist/install.sh`)

**Problem:** install.sh must ensure `monopsony-client` lands in `$PATH` (found during Alpine /
OpenRC CT staging).

**Anchors:** `INSTALL_DIR="/usr/bin"` at :39 (Darwin re-points to `/usr/local/bin` in
`set_platform_paths`, :124–128). `download_binary(kind)` at :268+; install sequence ends
:346–353 (`mkdir -p "${INSTALL_DIR}"` → `mv` → `chmod 755` → `rm -rf "${tmpdir}"`), with
`local bin_name="monopsony-${kind}"` at :349. Client flow dispatch :2515–2519. Helper `warn`
exists (used at :264).

**Change:** in `download_binary`, after `chmod 755` and before `rm -rf "${tmpdir}"`:

```bash
    # Sprint 31 I1: ensure the installed binary is reachable via $PATH.
    if ! command -v "${bin_name}" >/dev/null 2>&1; then
        {
            echo '# Installed by the monopsony installer: put the install dir on PATH.'
            echo 'case ":$PATH:" in'
            echo "  *\":${INSTALL_DIR}:\"*) ;;"
            echo "  *) export PATH=\"${INSTALL_DIR}:\$PATH\" ;;"
            echo 'esac'
        } > /etc/profile.d/monopsony-path.sh
        chmod 644 /etc/profile.d/monopsony-path.sh
        warn "${bin_name} installed to ${INSTALL_DIR}, which is NOT on PATH."
        warn "Wrote /etc/profile.d/monopsony-path.sh (new login shells pick it up)."
        warn "For the current shell: export PATH=\"${INSTALL_DIR}:\$PATH\""
    fi
```

Idempotent: fires only when `INSTALL_DIR` is genuinely off-PATH; re-runs overwrite the same file.
Keep the emitted snippet POSIX-sh compatible.

**Gate:** `bash -n dist/install.sh` clean; `bash dist/install.sh --check-templates` still green
(CI-enforced). Scratch smoke: replicate the case-statement with `PATH=/usr/bin:/bin` and
`INSTALL_DIR=/usr/local/bin` → snippet written, correct content, identical on second run.

## I2 — recipe-path row metadata (`main.rs` + `shadow_cli.rs`, monopsony-client)

**Problem:** `package register --recipe <file>` registers with empty service_name /
config_paths / binary_sha256 in the `shadow_packages` row — the data lives in the recipe TOML
and the `path` arg, but only the detect (no-`--recipe`) path populates the row.

**Anchors:** register arm `main.rs:2897+`. `--recipe` branch :2936–2942 (reads recipe file,
`detect_current_version`, leaves `report_opt = None`). `let install_paths = vec![path.clone()];`
at :2966. Row-metadata block :3121–3143 — its own comment admits: *"The --recipe path has no
report, so it registers with empty metadata as before"*; `reg_config_paths` / `reg_service_name`
/ `reg_sha256` all `unwrap_or_default()` when `report_opt` is `None`. Register call :3145–3162.
`shadow_cli.rs`: `extract_install_fields` :138–155 (lenient parse — `[install]` first, top-level
tolerated; returns `(Vec<String> install_paths, String service)`); tests :436–455.

**Change:**
1. When `report_opt` is `None` (the `--recipe` path): call
   `shadow_cli::extract_install_fields(&recipe_toml)` → `(recipe_install_paths, recipe_service)`.
   Set `install_paths = recipe_install_paths` when non-empty, else keep `vec![path.clone()]`.
   The NO-recipe path keeps `vec![path.clone()]` exactly as now.
2. `reg_service_name`: when `report_opt` is `None` → `recipe_service`; else existing logic.
3. `reg_config_paths`: when `report_opt` is `None` → the recipe's `[install].config_paths`.
   **Do NOT change `extract_install_fields`' 2-tuple signature** — it has other callers
   (`main.rs:1148`, `main.rs:3538`) and an arity change breaks every one of them. Add a sibling:

```rust
/// Extract [install].config_paths from a recipe TOML (lenient, same style as
/// extract_install_fields). Empty on missing/invalid TOML; never panics.
pub fn extract_config_paths(recipe_toml: &str) -> Vec<String> { /* ... */ }
```

4. `reg_sha256`: when `report_opt` is `None` and `path` is an existing regular file →
   `detect::sha256_hex(&bytes)` (`detect.rs:169–173`, already `pub`; `sha2`/`hex` already
   deps; detect computes the identical value at `detect.rs:644–646`). Otherwise `""`.

**Tests:** in `shadow_cli.rs`, mirror the :436–455 style for `extract_config_paths` — present /
absent / invalid TOML (never panics, returns empty). The register-arm wiring itself is CLI-flow
code: compile + suite green is the gate; a live re-register is the dispatcher's.

## I3 — hold outer message (`shadow_executor.rs` + `main.rs`, monopsony-client)

**Problem:** when `failure_policy = "hold"` deliberately skips restore, the CLI prints
`✗ Update FAILED (nothing to roll back yet)` — wrong: a backup exists and is being held on
purpose. The CLI cannot distinguish hold from no-backup because `UpdateOutcome` carries no
marker.

**Anchors:** `UpdateOutcome` struct `shadow_executor.rs:270–281`. Hold branch :602–636 —
`let hold = recipe.install.failure_policy == "hold";` at :605; hold skips restore and logs
`"failure_policy=hold: NOT restoring backup; awaiting operator intervention"` (:635).
Failure-path construction :637–645 (`rolled_back` from the match; `backup_path: None` here).
Success-path construction ~:568. When you add the field, the compiler enumerates EVERY
construction site — fix all of them.

**Change:**
1. Add to `UpdateOutcome`: `/// True when failure_policy=hold deliberately kept the failed
   state, awaiting operator action.` `pub held: bool,`
2. Failure construction (:637): `held: hold`. Every other construction: `held: false`.
3. `main.rs` CLI arm :3387–3398 — insert a held arm BEFORE the existing else:

```rust
} else if outcome.held {
    eprintln!(
        "✗ Update FAILED (failure_policy=hold — kept failed state, awaiting operator): {}",
        outcome.error.as_deref().unwrap_or("unknown error")
    );
} else {
    // existing "nothing to roll back yet" arm unchanged
```

Keep the rolled-back message unchanged.

**Tests:** extend the existing test module in `shadow_executor.rs` (find via grep `mod tests`;
Sprint 30 landed fake-systemctl-injectable fixtures — extend, don't reinvent): hold-failure →
`held == true && rolled_back == false`; rollback-failure → `held == false`; success →
`held == false`.
**Gate order:** `cargo test -p monopsony-client --no-run` FIRST (struct change — rule 4), then
the full suite.

## I4 — check ids filter (`grpc.rs`, monopsony-control)

**Problem:** `check_shadow_updates` ignores the per-package ids filter — it iterates ALL of the
client's rows, so `package update <name>`'s "Available:" line shows sibling rows' versions.

**Anchors:** handler `grpc.rs:1226–1287` — rows fetched :1236–1240
(`db.list_shadow_packages_for_client` → `Vec<Json<Value>>`); per-row id extracted to
`pkg_id_str` at :1248; `mgr.check_one` per row :1260. Proto `CheckShadowUpdatesRequest`
(`client_id`, `repeated string package_ids` with comment `// empty = check all`) at
`proto/monopsony.proto:395–398`. Client callers already send ids correctly
(`main.rs:3231–3234`, `main.rs:3272–3274`; contract documented `grpc_client.rs:1097–1110`).
**No proto change.**

**Change:**
1. Pure helper in `grpc.rs`:

```rust
/// Sprint 31 I4: empty filter = check all; otherwise exact id membership.
fn passes_id_filter(package_id: &str, filter: &[String]) -> bool {
    filter.is_empty() || filter.iter().any(|f| f == package_id)
}
```

2. In the row loop, immediately after `pkg_id_str` is extracted (~:1248, BEFORE the uuid parse
   so non-matching rows skip cheap):
   `if !passes_id_filter(pkg_id_str, &req.package_ids) { continue; }`
3. The no-shadow-manager branch (~:1283–1286) stays as-is.

**Tests:** `grpc.rs` has NO `#[cfg(test)]` module today — add one at file bottom:
empty filter → true; member → true; non-member → false.

## I5 — config_paths hygiene (`detect.rs`, monopsony-client)

**Problem:** (a) UnitArg values keep the unit file's trailing slash (live case: Sonarr
`--data=/var/lib/sonarr/`) while conventional guesses have none — both exist → BOTH get
uncommented in the draft AND both land in the register row (`main.rs:3125–3134` consumes
findings directly). (b) An existing `/root/*` finding is uncommented even when the unit's
`User=` is a non-root user — T1.3 (:612–619) only demotes confidence, and the renderer ignores
confidence.

**Anchors:** `push` closure in `discover_configs` :566–578 (exact-dup HashSet dedup); UnitArg
push :589–591; `extract_unit_arg_configs` :324–343 (verbatim values).
`DetectReport.unit_user` :160–161 (`Option<String>`, top-level). Renderer: `existing` filter
:862–864, `config_paths` emission :865–885, suggestion-comment loop for NON-existing findings
:886–892.

**Change:**
1. Normalizer, used as the first line of `push` (before `seen.insert`) — the existing HashSet
   dedup then collapses the slash-variant pair:

```rust
/// Sprint 31 I5: strip trailing '/' (except bare "/") so a UnitArg value
/// like "/var/lib/sonarr/" dedups against conventional "/var/lib/sonarr".
fn normalize_config_path(path: &str) -> String {
    if path.len() > 1 && path.ends_with('/') {
        path.trim_end_matches('/').to_string()
    } else {
        path.to_string()
    }
}
```

2. Renderer `/root/*` demotion — compute
   `let root_demoted = report.unit_user.as_deref().is_some_and(|u| u != "root");` then:
   - `existing` filter (:862–864): additionally exclude findings where
     `root_demoted && f.path.starts_with("/root/")`.
   - Suggestion-comment loop (:886–892): change the filter to
     `|f| !f.exists || (root_demoted && f.path.starts_with("/root/"))` so demoted-but-existing
     `/root` paths still appear as comment lines (never silently vanish).
3. Scope note: do NOT add the `/root` filter to the register-row metadata in `main.rs` — this
   item is draft-renderer scope per the roadmap.

**Tests:** in `detect.rs`'s test module (render-fixture pattern exists ~:1114):
`normalize_config_path` (`"/var/lib/x/"` → `"/var/lib/x"`; `"/"` stays; no-slash unchanged);
render with an existing `/root/.config/X` finding + `unit_user: Some("sonarr")` → `config_paths`
line does not contain the `/root` path AND a suggestion comment for it appears;
`unit_user: None` or `Some("root")` → still uncommented.

---

## Report format (per item)

Changed files · test counts before/after (crate scope) · clippy delta vs baseline · anchor drift
encountered (quote the actual line numbers if moved) · any deviation from this spec with a
one-line rationale. End of run: one summary table (item → files → tests → status).
