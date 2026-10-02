# Sprint 26 Phase 1 (B1 Homebrew) — shared context pack

Facts below are BENCH-VERIFIED (m2mini, brew 7.0.7, macOS 26.3.1, 2026-10-01) or
LIVE-PROBED (2026-10-01, curl from thequietbox). Do not re-derive; encode.

## Wire facts (from the spike, ~/.hermes/cache/scratch/brew-spike/)

Client env vars point brew at the plain-http shim (mirror-list semantics: on
shim 404 brew SILENTLY falls back to origin — safe degradation, never breaks
installs):

- `HOMEBREW_API_DOMAIN=http://<proxy:3128>/brew/api` → brew requests
  `/brew/api/formula/<name>.json`, `/brew/api/cask/<name>.json`,
  `/brew/api/internal/packages.<platform-tag>.jws.json` (15.6 MB aggregate JWS).
  Upstream: `https://formulae.brew.sh/api/...` — probe: HTTP 200,
  `content-type: application/json; charset=utf-8`, `etag`, `cache-control:
  max-age=600`, identity encoding. Platform tags are OS-shaped (`arm64_tahoe`).
- `HOMEBREW_BOTTLE_DOMAIN=http://<proxy:3128>/brew/bottles` → brew requests
  TWO routes, in this order (hits.log proof):
  1. `GET /brew/bottles/<name>/manifests/<version>-<rebuild>` — upstream
     `https://ghcr.io/v2/homebrew/core/<name>/manifests/<version>-<rebuild>`.
     LIVE-PROBED: **200 direct** (no redirect), `etag` present,
     `content-type: application/vnd.oci.image.index.v1+json`, ~24 KB.
  2. `GET /brew/bottles/<name>-<version>.<platform>.bottle.<rebuild>.tar.gz` —
     upstream is the ghcr blob for the digest in the formula JSON.
     LIVE-PROBED: ghcr blob GET is **307 → signed CDN URL**
     (pkg-containers.githubusercontent.com, ~8h signed). The proxy's hyper
     upstream client does NOT follow redirects → blob fetch MUST use reqwest
     (redirect-following, same as docker.rs).

ghcr anonymous token dance (verified in fetch_manifest.py/fetch_bottle.py):
```
GET https://ghcr.io/token?scope=repository:homebrew/core/<name>:pull&service=ghcr.io
→ {"token": "..."} ; then Authorization: Bearer <token> on manifest/blob GET.
```
Manifest GET also needs Accept:
`application/vnd.oci.image.index.v1+json, application/vnd.oci.image.manifest.v1+json`.

Digest source: the formula JSON (cache key `brew/api/formula/<name>.json`)
carries `bottle.stable.files.<platform>.url` =
`https://ghcr.io/v2/homebrew/core/<name>/blobs/sha256:<digest>` and
`.sha256` = bare hex. `bottle.stable.rebuild` = int. `versions.stable` = "1.8.2".
Platform keys: `arm64_tahoe`, `arm64_sequoia`, `sonoma`, `x86_64_linux`, …
(underscores, never dots).

Bottle filename ambiguity: `<name>-<version>.<platform>.bottle.<rebuild>.tar.gz`
— names may contain dashes, so the name/version split is NOT derivable from the
filename alone. Resolution: brew requests the manifest (which contains `<name>`)
immediately before the blob; keep a small LRU of
`prefix "{name}-{version}." + rebuild → name` populated at manifest-serve time;
blob lookup = linear scan of prefixes. Fallback on miss (restart mid-install):
`storage.list_keys()` filtered to `brew/api/formula/`, parse each JSON, check
whether `versions.stable`+`bottle.stable.rebuild`+platform reconstruct the
filename. Still miss → 404 → brew falls back to origin (verified safe).

## Client config surface (docs-verified, Homebrew discussions #6221/#5232)

`~/.homebrew/brew.env` — brew's NATIVE env file (user-specific; also
`$HOMEBREW_PREFIX/etc/homebrew/brew.env`, `/etc/homebrew/brew.env`; user >
prefix > system). Format: plain `KEY=value` lines, no `export`, no shell
expansion. Monopsony owns this file outright — no shell-profile clobbering.
XDG caveat: if XDG_CONFIG_HOME is set brew reads
`$XDG_CONFIG_HOME/homebrew/brew.env` instead — v1 writes the default path,
documented caveat. Write only API+BOTTLE domains (ARTIFACT_DOMAIN cask
prefixing is UNVERIFIED — deliberately out of scope, follow-up).

`config_file_is_proxied` (client lang_pm_config.rs:78) checks
`content.contains("http://")` — works on brew.env unchanged.

## Cache classes (decision matrix)

- formulae.brew.sh API JSON (incl. JWS bundle): **LangMetadata** (mutable;
  TTL revalidate rides free — etag present, no auth needed on revalidation).
- ghcr manifests: **LangArtifact** (immutable per name+ver+rebuild; avoids the
  revalidation-auth gap — the TTL-revalidate path would fetch WITHOUT our
  injected Bearer and 401). verify-on-serve on 24 KB is trivial.
- bottle blobs: **LangArtifact** (immutable; sha256 verified against the
  formula digest at commit — origin integrity for free).

## Cache keys / URL patterns

Canonical upstreams flow through `handle_request_uri`; `find_backend_for_url`
must match:
- `https?://[^/]*formulae\.brew\.sh/api/.*` → LangMetadata, priority 100.
- `https?://ghcr\.io/v2/homebrew/core/[^/]+/manifests/[^/]+$` → LangArtifact,
  priority 110. (Anchored to homebrew/core — docker.rs's generic ghcr.io
  handling is a separate bespoke path, not a PackageManagerBackend; no
  collision, but keep the anchor tight anyway.)

`url_to_cache_path` (brew.rs): `brew{url.path}` for both shapes
(`brew/api/formula/jq.json`, `brew/v2/homebrew/core/jq/manifests/1.8.2-1`).
Blob keys are constructed by the bespoke handler (below), NOT via patterns:
`brew/bottles/<name>/<version>/<filename>` — segments give retention a clean
(name, version) without filename-splitting.

## Code anchors (grounded 2026-10-01, master a4ae4b0)

- Backend registration, 7 edits: crates/monopsony-common/src/package_manager/mod.rs
  — `mod npm;` L15 area, `pub use` L28-36, enum ~L60 (add `Homebrew` after
  `Nix`), `as_str` ~L87, `parse_kind` ~L112, `get_all_backends` ~L335,
  `get_backend` ~L370. `UrlPattern::with_class(regex, prio, desc, CacheClass)`.
- Shim routes: crates/monopsony-proxy/src/proxy.rs `create_proxy_router`
  L254-266 — `.route("/npm/*path", any(handle_npm_shim))` pattern; static
  prefixes beat the `/*path` catch-all (matchit). HUB owns these edits.
- Enablement arm: proxy.rs ~L548 `PackageManagerKind::Npm => Some("npm"), …`
  add `Homebrew => Some("brew")`. HUB owns.
- Retention: crates/monopsony-proxy/src/retention.rs `parse_artifact_path` L71
  (`match segments[0]` — add `"brew" => parse_brew_path(&segments)`); the fn
  must return None for non-bottles paths (api/, v2/ = metadata).
- CVE ecosystem: crates/monopsony-proxy/src/scan_cve.rs `infer_ecosystem` —
  add a brew arm (path contains "brew" → OSV ecosystem "Homebrew").
- Client CLI: crates/monopsony-client/src/lang_pm_config.rs `parse_lang_pm`
  L16 — add `"brew" => Some(PackageManagerKind::Homebrew)`. Everything else
  (status/apply dispatch in main.rs `run_lang_pm_command`) is generic — no
  main.rs edit needed.
- Admin list: crates/monopsony-admin/src/language_pms.rs L28
  `SUPPORTED_LANGUAGE_PMS: [&str; 3]` → 4 entries (all usages are
  size-agnostic: contains/join/iterate).
- Dashboard: dashboard/src/lib/components/LanguagePmPanel.svelte —
  `type PmName = 'npm'|'pip'|'cargo'` L5, `PMS` array L10.
- ProxyState fields are `pub` (client, storage, db, …). CacheStorage:
  `spool(key) → SpoolHandle` (write/commit/abort, set_cache_class, auto
  sha256), `read_stream(key, range)`, `exists`, `read`, `list_keys`.
  SpoolHandle.commit() → StorageMeta (checksum inside).
- docker.rs (module to mimic for the blob handler): try_cache /
  fetch_upstream(reqwest) / cache_and_respond(streaming spool, never buffer).
  reqwest already a dep of monopsony-proxy.

## Pitfalls (from project skills — binding)

- `crate::Error::Io` is a struct variant `{path, source}`; `From<io::Error>`
  exists — use `?` directly. Linter "async fn in Rust 2015" on new files is a
  false positive — verify with cargo check.
- Every PackageRef needs `installed_at: 0`.
- Adding a PackageManagerKind variant breaks exhaustive matches — the known
  non-test match sites are in proxy.rs (enablement arm, HUB) and
  monopsony-common mod.rs (both owned by slice 1). Run `cargo check --workspace`
  after edits; test-target breaks only surface under `cargo test --no-run`.
- Clippy gate = `cargo clippy --all-targets` with ZERO `^error` lines (deny
  lints fire in test code — no `|| true` scaffolding, no unused imports).
- Parallel slices share crates: a sibling's mid-fix file can fail YOUR
  `cargo test -p` — check the error's file path before diagnosing your own.
- URL patterns must anchor hosts with `[^/]*` (never `.*`).
- Inventory: `brew list --formula --versions` + `brew list --cask --versions`
  (plain `name version` lines; strip `homebrew/` tap prefixes if present).
  `is_available()` = `which("brew")`.
- Fetch client is HTTP/1.1-only by design (CloudFront h2 resets) — do not
  "upgrade" it.
- Manifest auth injection: `request.headers_mut().insert(AUTHORIZATION, …)`
  in the shim BEFORE `handle_request_uri`; fetch_upstream copies it upstream
  (skips only hop-by-hop).
- Blob handler must check per-PM enablement itself (bespoke path bypasses
  handle_request_uri's gate): disabled → 404 (origin fallback).

## Acceptance (from the kickoff plan; bench gate is user-assisted)

Fresh `brew install <formula>` → MISS + cached; refetch/second machine → HIT;
`brew upgrade` serves bottles from cache; brew's sha256 verification passes
untouched; zero regression on Linux PMs; macOS lane green.
