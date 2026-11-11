# Monopsony store-source spike: Accrescent

**Task:** t_046d00f4 — Monopsony research lane, store-source spike (Accrescent); one of five store-source spikes required by the mobile roadmap kickoff gate.
**Status:** complete (doc-grounded, no code, no git operations, no builds, no bench access).
**Output:** this single file only — `docs/plans/2026-11-11-store-spike-accrescent.md`.
**Format:** mirrors `docs/plans/2026-11-10-mobile-store-source-spikes.md` (read-only, not edited).

## Method

Every load-bearing claim cites a corpus file fetched 2026-11-10 (official Accrescent site/FAQ/GitHub README/blog) or is marked `UNVERIFIED` with a one-line reason. Nothing fabricated or guessed. Network discipline: corpus first, no searches used — all gaps below are corpus-capped and marked UNVERIFIED rather than inferred. APP signing vs REPO-metadata signing are distinguished throughout; the corpus FAQ contains the apksigner certificate-hash story but thin repo-metadata signing detail, so repo-signing claims are marked UNVERIFIED rather than inferred.

Repo context cited read-only: `crates/monopsony-common/src/shadow_types.rs` — `ShadowSource { GithubRelease, GitRelease, DirectUrl }`, `ShadowRecipe { package, install, asset_selector { tag_pattern, rules[{os, arch, libc, pattern}] }, version { detect_command, detect_pattern } }`, plus `VersionDetect { method: Command|File|Header, source, pattern }` and `SignedRecipe` (Ed25519 operator signature over canonical recipe TOML).

---

## 1. Mechanism — index/wire format + client update discovery

- Accrescent is a centralized Android app store (alpha; Android 10+). Developers submit already-built, already-signed apps directly through the Accrescent Developer Console (console.accrescent.app); Accrescent does not build, sign, or update developers' apps itself. [accrescent-0: https://accrescent.app/faq/]
- Client-facing features: app signing key pinning (TOFU on first install), signed repository metadata, automatic unattended unprivileged updates (Android 12+), split APKs optimized per device, no remote APK signing, no account required. [accrescent-1: https://accrescent.app/]
- Update discovery: the client fetches signed repository metadata from Accrescent's servers and discovers new versions per app. The exact index/wire format (metadata URL, schema, diff support) is NOT documented in the corpus → UNVERIFIED: corpus lists "signed repository metadata" as a feature but publishes no endpoint or format.
- APK delivery: split APKs — downloaded APKs are optimized for the device to save bandwidth. Whether APK URLs are static and shared across clients or derived per device is not documented → UNVERIFIED (has cache-partitioning implications if per-device).
- The Accrescent client app itself is distributed via GitHub Releases (e.g. https://github.com/accrescent/accrescent/releases/download/0.29.1/accrescent-0.29.1.apk). That is distribution of the store client, not of store content. [accrescent-1]

## 2. Signing/integrity — certificate-hash pinning model, verification path, what a proxy CANNOT substitute

- APP signing: developers sign APKs with their own keys; no remote APK signing — developers retain full control of app signing keys. [accrescent-1; accrescent-0]
- App signing key pinning: first-time installs are verified via TOFU (trust on first use); the corpus does not document whether the pinned key is refreshed on update or pinned permanently → UNVERIFIED.
- CLIENT trust anchor: Accrescent's own signing certificate SHA-256 hash `067a40c4193aad51ac87f9ddfdebb15e24a1850babfa4821c28c5c25c3fdc071` is published on the website, the GitHub README, and Twitter. Verify the downloaded client APK with `apksigner --print-certs`, checking the hash from a different source than the app download. [accrescent-0; accrescent-2: https://raw.githubusercontent.com/accrescent/accrescent/main/README.md]
- REPO-metadata signing: "Signed repository metadata: repository contents are protected against malicious tampering." [accrescent-1] No signing algorithm, key distribution, or verification path is documented → UNVERIFIED (corpus thin; do not infer Ed25519/JAR-signing/etc.).
- What a proxy CANNOT substitute: any trust anchor. App signing key pinning (TOFU, client-side) and repo-metadata signature verification (client-side) are end-to-end; a proxy serves bytes only and cannot vouch for content integrity. The FAQ's guidance to check the certificate hash from a different source than the download confirms out-of-band trust anchoring is expected of users. [accrescent-0]

## 3. Proxy/mirror compatibility — static vs dynamic, CDN layout, byte-servable vs client-side

- Accrescent explicitly does NOT support third-party repositories and has no plans to: it breaks the Android single-app-source assumption, Accrescent cannot maintain hardcoded keys for all third-party repos, and TLS pinning is impractical for third-party servers. This is a project-level architectural rejection of mirroring/federation — the strongest compatibility signal in the corpus. [accrescent-0]
- No CDN/static-file layout is documented (repo metadata URL, APK URL patterns, ETag/Last-Modified support) → UNVERIFIED whether a caching proxy can byte-serve Accrescent repo traffic; no live endpoint checks available (no network beyond corpus).
- Split APKs per-device optimization: if assets are device-specific, a shared cache serves different bytes to different clients → cache-partitioning concern, unverified in corpus.
- What a proxy can byte-serve vs what is end-to-end client-side: unknown from corpus; anything served would still require client-side verification (repo-metadata signature, app signing key TOFU).

## 4. ToS/legal notes incl. Google developer-verification interaction

- No mirroring Terms of Service located in the corpus; the project's stance is anti-third-party-repo by design rather than a published anti-mirroring ToS — absence noted, not asserted. [accrescent-0]
- Google Android developer verification: starting September 2026 in Brazil, Indonesia, Singapore, Thailand (global rollout planned after), all apps must be registered by identity-verified developers to install on certified devices; Google later amended to add an experienced-user flow for unregistered apps. [accrescent-3: https://blog.accrescent.app/posts/android-developer-verification/]
- Accrescent position: no conflict with its model (developers sign their own apps from the start); it will continue allowing developers to submit unregistered apps; Accrescent registered for early access and registered itself in the program; slight submission-policy changes are forthcoming. Impact on client-side install flows unresolved → UNVERIFIED. [accrescent-3]
- Trademark: "Accrescent" name and logo are common law trademarks; derivatives (forks, unofficial builds) are forbidden. Relevant if an integration rebrands or derives Accrescent assets. [accrescent-2]
- Funded purely by donations. [accrescent-0]

## 5. VERDICT: PARTIAL

Confidence: MEDIUM.

Reasoning: Accrescent can be consumed as a download source for individual apps (developer-signed APKs served over HTTPS; client verifies app signing keys TOFU and signed repo metadata end-to-end), which maps to Monopsony's `ShadowSource::DirectUrl` (url_pattern/scrape discovery + `asset_url_template`) at best — but the corpus documents no repo index/wire format, no static/CDN layout, and no per-app version-discovery mechanism, so a recipe-driven store sync cannot be specified from this corpus. The project explicitly rejects third-party repositories by design, and split APKs may be per-device, both cutting against a transparent caching-proxy integration. Not NOT-FEASIBLE because individual APK byte-caching is not ruled out and `DirectUrl` can express per-app downloads; not FEASIBLE because no store-level mirror contract exists in the corpus. All trust decisions remain client-side (TOFU pinning, repo-metadata signature, cross-source certificate-hash checks), so Monopsony's proxy adds no trust value and cannot substitute the anchors.

## Mapping Accrescent → Monopsony shadow recipe (shadow_types.rs, read-only)

| Accrescent concept | Monopsony equivalent (`crates/monopsony-common/src/shadow_types.rs`) |
|---|---|
| Per-app developer-signed APK download | `ShadowSource::DirectUrl`, `source_url` = app APK/page URL; `direct_url.asset_url_template` + `version_discovery {scrape|url_pattern}` |
| Signed repository metadata / update index | No equivalent schema field; wire format undocumented → gap |
| App signing key pinning (TOFU) | Client-side trust anchor; not represented in the recipe schema — a proxy cannot substitute it |
| Per-device split APKs / ABI filtering | `asset_selector.rules[{os, arch, libc?, pattern}]` partial fit; Accrescent optimization is server-side, not client-selectable → unverified |
| Per-app version detection | `version.detect_command`/`detect_pattern` or `VersionDetect {method: Header|File|Command}` — mechanism undocumented → UNVERIFIED |
| Accrescent client distribution (GitHub release 0.29.1) | `ShadowSource::GithubRelease`, `source_url = https://github.com/accrescent/accrescent` — store client only, not store content |
| Signed recipe (Ed25519 over canonical recipe TOML) | Monopsony operator signs recipes; distinct from Accrescent's repo-metadata signing (scheme unverified) |

## Appendix — working notes

**Fetched and used as sources**
1. https://accrescent.app/faq/ (accrescent-0) — apksigner `--print-certs` verification; cert hash `067a40c4...fdc071`; check hash from a different source than the download; no third-party repos rationale; Developer Console submission; quality control via developer docs requirements.
2. https://accrescent.app/ (accrescent-1) — features incl. app signing key pinning (TOFU), signed repo metadata, unattended unprivileged updates (Android 12+), split APKs, no remote signing, no account; download URL `.../releases/download/0.29.1/accrescent-0.29.1.apk`; GrapheneOS App Store recommended for GrapheneOS users.
3. https://raw.githubusercontent.com/accrescent/accrescent/main/README.md (accrescent-2) — same cert hash; verify against website + Twitter; trademark + derivatives policy.
4. https://blog.accrescent.app/posts/android-developer-verification/ (accrescent-3) — Google developer verification from Sept 2026 (BR/ID/SG/TH first); amended experienced-user flow for unregistered apps; Accrescent early access + registration; unregistered submissions continue.

**URLs probed that did not yield usable docs:** none — corpus-limited; no searches performed per network discipline.

**Deliberately not claimed (UNVERIFIED in-text):** repo metadata URL/format/diffs; repo-metadata signing algorithm + key distribution; whether the app signing key pin refreshes on update; APK URL staticness and ETag support; CDN layout; per-device vs shared split-APK assets; effect of Google developer verification on submission policy/install flow; any mirroring ToS.

**Decisions for downstream implementation (recommendations, not implemented in this lane)**
- Treat Accrescent as a per-app `DirectUrl` candidate only if an app's APK URL proves static and shareable; repo-level sync is not supported by the project design.
- Any integration must leave trust anchors client-side (TOFU pinning, metadata signature) — Monopsony can cache bytes, not vouch for them.
- Re-check after Accrescent publishes repository-metadata docs or a mirrors program (project currently rejects both).
