# Slice 1 — Phase C: retention engine (floors + LRU eviction)

**Pack first:** `docs/plans/sprint23-remaining/context-pack-common.md`
**Safe-list:** `crates/monopsony-proxy/src/retention.rs` (NEW file) — nothing else.

## Deliverable

One new module `crates/monopsony-proxy/src/retention.rs` implementing eviction as a
pure, fully unit-tested module. The parent wires it into the janitor loop later; do
NOT touch purge.rs or proxy.rs. **`mod retention;` is ALREADY declared in lib.rs**
(parent glue, landed) with a placeholder file — write your module into the existing
file, replacing the placeholder comment.

## Design (follow exactly — plan §2.2, plan doc `docs/plans/2026-09-21-language-package-managers.md`)

Inputs: list of (cache_key, size_bytes, last_accessed epoch secs, cache_class) for all
LangArtifact entries + `LanguageArtifactConfig` + a version-key parser. Output: the
set of keys to evict.

1. **Version floors** (never evict, computed from the entry's ecosystem/name/version):
   - Keep the N most recent MAJOR versions per (ecosystem, name) — `floor_majors`.
   - Within the current 0.x line: keep N most recent minors — `floor_minors`.
   - Within the current 0.0.x line: keep N most recent patches — `floor_patches`.
   - Parse versions with `monopsony_common::version_ord` (landed Sprint 21: has
     `normalize`/`vercmp`/`order_key`; see its module docs — use `vercmp` for
     ordering, do NOT sort order_key strings lexicographically, tilde pre-releases
     misrank). If a version fails to parse, treat as lowest priority (evictable first)
     and never floor-protected.
2. **LRU** among non-floor-protected entries, oldest `last_accessed` first.
3. **Budget**: evict only while total_size − evicted_size > budget_bytes. budget 0 =
   no eviction at all (return empty set).
4. Semver is the rule for all three ecosystems (npm/PyPI/cargo are semver-family);
   pass parsed (major, minor, patch, pre) tuples into the floor logic.

## API to land (pinned — parent wiring compiles against this)

```rust
pub struct RetentionCandidate {
    pub key: String,
    pub ecosystem: String,   // "npm" | "pypi" | "cargo" — first path segment
    pub name: String,        // parsed per-backend from the cache path
    pub version: String,     // may be "" if unparseable
    pub size_bytes: u64,
    pub last_accessed: i64,
}
pub fn plan_eviction(candidates: &[RetentionCandidate], cfg: &LanguageArtifactConfig)
    -> Vec<String>;  // keys to evict, oldest-first
```

## Tests (minimum)

- Floor matrix: N-major boundary (3 majors, floor 2 → oldest major evictable);
  0.x minor boundary; 0.0.x patch boundary.
- Budget-LRU interaction: over-budget evicts oldest-accessed non-floored first until
  under budget; under-budget evicts nothing; budget 0 evicts nothing.
- Unparseable version: evictable first, never floored.
- Name/version/ecosystem parsing from real cache paths: `npm/artifact/left-pad/-/left-pad-1.3.0.tgz`,
  `npm/left-pad` (metadata — NOT a retention candidate; assert your caller contract:
  only artifact-class entries are passed in), and one pip + one cargo path shape
  (read pip.rs / cargo.rs `url_to_cache_path` for the real shapes).

## Verify (once, from repo root)

`cargo test -p monopsony-proxy --lib retention` — all pass, no new warnings naming
retention.rs.
