# Mobile roadmap kickoff gates: decision brief (VpnService policy + PWA-vs-native)

**Task:** t_fca487ad — Monopsony night-2 lane 2/3 (docs-only).
**Status:** decision BRIEF. It makes no decision; it arms the operator to make the two
remaining mobile-kickoff calls fast. Nothing here edits the roadmap, the spike docs,
the gate summary, or any code.
**Inputs read (read-only, unmodified):**
`docs/plans/2026-11-11-store-spike-gate-summary.md`,
`docs/plans/2026-11-10-mobile-store-source-spikes.md`,
`docs/plans/2026-11-11-store-spike-accrescent.md`,
`docs/plans/2026-11-11-store-spike-aurora.md`,
`docs/plans/2026-11-11-store-spike-grapheneos.md`,
`docs/sprint-roadmap.md` § "Mobile — Android client + management apps, iOS management app"
(gitignored; read at lines 354–394) and § Sprint 34 (lines ~491–505, header line 5).
Tree surfaces cited read-only: `dashboard/src/routes/updates/+page.svelte`,
`dashboard/src/routes/` listing, `dashboard/static/`, `crates/monopsony-admin/src/updates.rs`.
**Method:** every store-side claim cites a local plan doc by path; every Android-platform
or store-policy claim cites a URL fetched 2026-11-12 (recorded in Sources). Anything
recalled but not fetched from a primary source is marked `UNVERIFIED`. No battery or
latency numbers are asserted anywhere — those are bench questions, and this brief
explicitly refuses to invent them.

---

## 0. Context — what the satisfied spike gate already proved

The five store-source spikes are banked (gate summary, `docs/plans/2026-11-11-store-spike-gate-summary.md`):

| Store | Verdict | Source doc |
|---|---|---|
| F-Droid | FEASIBLE / HIGH | `docs/plans/2026-11-10-mobile-store-source-spikes.md` |
| Obtainium | PARTIAL / MEDIUM-HIGH | same |
| Accrescent | PARTIAL / MEDIUM | `docs/plans/2026-11-11-store-spike-accrescent.md` |
| Aurora Store | NOT-FEASIBLE / MEDIUM-HIGH | `docs/plans/2026-11-11-store-spike-aurora.md` |
| GrapheneOS App Store | NOT-FEASIBLE / MEDIUM-HIGH | `docs/plans/2026-11-11-store-spike-grapheneos.md` |

The pattern that shapes both gates (gate summary line 17): **only 1 of 5 stores has a
sanctioned, mirrorable store contract (F-Droid)**; 2 are per-app source models with no
store-level index (Obtainium, Accrescent); 2 are credential- or OS-gated with no
third-party path at all (Aurora, GrapheneOS). For any store other than F-Droid,
Monopsony's realistic role is opportunistic in-transit byte caching — and no store's
trust anchor can be substituted by the proxy.

Two concrete coverage facts carry most of the weight for the client-mode gate:

- **F-Droid (the one real mirror win) does not need any client-side transport trick.**
  It is a sanctioned-mirror static `/repo/` tree with a signed index; a Monopsony mirror
  re-serves bytes, signature verification stays client-side
  (`docs/plans/2026-11-10-mobile-store-source-spikes.md`, Spike 1). Third-party F-Droid
  repos are a first-class ecosystem concept (repo URL + app ID configured per repo —
  same doc, Spike 2, F-Droid source section). The client is *pointed at* the mirror;
  nothing must be intercepted.
- **Obtainium works through ordinary system proxy settings.** Its Android networking
  default is embedded Cronet, which "honors … system proxy settings"
  (`docs/plans/2026-11-10-mobile-store-source-spikes.md`, Spike 2, Signing/integrity +
  Proxy/mirror compatibility). Its app sources are GitHub-release-shaped and map onto
  `ShadowSource::GithubRelease` + `asset_selector` (mapping table, same doc).

Consequence, stated plainly so the gates are judged against the right prize:
**the two store integrations that survived the spike gate both work over the
privilege-free baseline.** VpnService buys coverage of *everything else* — cellular
traffic, apps that ignore proxy settings, and in-transit caching for store clients whose
proxy behavior is UNVERIFIED (Accrescent, Aurora, GrapheneOS client proxy-honoring are
all `UNVERIFIED` in their respective spike docs). The management-surface gate is
unaffected by any of this — it reads the same admin API regardless.

---

## 1. Gate 1 — VpnService policy review

### The roadmap framing

`docs/sprint-roadmap.md` § Mobile (lines 360–364): the Android **proxy client lane** is
"per-WiFi HTTP proxy config as the privilege-free baseline; a VpnService-based local
proxy for system-wide coverage (foreground service) is the stretch — kickoff decision on
battery/policy tradeoffs." Line 394 names this review as one of the three kickoff gates.

### What the platform actually demands (fetched 2026-11-12, URLs in Sources)

A VpnService-based mode is not "just another option in the app." It changes the app's
platform contract:

- **Consent + visibility.** Before a VPN app can become active for the first time, the
  system shows a connection-request dialog the user must accept; the VPN settings screen
  lists the app; the Quick Settings tray and a status-bar key icon show an active
  connection; while active the app must keep a **non-dismissible notification**
  (developer.android.com/develop/connectivity/vpn).
- **Always-on / kill switch semantics.** Android supports always-on VPN with a
  "blocked connections" mode (block connections without VPN) and per-app VPN
  (allowed/disallowed app lists) — all surfaced in system settings, all of which a
  Monopsony mode must coexist with and document (same guide).
- **Foreground-service typing (Android 14+).** Targeting Android 14 (API 34) requires an
  appropriate foreground-service type for each foreground service; `specialUse` exists
  for use cases the standard types don't cover
  (developer.android.com/about/versions/14/changes/fgs-types-required). Which type a
  local-proxy VPN service should declare is an implementation-time determination —
  **UNVERIFIED**, decide against the target API level when the slice is scheduled.
- **Single VPN slot.** The system UI presents one active VPN app; starting another VPN
  deactivates the previous one is standard platform behavior (exact revocation semantics
  **UNVERIFIED** here — re-verify against the VpnService API reference before
  implementation). This is the sharpest operational fact for THIS operator: the
  environment is Tailscale-centric, and Tailscale's Android client is itself a
  VpnService app (common knowledge, **UNVERIFIED** from primary source here).
  **VpnService mode and Tailscale compete for the same system slot; per-WiFi proxy and
  Tailscale compose** (app-layer proxy setting under an IP-layer VPN).
- **Play policy, for completeness.** Play's VpnService policy permits the API only for
  apps where "VPN [is] their core functionality … tunnel to a remote server," plus a
  fixed exceptions list; it requires a declaration form, listing disclosure, and
  device→tunnel-endpoint encryption; redirecting/manipulating other apps' traffic is
  prohibited outright (support.google.com/googleplay/android-developer/answer/12564964).
  A local LAN-caching proxy is not in the permitted shape, so **a Play listing is
  effectively closed** to the VpnService mode. Monopsony's Android distribution channels
  are F-Droid-repo and Obtainium-over-GitHub anyway (both proven feasible by the spikes),
  and neither is bound by Play policy — so this cost only materializes if Play
  distribution ever becomes a goal.

### What VpnService buys that the baseline doesn't

1. **Cellular-network coverage.** Per-WiFi proxy config by definition covers WiFi
   networks only; a VpnService TUN covers mobile data too.
2. **Apps that ignore proxy settings.** Anything doing raw sockets or non-proxy-aware
   HTTP gets captured only at the TUN layer. (Proxy-honoring apps — Cronet-based ones
   like Obtainium, browsers — are already covered per-network.)
3. **Zero per-network setup friction.** One consent dialog vs configuring a proxy per
   SSID per device.
4. **In-transit caching for proxy-UNVERIFIED store clients** — Accrescent, Aurora,
   GrapheneOS clients' proxy behavior is UNVERIFIED per their spike docs, so only a
   TUN-layer capture is *guaranteed* to see that traffic. But note what the traffic is
   worth: Aurora's spike concludes fleet aggregation concentrates the exact signals
   Google invalidates on — a mode that guarantees capturing Aurora traffic for a whole
   fleet is a mode that **worsens** the Aurora risk (`docs/plans/2026-11-11-store-spike-aurora.md`,
   Fleet-aggregation hazard). GrapheneOS traffic is HTTPS/CONNECT-opaque without TLS
   interception, which its spike rejects as a design outright
   (`docs/plans/2026-11-11-store-spike-grapheneos.md` §3). So item 4's real value is
   mostly Accrescent bytes, the lowest-confidence PARTIAL, pending its client's proxy
   behavior.

### What the baseline (proxy-only) doesn't cover, honestly

- Cellular traffic (see above).
- Per-SSID configuration friction on every client device.
- Any non-proxy-honoring app.

### The three options

**Option A — Proxy-only, permanently.** Ship the per-WiFi baseline; never build the
VpnService mode.
*Commits the project to:* no Android VPN permission surface ever; accepts cellular and
non-proxy-app gaps; simplest maintenance; no Play-policy exposure; coexists cleanly with
Tailscale. The store lane loses nothing that matters (F-Droid is repo-pointed;
Obtainium is proxy-honoring).

**Option B — VpnService mode as a first-class lane.** Build the local-proxy foreground
service for system-wide coverage now.
*Commits the project to:* consent/notification/always-on UX; FGS-type compliance on
Android 14+; battery/doze bench work before shipping; a hard mutual exclusion with any
real VPN (Tailscale included) on the same device; Play permanently closed for the
client; and an interception surface that mostly serves UNVERIFIED-value traffic (only
Accrescent benefits, and only if its client's behavior confirms the capture is even
useful).

**Option C — Baseline first, VpnService as a bench-gated experimental flag.** Ship A;
carry VpnService behind an off-by-default experimental flag; build the slice only after
a bench pass proves (i) acceptable battery on a real phone over a week, (ii) the
Tailscale-exclusion UX is acceptable, (iii) at least Accrescent's client behavior makes
the captured traffic cache-worthy.
*Commits the project to:* A's costs now, plus a reservation of scope; delays B's costs
until evidence exists. The roadmap's own framing ("the stretch — kickoff decision on
battery/policy tradeoffs") reads naturally as this shape.

**Recommendation (not a decision): Option C.** Reasoning in one line per point:
the surviving store integrations don't need it; the operator's Tailscale-centric
environment makes the mode mutually exclusive with the stack the fleet already runs;
and its unique wins (cellular, proxy-blind apps) are unquantified until benched. Flip to
B cheaply later — nothing in A forecloses B.

**Bench gate before any B work** (this brief asserts no numbers): 1-week real-phone
battery delta proxy-on vs off under normal fleet use; doze behavior; capture-rate of
Accrescent/Aurora traffic through the TUN; coexistence UX test with Tailscale active.

---

## 2. Gate 2 — PWA vs native management app

### What exists today (tree-grounded)

- The dashboard is a SvelteKit SPA embedded into the server (rust-embed), served at
  :3130 with tiered auth (roadmap § Mobile line 392: "dashboard API + tiered auth
  (landed)"). Routes on disk: `cache`, `clients`, `enrollment-keys`, `login`, `mirrors`,
  `monetization`, `pairing`, `security`, `settings`, `shadow`, `tracker`, `updates`
  (`dashboard/src/routes/`).
- **Updates panel + rollout creator (Sprint 34 P4b)** are implemented on tree, gated by
  live-bench evidence per the roadmap header (2026-11-11: "dashboard fleet skew verified
  (rc2 ×2 vs legacy ×14)"; commit train awaiting operator signature): rollout state and
  API bindings live in `dashboard/src/routes/updates/+page.svelte` (`Rollout`,
  `rolloutVersion`, `rolloutBusy`, `rolloutMsg`, `RolloutsResponse`, `RolloutAssignment`),
  backed by `crates/monopsony-admin/src/updates.rs`.
- App-based approval of new clients rides the **landed** pairing-v2 approve API
  (roadmap § Mobile line 380).
- **No PWA plumbing exists yet:** `dashboard/static/` contains only `favicon.svg` — no
  web manifest, no service worker. That is the entire delta of the PWA option's
  "installability" claim.

### The options

**Option A — Mobile-first PWA** (responsive polish + web manifest + optional Web Push
service worker on the existing dashboard).
*Reuses:* everything — the whole admin API surface, tiered auth/session model, all
twelve route panels including updates + rollout creator, pairing approvals,
rust-embed serving. Zero server-code reuse lost; the added work is frontend polish,
a manifest/SW slice, and — only if push is wanted — a VAPID + subscription-storage
slice on the admin side.
*Mobile reality:* Android Chrome supports Web Push for installed web apps as standard
platform capability; iOS/iPadOS home-screen web apps gain Web Push from 16.4+
(webkit.org/blog/13878). iOS is management-only by explicit operator decision anyway
(roadmap § Mobile, "iOS (management only)"), so the PWA simultaneously **is** the iOS
management story — one surface, two platforms.
*Distribution:* none needed. The dashboard is self-hosted; the operator reaches it over
the existing Tailscale/tunnel path. Notably, the management app is the one mobile
surface that *cannot* ride the store channels we just spiked (self-hosted origin, not a
public release) — and that's fine, it's operator-only.
*Risks:* push latency and reliability behind a tunnel depend on the reverse path;
browser-only ergonomics (no biometric-locked app icon, no widget).

**Option B — Native Android management app** (Kotlin/Compose admin client).
*Reuses:* only the API contract — login/session (the `/api/v1/auth/login` cookie flow
the pair CLI already uses), pairing approve endpoints, updates/rollout endpoints. Every
UI dollar is new; feature parity with the dashboard is a permanent tax (twelve panels
and counting).
*Adds over A:* native notification tray (push via FCM — a Google dependency the
project's stores-spike posture doesn't love — or self-hosted UnifiedPush/ntfy + periodic
poll), biometric unlock, launcher/widget ergonomics.
*Distribution:* dogfoods the store lane (own F-Droid repo; Obtainium over GitHub
releases — both proven feasible).
*iOS story:* none — a second native app (Swift) or a PWA anyway. Roadmap says iOS
management-only; B roughly doubles for two platforms.

**Option C — Wrapper (Capacitor/TWA shell around the SPA).**
*Reuses:* 100% of the SPA unchanged; adds a store-distributable shell + native push
container (Capacitor push plugin / TWA + Web Push).
*Cost:* a shell layer to maintain across Android versions; still needs a notification
path decision; gets store distribution without rewriting UI.
*Sensible only if* store distribution of the manager itself becomes a goal — which is a
want, not a need, while the PWA is reachable over the tailnet.

### Comparison

| | A: PWA | B: native Android | C: wrapper |
|---|---|---|---|
| Reuses | all panels, auth, API | API only | all panels (unchanged) |
| New work | manifest+SW polish; push = small admin slice | full app + auth/session + push + biometrics | shell + push plugin |
| Notifications | Web Push (16.4+ on iOS) | native tray (FCM or self-hosted push) | via plugin |
| iOS coverage | same surface, free | none (second app) | wrapper story weak on iOS |
| Distribution | none needed (tailnet) | F-Droid repo / Obtainium (dogfood) | store channels |
| Parity tax | zero (it IS the dashboard) | permanent, every panel twice | zero |
| Failure mode | tunnel reachability | feature lag vs dashboard | plugin/shell rot |

**Recommendation (not a decision): Option A, with push deferred to its own slice.**
The dashboard already owns the feature surface the roadmap's management lane names
(approvals, status/health, trust state, updates + rollout); the PWA option reuses all of
it and covers Android + iOS with one artifact. Native (B) is justified only when a
native-only capability becomes a requirement — background-sync approval latency tighter
 than a push-through-tunnel, biometric-gated admin access, or widget surfaces. C is the
upgrade path from A if store distribution of the manager ever matters; nothing in A
forecloses C.

**PWA v1 slice shape** (for sizing, not scheduling): responsive pass over
`+layout.svelte`/panels on small viewports; `manifest.webmanifest` + icons in
`dashboard/static/`; installable origin story documented. Push (Web Push + VAPID +
subscription table in `server_settings`-style storage) is a separate follow-up slice
only if the operator wants out-of-band approval notifications.

---

## 3. What this doc does NOT do

- No decision is made on either gate; the recommendations above are proposals for the
  operator to accept or flip.
- No roadmap edit (kickoff itself is where the roadmap gets its Sprint entry; the
  roadmap file is also gitignored and outside this lane's write scope).
- No edit to any spike doc or the gate summary; no code, no cargo, no bench.
- No battery, latency, or capture-rate numbers — all such claims are explicitly
  deferred to the bench gates stated above.

## 4. Open questions (each with an assumed default the operator can flip cheaply)

1. **If VpnService is ever built:** which FGS type on Android 14+ (`specialUse` vs
   alternatives)? Default: decide at implementation against the targeted API level.
2. **PWA push:** in scope for v1 or deferred? Default: deferred; v1 = responsive +
   installable, dashboard polls as it does today.
3. **Android client distribution channel:** own F-Droid repo vs Obtainium-over-GitHub
   vs both. Default: both — each is proven feasible by a banked spike, and the client
   is a normal APK, not a store contract.
4. **Cellular coverage as a requirement:** does any real fleet device need proxied
   mobile data? Default: no — deployment is LAN/WiFi-first; cellular is a non-goal
   until a fleet demands it.
5. **Sprint number:** none claimed here (34 is reserved per the roadmap; kickoff
   assigns). Plan docs informally reserve numbers — this doc claims none.

## 5. Sources

**Local (cited by path above):** the four spike docs + gate summary listed in the
header; `docs/sprint-roadmap.md` § Mobile (lines 354–394) and § Sprint 34
(lines ~491–505, header line 5); tree surfaces `dashboard/src/routes/`,
`dashboard/src/routes/updates/+page.svelte`, `dashboard/static/`,
`crates/monopsony-admin/src/updates.rs`.

**Web (fetched 2026-11-12):**
- Play VpnService policy — https://support.google.com/googleplay/android-developer/answer/12564964
- Android VPN app guide (consent dialog, non-dismissible notification, always-on,
  blocked connections, per-app VPN) — https://developer.android.com/develop/connectivity/vpn
- Android 14 FGS types requirement — https://developer.android.com/about/versions/14/changes/fgs-types-required
- FGS types reference — https://developer.android.com/develop/background-work/services/fgs/service-types
- VpnService API reference (to re-verify single-slot semantics at implementation) —
  https://developer.android.com/reference/android/net/VpnService
- Web Push for home-screen web apps on iOS/iPadOS 16.4+ — https://webkit.org/blog/13878/web-push-for-web-apps-on-ios-and-ipados/
