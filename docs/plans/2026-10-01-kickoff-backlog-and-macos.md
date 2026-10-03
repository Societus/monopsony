# Kickoff: Backlog + macOS Support (Sprints 25–26)

**Date:** 2026-10-01 · **Status:** requirements complete, ready for review → implementation planning
**Preceding:** v1.0.0-rc1 / Sprint 23 complete (Phase F fleet gate passed 2026-10-01).

## Decisions recorded (user, 2026-10-01)

1. **macOS + backlog now; Sprint 24 (public-server hardening) deferred** until a public endpoint is actually being stood up. Not just deprioritized — user is explicitly not ready to test against a public endpoint.
2. **"App updates" = both** Homebrew upgrades *and* App Store forwarding.
3. **OS/App interception: investigate both mechanisms, choose per-piece on security; rule one out only if it can't work.** Outcome of investigation (below): MITM/transparent intercept is ruled out; supported-redirect mechanisms win on every axis.

## Bench (verified facts)

- **m2mini** (M2 Pro Mac mini): macOS 26.3.1 / Darwin 25.3.0, arm64. SSH as `martin-ci` works (key-only, no sudo by design — privileged commands are user-run).
- Rust 1.98.1 stable aarch64-apple-darwin installed + verified as martin-ci (cargo build/run proven over ssh). PATH for non-interactive ssh lives in `~/.zshenv`.
- LuLu deny-by-default firewall: silently blocks sshd-spawned outbound HTTPS until approved at console — download-on-box may need scp workaround.
- **No Apple ID on the box** → no App Store sign-in, no notarization. GUI app = ad-hoc signed, built locally. App Store *update* verification is constrained (see B2 risks).
- Forgejo plan: `macos:arm64`-labeled runner on this box is the Monopsony Darwin CI lane (not yet wired).
- Tailscale doesn't start until GUI login after reboot — bench reachability requires a prior console login.

---

## Part A — Backlog → Sprint 25

| Item | Effort | Notes |
|---|---|---|
| **Personal OCI registry (push path)** | Sprint-sized headline | `POST /v2/<name>/blobs/uploads/` + chunked upload/PUT assembly + `PUT /v2/<name>/manifests/<ref>`, GET via artifact/spool storage. Untagged pushes auto-version `YYYYMMDD-HHMMSS`; refcount blobs; auto-tags never GC'd. NOT a pull-through mirror. |
| Release checksum lifecycle collapse | ~0.5 d | On full-RC declaration, invalidate all prior checksums except that version+. |
| docker.rs `fetch_upstream` body collection | ~0.5 d | Memory hygiene fix. |
| prefetch.rs → spool() migration | 1–2 d | Filler; schedule after OCI lands so spool usage settles once. |
| sqlx 0.8.6+ unblock | infra | Needs sqlx 0.9 (libsqlite3-sys conflict). Track, don't block on. |

**Exit criteria:** docker push/pull through the registry on hometest swarm; tagless push addressable + rollback-able; refcount survives manifest re-push; checksum list collapses on RC declaration.

---

## Part B — macOS support → Sprint 26

### Investigation findings (drive the whole design)

| Mechanism | Security | Cacheable? | Verdict |
|---|---|---|---|
| **Supported env-var mirror (brew)** — `HOMEBREW_API_DOMAIN`, `HOMEBREW_BOTTLE_DOMAIN`, `HOMEBREW_ARTIFACT_DOMAIN` | ✅ Best: no TLS interception, no new trust anchors; brew still verifies bottle checksums from API metadata it fetches | ✅ Full (metadata + artifacts) | **B1 mechanism. Proven by Tsinghua/USTC production mirrors.** |
| **CatalogURL redirect (Reposado model)** — point `softwareupdate` at a Monopsony-served sucatalog whose package URLs are rewritten to the mirror | ✅ Good: no TLS interception; Apple package signatures verified by installer regardless of source; downgrade risk mitigated by our Sprint 20 Phase 4 client ledger | ✅ Full (catalog + packages) | ~~B3 mechanism~~ **DEAD on macOS 26.3.1** — EV host gate (see Phase 0 findings below). Was the right design; Apple closed it. |
| **System proxy CONNECT pass-through** (Sprint 19 Phase 1 already landed) | ✅ Good: opaque tunnel, pinning unaffected | ❌ None (by design) | **B2 tier-1: "forwarding-only."** All traffic routes via Monopsony; no cache benefit. |
| **MITM / transparent intercept** (pfSense redirect, trust-anchor insertion) | ❌ Worst: breaks Apple cert pinning, requires injecting a CA into the system trust store | ✅ | **RULED OUT** — fails the security bar (user rule: only rule out if it wouldn't work; this "works" only by defeating the platform's trust model). |
| Apple Content Caching (`AssetCacheManagerUtil`) | Apple-sanctioned | ✅ (Apple content only) | **Investigated + declined (2026-10-01, bench-verified)** — three fatal walls: (1) peer/parent hierarchy is Macs-only over a proprietary protocol — a Linux Monopsony server cannot join as parent/peer; (2) discovery runs through Apple's cloud lookup service (caches register with Apple; clients resolve via IP-mapping + `_aaplcache._tcp` TXT) — not interceptable; (3) Apple's deployment guide explicitly documents against proxy-in-front-of-cache topology ("Don't use a proxy to accept client requests and pass them to content caches"). **Coexistence pattern adopted instead:** an always-on Mac (e.g. m2mini) runs Content Caching scoped to shared content only (`AllowPersonalCaching = false` → no iCloud blobs) for OS-update/App-Store LAN caching, alongside Monopsony for brew + all other PMs. Documented as ops guidance, not code. |

**Architecture note:** Monopsony's client proxy is plain HTTP locally and fetches HTTPS upstreams — exactly the Sprint 23 npm/pip/cargo shim shape. Brew's mirror vars accept any scheme, so `HOMEBREW_BOTTLE_DOMAIN=http://127.0.0.1:<port>/…` works without any TLS on the local hop.

### Phase 0 — CatalogURL spike (m2mini) — EXECUTED 2026-10-01, VERDICT: NO-GO on macOS 26.3.1

**Method:** system-level `CatalogURL` → agent-controlled HTTP server (Apple's own live catalog served byte-identical, plus 404 stub + localhost-tunnel variants); daemon behavior read from world-readable `/var/log/install.log`.

**Findings (all primary-source, from the box):**
1. `softwareupdate --set-catalog` CLI flag is **gone** on 26.3.1 — no catalog options in `--help` at all.
2. **User-level** `CatalogURL` (no sudo) is ignored — daemon scans stock swscan URL, zero requests to our server.
3. **System-level** `CatalogURL` **is honored**: daemon watches the key live (`Catalog URL changed (…) — Resetting state`), fetches our URL over plain http with `AE=gzip, deflate`.
4. **The EV gate kills it**: with Apple's genuine catalog bytes served from our host, `softwareupdated` logs `Can't load catalog from %s: Host cert is not EV Validated!` → `SUErrorDomain Code=103`. Fires on http AND on loopback (`SUErrorUpdateServerName=127.0.0.1` via ssh -R tunnel to our server). It is a post-fetch **host** check, not a content or transport check — any non-Apple host fails.
5. `BypassEVChecking` defaults key is **confirmed dead code**: the string appears exactly once in the dyld shared cache — inside the log-format string `"BypassEVChecking" is set; forcing EV Enforcement OFF` — with no bare defaults-key constant (contrast `CatalogURL`, which appears both bare and in context). Setting it (`=1`, confirmed in plist) does nothing. **Fresh-daemon test (post-reboot):** SIP blocked `killall`/`launchctl kickstart`, so the box was rebooted with both keys set; the newly-spawned daemon (pid 595, launched after the keys existed) fetched our catalog and still EV-rejected it — the bypass log line never fired. The startup-read hypothesis is empirically eliminated.
6. Cert pinning on swscan.apple.com confirmed in-log (`MiniSoftwareUpdate: using pinning policy … MacSoftwareUpdate`) — validates ruling out MITM independently.

**Verdict:** the deprecated mechanism is **dead on arrival on macOS 26.3.1** — deprecated CLI removed, user key ignored, system key honored but EV-gated with no bypass. Apple has fully closed the self-hosted-catalog path on current macOS. **B3's cacheable mechanism does not exist on current macOS; B3 = forwarding-only (CONNECT pass-through, already landed).** Fallback exit criteria: proxy CONNECT forwarding verified on macOS client.

**Residual unknown, low value:** whether a real EV-class cert on a public domain would satisfy the gate (untestable without buying one; would also break the LAN/private deployment model anyway). Recorded as N/A for Monopsony's use cases.

**Bench state restored:** CatalogURL + BypassEVChecking deleted (user-run), spike server + ssh tunnel torn down. Evidence retained in `~/.hermes/cache/scratch/su-spike/` (hits.log.complete, serve_catalog.py, catalog files).

### Phase 1 — Homebrew backend (B1) — SPIKE PASSED 2026-10-01 (brew 7.0.7 on macOS 26.3.1)

**Spike results (all primary-source, from m2mini + wire log):**
1. **Env vars fully honored** — `HOMEBREW_API_DOMAIN` + `HOMEBREW_BOTTLE_DOMAIN` redirect every request to the mirror first; on mirror 404 brew **silently falls back to origin** (mirror-list semantics — ideal for a cache: fill misses, serve hits).
2. **Brew 7.x protocol mapped (the big correction):** bottles are fetched via **OCI-registry paths** — `GET {BOTTLE_DOMAIN}/{name}/manifests/{version}-{rebuild}` (an `application/vnd.oci.image.index.v1+json` with per-platform inner manifests) then `GET {BOTTLE_DOMAIN}/{name}-{version}.{platform}.bottle.{rebuild}.tar.gz`. The mirror must serve both routes; rebuild numbering (`-1`, `.1.`) is part of the artifact identity.
3. **Full-chain proof:** served real JWS API bundle (15.6 MB) + formula JSON + OCI manifest + bottle blob from a plain-HTTP mirror — brew fetched all four, verified sha256 (`ca67…` matched formula), completed with zero fallback warnings. Fresh download confirmed via new cache-key prefix.
4. **API bundle is JWS-signed** — must be served byte-verbatim (no curation possible; brew metadata deferral = artifact-gating only, feeds the deferral-gate spec's track design).
5. **Client action = env vars** (`SetEnv`/shell-profile write), fully reversible, no sudo.
6. Bench quirks: `/opt/homebrew` owned by `martin` — install/fetch as other users needs lock-dir+Cellar ownership or run-as-owner; API-domain fallback absorbed mirror 404s invisibly (test with real content, not 404 stubs).

**Implementation scope (updated by spike):**
- `HomebrewBackend` (PackageManagerBackend) in monopsony-common: `is_available()` = `which("brew")`; `list_installed()` via `brew list --formula --versions` (+ `--cask`); register in `get_all_backends()`/`get_backend()` + `PackageManagerKind::Homebrew`.
- Mirror shim on the Sprint 23 pattern (monopsony-mirror), serving: `/api/internal/packages.{platform}.jws.json` + `/api/formula|cask/{name}.json` (LangMetadata, TTL revalidation), `{name}/manifests/{ver}-{rebuild}` OCI manifest route + `{name}-{ver}.{platform}.bottle.{rebuild}.tar.gz` blobs (LangArtifact, immutable). ghcr.io upstream fetch = existing OCI/token machinery. Per-PM enablement + retention/scrubber ride Sprint 23 machinery free.
- Client config actions: env-var writes (`proxy on/off/status --pm brew`), casks via `HOMEBREW_ARTIFACT_DOMAIN` (verify coverage at impl — cask URLs are vendor CDNs; ARTIFACT_DOMAIN prefixing behavior needs the cask test from the original Phase 0 list).

**Acceptance:** fresh `brew install <formula>` → MISS + cached; refetch/second machine → HIT; `brew upgrade` serves bottles from cache; brew's sha256 verification passes untouched; zero regression on Linux PMs; macOS lane green.

### Phase 2 — OS updates via catalog mirror (B3) — **NO-GO (Phase 0 verdict, 2026-10-01)**

Phase 0 spike proved the CatalogURL/Reposado path dead on macOS 26.3.1 (EV host gate, no bypass; see Phase 0 findings above). B3 shrinks to: CONNECT pass-through (landed) + the `networksetup` client action + fleet verification. macOS OS-update timing control is MDM territory (documented boundary; possible future: Monopsony *generates* the deferral profile — orchestration, not interception).

**Acceptance:** `softwareupdate --list`/`--install` (user-run) resolves + downloads via Monopsony; refetch on second client = HIT; SHA-256 of served payload identical to Apple's; install completes with Gatekeeper acceptance; downgrade attempt via doctored catalog rejected by ledger; `--clear-catalog` restores stock behavior.

### Phase 3 — App forwarding (B2) — tiered by what Apple allows

> **Tier-2 spike RESULT (2026-10-03, m2mini, brew 7.0.7 / macOS 26.3.1): cask
> caching via `HOMEBREW_ARTIFACT_DOMAIN` is NO-GO.** Method: localhost 200-
> serving listener + `NO_FALLBACK=1` differential. With identical env, the
> bottle fetch routed through the listener (`GET /v2/homebrew/core/jq/…`,
> errored on stub bytes as expected) while the cask fetch (`hiddenbar`)
> succeeded straight from origin — ARTIFACT_DOMAIN was **never contacted for
> casks**. Source corroboration: ARTIFACT_DOMAIN is consumed only by the
> bottle path (`bottle.rb` semantics: bottle URLs get their DOMAIN replaced,
> not prefixed; cask URLs would need prefix semantics the cask download path
> does not implement). **Phase 3 ships Tier 1 only.** If cask caching ever
> matters, the supported path is a future brew-side change — do not
> re-spike on the same macOS/brew pair.

- **Tier 1 (always):** system-proxy CONNECT pass-through — App Store + all GUI-app HTTPS traffic *routes* through Monopsony (forwarding; no cache). Client action wraps `networksetup -setwebproxy/-setsecurewebproxy` (scoped per network service) + clean removal. Honest labeling in docs/UI: "forwarded, not cached."
- **Tier 2 (if Phase 0 proves it):** Homebrew cask caching for direct-download `.app` installs (the common "app install" path outside the App Store).
- **Ruled out:** MITM of App Store sessions. If Phase 0 shows App Store update checks break under plain CONNECT (they shouldn't — no pinning violation in tunnel mode), document as known limitation rather than escalate to interception.

**Acceptance:** with proxy on, `softwareupdate --list` + App Store update check function (forwarding proof); cask install cached (if Tier 2); proxy off restores pristine network config; no certificate warnings anywhere.

### Phase 3.5 — Apple-content lane via scripted Content Caching (bench-verified 2026-10-01)

**Root-scriptable, zero RE, sanctioned.** Bench evidence: `AssetCacheManagerUtil canActivate` → "can be activated" (rc=0); unprivileged `activate` → "must run as root" (rc=77) — the config surface is fully scriptable with one sudo prompt, no GUI needed.

`monopsony setup mac` (installer / client CLI) gains a step:
1. `sudo AssetCacheManagerUtil activate`
2. `sudo defaults write /Library/Preferences/com.apple.AssetCache AllowPersonalCaching -bool false` (shared-content-only scope — no iCloud blob caching, privacy-preserving)
3. Optional: `ListenRangesOnly` / cache-limit keys per site config
4. `sudo AssetCacheManagerUtil reloadSettings`

Effect: OS updates + App Store apps for *other* Apple devices on the LAN get served from the Mac's Apple-controlled cache; Monopsony covers brew + all non-Apple PMs; CONNECT-forwarding carries the rest. Apple-content caching rides Apple's own machinery (locator, signatures, updates) — nothing for us to maintain, nothing for Apple to break.

**Also investigated and ruled out (2026-10-01): protocol emulation (Ghidra/RE).** Public prior art exists — azenla/AppleCache (MIT, 2019) mapped the Cache Server API, Locator Service, and the Signature problem; its successor asset-cache-tool continues the work. The Cache Server API itself is plain HTTP (fetch/upload) and trivially clonable; the fatal piece is the locator signature — a cache must register with Apple's cloud locator to receive any clients, and that registration carries a signature an impostor can't produce. A spoofed cache = "rogue cache registration" (Apple's term; they ship an MDM restriction specifically to block it). For a shipping product this fails the same bar as MITM: we don't defeat the platform's trust model, we don't ship what Apple can break unilaterally, and 2019-era RE against a moving protocol is a maintenance treadmill. Personal-lab experiment: maybe. Monopsony feature: no.

**Phase 3.5b — Content Caching discovery/hierarchy orchestration (sanctioned; designed 2026-10-01).** For scenarios where Content Caching auto-discovery fails (VLAN segmentation, cross-site with no shared public IP, tailnet-only clients), Apple ships documented escape hatches and Monopsony can orchestrate them — configuration levers only, locator/signature untouched:
1. **DNS TXT (`_aaplcache._tcp`)** in the client search domain: `prs=` public-range cross-matching + **favored local cache IPs (`pr1=`/`pr2=`, macOS 10.15+)** — can point a Mac at any routable cache address incl. a tailnet IP (Apple-documented mechanism; requires local DNS zone control, which split-horizon deployments have).
2. **Parent/child hierarchy** (`com.apple.AssetCache Parents` defaults key, verified present on bench): child cache fetches through a parent specified by address — legal cross-site topology over any IP fabric (e.g. two offices joined by tailnet: site B child → site A parent's tailnet IP).
3. **mDNS reflection** of `_aaplcache._tcp` across VLANs (Bonjour dies at routers; Bonjour-gateway/reflector is standard enterprise middleware; Monopsony already ships `mdns-sd` machinery for server discovery).

Feature shape: auto-detect LAN caches (browse `_aaplcache._tcp`), generate the TXT record content for the local zone, write `Parents` config on managed Macs over the existing command channel, optional reflector mode. **Caveats:** caches still register with Apple (internet egress required); favored-IP TXT is Mac-client behavior — iPhone/iPad lean on the locator's public-IP mapping, so per-platform verification is part of the spike, not assumed.

### Phase 4 — Graphical client (B4) + Darwin CI lane

**SwiftUI menu-bar app (Tailscale-style):**
- `MenuBarExtra` (macOS 13+ API; bench is 26.3) — toolbar icon + dropdown menus: status (server, cache HIT ratio, connected clients), Connect/Disconnect, Configure per-PM toggles (brew/system proxy/catalog), Open Dashboard. Base macOS styling, no custom chrome.
- Thin shell over the existing Rust daemon: manages/launches `monopsony-client` via LaunchAgent (`launchctl`), reads status from the daemon's local surface, triggers existing CLI actions. **No new protocol work** — mTLS/gRPC stays in the Rust daemon.
- Build via SPM `swift build` on m2mini (CLT sufficient per Phase 0 check); ad-hoc `codesign` (no Apple ID — notarization deferred until a public release needs it).
- Repo layout: `clients/macos/` (or `crates/../macos-app` — decide at impl), built by the Darwin CI lane, not by Linux CI.

**Darwin CI lane:** forgejo-runner on m2mini with `macos:arm64` label (per existing Forgejo plan). Lane scope: `cargo test` workspace subset that compiles on Darwin (client + common + relevant crates), the macOS app build, and eventually the Phase 0 spike script as a smoke test.

**Acceptance:** app builds clean on m2mini; menu-bar icon present with working menus; daemon start/stop from the menu; status panel shows live server state over the tailnet; per-PM toggles actually flip brew env/catalog/proxy state; CI lane green on a commit touching darwin paths.

---

## Sequencing

```
Sprint 25 (parallel-tracked, lower priority per user's "macOS now"):
  OCI registry push path + checksum collapse + docker.rs fix
Sprint 26 (lead):  Phase 0 spike (m2mini)
  ├─ Phase 1 Homebrew backend      ← start immediately, no deps
  ├─ Phase 2 OS updates            ← gated on Phase 0 CatalogURL go
  ├─ Phase 3 App forwarding        ← tier 1 unconditional, tier 2 per Phase 0
  └─ Phase 4 GUI client + CI lane  ← after Phase 1 (needs something to toggle)
```

macOS leads (user decision); OCI can run as a background lane or fill gaps — both are independent.

## Risks / open items

- **CatalogURL on macOS 26.3 is the single biggest unknown** — deprecated 7 years, alive through Sequoia-era, but Apple can kill it any release. Mitigation: Phase 0 spike before any B3 code; fallback is forwarding-only (Tier 1) + documented limitation.
- **No Apple ID on bench** — App Store *update* flows may be unverifiable (sign-in required). B2 acceptance may need a user-assisted session or a different bench.
- **Sudo boundary** — martin-ci has no sudo (by design). All `softwareupdate`/`networksetup`/trust-store steps are user-run; the plan keeps them as explicit, listed commands.
- **LuLu + Tailscale-after-reboot quirks** on m2mini may slow agent-driven bench loops (scp-first downloads; console login after power cycles).
- Homebrew 7.0 (Sep 2026) changed download internals (concurrency, sandboxing) — Phase 0 includes a positive-control mirror test to catch behavioral drift.
- Roadmap hygiene: `docs/sprint-roadmap.md` still lists "macOS client testing — LAST in backlog"; update to point at this plan once approved.

## Post-investigation addendum — paid-server deferral/audit track (designed 2026-10-01)

**Requirement (user):** enterprise-style update retention for a paid public server — audit updates, hold them back so fleets don't hit day-zero releases (motivation: Microsoft's SSD-killing patch Tuesdays).

**Ruled out: Content Caching "same public IP" amplification.** Mechanically conceivable (VPN egress + locator cross-matching via `prs=`), categorically wrong as a product: customers won't join a third-party VPN; public operators can't publish into customer DNS zones; Apple scopes caches to local subnets by default (`LocalSubnetsOnly = true`, bench-verified); and Content Caching is opaque — it can dedupe Apple bytes but cannot audit, hold back, or gate anything, by design.

**The deferral/audit engine (Monopsony-native, mostly assembled from landed machinery):**
- Scan pipeline (ClamAV/YARA/entropy + CVE lookup) — **landed** (Sprints 13–18)
- Retention/version floors (`floor_majors`, eviction policy) — **landed** (Sprint 23, fleet-proven in Phase F)
- High-water ledger + downgrade warn/reject — **landed** (Sprint 20 Ph4)
- Statistical freeze detection — **designed** (Sprint 21 sourcedb)
- **NEW: serve-time deferral gate** — per-PM, per-track policy: don't serve version N until (age ≥ window) AND (audit-clean or admin-promoted). The ring model: deferral window default, canary cohort opt-in, admin promotion as escape hatch.
- **NEW: ring/canal workflow** — dashboard + admin API for promote/hold/rollback per track.

**macOS boundary (documented, not a feature):** Apple's sanctioned timing control is MDM deferral keys (`forceDelayedSoftwareUpdates` / `enforcedSoftwareUpdateDelay`, 30d default / 90d cap, macOS 11.3+ — verified). No wire-level control point exists (tonight's Phase 0 + 3.5 findings). Future option: Monopsony *generates* the MDM profile; never intercepts.

**Positioning:** this is the paid-server core feature — applies to every PM surface Monopsony controls (distro PMs, npm/pip/cargo, brew post-B1, container images). Candidate Sprint 25 companion to the OCI registry or Sprint 26.5 after macOS Phase 1. **Full spec:** `docs/plans/2026-10-01-deferral-gate-spec.md` (concepts, data model, serve-time gate semantics, admin API, failure matrix, slices).
