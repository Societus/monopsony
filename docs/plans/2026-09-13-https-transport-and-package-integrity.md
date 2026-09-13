# HTTPS Transport & Package Integrity — CONNECT, Windows shims, pinning, downgrade protection, sourcedb

**Date:** 2026-09-13
**Status:** Plan — decisions settled in design review; ready for phased implementation
**Layers:** monopsony-client (proxy, ledger, verifier), monopsony-proxy (CONNECT, pin store), monopsony-common (types), new `monopsony-sourcedb` component, monopsony-tracker (future federation)
**Priority:** P1 transport (blocks Windows PM testing), P1 integrity (supply-chain floor for public-server ambitions)
**Related:** `2026-09-13-large-package-proxy-hardening.md` (streaming, landed); ca_guard work (landed)

---

## 0. Problem statement

Three gaps, all live-verified or primary-doc-verified on 2026-09-13:

1. **HTTPS transport dead.** Neither proxy implements HTTP CONNECT. PMs configured
   for https URLs fail with 404 at the client proxy (.20 mirrorlist, live hit).
   Distro repos are workaround-able (rewrite to http), but Windows PMs
   (winget/choco CDNs) have no workaround — three implemented backends are
   untestable.
2. **No downgrade/freeze defense.** PM-native integrity covers content
   (hashes-in-signed-metadata) on Linux but freshness almost nowhere: pacman's DB
   carries no signed timestamp; apk/dnf don't enforce index age; Windows PM
   metadata is unsigned-to-the-client. A compromised cache can rewind or freeze
   fleets without tripping a single PM check.
3. **No independent freshness anchor.** Every freshness signal currently transits
   the cache itself — there is no out-of-band "what versions exist upstream."

## 1. Decisions log (from 2026-09-13 design review)

| Decision | Choice | Rationale |
|---|---|---|
| Transport | **C+E**: http-rewrite stays supported + per-PM shims for Windows | B (full MITM) is strictly dominated once client-side pinning exists: same integrity guarantees, no client trust-store surgery, no second CA, no pinned-cert casualties. CONNECT pass-through ships as enabling step (Phase 1). |
| MITM (B) | **Rejected**, not deferred | What B uniquely adds is transparent coverage of arbitrary https traffic — a package cache doesn't need it. Revisit only if a fleet requirement appears. |
| If MITM is ever revived | MITM CA must be a **separate intermediate**, never the mTLS pairing CA | Conflating them turns a proxy outage into fleet-wide TLS compromise. ca_guard pattern (backup/guard) would need duplication for the second key. |
| Integrity enforcement point | **Client proxy** | Universal interception: sees every index and payload before the PM does. One mechanism covers all 8 PM backends. Server-side checks complement but cannot enforce on behalf of PMs. |
| Freshness anchor | **sourcedb, demand-driven** (user scoping) | Don't map the universe — map what the fleet runs. Inventory (already collected) is the worklist. External datasets (Anitya/Repology) are optional shortcuts where coverage overlaps, never a dependency. |
| sourcedb delivery | **Deltas bypass the cache**: client daemon pulls signed deltas direct from origin (GitHub Releases + one secondary), pinned Ed25519 key | Subject and channel of the oracle must both sit outside the cache trust domain. A compromised server can rewind packages but not the map of what exists. |
| Bootstrap seed | Kept, **demoted** to cold-start convenience | Growth is local from day one; seed accelerates first hours only. Matches data/reference snapshot pattern. |
| Cross-server sharing | **Designed-for, not built** (far-off, paid/public tier) | Append-only observation log with stable identity keys → future merge is log-CRDT over tracker/peer-exchange. Format frozen now so federation needs no migration. |
| Paid-tier boundary | Integrity *protection* free; integrity *intelligence* paid | Same telemetry, two products: floors/alarms free-tier; CVE exposure, EOL flags, compliance %, audit attestations, release monitoring paid. Flags in `server_settings` JSONB. |

## 2. Trust model & the ladder

| Level | Mechanism | Defends against | Blind to |
|---|---|---|---|
| L1 | PM-native (signed index + pinned hashes) | Byte substitution (Linux) | Freshness everywhere; Windows payloads entirely |
| L2 | Client hash pinning (server-pinned origin hashes, client-verified) | Byte substitution (all PMs incl. Windows) | Stale-but-valid content |
| L3 | Client monotonic ledger | Rewinds of anything the client ever saw | First contact; freezes |
| L4 | Staleness policy (index age TTLs, apt Valid-Until native) | Naive replay | Valid-signed freeze |
| L5 | sourcedb floor + statistical freeze detection (demand-driven upstream oracle, out-of-band delivery) | Freeze fingerprints; AUR replay; per-package lag beyond grace | Unmapped packages (fall back to L6) |
| L6 | Direct origin index sampling (client's own TLS, rare/conflict-triggered) | Everything above, for sampled packages | Bandwidth cost (bounded by rarity) |
| L7 | Quorum across servers via tracker/peer-exchange (future, paid/public) | Single compromised cache | Needs multi-server deployments |

**Information-theoretic floor (stated honestly):** a compromised cache serving an
isolated, history-less client is undetectable client-side. Freshness requires an
anchor outside the cache; the ladder supplies three (L5, L6, L7).

**Key inventory (new):**

| Key | Purpose | Custody |
|---|---|---|
| mTLS CA (existing) | pairing + gRPC | ca_guard (landed) |
| **Mirror/pin signing key** (existing Ed25519 in monopsony-mirror) | signs pin attestations + sourcedb deltas | New: same backup/guard pattern as ca_guard; `/var/lib/monopsony/keys/`; **never** the mTLS CA |
| Client-pinned oracle key | verifies sourcedb deltas | Delivered in bootstrap seed + initial pairing payload; rotation = TUF-style explicit key-revocation record |

## 3. Phase 1 — CONNECT pass-through (enabling step, ~1–2 days)

### Server proxy (monopsony-proxy/proxy.rs)
- `CONNECT host:port` handler: 200-interim, raw TCP forward to host:443, bidirectional copy (`tokio::io::copy_bidirectional`).
- SSRF guard applies (existing `validate_url_host`); `allow_private_upstreams` flag respected.
- Rate-limit + concurrent-slot accounting unchanged (acquire/release per CONNECT, not per tunneled byte). Byte accounting via copy loop counters (metrics: `proxy_connect_bytes`).
- No caching, no body inspection — tunnel is opaque by design.

### Client proxy (monopsony-client/proxy.rs)
- Same: CONNECT → tunnel to server proxy :3128. Preserves the existing no-fallback 503 semantics for connect failures.
- `XferCommand` PMs unaffected (absolute-URI path unchanged).

### Config
- `[proxy] connect_enabled = true` (server), client automatic. Reject-list for connect destinations optional (`[proxy] connect_deny = [...]`).

### Tests
- `curl -px http://client:3129 https://api.github.com` end-to-end through both hops → 200.
- Server down → clean failure, no fallback.
- Unfiltered: `curl -px ... https://10.10.20.1/` → SSRF reject.
- Live fleet: winget source update over tunnel on Windows To Go bench.

## 4. Phase 2 — Windows PM shims (C+E core, ~3–5 days)

Shims solve *metadata/discovery*; payloads ride the existing http cache path
(the shim controls the URLs, so it points them at Monopsony).

### 4.1 winget — REST source (documented contract)
- Serve `Microsoft.Rest` source type: `winget source add -n monopsony -a http://<server>:3129/winget/ -t Microsoft.Rest` (+ optional `--header` auth token for private feeds).
- Endpoint inventory (implement per winget REST reference): service index, package
  manifest search, version list, installer manifest. Manifest `InstallerUrl`
  rewritten to `http://<server>:3128-fetch/...` (absolute-URI cache path);
  `InstallerSha256` passed through **and** extracted into the pin store (Phase 3).
- Source catalog built by server-side sync job: pull community repo manifests via
  the server's https client (it has one), store in DB, serve over http.
- msstore: out of scope (proprietary protocol).

### 4.2 choco — NuGet v2 feed
- Serve NuGet v2 endpoints (`/Packages()`, `FindPackagesById()`, `.nupkg` download) from a server-side sync of the community feed.
- **Honest caveat:** only *internalized* packages embed binaries; non-internalized
  choco packages embed install scripts with arbitrary download URLs — we rewrite
  the common `$url`/`$url64` cases, document the rest as pass-through (no cache).
- Long-term enterprise pattern anyway: recommend `choco internalizer`-style
  embedding at sync time (flag-gated future work).

### 4.3 scoop — bucket server (no shim protocol needed)
- Serve bucket JSON over http with `url:`/`hash:` fields rewritten to Monopsony
  paths. Client: `scoop bucket add monopsony http://<server>:3129/scoop/<bucket>`.
- Hash pass-through into pin store as above.

### Client wiring
- `monopsony-client configure winget|choco|scoop` executes the source-add/bucket-add
  commands (ProxyConfigAction pattern, `#[cfg(windows)]` for winget/choco paths).

### Tests
- winget search + install of a pinned package through the chain on Windows To Go;
  choco install internalized pkg; scoop install; each verified in cache_entries
  with X-Cache HIT on refetch.

## 5. Phase 3 — Hash pinning (integrity core, ~3–4 days)

### Server: pin at commit
- Spool-commit hook (exact site that fixed the scheme bug) extracts origin
  integrity data for the URL class:
  - pacman: `.sig` sidecar + DB `%SHA256%` (index parser, Phase 4 reuses it)
  - apt/dnf/apk: index-declared hashes (from their parsers)
  - winget/scoop/choco: InstallerSha256 / bucket hash / nuspec checksum
  - bare fetches: origin `.sha256`/`.sha512` sidecar or `x-checksum`-class headers when present; else `origin_unverified` marking
- Store `(cache_key → expected_hash, source_of_truth, observed_at)`; sign
  attestation records with the mirror Ed25519 key.

### Client: verify before handoff
- Client proxy intercepts payload responses for pinned cache keys, streams hash
  while forwarding (O(1), matches streaming architecture), compares to the signed
  pin, **then** completes the response to the PM.
- Mismatch → truncate + 503 to PM, alarm event to server, cache entry quarantined
  (existing purge/quarantine routes), log with evidence.
- `origin_unverified` entries: verify against pin re-fetch at commit time
  (server fetches twice / range-checks) or flag-only policy (config).

### Tests
- Tamper injection test (bit-flip in cached object) → client rejects, PM sees
  failure, quarantine recorded.
- Real PM flow: pacman -Syu still green (L1 + L2 redundancy), scoop install
  protected where it previously had nothing.

## 6. Phase 4 — Ledger + downgrade enforcement (~3–4 days)

### Client ledger
- SQLite (client already has state dirs): `high_water(package_key TEXT PRIMARY KEY, version TEXT, version_ord TEXT, source TEXT, seen_at INT)`.
- `version_ord` = normalized orderable form (epoch!version-release encoding; borrow
  alpm-version + Repology normalizer *notes* as reference).
- Feeding: every verified index response (Phase 4 parsers) updates high-water for
  every package seen. Payload pins cross-check (`pin hash must match index hash for
  the offered version`).

### Enforcement policy (config, default reject)
- Offered version < high-water → per config: `warn` (log + header) or `reject`
  (503 + alarm). `monopsony-client configure downgrade-policy warn|reject`.
- Admin override UX: explicit `monopsony-client override --package X --to VERSION
  --reason "..."` writes a signed exception record with TTL (ledger footgun
  guard — legit downgrades must be deliberate).

### Staleness policy
- Per-PM max index age (`[integrity] max_index_age_secs` per backend; defaults:
  apt Valid-Until native, pacman 7d, apk 7d, dnf 7d, Windows shims 24h).
- Expired index → warn/reject per policy. This kills naive replay attacks.

### Attestation headers
- Monopsony adds `X-Monopsony-Verified: pin-ok; ledger=high-water; index-age=14200`
  to responses — observable, loggable, feeds paid telemetry later.

## 7. Phase 5 — sourcedb (the oracle, ~1.5–2 weeks)

### Data model (append-only, federation-ready)
```sql
-- one row per observation, never mutated
observations(id INT PK, package_key TEXT, source TEXT,        -- "github:owner/repo" | "gitlab:..." | "aur"
             version TEXT, version_ord TEXT, scheme TEXT,      -- raw + normalized
             observed_at INT, observed_by TEXT)                -- server/client id
package_map(package_key TEXT PK, source TEXT, resolved_at INT, resolve_method TEXT,
             etag TEXT, last_poll INT)                         -- resolution cache
```
- Identity key: `(manager, repo, name, arch)` — stable across versions.
- Merge = union by `(package_key, source, version)`; monotonic per-key max is the
  floor. This shape is deliberately CRDT-ready for the future federation tier.

### Resolution pipeline (demand-driven — user's core scoping)
1. **Worklist:** union of `client_packages` inventory (weight by client_count —
   same signal as prefetch). Never poll for packages nobody runs.
2. **Resolve once per package** (priority order):
   - **AUR first** — AUR RPC returns project URL directly; weakest external
     coverage, cheapest internal resolution.
   - Arch core/extra: PKGBUILD/`.SRCINFO` `url`/`source` from packaging git.
   - apt/dnf/apk: Homepage/Source index fields (partial — degrade gracefully).
   - GitHub-shaped URL → `owner/repo` releases feed. Non-GitHub (GitLab,
     kernel.org, distro tarballs): record as observation source where a parseable
     feed exists; else package stays unmapped → falls back to L6 sampling.
3. **Poll cheaply:** ETag-cached GitHub API, token-gated, per-server rate budget
   surfaced as a dashboard stat. Fleet scale = hundreds of repos, not 40k.
4. **External datasets as shortcut, not dependency:** seed importer for
   Anitya/Repology mappings *where they overlap* the fleet's packages; skip silently
   where they don't (niche distros, AUR long tail — the cracks this design exists
   to fill).

### Normalization v1
- `v`-prefix strip, epoch `N:` handling, `-rN` suffix (pacman), `+really`,
  tilde ordering. Version-scheme edge cases are the ongoing maintenance tax —
  keep the normalizer table data-driven and patchable via sourcedb deltas
  (scheme fixes propagate without client releases).

### Delivery (out-of-band by construction)
- **Bootstrap seed:** signed snapshot in the server package / initial pairing
  payload; client pins the oracle Ed25519 key from it. Cold-start convenience only.
- **Deltas:** generated server-side (or by a CI publisher for the seed),
  signed, published to GitHub Releases + one secondary channel. Client daemon
  fetches **directly** (never via cache), verifies pin, merges (union/monotonic).
- TUF-lite metadata shape: signed snapshot → manifest of delta files with
  versions + expiry; expired deltas = freeze alarm, not silent ignore.
- Censoring the feed bounds oracle staleness — the same bounded-staleness property
  as every anchor; surfaced via `oracle_last_update` dashboard stat.

### Enforcement
- Per-package **floor with grace**: reject `version < oracle_max - grace_window`;
  grace per repo class (core: 48h, extra: 7d, AUR: 30d — distro lag is normal).
- **Statistical freeze detection** (the strong signal, server-side): share of
  tracked set whose oracle max moved since cache's newest served version; > threshold
  while cache serves nothing newer than X → quarantine + admin alert. Fleet-level,
  no legit lag pattern produces it.

## 8. Phase 6 — future / paid tier (designed-for, not built here)

- **L6 direct origin sampling:** client occasionally fetches only indexes
  direct-from-origin (daily / 1% / on ledger-oracle conflict). Config-gated off
  for airgapped fleets. Small.
- **L7 quorum:** poll 2–3 discovered peers (tracker :3130 / peer exchange, both
  landed in Phase 2 mirror work) for max-seen versions of the installed set;
  inventory reports are mTLS-authenticated so a signed fleet high-water index is
  trustworthy input. Attacker must compromise a quorum.
- **Federation:** observation-log CRDT merge over the tracker network; public/paid
  servers share coverage maps. Format already frozen (§7).
- **Paid intelligence (same telemetry, server_settings flags):** CVE-exposure map
  (inventory × version × feed), EOL flags, fleet compliance %, audit attestations
  (ledger + pin results as queryable events), new-release/EOL monitoring from the
  sourcedb (Renovate/Dependabot-style, fleet-scoped). Security floors (L1–L5) stay free.

## 9. What each layer does NOT defend (repeated for honesty)

- Compromised **server** (not just cache): can sign lies with its own pin key.
  Detected only by L6/L7 anchors or out-of-band client observation. This is why
  deltas bypass the cache and why quorum exists.
- Freeze with a fully-compromised oracle channel AND expired-delta silence:
  bounded staleness is surfaced, not eliminated.
- Unmapped packages: L6 fallback only.
- Legit downgrades: require explicit signed exceptions (§6) — by design.

## 10. Build order & effort

| Phase | Scope | Effort | Unblocks |
|---|---|---|---|
| 1 | CONNECT both proxies | 1–2 d | Windows PM functional (uncached), tunneled tools |
| 2 | winget REST + choco NuGet + scoop bucket shims | 3–5 d | Windows PMs cached — the original blocker |
| 3 | Pin store + client verify | 3–4 d | L2 everywhere incl. Windows |
| 4 | Ledger + downgrade/staleness policy | 3–4 d | L3/L4; first ship-able integrity release |
| 5 | sourcedb pipeline + deltas + floors | 1.5–2 wk | L5; AUR coverage; freeze detection |
| 6 | L6/L7 + paid intel | designed-for | future tiers |

Phases 1+2 are the Windows unblock; 3+4 are the minimum integrity release;
5 is the differentiator. Each phase is independently shippable and testable on
the existing fleet (.20/.62/.235/.242/.159 + Windows To Go bench).

## 11. Open questions

1. winget source type: `Microsoft.Rest` (chosen — documented schema) vs
   `Microsoft.PreIndexed.Package` (file-based, simpler but opaque indexing)?
   Plan assumes Rest; flip cheaply if PreIndexed proves simpler at impl time.
2. choco non-internalized policy: rewrite common `$url` cases + pass-through
   (assumed) vs. hard-require internalization at sync time?
3. Default grace windows (§7) — 48h/7d/30d reasonable, or tighten core to 24h?
4. Pin `origin_unverified` handling: double-fetch at commit (assumed default)
   vs flag-only?
5. Client `--cert-dir`-style placement of the oracle key: inside the paired cert
   dir (simple, rides pairing) vs separate (survives re-pair)? Plan assumes
   separate file under the client state dir.
