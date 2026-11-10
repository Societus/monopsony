# Mobile store-source feasibility spikes: F-Droid + Obtainium

**Task:** t_5128761f — Monopsony mobile store-source research lane (kickoff gate: five store-source spikes; this card covers the two most promising).
**Status:** complete (doc-grounded, no code, no git operations).
**Output:** this single file only — `docs/plans/2026-11-10-mobile-store-source-spikes.md`. Working notes are appended below as an appendix; no separate progress file exists for this lane.

## Method

Every load-bearing claim below cites a URL I actually fetched (official docs, source repo, or live endpoint). Where documentation is thin or a claim rests on inference, it is marked `UNVERIFIED`. Nothing was fabricated or guessed. Fetched 2026-11-10 unless noted.

Repo context cited read-only: `crates/monopsony-common/src/shadow_types.rs` — `ShadowSource { GithubRelease, GitRelease, DirectUrl }`, `ShadowRecipe { package, install, asset_selector { tag_pattern, rules[{os, arch, libc, pattern}] }, version { detect_command, detect_pattern } }`, plus `VersionDetect { method: Command|File|Header, source, pattern }` and `SignedRecipe` (Ed25519 operator signature over canonical recipe TOML).

---

## Spike 1 — F-Droid (https://f-droid.org/)

### Mechanism — index wire format and update discovery

F-Droid publishes a **signed repository index** as the single source of truth for what apps/versions exist.

- Index files (live, fetched headers/body samples):
  - `https://f-droid.org/repo/index-v2.json` — 72,509,659 bytes, `application/json`, `ETag` + `Last-Modified` + HSTS. Top-level `repo` object with per-language name/description observed (packages array follows; full 72 MB not downloaded).
  - `https://f-droid.org/repo/index-v1.json` — listed as available.
  - `https://f-droid.org/repo/index-v1.jar` — 17,576,932 bytes, `application/java-archive`: the v1 index ships as a signed JAR containing `index-v1.json`; verify the JAR signature, then extract (docs name `fdroidserver.download_repo_index()` as the helper).
  - Diffs: `https://f-droid.org/repo/entry.json` lists `diffs` keyed by timestamp, e.g. `/diff/1793989171752.json` with `sha256`, `size`, `numPackages` — incremental change-only index files ("support for files that only contain the changes"). `maxAge: 14` (days) observed in entry.json.
  - Entry point: `https://f-droid.org/repo/entry.jar` (2,599 bytes) + `entry.json` + `entry.json.asc` — signed by JAR and GPG, lists available index and diff files.
  - Signer index: `https://f-droid.org/repo/signer-index.json` — SHA-256 fingerprint of each signer f-droid.org uses per package; also signed via `entry.json`, or directly `signer-index.jar` / `signer-index.json.asc`.
  - GPG signatures verified live: `index-v2.json.asc`, `index-v1.json.asc`, `entry.json.asc` (all `application/pgp-signature`, HTTP 200).
  Source: https://f-droid.org/docs/All_our_APIs/ ; live endpoint checks on f-droid.org/repo/*.

- APK files live under `/repo/`: `https://f-droid.org/repo/org.fdroid.fdroid_2000053.apk` (HTTP 200, 12,774,751 bytes, `application/vnd.android.package-archive`, `Last-Modified`) plus per-APK PGP signature `..._2000053.apk.asc`. Source: packages page https://f-droid.org/packages/org.fdroid.fdroid/ .

- Build metadata (recipes) for every app: `https://gitlab.com/fdroid/fdroiddata/-/raw/master/metadata/<ApplicationID>.yml` (YAML); GitHub mirror at raw.githubusercontent.com/f-droid/fdroiddata/master/metadata/*.yml. Source: https://f-droid.org/docs/All_our_APIs/ .

- Update discovery for a client: fetch `index-v2.json` (or the applicable diff chain from `entry.json`), compare version codes per package, download the target APK from `/repo/<pkg>_<versionCode>.apk`. The packages API (`https://f-droid.org/api/v1/packages/<id>`) is display-only per the docs — "not intended to be used for fetching or verifying apps"; signed index files are the recommended path.

- Mirror program: the index files embed the official mirror list; mirrors are also published at `https://gitlab.com/fdroid/fdroiddata/-/raw/master/config/mirrors.yml`; automated `mirror-monitor` checks mirrors. Security Model: "all of this is mirrored in many places across the internet by many different parties."

Sources: https://f-droid.org/docs/All_our_APIs/ ; https://f-droid.org/docs/Security_Model/ ; live endpoint checks.

### Signing/integrity

- Repo key: each repo is defined by a unique signing key (Security Model).
- index-v1 JAR signed `SHA1withRSA`; index-v2 uses `apksigner`-supported algorithms on Android 23+ (at launch `SHA256withRSA` / `SHA-256`); index-v1 still considered strong against second pre-image (relevant attack model for index JARs).
- As of index-v2, files from the repo are verified by **SHA-256**, including icons and screenshots (hashes live in the signed index).
- Signed metadata includes hashes of the app **and its signing key**, plus timestamp and expiry.
- The public key for verifying metadata signatures is **built into the F-Droid client app**; signing done on a fully offline machine for f-droid.org.
- APKs: Android enforces a valid signature over the entire APK; updates verified against the installed app's signature (same-key requirement). Per-APK `.asc` PGP signatures also published (observed live). `signer-index.json` maps package → signer SHA-256 fingerprint.
- `UNVERIFIED`: exact current algorithm in use for index-v2 at time of writing (docs state launch algorithm and reliance on OpenJDK/Google maintenance).
- `UNVERIFIED`: no published F-Droid root key fingerprint cited here — not fetched from a primary source.

Source: https://f-droid.org/docs/Security_Model/ ; live endpoint checks.

### Proxy/mirror compatibility

- **Highly compatible.** All repo traffic is static files served over HTTPS from nginx (index JSON/JAR, APKs, `.asc`, entry point), with `ETag`/`Last-Modified` conditional GET and HSTS — exactly what a caching proxy/mirror can serve.
- Clients choose mirrors themselves based on availability and freshness (local criteria incl. Tor usage); mirror list is inside the signed metadata, so a malicious mirror can only serve stale bytes, not forge index content (client verifies signature with built-in key).
- TLS: HTTPS by default, HTTP redirects to HTTPS. `UNVERIFIED`: certificate pinning for repo/mirror traffic — the Security Model lists "pinned TLS certificate built into the client app" under future improvements, i.e. **not documented as implemented**; client requires Certificate Transparency for f-droid.org and common repo/mirror domains.
- What a proxy **cannot** substitute: the trust anchor (built into the client) and the signed index — the proxy serves bytes only; signature verification is end-to-end client-side. The packages API and search API (`search.f-droid.org`) are separate services, not part of the mirror program (mirrors.yml covers `repo/` content; observed layout).
- Android APK signature check happens on install, independent of the proxy.

Sources: https://f-droid.org/docs/Security_Model/ ; https://f-droid.org/docs/All_our_APIs/ ; live header checks.

### ToS/risk notes

- Mirroring is explicitly endorsed by the project (Security Model; official mirrors list + mirror-monitor). No separate mirroring Terms of Service located in fetched docs — absence noted, not asserted.
- Index-v2.json is 72 MB full re-fetch; diff chain mitigates bandwidth but a mirror must keep diffs for the retention window it advertises.
- F-Droid APKs may carry a different signing key than the same app from other sources (Obtainium wiki documents cross-source update failures) — mixing sources for one app ID is a client-side risk, not a proxy risk.
- `UNVERIFIED`: any rate-limit or abusive-mirror policy enforced by f.droid.org on mirror fetches (not found in fetched docs).

Sources: https://f-droid.org/docs/Security_Model/ ; https://wiki.obtainium.imranr.dev/sources/ (F-Droid key note).

### VERDICT: FEASIBLE

Confidence: HIGH — index v1/v2 wire format, entry.jar/diff signing model, mirror program, and cache-friendly static file serving all verified against live f-droid.org endpoints and official docs; the only open items (current index-v2 algorithm in force, mirror fetch rate policy) are non-blocking for a caching-proxy integration.

---

## Spike 2 — Obtainium (https://github.com/ImranR98/Obtainium)

### Mechanism — source model and update discovery

Obtainium is a Flutter Android app that installs/updates apps **directly from their release sources** rather than a store index: GitHub, GitLab, Forgejo/Codeberg, F-Droid, third-party F-Droid repos, IzzyOnDroid, SourceHut, APKPure, Aptoide, Uptodown, itch.io, Huawei/Samsung/Tencent/vivo/RuStore/Farsroid/LiteAPKs/APK4Free/CoolApk/SourceForge stores, Jenkins jobs, APKMirror/RockMods (track-only), Telegram, Neutron Code, direct APK link, and an "HTML" fallback source that scrapes any URL for `.apk` links.

- Core loop (from the developer guide): one file per source in `lib/app_sources/` (29 sources + githubstars), each a subclass of `AppSource` registered in `SourceProvider._buildSources()`. The source contract method `getLatestAPKDetails(standardUrl, additionalSettings)` returns `APKDetails(version, apkUrls, names, releaseDate, changeLog, releaseUrl, appId, apkSizeBytes)`. Per-app options live in `additionalSettings: Map<String, dynamic>`; source-wide credentials (e.g. `github-creds`, `gitlab-creds`) in encrypted `flutter_secure_storage`.
- GitHub source: uses the GitHub API for release info; rate-capped (fine-grained personal access token workaround). Options: verify "latest" tag vs most recent release; sort method (date / smart name / name); use latest asset date as release date; check for repo rename; optional GitHub proxy prefix (e.g. gh-proxy.com) so all GitHub API traffic can route through a proxy; search with min-star filter.
- F-Droid source within Obtainium: reads `https://f-droid.org/en/packages/<id>/` once per check (10-minute in-memory cache) for per-APK ABI filtering and `Added on <date>` release dates (the F-Droid API itself does not expose ABIs or dates); third-party F-Droid repos need repo URL + app ID/name, version selection = "suggested version code" (default) or "highest version code".
- HTML fallback source: fetches URL, parses HTML, filters `.apk` links, sorts, applies user regex filter, picks first/last; pseudo-versioning by partial APK hash (default), APK link hash, or HTTP ETag; optional version-extraction regex with match group; custom request headers supported.
- Version detection: per-app `versionExtractionRegEx`, `releaseDateAsVersion` toggle, arch filtering by filename (`autoApkFilterByArch`); app ID inferred best-effort from URL/feed.
- Crowdsourced per-app configurations at https://apps.obtainium.imranr.dev/ (repo `ImranR98/apps.obtainium.imranr.dev`); users can also configure arbitrary sources manually in-app.
- `UNVERIFIED`: exact GitHub API endpoints Obtainium calls (docs state "uses the GitHub API to grab release info"; endpoint paths not fetched from the repo source tree).

Sources: https://github.com/ImranR98/Obtainium (README) ; https://wiki.obtainium.imranr.dev/ ; https://wiki.obtainium.imranr.dev/sources/ ; https://raw.githubusercontent.com/ImranR98/Obtainium/master/docs/DEVELOPER_GUIDE.md

**Correction note:** the task body's suspicion of "Ravendguy/Obtainium" is wrong — `Ravendguy/Obtainium` returns HTTP 404 on both GitHub's API and HTML; the real project is `ImranR98/Obtainium` (Dart, ~21.4k stars, verified via GitHub search + live repo README). Working notes below record the probed URLs.

### Signing/integrity

- Obtainium itself: package ID `dev.imranr.obtainium`; SHA-256 of signing certificate `B3:53:60:1F:6A:1D:5F:D6:60:3A:E2:F5:0B:E8:0C:F3:01:36:7B:86:B6:AB:8B:1F:66:24:3D:A9:6C:D5:73:62`; PGP public key (contact@imranr.dev) for verifying APK hashes; F-Droid flavour is bit-for-bit identical to self-built APK via reproducible builds.
- Obtainium does **not** sign the upstream APKs it distributes — it downloads them as-is from the source; integrity (if any) comes from the source (e.g. GitHub release assets over TLS, F-Droid index+APK signatures, per-APK `.asc`).
- Certificate pinning: opt-in per host in Obtainium (`builtInPinnedHosts` + user pin store `pinned_certificates.json`, schema v1, atomic temp+rename saves). Pinned hosts use `dart:io` with only that set's certificates, ignoring the Android trust store; more-specific host rules win, user sets win ties; resolution per redirect hook. Default off for legacy installs.
- Networking: all app HTTP via `HttpService`; on Android the default client is embedded **Cronet**, which "honors the OS trust store, user-installed CAs, disabled system CAs, **and system proxy settings**"; `network_security_config.xml` opts into system and user CAs. `dart:io` fallback only for `allowInsecure`/pinned requests or when Cronet DCL is blocked.
- `UNVERIFIED`: whether Obtainium pins GitHub release download hosts (`objects.githubusercontent.com`, `releases.github.com`, `github.com`) by default — built-in pinned host list not fetched.

Sources: https://github.com/ImranR98/Obtainium (README verification section) ; https://raw.githubusercontent.com/ImranR98/Obtainium/master/docs/DEVELOPER_GUIDE.md (networking + pinning sections) ; https://wiki.obtainium.imranr.dev/ (verification info).

### Proxy/mirror compatibility

- **Compatible at the transport level:** Cronet honors system proxy settings, so Obtainium's fetches of GitHub releases and store pages flow through a system-level caching proxy without app changes.
- GitHub source supports an explicit **GitHub Proxy Prefix** (e.g. gh-proxy.com) for API traffic — evidence the project expects API traffic behind proxies (rate limiting / region accessibility).
- What cannot be "mirrored" in the F-Droid sense: Obtainium has no publishable server-side index or repo of its own; each app is a separate source URL checked client-side. A Monopsony proxy could cache GitHub release assets (objects.githubusercontent.com etc. — standard TLS, no documented pinning obstacle on Obtainium's side) but per-app source configuration still lives on the client.
- APKMirror is track-only at the maintainer's request (issue #44) — scraping/download automation is explicitly unwelcome there; a proxy integration must respect source-specific ToS per app.

Sources: https://raw.githubusercontent.com/ImranR98/Obtainium/master/docs/DEVELOPER_GUIDE.md ; https://wiki.obtainium.imranr.dev/sources/ ; https://github.com/ImranR98/Obtainium (README).

### ToS/risk notes

- "For some sources, data is gathered using Web scraping and can easily break due to changes in website design" (wiki home + README Limitations) — HTML/store sources are fragile by design.
- GitHub API rate limits affect bulk app tracking; PAT or proxy prefix mitigation is user-configured.
- Cross-source signing key changes break updates (F-Droid key vs other sources) — client-side, documented in wiki.
- XAPK support incomplete/unreliable (wiki).
- Any source without a fixed host (third-party F-Droid repos, Jenkins, SourceHut) is not auto-recognized; manual "Override Source" selection required.

Sources: https://wiki.obtainium.imranr.dev/ ; https://wiki.obtainium.imranr.dev/sources/ ; https://github.com/ImranR98/Obtainium (README).

### VERDICT: PARTIAL

Confidence: MEDIUM-HIGH — Obtainium's GitHub-release source model maps cleanly onto Monopsony's existing shadow-recipe machinery (`source_type = github_release`, `source_url` = repo URL, `tag_pattern`/`asset_selector.rules` ≈ Obtainium's release sorting/filters/APK regex + arch filtering, per-app `additionalSettings` ≈ per-recipe options), and its networking is proxy-friendly. It is not FEASIBLE as a turnkey "store source" because Obtainium is a client app with no server-side index to mirror and several scraping-based, rate-limited, or track-only sources. Confidence is not higher because the endpoint-level mapping was not exercised against Monopsony code (research lane, no code).

---

## Mapping Obtainium → Monopsony shadow recipe (read-only schema reference)

| Obtainium concept | Monopsony equivalent (`crates/monopsony-common/src/shadow_types.rs`) |
|---|---|
| Source: GitHub releases | `ShadowSource::GithubRelease`, `source_url` = `https://github.com/<owner>/<repo>` |
| Release selection (latest tag / sort date / smart name) | `asset_selector.tag_pattern` + `asset_selector.rules[].pattern` over asset names |
| APK filter regex / arch filtering | `asset_selector.rules[{os, arch, libc?, pattern}]` |
| Version detection | `version.detect_command`/`detect_pattern` (existing) or `VersionDetect { method: Header|File|Command, source, pattern }` |
| Per-app options (additionalSettings) | per-recipe `[install]` fields + recipe TOML itself |
| Source credentials (github-creds) | `UNVERIFIED` for Monopsony — no credential store for recipes in fetched schema |
| HTML fallback / pseudo-versioning (ETag, link hash) | `ShadowSource::DirectUrl` with `version_discovery` + `asset_url_template`; ETag/link-hash versioning not represented in schema — gap |

Sources: shadow_types.rs (read); https://wiki.obtainium.imranr.dev/sources/ ; https://raw.githubusercontent.com/ImranR98/Obtainium/master/docs/DEVELOPER_GUIDE.md

---

## Appendix — working notes

**Fetched and used as sources**
1. https://f-droid.org/docs/ — docs index
2. https://f-droid.org/docs/Security_Model/ — signing model (index-v1 SHA1withRSA, index-v2 SHA-256 file verification, client-built-in verify key, offline signing, mirror list in signed metadata, HTTPS default, CT requirement, pinned-client-cert listed as improvement-not-implemented)
3. https://f-droid.org/docs/All_our_APIs/ — index-v1/v2.json, entry.jar/entry.json(+asc), signer-index.jar/json/asc, diff files, maxAge, fdroidserver.download_repo_index(), mirrors.yml raw URL, packages API display-only warning, fdroiddata YAML metadata, verification.f-droid.org
4. Live endpoint checks 2026-11-10: index-v2.json (200, 72,509,659 B, ETag/Last-Modified/HSTS), index-v1.jar (200, 17,576,932 B), entry.jar (200, 2,599 B), signer-index.json (200, 564,965 B), index-v2.json.asc (200), entry.json.asc (200), entry.json (200, timestamp/maxAge/diffs structure), /repo/org.fdroid.fdroid_2000053.apk (200, 12,774,751 B) + .apk.asc pattern from packages page HTML
5. https://f-droid.org/packages/org.fdroid.fdroid/ — APK URL pattern `/repo/<pkg>_<versionCode>.apk`
6. https://github.com/ImranR98/Obtainium — README (supported source list, verification info, install badges)
7. https://raw.githubusercontent.com/ImranR98/Obtainium/master/README.md — same (200)
8. https://wiki.obtainium.imranr.dev/ — wiki home (core goal, limitations)
9. https://wiki.obtainium.imranr.dev/sources/ — app sources (GitHub rate limits/PAT/filters/proxy prefix; F-Droid third-party repo rules; HTML source mechanics incl. ETag/link-hash pseudo-versioning; APKMirror track-only per issue #44; cross-source signing-key warning)
10. https://raw.githubusercontent.com/ImranR98/Obtainium/master/docs/DEVELOPER_GUIDE.md — AppSource contract, getLatestAPKDetails → APKDetails, additionalSettings, Cronet + system proxy, certificate pinning opt-in, F-Droid page scraping

**URLs probed that did not yield usable docs**
- `Ravendguy/Obtainium` (GitHub API + HTML): HTTP 404 — not the Obtainium repo; real project is `ImranR98/Obtainium`.
- https://wiki.f-droid.org/* — blocked from this environment (private/internal network address); used f-droid.org/docs/* instead.
- https://api.github.com/repos/Ravendguy/Obtainium — 404 (same misidentification).
- https://obtainium.app — Cloudflare parking/lander page (HTTP 200, `/lander`); project site is https://obtainium.imranr.dev (linked from README, not fetched).
- GitHub API rate-limit endpoints for Obtainium — not fetched (UNVERIFIED above).

**Deliberately not claimed** (thin documentation, marked UNVERIFIED in-text): current index-v2 signing algorithm in force; f-droid.org mirror fetch rate policy; Obtainium default pinned hosts; exact GitHub API endpoint paths Obtainium calls; F-Droid root key fingerprint; any mirroring Terms of Service beyond endorsement in Security Model.

**Decisions for downstream implementation** (recommendations, not implemented in this lane)
- F-Droid: mirror `https://f-droid.org/repo/` content (index v1/v2 + diffs + APKs + .asc) with ETag/If-Modified-Since passthrough; client verifies signatures end-to-end, so the proxy need not terminate customer TLS with pinned certs — but honor HSTS/HTTPS upstream.
- Obtainium: reuse `ShadowSource::GithubRelease` for GitHub-tracked apps; treat Obtainium's per-app configs (apps.obtainium.imranr.dev) as a candidate recipe catalog source — recipe authoring remains manual due to per-app source options and scraping fragility.