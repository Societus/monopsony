# Sprint 23 completion wave — shared context pack

**Read this file, then your slice spec, then your owned files. Nothing else.**
**Date:** 2026-09-30. Repo: /home/martin/projects/monopsony (all paths relative to repo root).

## Hard rules (every slice)

- HARD ~600s cap: work linearly, write files immediately, verify ONCE at the end.
- NO commits, NO `git add`, NO `cargo fmt`, NO `git stash`. The user signs commits.
- NO new dependencies (everything needed is already in the workspace dep graph).
- The tree is DIRTY BY DESIGN: Sprint 23 Phase B (~1,200 lines) is staged-uncommitted.
  Do not touch, revert, or "clean up" any file outside your safe-list.
- `git status` will show staged files (npm.rs, pip.rs, cargo.rs, mod.rs, proxy.rs,
  language_artifacts.rs, .git-commit-message.tmp, sprint23-phase-a-handoff.md) —
  that is the PRE-EXISTING state, not your lane's mess. Leave it.
- NEVER add an `':!…'` exclude pathspec anywhere. Never run `git add -A`.
- Read-only git (status/diff/log) is fine.

## Do-not-touch list (named rails)

- `.git-commit-message.tmp` — user's staged commit message. Never edit, never stage.
- `docs/plans/sprint23-phase-a-handoff.md` — staged handoff doc.
- `crates/monopsony-common/src/package_manager/{npm,pip,cargo}.rs` and
  `crates/monopsony-common/src/package_manager/mod.rs` — staged Phase B backends.
  Slices must NOT edit mod.rs (a 7-edit registration change is a parent-glue class edit).
- `AGENTS.md` — protected agent-instruction file; a refused patch is the protection working.

## Baseline (hub-measured 2026-09-30, this tree)

- `cargo test -p monopsony-common --lib` → 133 passed, 0.4s
- `cargo test -p monopsony-proxy --lib` → 126 passed, 1 ignored, ~2.4s warm
- Workspace check clean. Clippy: 3 pre-existing dead_code warnings in shadow/db modules —
  pre-existing noise, NOT your problem. Failure condition: a NEW warning or error naming
  YOUR files.
- No server is running on this host that your tests need; all tests are unit/offline.

## Foreign-failure protocol (same-crate parallel lanes)

Leaves edit different files but share the build. Test/compile failures citing files
OUTSIDE your safe-list are a sibling's in-flight edits: do NOT fix them, note them as
foreign in your report, and take your verdict ONLY from your targeted verify command
(module filter). If a foreign compile error blocks your verify, wait ~60s, retry once,
then report `blocked-by-foreign` with the error text.

## Landed signatures (verified against tree 2026-09-30 — do not re-research)

```rust
// crates/monopsony-common/src/config.rs (lines ~523-575)
pub struct LanguageArtifactConfig {
    pub enabled: bool,               // default true
    pub budget_bytes: u64,           // default 0 = unlimited, eviction off
    pub metadata_ttl_secs: u64,      // default 300
    pub floor_majors: u32,           // default 2
    pub floor_minors: u32,           // default 2 (within current 0.x line)
    pub floor_patches: u32,          // default 3 (within current 0.0.x line)
    pub scrub_interval_secs: u64,    // default 604800, 0 disables
    pub scrub_min_age_secs: u64,     // default 604800
}

// crates/monopsony-proxy/src/object_store_backend.rs (line 57)
//   ObjectMeta has: last_verified_at: i64  (default 0 at line 228)
// crates/monopsony-proxy/src/cache_storage.rs (line 22): StorageMeta — carries
//   cache_class (see handoff doc for full shape)
// crates/monopsony-proxy/src/purge.rs (line 40):
pub async fn start_purge_task(db: Arc<Database>, config: PurgeConfig) -> anyhow::Result<()>
// crates/monopsony-db/src/db.rs (~1843-1880):
pub async fn get_language_pms(&self, ...) -> Result<...>   // map pm→LanguagePmState
pub async fn set_language_pms(&self, ...) -> Result<()>
pub async fn record_pm_enablement_audit(...)  // audit rows
// monopsony_db::LANGUAGE_PMS_KEY = "language_pms"
```

## Cache-path layout (Phase B backends, staged — verified from npm.rs)

- npm: packument → `npm/<name>`; tarball → `npm/artifact/<name>/-/<file>.tgz`
  (artifact segment namespaced under `artifact/` to avoid file/dir collision).
- pip / cargo follow the same `<ecosystem>/...` prefix convention (see pip.rs /
  cargo.rs `url_to_cache_path` for exact shapes — read your target backend file
  only if your spec says so).

## Shims (staged in proxy.rs, live in this tree)

`/npm/*path`→registry.npmjs.org · `/pypi/simple/*path`→pypi.org ·
`/pypi/packages/*path`→files.pythonhosted.org · `/cargo-index/*path`→index.crates.io ·
`/crates/*path`→static.crates.io (handler re-adds the `/crates` path segment after
stripping the route prefix). All flow into `handle_request_uri` → identical cache
pipeline (classification/TTL/sidecars).

## Wiring ownership (hub-owned, NOT leaf work)

Files touched by more than one concern are wired by the PARENT after all lanes land:
`purge.rs` (eviction + scrub loop wiring), server `main.rs` (startup wiring),
`monopsony-common/src/package_manager/mod.rs`. Slices deliver tested modules with the
exact pub signatures pinned in their specs; the parent wires. If your slice cannot
proceed without editing a hub-owned file, STOP and report `blocked: needs <file>`.

## Report contract (every slice)

Final message: a `summary:` block with `done:` / `not_done:` / `deviations:` /
`foreign_observations:`. A clean not-done beats a half-integrated straggler. Never
`#[ignore]` a failing test to get green — report it.
