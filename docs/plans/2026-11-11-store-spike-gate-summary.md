# Store-source spike gate: five-store verdict summary

**Task:** t_6ebc9abf — consolidation card for the mobile roadmap kickoff gate (five store-source spikes).
**Status:** complete. Mechanical consolidation only — no new research, no network calls, no code, no git operations.
**Sources read (read-only, unmodified):** `docs/plans/2026-11-10-mobile-store-source-spikes.md` (F-Droid + Obtainium), `docs/plans/2026-11-11-store-spike-accrescent.md`, `docs/plans/2026-11-11-store-spike-aurora.md`, `docs/plans/2026-11-11-store-spike-grapheneos.md`. All four existed and were well-formed at card start; no spike is missing.

## Verdict table

| Store | Verdict | Confidence | One-line reason | Largest UNVERIFIED residue |
|---|---|---|---|---|
| F-Droid | FEASIBLE | HIGH | Signed index (v1 JAR / v2 JSON + entry.jar diff chain) over a static `/repo/` tree with an existing official mirror program — exactly the shape a caching proxy can re-serve; signature verification stays client-side. | Current index-v2 signing algorithm in force, f-droid.org mirror-fetch rate/abuse policy, and no primary-source root-key fingerprint. |
| Obtainium | PARTIAL | MEDIUM-HIGH | Client app, no server-side index to mirror — GitHub-release sources map cleanly onto `ShadowSource::GithubRelease` + `asset_selector` rules, and Cronet honors system proxy settings, so per-app recipe sync works while "store sync" does not. | Default pinned-host list (governs whether a LAN proxy can serve GitHub assets) and the exact GitHub API endpoints Obtainium calls. |
| Accrescent | PARTIAL | MEDIUM | Developer-signed APKs served over HTTPS are individually cacheable (`ShadowSource::DirectUrl` at best), but the project rejects third-party repositories by design and publishes no repo index, wire format, or CDN layout. | No published metadata endpoint/format or repo-metadata signing scheme; whether split APKs are per-device (cache-partitioning risk). |
| Aurora Store | NOT-FEASIBLE | MEDIUM-HIGH | Unofficial Play client with no mirrorable contract: every download URL is minted inside a credential-gated, reversed-engineered session; anonymous mode rides third-party dispensers pooling ~250 real Google accounts, and fleet aggregation concentrates the exact signals Google invalidates on (Aug–Sep 2026 anonymous-install cutoff, issue 1566). | Full Play ToS "Unauthorized Access to Accounts" clause text (heading only — extraction truncated twice), Play delivery-URL stability/expiry, and protocol internals. |
| GrapheneOS App Store | NOT-FEASIBLE | MEDIUM-HIGH | Cleanest wire contract of the five (static HTTPS, signify-signed metadata with generation-key rotation, APK + fs-verity/v4 end-to-end verification = fail-safe cache) but no redirect path (OS-integrated hard-wired `apps.grapheneos.org`), no authorization (no mirror program; EFF DNT is a privacy policy, not a license; Play apps/Android Auto are Google-proprietary), and no `shadow_types` fit. | Whether the App Store client honors Android system proxy settings or any alternate-repo setting exists (none documented); metadata file layout/parse contract; TLS pinning/CT enforcement. |

**Pattern:** only 1 of 5 stores has a sanctioned, mirrorable store contract (F-Droid); 2 are per-app source models with no store-level index (Obtainium, Accrescent); 2 are credential- or OS-gated with no third-party path at all (Aurora, GrapheneOS). For any store other than F-Droid, Monopsony's realistic role is opportunistic in-transit byte caching, not store mirroring — and no store's trust anchor can be substituted by the proxy.

## Kickoff gate status

The mobile roadmap kickoff gate demanded **five store-source spikes**. Count from the docs on disk:

- 2 banked at handoff (2026-11-10, task t_5128761f, one file): F-Droid, Obtainium.
- 3 landed in the 2026-11-11 lane (t_046d00f4, t_2ffac304, t_30c8d713): Accrescent, Aurora, GrapheneOS.
- **2 + 3 = 5 → the store-source spike gate is SATISFIED.**

Arithmetic note: the evening handoff line "3 of 5 done" was an internal wording slip. The docs prove only 2 spikes existed before the evening cards (the 2026-11-10 file covers exactly two stores), and the GrapheneOS card's own sibling list ("F-Droid + Obtainium banked 2026-11-10, Accrescent + Aurora banked 2026-11-11") confirms the 2-then-3 sequence. No repo file records a "3 of 5" claim; it appears only in the handoff prose.

**Remaining kickoff gates (not covered by this card):**
1. **VpnService policy review** — undecided.
2. **PWA-vs-native management-app call** — undecided.

Provenance caveat: the two remaining gates are named in this card's body; no mobile-roadmap doc in `docs/plans/` enumerates them (grep for `VpnService` / `PWA` across `docs/` returns only the four spike headers), so their full criteria must be read from the roadmap source before either is treated as scoped.

**Gate satisfied: yes (store-source spikes complete). Roadmap kickoff: still gated on the two items above.**
