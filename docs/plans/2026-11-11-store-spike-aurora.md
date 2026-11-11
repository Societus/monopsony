# Store-source feasibility spike: Aurora Store (unofficial Google Play client)

**Task:** t_2ffac304 — Monopsony research lane, mobile roadmap kickoff gate (five store-source spikes).
**Status:** complete (doc-grounded; no code, no git operations, no builds).
**Output:** this single file — `docs/plans/2026-11-11-store-spike-aurora.md`. Progress log: `context/kanban-progress-store-spike-aurora.md`.
**Sibling spikes:** F-Droid + Obtainium banked in `docs/plans/2026-11-10-mobile-store-source-spikes.md` (format reference, not edited); Accrescent in `docs/plans/2026-11-11-store-spike-accrescent.md`; GrapheneOS pending (t_30c8d713).

## Method

Primary source: the dispatcher-fetched corpus — `tmp/spike-corpus-2026-11-10/aurora-0.md` (GitHub raw README) and `aurora-2.md` (GitLab mirror). The two files are byte-identical (7,726 bytes each, `wc -c`), so README citations apply to both. `aurora-3.html` (auroraoss.com landing page) was inspected and carries no load-bearing content. The corpus contains **no protocol/architecture internals** — such claims are marked UNVERIFIED below, not inferred.

Capped network budget (card: ≤3 searches total, ≤1 fetch + 1 retry per topic), all spent 2026-11-10:
- `web_search` ×3: (1) Play ToS unauthorized-client clause; (2) Aurora anonymous-account block/ban history; (3) Aurora anonymous-login/token-dispenser docs.
- `web_extract` ×4 across 3 topics: Play ToS page (initial + 1 retry — both extractions truncated before the restrictions section), AuroraOSS Wiki "Anonymous Logins", deniz.in report on the Sept 2026 Aurora install failures.

Repo context cited read-only: `crates/monopsony-common/src/shadow_types.rs` — `ShadowSource { GithubRelease, GitRelease, DirectUrl }`, `InstallMethod { Extract, Build, Script }`, `AssetSelector`/`AssetRule`, `VersionDetect { Command|File|Header }`, `SignedRecipe` (Ed25519 over canonical TOML).

---

## Spike — Aurora Store (https://gitlab.com/AuroraOSS/AuroraStore)

### Mechanism — how Aurora talks to Google Play

- **Positioning (corpus):** FOSS GPLv3 client that searches and downloads apps **from Google Play**: "Aurora Store does not own, license or distribute any apps. All apps, app descriptions, screenshots and other content in Aurora Store are directly accessed, downloaded and/or displayed from Google Play." It "works exactly like a door or a browser" onto the user's Play account, and carries "no approval, sponsorship or authorization from Google, Google Play."
- **Two login modes (corpus):** a personal Google account (sign-in "via microG" — `android.permission.USE_CREDENTIALS`) or an **anonymous account**. Anonymous mode loses Library, Purchase History, Editor's Choice, Beta Programs, and review posting.
- **Anonymous-mode internals (AuroraOSS Wiki, fetched):** anonymous login draws an AAS `AuthToken` from community-run **token dispensers** — web clients holding "~250 real google accounts (inclusive of all servers)" that log in server-side and hand the token to any requester in a defined format. Dispensers use **dummy device profiles, default Xiaomi Mi5**. "Other complementary tokens required to make the API work" are generated client-side by Aurora. Dispensers sit behind Cloudflare; users may also donate email + AAS-token pairs to a shared account pool.
- **API provenance (corpus):** "The underlying API used is reversed engineered from the Google Play Store, changes on side may break it." The Sept 2026 report (secondary source) states Aurora "speaks to the same Play backend endpoints as the official store app."
- **`UNVERIFIED` — protocol internals:** concrete endpoints, auth handshake, and download-URL acquisition are absent from the corpus; no primary source landed within budget. Do not model them.
- **Download flow (corpus):** APKs download directly from Google's servers; manual **older-version downloads** work given the version code, "provided the APKs are available with Google"; OBB expansion files are handled via storage permissions; **SAI** (split-APK installer) is a listed project reference. Hard gaps: "Can not download or update paid apps" and "Can not update apps/games with Play Asset Delivery." Exact split/bundle delivery handling: `UNVERIFIED` (protocol detail).
- **Search/metadata (corpus):** app descriptions, screenshots, updates, and reviews all come from Play. Search endpoint specifics: `UNVERIFIED`.
- **Reliability (corpus + Wiki):** "Token dispenser server is not super reliable, downtimes are expected" (corpus); "Session-expired" prompts are routine — shared dummy accounts across geographies, an observed ~12-concurrent-session limit ("through observations, not officially sourced"), and Google rate-limit token invalidation (Wiki).

### Signing/integrity

- **Downloaded apps:** Aurora does not sign, re-package, or attest Play content — it distributes nothing (corpus positioning). The trust anchor for app integrity is the **Android platform**: package-signature verification at install and same-signing-key enforcement on updates. This is standard Android behavior; Aurora-specific verification steps are `UNVERIFIED` (no primary source in corpus or budget).
- **Store metadata:** unlike F-Droid's signed index, Play metadata arrives over the authenticated session with **no operator-verifiable signature**. Any Aurora-side pinning of metadata: `UNVERIFIED`.
- **Aurora's own APK (corpus):** README publishes release-cert fingerprints (SHA1 `94:42:75:D7:59:8B:C0:3E:48:85:06:06:42:25:A7:19:90:A2:22:02`; SHA256 `4C:62:61:57:AD:02:BD:A3:40:1A:72:63:55:5F:68:A7:96:63:FC:3E:13:A4:D4:36:9A:12:57:09:41:AA:28:0F`) and channels: auroraoss.com, GitLab releases, IzzyOnDroid (**reproducible**), F-Droid (**signed by F-Droid**), Huawei App Gallery; GitLab CI debug builds use **AOSP test keys**. Multi-key reality: an F-Droid-signed install and an Aurora-signed install have different trust chains — cross-channel updates are a client-side breakage class.

### Proxy/mirror compatibility — actively anti-mirror

- **No mirrorable contract exists.** F-Droid publishes a signed index plus a static `/repo/` tree; GrapheneOS serves signed update metadata from CDN-backed static hosting. Play offers **neither**: every search, metadata view, and download URL is minted inside an authenticated, device-profile-bound session of a reversed-engineered API. There is no public per-app URL space for a Monopsony mirror to re-serve.
- **What a proxy can do:** transparently cache APK/OBB **bytes in transit** for clients already running Aurora (Monopsony's normal HTTP-proxy role, no integration). Hit-rate applies only to apps previously fetched through the same proxy. Cacheability specifics (URL stability, expiry, signed-URL windows) are `UNVERIFIED` — no primary source landed; not filled by inference.
- **What is actively defended (empirical):** Google invalidates auth tokens on API overuse and observed session limits (Wiki). In **Aug–Sep 2026**, anonymous-mode installs failed across stable (4.8.4) and nightly with malformed "Server busy, please try again later" rejections that survived VPN, cache clears, and account refreshes (GitLab issue 1566, reported via deniz.in 2026-09-01, reached HN front page) — interpreted by the reporter and HN discussion as a server-side block of the unofficial client; no Google confirmation exists. A separate HN thread, "Aurora Store Accounts Blocked by Google" (item 36074646; search metadata only), documents account-level block events; an Android SE answer attributes the errors to Google anti-fraud updates flagging Aurora's accounts ("probably Aurora's accs get flagged quickly now").
- **Fleet-aggregation hazard:** a Monopsony deployment concentrates every client's Play traffic on one egress IP and, in anonymous mode, on a small pool of dummy-profile shared accounts — exactly the shared-session/overuse signals the Wiki names as invalidation triggers. Integration would degrade, not improve, fleet reliability.

### ToS/legal — load-bearing

- **No authorization (corpus):** Aurora's own README disclaims any Google approval, sponsorship, authorization, affiliation, cooperation, or connection. Using it is, definitionally, accessing Play through an unofficial client.
- **Play ToS (fetched: `https://play.google.com/intl/en-US_us/about/play-terms/index.html`, version July 29 2026; update pending Nov 17 2026):** §2 grants use "to browse, locate, view, stream, or download Content for your mobile, computer, tv, watch, or other supported device" and requires "a valid Google account." A dispenser serving strangers from ~250 pooled accounts under dummy device profiles is outside any reading of that grant. The ToS contains an "**Unauthorized Access to Accounts**" provision — heading observed in search snippets only; **full clause text `UNVERIFIED`** (both page extractions truncated before the restrictions section).
- **Enforcement history:** suspected server-side cutoff of anonymous installs (Aug–Sep 2026, issue 1566); HN account-block thread; Wiki-documented token invalidation. For **personal-account** mode (own Google account through Aurora), no ban-rate data located: `UNVERIFIED` — but the user's own account is the collateral.
- **Redistribution:** re-serving Play APKs from a mirror has no license basis — Aurora itself disclaims distribution rights (corpus), and app content remains Google/developer-copyrighted. No Play policy on third-party redistribution was located within budget: `UNVERIFIED`.

### Mapping → Monopsony machinery (`shadow_types.rs`, read-only)

| Aurora concept | Monopsony equivalent | Fit |
|---|---|---|
| Play session (account + device profile + tokens) | — | No `ShadowSource` variant can express a credential-gated, session-scoped store fetch |
| Play delivery URLs | `ShadowSource::DirectUrl` + `asset_url_template` | Broken: URLs are session-minted (`UNVERIFIED` detail), not publicly constructible |
| Version / version-code discovery | `VersionDetect { Command|File|Header }` | No public index to probe; discovery lives inside the authenticated session |
| APK + split-APK + OBB install | `InstallMethod { Extract, Build, Script }` | No Android package-install verb in the schema |
| Aurora's own updates (its APK on GitLab/F-Droid) | `ShadowSource::GitRelease` (GitLab) | Fits — but that is Aurora-as-package, not Play-as-source |
| Operator-signed recipes | `SignedRecipe` / `UpdateAnnotation` | Workable only over sources the server resolves itself; a Play recipe would embed credentials the schema does not carry |

### VERDICT: NOT-FEASIBLE

Confidence: MEDIUM-HIGH.

Aurora Store is a client for one user's Play session, not a distributable source. There is no index to mirror, no stable URL space to cache against, and the underlying protocol is reversed-engineered, credential-gated, and empirically defended (Aug–Sep 2026 suspected server-side cutoff of anonymous installs; documented token invalidation on shared-session overuse). Anonymous mode depends on a third-party token dispenser running ~250 pooled real Google accounts under dummy device profiles — a SPOF operating outside Play's account grant, and routing an entire fleet through it concentrates the exact signals that trigger Google's invalidations, at account risk no operator should accept. Coverage holes (no paid apps, no Play Asset Delivery) would make fleet deployment misleading even if the transport worked. The only legitimate Monopsony touchpoint is incidental in-transit byte caching for clients already running Aurora — which requires no integration and confers no store-source capability. Neither FEASIBLE nor PARTIAL is defensible on this evidence. Confidence is not HIGH because the full ToS restrictions clause was not extracted (heading only), the 2026 block is reported evidence rather than a Google admission, and protocol internals remain UNVERIFIED.

---

## Provenance

**Corpus (primary, fetched by dispatcher 2026-11-10):**
1. `tmp/spike-corpus-2026-11-10/aurora-0.md` <- https://raw.githubusercontent.com/AuroraOSS/AuroraStore/master/README.md
2. `tmp/spike-corpus-2026-11-10/aurora-2.md` <- https://gitlab.com/AuroraOSS/AuroraStore/-/raw/master/README.md (byte-identical to #1)
3. `tmp/spike-corpus-2026-11-10/aurora-3.html` — auroraoss.com landing page; inspected, not load-bearing.

**Capped searches (3) and fetches (4 calls / 3 topics), 2026-11-10:**
1. https://play.google.com/intl/en-US_us/about/play-terms/index.html — Play ToS, July 29 2026 version, Nov 17 2026 update pending; §2 use grant; "Unauthorized Access to Accounts" heading (search snippet only — extraction truncated twice, retry consumed).
2. https://auroraoss.gitbook.io/wiki/references/anonymous-logins — dispenser pool ~250 accounts, Xiaomi Mi5 dummy profiles, ~12-session observed limit, rate-limit invalidation, Cloudflare fronting, account-pool donations.
3. https://deniz.in/aurora-store-installs-fail-with-server-busy-errors-as-google-play-access-appears (2026-09-01) — GitLab issue 1566; anonymous installs failing on 4.8.4 + nightly (CalyxOS 7.2.4.20 / Android 16, Fairphone 5); suspected server-side block; HN front page Sept 1 2026.
4. https://news.ycombinator.com/item?id=36074646 — "Aurora Store Accounts Blocked by Google" (search metadata only; date not fetched).
5. https://android.stackexchange.com/questions/265713/ — "Server busy" Q&A (search snippet only).

**UNVERIFIED items (consolidated):** Play endpoints/auth handshake/download-URL acquisition; split/bundle delivery handling; search endpoint specifics; Aurora-side APK/metadata verification steps; Play delivery-URL cacheability/expiry; full Play ToS restrictions-clause text (extraction truncated); personal-account ban rates; Play third-party redistribution policy.

**Deliberately not claimed:** any Google-official statement about Aurora or anonymous-mode blocks; cache-hit expectations for Play bytes; any inference that a mirror could lawfully serve Play content from pooled credentials.
