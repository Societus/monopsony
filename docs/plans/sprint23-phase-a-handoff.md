# Sprint 23 Handoff — Phase A + Phase B code landed; bench verification next

**Date:** 2026-09-21
**Status:** Phase A committed (`a511c46`). **Phase B backends landed in working tree** (npm/pip/cargo in `monopsony-common`, registered in `mod.rs`; 133 common + 140 proxy tests green; workspace check clean). **Remaining for Phase B exit:** live bench verification on `.58` (MISS→HIT + TTL revalidation per PM) per the roadmap exit criteria.
**Authority:** `docs/plans/2026-09-21-language-package-managers.md`

---

## What Phase A delivered (already committed, verified green)

The foundation for cache classes — the plumbing that lets npm/pip/cargo artifacts get TTL revalidation + verify-on-serve without touching distro traffic. **Do not re-implement; build on it.**

| Area | What landed | Key API / file |
|------|-------------|----------------|
| `monopsony-common::config` | `CacheClass` enum (`Distro\|LangArtifact\|LangMetadata`) + `as_str()`/`parse_class()`, serde snake_case, `Default = Distro`. `[cache.language_artifacts]` config struct with **manual** Default (non-zero numeric defaults — do NOT use derive). | `crates/monopsony-common/src/config.rs` |
| `package_manager::UrlPattern` | New `with_class()` constructor + `cache_class: CacheClass` field on each pattern. Existing backends still use `new()` → default Distro. | `crates/monopsony-common/src/package_manager/mod.rs` |
| proxy sidecar plumbing | `CacheEntry.cache_class`, `StorageMeta/ObjectMeta/CacheMissConfig` carry it; serde-defaults keep pre-S23 sidecars loading as `"distro"`. `SpoolHandle.set_cache_class()` stamps class on commit. | `crates/monopsony-proxy/src/{proxy,cache_storage}.rs` |
| proxy classification | `ProxyState::classify_url(url)` → highest-priority matching pattern's class; miss path stamps it into spool + DB row (agree). | `crates/monopsony-proxy/src/proxy.rs` |
| TTL revalidation | `language_artifacts.rs::check_freshness()` — HEAD w/ ETag+If-Modified-Since, Fresh/Revalidated/Expired/ServeStale. Serve-stale on origin failure (airgap-safe). | `crates/monopsony-proxy/src/language_artifacts.rs` |
| verify-on-serve | `language_artifacts.rs::verify_on_serve()` — sha256 cached blob vs sidecar checksum; mismatch → tamper_alerts + 403. Distro class skips hashing entirely. | same file |
| DB | `cache_entries.cache_class` column (CREATE + idempotent ALTER). `get_language_pms()/set_language_pms()` + `LanguagePmState{enabled, source}` + `record_pm_enablement_audit()`. `TamperAlertInsert` struct for audit rows. | `crates/monopsony-db/src/db.rs` |

**Tests green:** proxy suite 126 lib + 3 integration + 11 lang_artifact (sidecar backward-compat, TTL branches, serve-stale state preservation, verify-on-serve pass/corrupt/skip). `cargo test -p monopsony-proxy` all pass. Workspace check clean.

**Note:** Phase A's `[cache.language_artifacts]` has `enabled: true` by default but **no language backends exist yet**, so nothing is actually cached — the three PMs are disabled via `server_settings.language_pms`. This is exactly the "bare docker compose behaves like today" state from the plan.

---

## Phase B — implemented (backend files + registration landed)

Each leaf = one backend (`npm` / `pip` / `cargo`). **artifact-first**: write the full backend file + unit tests first, wire it in last. NO cargo fmt.

### What landed (2026-09-21)
- `crates/monopsony-common/src/package_manager/{npm,pip,cargo}.rs` — full `PackageManagerBackend` impls with unit tests (21 new tests; 133 common total).
- `mod.rs` — `mod` decls, re-exports, `Npm|Pip|Cargo` enum variants + `as_str`/`parse_kind`, both registries (`get_all_backends()` + `get_backend()`).
- Transport model **resolved: transparent forward-proxy** (plan §4 preference held). npm gets `proxy=`/`https-proxy=` in `~/.npmrc`; pip gets `HTTP_PROXY`/`HTTPS_PROXY` env; cargo gets `CARGO_HTTP_PROXY`+`HTTPS_PROXY` env. No `/npm/...` path rewriting, no mirror-shim on :3129 — npm/pip/cargo all natively proxy HTTP(S), unlike scoop/winget which needed the :3129 shim for catalog reasons.

### Live-verified URL shapes (curl 2026-09-21 — patterns encode these, don't trust older docs)
- npm packument: `registry.npmjs.org/<name>` (single segment; scoped may arrive percent-encoded `/@scope%2fpkg`, 200)
- npm tarball: `/<name>/-/<file>-<ver>.tgz`; scoped `/<@scope>/<name>/-/<file>-<ver>.tgz` (200)
- PyPI simple: `pypi.org/simple/<name>/` (200); artifacts: `files.pythonhosted.org/packages/<2hex>/<2hex>/<hex ~59-60>/<file>.whl|.tar.gz|.zip` (200 — hash segment is NOT fixed 64)
- cargo sparse index: `index.crates.io/<layout>` — **no `/index/` prefix**; four name-length layouts, all 200: `/1/<c>`, `/2/<cc>`, `/3/<c>/<ccc>`, `/cc/cc/<name>`
- cargo downloads: `crates.io/api/v1/crates/<name>/<ver>/download` (302→static) and `static.crates.io/crates/<name>/<ver>/download` (200); legacy `static.crates.io/crates/<name>/<name>-<ver>.crate` still 200 — all three covered
- **Private registries deliberately NOT pattern-matched** (npm patterns anchored to registry.npmjs.org only): GitLab's `/-/` path segments would false-positive. Config-driven private-registry support is a follow-up.

### List-installed facts (don't re-derive these)
- `npm ls --json --all` → nested `dependencies` object tree, NOT an array; backend walks it recursively and dedupes (unit-tested).
- `cargo list` does not exist; installed surface is `cargo install --list` (plain text `name vX.Y.Z:` headers); backend parses it (unit-tested).
- `pip list --format=json` → JSON array (as expected).

### Backend contract reference (how the landed files map to the trait)

Implement the `PackageManagerBackend` trait — same shape as AptBackend:
- `kind()` → `PackageManagerKind::Npm|Pip|Cargo` (variants + `as_str()`/`parse_kind()` added this phase, registered in both `get_all_backends()` and `get_backend()`).
- `is_available()` — via the PM's own CLI on PATH (npm/pip/cargo).
- `list_installed()` — call the PM's CLI (`npm ls`, `pip list`, `cargo ...`).
- `upstream_url_patterns()` — **the core of Phase B.** Return patterns with `with_class()`:
  - npm: metadata `registry.npmjs.org/<pkg>` (class `LangMetadata`) + tarballs `<pkg>/-/<pkg>-<ver>.tgz` (class `LangArtifact`). Plus configured private registry base.
  - pip: `pypi.org/simple/<name>/` (metadata) + `files.pythonhosted.org` artifacts (`#sha256=` fragments).
  - cargo: `index.crates.io/<...>` sparse index (metadata) + `crates.io/api/v1/crates/<...>/download` + `static.crates.io` (artifacts).
- `url_to_cache_path()` — MUST encode name+version so Phase C floors are computable (`<ecosystem>/<name>/<version>/<blob>` layout).
- `proxy_config()` / `remove_proxy_config()` — config actions:
  - npm → write `~/.npmrc` `registry=http://<proxy>:3128/npm` (path-rewriting deferred to proxy; see §4 note in plan).
  - pip → `pip.conf` `[global] index-url = http://<proxy>:3128/pypi/simple` + trusted-host.
  - cargo → `~/.cargo/config.toml` replace-with + sparse registry.

### Per-backend gotchas (from the plan §5 risks)
- **npm Accept header:** packuments differ by `Accept` (`application/vnd.npm.install-v1+json` abbreviated vs full). The cache key must include the normalized Accept variant — this is Phase A's "accept header variant" TODO. If npm metadata currently keys without it, that's a latent bug to fix in B.
- **cargo config fragility:** pin behavior against cargo ≥1.78 (sparse default).
- **PyPI host split:** metadata on pypi.org, artifacts on files.pythonhosted.org — patterns must cover both and classify correctly.
- **npm scoped-token passthrough:** forward `Authorization` header on npm fetches; never persist/log it. Add a negative test (token never in cache dir).

### Wiring order per leaf
1. Backend file + unit tests (patterns, url_to_cache_path, availability).
2. Register backend struct in `get_all_backends()` (add `Box::new(NpmBackend::new())` etc.) AND `get_backend(kind)` match arm — both in `crates/monopsony-common/src/package_manager/mod.rs`. These are the ONLY registration edits; nothing else touches proxy wiring because `ProxyState::new` pulls from `get_all_backends()`, so classification flows automatically.
3. Verify classification: a request matching the new patterns classifies as `LangMetadata`/`LangArtifact` via `classify_url()` (highest-priority pattern wins).

---

## Phase C/E/F — already-in-place, do NOT redo (context only)

- **Scrubber + retention floors:** `last_verified_at` field + audit table are in place but unimplemented. Janitor extension is Phase C.
- **Admin API endpoints** (`GET/PATCH /api/v1/admin/language-pms`) — DB helpers `get_language_pms/set_language_pms/record_pm_enablement_audit` exist; the axum handlers are Phase D.

---

## §4 note (transport model) — resolved by live bench verification 2026-09-21/22

**Decision: transparent forward-proxy shipped in Phase B** (npm `proxy=`/`https-proxy=`, pip/cargo proxy env). Live findings on `.58`:

- **WORKS (cacheable)**: cargo sparse index over plain http (`index.crates.io` serves 200 over http) — MISS→HIT proven live, `cache_class=lang_metadata` persisted in Postgres. Distro paths unchanged (Arch core.db HIT).
- **GAP (uncached by upstream design)**: npm and PyPI and static.crates.io **301/403/308 plain-http to https**, and the proxy caches only legs it fetches itself — https goes through CONNECT as an opaque tunnel (verified: valid tarball bytes through the tunnel, zero caching, no monopsony headers). **RESOLUTION (Phase B+, this session): registry shims** — `handle_proxy_request` refactored into `handle_request_uri(state, limiter, addr, request, url)` taking a canonical upstream URL; six origin-form routes registered in `create_proxy_router`: `/npm/*path`→registry.npmjs.org, `/pypi/simple/*path`→pypi.org, `/pypi/packages/*path`→files.pythonhosted.org, `/cargo-index/*path`→index.crates.io, `/crates/*path`→static.crates.io. Shims flow into the identical cache pipeline (classification/TTL/sidecars) — a shim fetch of the cargo index lands as the same `lang_metadata` entry the proxy-form fetch created. Client config: npm `registry=http://ADDR/npm`, pip `index-url=http://ADDR/pypi/simple`, cargo `sparse+http://ADDR/cargo-index/`. **Remaining sub-gap: artifact bodies** — pip's simple pages embed absolute files.pythonhosted.org links and cargo's config.json dl template points at upstream https, so those artifact downloads bypass the shim unless the proxy rewrites bodies (npm tarballs DO go through the shim since they're fetched from the registry host itself).
- **TTL bug found & fixed during verification**: `check_freshness` gated on `lang_artifact` (inverted); corrected to `lang_metadata` — metadata revalidates, artifacts don't. Tests updated; proxy suite 140 green. **Live-proven on .58 after redeploy** (binary sha `5d32fca…`): cargo index entry cached at 1790052491 refetched at 1790089681 (~37k s > TTL 300s) → Revalidated via HEAD+ETag, sidecar `cached_at` refreshed to fetch time, served `x-cache: HIT` (correct — revalidated hits serve from cache; DB `cached_at` is miss-time-only by design, TTL bookkeeping lives in the sidecar).

## Verification gate before committing Phase B
Fleet verification on `.58` bench (plan §6): `npm --cache /tmp/nc install left-pad@1.3.0` twice → MISS then HIT; packument revalidation at TTL edge; STALE under upstream-block. Same shape for pip + cargo. Regression: pacman `-Sy` + apt fetch through proxy still green.
