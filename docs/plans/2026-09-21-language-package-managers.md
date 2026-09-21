# Sprint 23 Plan — Language Package Managers (npm / PyPI / crates.io)

**Date:** 2026-09-21
**Status:** Approved plan, not started
**Authority:** `docs/sprint-roadmap.md` §Sprint 23
**Execution model:** background batch-delegation leaves + in-session parent work, fleet verification on the `.58` bench as the user gate before commit
**Decisions locked at planning (user-approved 2026-09-21):** metadata option A (TTL revalidation), three-layer retention with version floors, scrubber verification loop, per-PM enablement auto-on-first-use.

---

## 1. Goal

Cache the dev-machine surface — npm, pip, cargo — so installs and re-installs resolve from the LAN instead of public registries. Follows the shim pattern proven by the 15 distro/OS backends, extended with one genuinely new capability: **cache classes** that distinguish immutable artifacts (cache forever) from mutable registry metadata (TTL revalidate).

### Exit criteria (from roadmap, made testable)

1. `npm install` → MISS (fetch + cache) → `npm install` again (fresh cache dir) → HIT, for a tarball and for metadata under TTL.
2. Same for `pip download/install` (wheel + simple-index page) and `cargo add/build` (`.crate` + sparse-index entry).
3. Private npm registry fetch with auth token works through the proxy; token never appears in cache, logs, or sidecars.
4. Zero regression on all 15 distro PMs (existing proxy test suite + one live pacman/apt pass).
5. Retention engine: over-budget eviction respects version floors; scrubber detects a corrupted blob (test-induced) and quarantines + alerts; `last_verified_at` skips blobs verified within N days.
6. Enablement: fresh server has all three language PMs disabled; first npm-pattern request auto-enables with an audit row; dashboard toggle flips it off and subsequent requests pass through uncached.

---

## 2. Design decisions

### 2.1 Metadata: TTL revalidation (option A)

Registry metadata (`registry.npmjs.org/<pkg>`, `pypi.org/simple/<name>/`, `index.crates.io/<...>` sparse entries) is cached with a short TTL (default 300s, config `metadata_ttl_secs`). Behavior on request:

- Cache hit within TTL → serve (X-Cache: HIT).
- Cache hit past TTL → revalidate upstream in the background semantics of the existing miss path; serve new body. If upstream is unreachable → **serve stale** + `X-Cache: STALE` header, warn log. (Offline/airgap stays functional for already-seen packages.)
- Artifacts (tarballs/wheels/`.crate`) ignore TTL entirely — immutable, cached forever subject to §2.2.

Rejected: option B (true registry shim on :3129 — heavier, three new protocol servers, defer until an airgap deployment demands it) and option C (pass-through metadata — breaks the goal).

Byte-faithfulness rule: metadata is cached and served byte-identical. Notably cargo's sparse-index responses are newline-sensitive and npm's packuments differ by `Accept` header (`application/vnd.npm.install-v1+json` abbreviated vs full) — the cache key must include the normalized `Accept` variant for npm metadata.

### 2.2 Artifact retention: three layers (artifact-class-generic)

| Layer | Rule | Purpose |
|---|---|---|
| Version floors | Never evict: 2 most recent major versions (x.0.0), 2 minors within the current 0.x line, 3 patches within the current 0.0.x line (user defaults; configurable) | Current generation always available offline |
| LRU on `last_accessed` | Evict outside floors **only when the class is over budget** | Pinned old versions (lockfiles) stay hot in practice |
| Budget cap | `budget_bytes` per class; unset = no eviction (current behavior) | VPS runners set a cap; LAN runners run unlimited |

Eviction never loses data that upstream still serves — the only true loss window is npm's ~72h unpublish window, which version floors already cover. "Forever" is therefore **default-forever, evictable-under-pressure**.

Withdrawal safety: floors protect nothing that fresh metadata contradicts — if revalidated metadata drops or re-hashes a version we hold (registry removal, `cargo yank` keeps serving but npm takedown does not), the artifact row is marked `contradicted`; contradicted blobs lose floor protection and become ordinary LRU candidates, and npm-removed packages are purged outright (§2.3 quarantine applies to hash mismatch; index-drop is a softer purge signal).

### 2.3 Scrubber: cron-like verification loop

Second janitor pass in `proxy.rs` (the existing sweep handles staged parts):

- Interval: `scrub_interval_secs` (default 7 days).
- Per blob: skip if `now − last_verified_at < scrub_min_age_secs` (default: same as interval → each blob verified roughly once per period, none twice).
- Re-hash file with sha2, compare to sidecar `checksum_sha256` (already stored at write time).
- Mismatch → quarantine: rename blob out of the served tree (`.quarantined/`), stop serving it, fire the existing tamper-alert path, log WARN. Next client request re-fetches clean from upstream (verify-on-serve catches the same rot first if requested before the scrubber reaches it).
- After pass: update `last_verified_at` in the sidecar.

`last_verified_at` is a new `StorageMeta` field (serde-defaulted for old sidecars — backward compatible, unverified blobs get scrubbed first).

### 2.4 Per-PM enablement: auto-on-first-use

- New `server_settings` key `language_pms` (JSONB): `{"npm":{"enabled":true,"source":"auto"},"pip":{"enabled":false,"source":"default"},...}`.
- Default: all three disabled.
- **Auto-enable:** proxy request matches a language-PM URL pattern AND the PM is not yet enabled → flip to enabled with `source:"auto"`, write an audit row (reuses `record_shadow_assignment_audit`-style table access; new audit kind `pm_auto_enable`). Zero-config runners get exactly the PMs their clients use.
- Manual override: dashboard toggle (Sprint 22 flag-gate UI pattern: setting read-through per request, no restart) and `server.toml` `[language_pms]` static config for airgap/pre-provision. Config-file entries win at boot; dashboard flips win at runtime; audit records which surface made each change.
- Disabled PM → requests matching its patterns pass through uncached (existing non-package path), no caching side effects.

### 2.5 Cache classes

`StorageMeta` gains `cache_class: CacheClass` enum: `Distro` (existing behavior, unchanged), `LangArtifact` (§2.2, §2.3), `LangMetadata` (§2.1). Classification happens at miss time from the matching backend's pattern metadata: backends declare `cache_class()` for each pattern (simplest: a method on the backend returning class + per-pattern hints). DB `cache_entries` gains the class column for stats/janitor queries.

## 3. Work breakdown (leaf batches)

### Phase A — foundation (parent or single leaf, do first)
- `CacheClass` enum + `StorageMeta.cache_class` + `last_verified_at` (serde defaults; old sidecars load as `Distro`/unverified).
- `[cache.language_artifacts]` config section: `enabled`, `budget_bytes` (unset = unlimited), `metadata_ttl_secs` (300), version-floor knobs, `scrub_interval_secs`, `scrub_min_age_secs`.
- `server_settings` `language_pms` read/write helpers in `monopsony-db`; audit kind.
- Proxy: classification at miss, TTL branch for `LangMetadata` (incl. serve-stale), verify-on-serve for `LangArtifact` (sidecar sha256 check before HIT).
- Tests: sidecar backward-compat, TTL/HIT/STALE branches, verify-on-serve quarantine.

### Phase B — three backends (three parallel leaves, artifact-first)
Each leaf: new file in `crates/monopsony-common/src/package_manager/`, `PackageManagerKind` variant + `as_str` + availability check + list_installed (via the PM's own CLI) + `upstream_url_patterns()` + `url_to_cache_path()` + `proxy_config()`/`remove_proxy_config()` + unit tests. NO cargo fmt. Patterns:

- **npm.rs** — `registry.npmjs.org` (metadata `<pkg>`, tarballs `<pkg>/-/<pkg>-<ver>.tgz`), plus configured private registry base. Config action: write `~/.npmrc` `registry=http://<proxy>:3128/npm` (path-rewriting handled at proxy: npm sends registry-relative fetches that hit the proxy as configured host — see §4 note). Scoped-token passthrough: forward `Authorization` header on npm fetches; never persist/log.
- **pip.rs** — `pypi.org/simple/<name>/` (metadata) + `files.pythonhosted.org` (artifacts, `#sha256=` fragments). Config action: write `pip.conf` `[global] index-url = http://<proxy>:3128/pypi/simple` + `[global] trusted-host`.
- **cargo.rs** — `index.crates.io/<...>` sparse index (metadata) + `crates.io/api/v1/crates/<...>/download` + `static.crates.io` (artifacts). Config action: `~/.cargo/config.toml` `[source.crates-io] replace-with = "monopsony"` + `[source.monopsony] registry = "sparse+http://<proxy>:3128/cargo/"` + `[net]` off-checksettings untouched.

Accept header variant note for npm (§2.1) lands in Phase A's cache-key work.

### Phase C — retention engine + scrubber (parent; depends on A)
- Janitor extension: eviction pass (budget → floors → LRU via sidecar `last_accessed`) + scrub pass (§2.3).
- Version-floor parsing: artifact cache paths must encode name+version (backend `url_to_cache_path` defines `<ecosystem>/<name>/<version>/<blob>` layout so floors are computable without DB joins).
- `contradicted` marking from metadata revalidation diffs (npm removal → purge; hash change → quarantine).
- Metrics: `monopsony_cache_evicted_total`, `monopsony_scrub_verified_total`, `monopsony_scrub_quarantined_total`.
- Tests: floor matrix (majors/minors/patches boundaries), budget-LRU interaction, scrub skip/mismatch paths.

### Phase D — enablement + dashboard (one leaf + parent wiring)
- Admin endpoints: GET/PATCH `/api/v1/admin/language-pms` (list + toggle), audit rows on every transition.
- Auto-enable hook in the proxy miss path (Phase A classification point).
- Dashboard: mirror-track toggle UI (Sprint 22 pattern) — three switches + source/auto badges + audit history panel. svelte-check 0 warnings.

### Phase E — client CLI
- `monopsony-client proxy on|off|status --pm npm|pip|cargo` mapping to the Phase B config actions; `status` reads current config state (file present? pointing at us?).
- `Configure` subcommand auto-includes language PMs when their daemon-reported availability check passes (npm/cargo/pip on PATH).

### Phase F — fleet verification (user gate, .58 bench)
1. Deploy server + one client; `npm --cache /tmp/nc install left-pad@1.3.0` twice → MISS then HIT (tarball), packument revalidation visible at TTL edge, STALE under upstream-block (iptables drop on .58 for the test).
2. Same shape for pip (requests wheel) and cargo (dep on a small crate).
3. Private npm registry test: configured second registry with token → fetch OK, `strings` over cache dir + logs shows no token.
4. Retention live test: seed budget below current usage → eviction respects floors; corrupt a blob on disk → scrubber quarantines + alert fires; re-request → clean refetch.
5. Enablement live test: fresh settings → first npm request auto-enables (audit row); dashboard toggle off → pass-through.
6. Regression: pacman `-Sy` + apt fetch through proxy; full workspace test suite green.

## 4. Config surface (server.toml)

```toml
[cache.language_artifacts]
enabled = true                # master switch; per-PM enablement below
budget_bytes = 107374182400   # 100 GiB; unset/0 = unlimited (eviction off)
metadata_ttl_secs = 300
# version floors (defaults shown)
floor_majors = 2
floor_minors = 2
floor_patches = 3
scrub_interval_secs = 604800  # 7 days
scrub_min_age_secs = 604800   # skip blobs verified more recently than this

[language_pms.npm]
enabled = false               # static config; dashboard/runtime flips audited
[language_pms.pip]
enabled = false
[language_pms.cargo]
enabled = false
```

(Defaults chosen so a bare `docker compose up` behaves exactly like today: no eviction, scrubber on but harmless, PMs enable on first client use.)

## 5. Risks / notes

- **npm registry rewrite model:** clients point at the proxy host directly; the proxy must translate incoming `/npm/<pkg>` style paths to upstream `registry.npmjs.org/<pkg>` (and same for pip/cargo prefixes). Alternative is transparent host-override via proxy env (`npm config set registry http://proxy:3128` sends `GET /<pkg>` — proxy infers upstream from the PM that owns the client). Decision during Phase B: prefer transparent (no path rewriting) since `proxy_config` already points clients at the proxy host; `/npm/...` prefixes only if transparent proves ambiguous when multiple registries are configured.
- **cargo config fragility:** `registry.replace-with` semantics differ across cargo versions; Phase B tests pin behavior against a modern cargo (≥1.78, sparse default).
- **PyPI host split:** metadata on pypi.org, artifacts on files.pythonhosted.org — patterns must cover both, classified correctly.
- **Token safety:** Authorization pass-through is fetch-only; sidecar/meta writes must strip headers. Add a negative test (token never in cache dir).
- **Old sidecars:** serde default for new fields; first scrub pass backfills `last_verified_at`.
- Deferred from this sprint: true registry shim (option B), scrubber extension to the distro cache class (design allows; config-line change later), PyPI private-index mirroring, npm org-scoped auth ACLs.

## 6. Out of scope (unchanged from roadmap)

JWT trust broker, mTLS proxy auth, public-mirror manifest signing (Sprint 24 items).
