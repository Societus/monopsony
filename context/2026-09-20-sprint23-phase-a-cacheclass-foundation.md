# 2026-09-20 — Sprint 23 Phase A: CacheClass foundation (language package managers)

## Status: PHASE A COMPLETE (code + tests green, uncommitted)

## Plan
`docs/plans/2026-09-21-language-package-managers.md` (untracked — include in this commit).
Phase A = cache-class foundation so npm/pip/crates artifacts can get TTL
revalidation + verify-on-serve without touching distro traffic behavior.

## What Landed

### monopsony-common (config + URL classification)
- `CacheClass` enum (`Distro` | `LangArtifact`) in config.rs with
  `as_str()`, `serde` snake_case, `From<&str>`, `Default = Distro`.
- `LanguageArtifactConfig` (`[cache.language_artifacts]`): enabled,
  metadata_ttl_secs (default 1800), verify_on_serve (default true),
  stale_while_revalidate (default true). **Manual `impl Default`** —
  derive(Default) would zero the numerics (documented pitfall).
  Per-PM overrides supported via `defaults_for_pm`.
- `UrlPattern.cache_class: Option<CacheClass>` — optional; 65 `new()`
  call sites unaffected (no struct literals in wild).

### monopsony-proxy
- `StorageMeta` + internal `ObjectMeta` sidecar carry `cache_class`
  (serde-defaulted → "distro" for pre-Sprint-23 sidecars) and
  `last_verified_at` (epoch secs, 0 = never).
- `SpoolHandle.set_cache_class()` — tee task stamps class pre-commit so
  sidecars get it. Class computed once in `fetch_and_stream_get` via
  `ProxyState.classify_url(url)`, shared into tee ctx.
- `CacheEntry.cache_class` (sidecar JSON + proxy.rs struct) and
  `CacheMissConfig.cache_class` — both default "distro".
- `CacheEntry` now serde (Serialize+Deserialize) — sidecar persistence.
- NEW `language_artifacts.rs` module:
  - `check_freshness()` — TTL gate (disabled/TTL=0/distro ⇒ Fresh),
    origin conditional GET revalidation (ETag/If-Modified-Since),
    Revalidated refreshes sidecar cached_at + last_verified_at,
    ServeStale on origin failure when swr enabled, Expired otherwise.
  - `verify_on_serve()` — for lang_artifact: hash cached blob vs stored
    checksum; mismatch ⇒ tamper_alerts row + refuse (403 +
    X-Monopsony-Quarantine: 1). Distro skips (zero overhead).
  - Wired into `serve_cache_hit`: quarantine check → freshness →
    verify-on-serve → range read stream.
  - `X-Cache: HIT|STALE` header reflects freshness outcome.
- Tests: 7 new (backward-compat sidecar default, TTL disabled/distro
  fresh, within-TTL fresh, unreachable-origin serve-stale preserving
  cached_at, distro skip verify, intact-blob pass, corrupt-blob refuse).

### monopsony-db
- `cache_entries.cache_class TEXT NOT NULL DEFAULT 'distro'` in CREATE +
  idempotent `ALTER TABLE ... ADD COLUMN` (old DBs migrate on boot).
- `language_pms` server_settings helpers: `get_language_pms()`,
  `set_language_pms()`, `get_language_pm()`, `set_language_pm()` +
  `LanguagePmState` struct + `LANGUAGE_PMS_KEY` const.
- `record_cache_miss` writes cache_class (10th bind).

### monopsony-server (main.rs wiring)
- `[cache.language_artifacts]` section parsed, threaded through
  `run_proxy(...)` (new param) → `ProxyState.language_artifacts`.
  Client main.rs untouched (its run_proxy is a different function).

## Verification
- `cargo check --workspace` — 0 errors.
- `cargo test -p monopsony-proxy --lib` — 126 passed (7 new).
- `cargo test -p monopsony-proxy` — integration suites 3 + 11 pass.
- clippy: only pre-existing warnings remain in changed crates.

## Deliberately NOT in Phase A (later phases)
- npm/pip/crates backend modules (Phase B–D) — enum slots exist.
- Scrubber loop + retention floors (Phase E) — `last_verified_at`
  field + audit table ready for it.
- Admin API endpoints (Phase F) — DB helpers in place.

## Pitfalls Hit (for skill later)
- patch tool spliced a function mid-SQL when old_string wasn't unique —
  always include enough context to be unique; repair immediately.
- `#[serde(default)]` on a non-Option String yields "" not the typed
  default fn value unless you use `default = "fn"` form.
- patch tool false-positive edition errors on this workspace (standalone
  linter; cargo is arbiter).

## Next Session
- Phase B (npm backend) per plan, or admin endpoints (Phase F) if
  dashboard work preferred first.
