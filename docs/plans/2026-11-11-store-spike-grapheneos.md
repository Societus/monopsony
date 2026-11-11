# Store-source feasibility spike: GrapheneOS App Store (apps.grapheneos.org)

**Task:** t_30c8d713 — store-source spike lane (five-spike kickoff gate; sibling spikes: F-Droid + Obtainium banked 2026-11-10, Accrescent + Aurora banked 2026-11-11).
**Status:** complete (doc-grounded; corpus-first, capped network: 3 searches + 2 fetch topics used, no code, no git writes).
**Output:** this file + append-only progress notes in `context/kanban-progress-store-spike-grapheneos.md`.

## Method

Grounding corpus `tmp/spike-corpus-2026-11-10/grapheneos-{0,1,2}` (dispatcher-fetched 2026-11-10 from the URLs cited inline) was read in full before any network use; `grapheneos-2.txt` (usage guide, 574 lines) was section-mapped by header search and the Updates / Sandboxed Google Play / Android Auto regions read. The repo README corpus file was re-fetched live (2026-11-10) and is byte-identical — the corpus README is the complete upstream README. Every load-bearing claim cites corpus file + source URL, or a capped-search URL. `UNVERIFIED` marks the rest. Nothing fabricated.

Repo context cited read-only: `crates/monopsony-common/src/shadow_types.rs` — `ShadowSource { GithubRelease, GitRelease, DirectUrl }`, `InstallMethod { Extract, Build, Script }`, `RecipeInstall` (target_dir, service, pre/post_update hooks), `HealthCheck { Http, Port, Process }`, `ReleaseAsset { name, url, size, sha256 }`, `SignedRecipe` (Ed25519 operator signature).

---

## 1. Mechanism — repository layout, client discovery, verification

- **What the store is:** the GrapheneOS "app repository client (App Store) fetches generic signed update metadata and signed package updates (APKs) from https://apps.grapheneos.org/ (a separate name for the same servers as https://releases.grapheneos.org). It provides out-of-band updates to certain apps bundled with the OS and other apps available in our repository." (corpus `grapheneos-1.txt` L383 ← https://grapheneos.org/faq)
- **Repository content** (from the AppStore repo release notes, capped-search snippet): "our standalone apps, out-of-band updates to certain GrapheneOS components and **a mirror of the core Google Play apps and Android Auto** to make it easy for GrapheneOS users to install sandboxed Google Play with versions of the Google Play apps we've tested with our sandboxed Google Play compatibility layer." Users "must obtain GrapheneOS app updates through our App Store since verified boot metadata is required for out-of-band system app updates." (https://github.com/GrapheneOS/AppStore/releases, search snippet 2026-11-10)
- **Package artifact format:** each published app ships as APK + fs-verity signature sidecar — `fsverity sign app-release.apk app-release.apk.fsv_sig --key fsverity_private_key.0.pem --cert fsverity_cert.0.pem`; "`generate.py` will automatically sign all the published apps with the fs-verity key." Sidecar-install path confirmed by release notes: users without the store install "with adb install-multiple with both the APK and fs-verity metadata." (corpus `grapheneos-0.md` ← repo README; https://github.com/GrapheneOS/AppStore/releases/tag/23, search snippet)
- **Android 15+ change:** "GrapheneOS uses APK signature scheme v4 signed with the same key as the APK instead of a separate fs-verity key. You don't need the fs-verity setup anymore if you only support current versions of GrapheneOS." (corpus `grapheneos-0.md`)
- **Update discovery (client side):** the App Store is bundled with the OS; the OS System Updater on the same server infra polls metadata ~every 6 h from fixed static URLs (`releases.grapheneos.org/DEVICE-CHANNEL`, delta `DEVICE-incremental-OLD-NEW.zip`, fallback `DEVICE-ota_update-NEW.zip`) and "No query or data is sent to the server." (corpus `grapheneos-1.txt` L371–379, L375; `grapheneos-2.txt` §Updates L189–193 — background updates, delta-then-full, resumable)
- **`UNVERIFIED`: exact filenames/parse layout of the "generic signed update metadata".** The README documents signing-key generation only, not the metadata file layout; the client source (GrapheneOS/AppStore) was not fetched (fetch budget spent; one-line reason: layout not in corpus, client repo unvisited). Also `UNVERIFIED`: whether the client exposes any alternate-repo/alternate-server setting — none documented anywhere in the corpus.

## 2. Signing/integrity — key lineage, client verification, trust anchor

- **Metadata signing:** repository metadata signed with a signify key — `signify -G -n -p apps.0.pub -s apps.0.sec`; "The `0` refers to the generation of the key. This is used for key rotation." (corpus `grapheneos-0.md`) Generation-numbered keys = explicit rotation path.
- **fs-verity (pre-Android-15 / cross-OS path):** optional per-OS fs-verity signing key (RSA-4096 self-signed cert, CN=GrapheneOS, shipped as `.pem` + `.der`); an OS vendor embeds the fs-verity key "in the supported keys built into the OS" to get **continuous verification via verified boot** instead of one-time APK signature checks at boot — which are "actually largely skipped for most boots for apps without fs-verity due to the performance cost." (corpus `grapheneos-0.md`)
- **Why GrapheneOS mandates this:** "GrapheneOS requires fs-verity for system app updates as part of fully extending verified boot to system app updates. Android doesn't enforce any form of verified boot for system app updates so they can be used to bypass verified boot… since signature checks and downgrade protection aren't enforced at boot." (corpus `grapheneos-0.md`)
- **What the client verifies:** update-server data is untrusted — "signature verification with downgrade protection" client-side, with verified boot as the second layer; the OS updater additionally rejects any update carrying a `serialno` constraint. (corpus `grapheneos-1.txt` L379; `grapheneos-2.txt` §Updates/Security L217–219) Android's package installer independently enforces APK signatures at install.
- **Trust anchors:** fs-verity trust anchor = keys built into the OS (per the own-OS vendor note above). `UNVERIFIED`: where the App Store client embeds the signify public key (`apps.0.pub`) — corpus shows key generation/rotation but not client-side embedding.
- **Key custody:** "GrapheneOS servers do not have access to GrapheneOS signing keys" (corpus `grapheneos-1.txt` L379); signing happens on "hardened local machines for building and signing rather than servers outside our physical control" (https://apps.grapheneos.org/ "GrapheneOS servers" article, fetched 2026-11-10).

## 3. Proxy/mirror compatibility

- **Transport profile is proxy-clean:** same origin servers as `releases.grapheneos.org` (corpus FAQ L383); static GETs of fixed-path files with no query data (corpus FAQ L371–375); "The update protocol doesn't send identifiable information to the update server and works well over a VPN or Tor" (corpus `grapheneos-2.txt` L217). Byte-serving these files is trivial — standard static HTTPS.
- **Premise correction (the card assumed "already mirror-served infrastructure"):** GrapheneOS explicitly runs **no third-party mirror program** — "Services are hosted entirely via our own dedicated servers and virtual machines without involving any additional parties for CDNs, SaaS platforms, **mirrors** or other services." (https://grapheneos.org/features, search snippet 2026-11-10). `releases.grapheneos.org` is mirror-*able* static hosting, not mirror-*sanctioned*. The signed-metadata design means a corrupt mirror is fail-safe (client rejects), but nothing sanctions running one.
- **What a Monopsony proxy can legitimately do:** nothing beyond opportunistic in-transit byte caching, and even that is constrained: `apps.grapheneos.org` is HTTPS (CONNECT-tunnel traffic is opaque — no byte cache without TLS interception). Whether the App Store client honors Android system proxy settings is `UNVERIFIED` (client networking stack undocumented in corpus). TLS-intercept MITM via a LAN-trusted CA (or DNS override to a like-named local vhost) would defeat the GrapheneOS threat model the store exists to enforce and is **rejected as a design** here, independent of whether pins would allow it (`UNVERIFIED`: cert pinning/CT enforcement in the client).
- **What a Monopsony mirror cannot do:** substitute as origin. The client's repo host is the OS-integrated `apps.grapheneos.org`; no corpus evidence of any configurable alternate repo base URL (see §1 `UNVERIFIED`), and users are told to obtain updates "through our App Store" (AppStore release notes). A like-for-like F-Droid-style mirror would serve files nobody's client ever fetches.

## 4. ToS/legal — what a third party may mirror or redistribute

- **No mirroring/redistribution permission found anywhere:** the only stated policy for `apps.grapheneos.org` is the privacy side — GrapheneOS services follow the EFF Do-Not-Track policy for all users of public services, with `apps.grapheneos.org` in the enumerated list. (corpus `grapheneos-1.txt` L497–509) A DNT privacy policy is not a content license. Absence of a mirror program (§3) plus absence of any redistribution grant ⇒ treat the repository as **not licensed for third-party redistribution** until GrapheneOS says otherwise.
- **App Store client license:** MIT — "Copyright © 2021-2026 GrapheneOS Permission is hereby granted, free of charge… to deal in the Software without restriction, including… publish, distribute, sublicense, and/or sell copies…" (https://github.com/GrapheneOS/AppStore LICENSE, search snippet; org listing shows "MIT License"). The *client code* being MIT does not license the *served content*.
- **Third-party content in the repo:** the Google Play apps and Android Auto APKs are Google-proprietary; GrapheneOS carries them as a tested-compatibility mirror for its sandboxed-Play layer (AppStore release notes; corpus `grapheneos-2.txt` L401–403, L462). Re-redistribution of those binaries by another party has no grant from GrapheneOS to inherit, and Google's position on this specific mirror is `UNVERIFIED`.
- **Per-app licenses of GrapheneOS standalone apps in the repo:** `UNVERIFIED` (only the App Store client's MIT license verified; per-app LICENSE files not fetched — budget).

## 5. VERDICT: NOT-FEASIBLE

Confidence: MEDIUM-HIGH. The wire contract is the cleanest of the five stores — static files, signify-signed metadata with generation-numbered key rotation, APK+v4/fs-verity end-to-end verification that makes any cache fail-safe — and it runs on plain static HTTPS. It is nevertheless not feasible as a Monopsony store source for three independent reasons: (1) **no redirect path** — the App Store client is OS-integrated with a hard-wired `apps.grapheneos.org` origin and no documented alternate-repo setting, so a Monopsony mirror would serve files no client ever requests; the only interception routes are TLS MITM/DNS hijack, which is an adversarial posture against the exact verified-boot guarantees this store exists to enforce, and is rejected; (2) **no authorization** — GrapheneOS runs no mirror program ("without involving any additional parties for … mirrors"), publishes no redistribution grant (EFF DNT is a privacy policy, not a license), and the repo's most-bandwidth-heavy content (Google Play apps, Android Auto) is Google-proprietary with no grant GrapheneOS could inherit from; (3) **no schema fit** — `ShadowRecipe`/`InstallMethod` are POSIX-desktop shapes (extract/build/script, target_dir, port/process health checks) with no Android APK-install concept, and upstream's signify-signed metadata is not Ed25519 operator-recipe material. Legitimate touchpoint, as with Aurora: none beyond uncertain incidental in-transit CONNECT-tunnel caching (client system-proxy behavior `UNVERIFIED`). If GrapheneOS ever sanctions mirrors, revisit — the technical serving side would then be a day of work.

### Mapping → `crates/monopsony-common/src/shadow_types.rs` (read-only)

| GrapheneOS App Store concept | Monopsony equivalent | Fit |
|---|---|---|
| signify-signed update metadata (layout `UNVERIFIED`) | `SignedRecipe` (Ed25519 over canonical TOML) | none — different scheme; upstream metadata not recipe material |
| APK + `.apk.fsv_sig` sidecar / v4 `.idsig` (Android 15+) | `ReleaseAsset { url, sha256? }` | partial — could list assets, but no honest sha256 source without verified metadata layout |
| hard-wired `apps.grapheneos.org` origin | `ShadowSource::DirectUrl { version_discovery, asset_url_template }` | none — client cannot be pointed elsewhere |
| APK install/update on Android | `InstallMethod { Extract, Build, Script }` | none — no APK-install method; adb-scripting would bypass the store's own contract |
| GrapheneOS-side downgrade protection + rollback | `downgrade_unsafe`, `failure_policy`, `SuccessMonitor` | conceptual parallel only (OS-enforced, not recipe-enforced) |

---

## Appendix — provenance & check-states

**Corpus files (all read 2026-11-10):**
1. `grapheneos-0.md` ← https://raw.githubusercontent.com/GrapheneOS/apps.grapheneos.org/main/README.md (read in full, 38 lines; live re-fetch byte-identical — complete README)
2. `grapheneos-1.txt` ← https://grapheneos.org/faq (App Store L383; OS-updater connections L363–381; privacy-policy service list L497–509)
3. `grapheneos-2.txt` ← https://grapheneos.org/usage (574-line extract; header-mapped, Updates §L187–241 read in full, Sandboxed Google Play L397–412, Android Auto L456–462)

**Capped-search URLs used (3 queries, 2026-11-11):** github.com/GrapheneOS/AppStore/releases + /releases/tag/23 (repo content + fs-verity-metadata install requirement); grapheneos.org/features (no-mirrors architecture quote — search-snippet provenance, page not fetched); github.com/GrapheneOS/AppStore + LICENSE + orgs/GrapheneOS/repositories (MIT license).

**Fetches (2 topics, no retries needed except the one spent):** repo README (confirmed corpus-complete); apps.grapheneos.org root (returns the "GrapheneOS servers" article; second attempt with larger budget returned the same DNS-fleet head — deeper server-hardware sections not captured).

**UNVERIFIED list (residuals):**
- Exact filenames/layout of the signed update metadata; client-side parse contract (client repo not fetched).
- Any alternate-repo/server configuration in the App Store client (none documented).
- TLS pinning / Certificate-Transmission enforcement for apps.grapheneos.org in the client.
- App Store client's honoring of Android system proxy settings (governs in-transit caching).
- Where the client embeds the signify public key (`apps.0.pub`).
- Per-app licenses of GrapheneOS standalone apps in the repository.
- Google's position on GrapheneOS's mirror of Play apps / Android Auto (and any third-party re-redistribution).
- Deep sections of the "GrapheneOS servers" article (extraction returned only the DNS-fleet head both times).

**Check-states quoted back:** FAQ L383 (store = "generic signed update metadata and signed package updates… a separate name for the same servers as releases.grapheneos.org") — verbatim in §1. Features no-mirrors quote — verbatim in §3. README Android-15 v4 note — verbatim in §1. AppStore release-notes "mirror of the core Google Play apps and Android Auto" + "must obtain GrapheneOS app updates through our App Store" — verbatim in §1. MIT license grant — verbatim in §4.
