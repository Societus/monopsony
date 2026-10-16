# Deferral Gate — Kickoff Re-Grounding (Sprint 25 companion)

**Date:** 2026-10-16 · **Status:** PREP — spec is approved and unchanged
(`2026-10-01-deferral-gate-spec.md`); this doc re-verifies its tree claims against the
2026-10-16 tree and records what changed since the spec was written. Slices §10 of the spec
remain the build order; nothing here re-opens settled design.

## Re-verified tree claims (spec grounding vs today)

| Spec claim (2026-10-01) | Status 2026-10-16 |
|---|---|
| Serve-time per-PM gate at `resolve_pm_enabled`, proxy.rs **:554** | ✓ exists — now at **proxy.rs:580** (line drift only; the call site is the gate's insertion neighbor) |
| `retention.rs::parse_artifact_path` → (ecosystem, name, version) | ✓ `retention.rs:73` |
| `body_rewrite.rs::rewrite_pypi_simple` / `rewrite_cargo_config` | ✓ `body_rewrite.rs:30` / `:47` |
| `server_settings` JSONB getter/setter | ✓ `db.rs:1812` (`get_setting`) / `:1828` (`set_setting`) |
| Admin route pattern (`language_pms.rs` GET/PATCH) | ✓ `crates/monopsony-admin/src/language_pms.rs` |
| Scan pipeline (ClamAV/YARA/entropy + CVE), pins, ledger rails | ✓ unchanged since spec |

**Verdict: all eight grounding claims hold.** No spec text invalidated; only line numbers moved.

## What changed in the tree since the spec (2026-10-01 → 2026-10-16) — and its effect

1. **Homebrew backend + bottle shim landed (Sprint 26 Ph1).** Spec §1 said brew tracks land
   "post-B1" — B1 is now code-complete and tested, so `brew/bottles` is a real v1 track
   candidate. Casks stay OUT (Tier-2 spike NO-GO 2026-10-03: `HOMEBREW_ARTIFACT_DOMAIN` is
   never contacted for casks — there is no cacheable cask surface to gate).
2. **Serve path is spool-streaming end to end.** The miss-path gate MUST fire before upstream
   fetch (spec §3.5: "the bytes never transit until released") — insertion point is beside
   `resolve_pm_enabled` (proxy.rs:580) and before spool commit, mirroring how per-PM
   enablement short-circuits today. Hit-path gate goes in front of `serve_cache_hit`.
3. **Sprint 27 Part A landed** (`client_health.rs`, staleness gauges). No conflict; deferral
   metrics (`deferral_blocked_requests_total` etc.) reuse the same exporter + metric-name
   pattern in `monopsony-common/src/metrics.rs`.
4. **Docker pull-through confirmed UNMOUNTED; Sprint 25 OCI registry now staged**
   (`2026-10-16-sprint25-oci-registry-push.md`). Confirms spec open-question 4: the
   docker/OCI deferral track lands after the registry so both share tag/version plumbing.

## Kickoff decisions (user calls; spec §11 + new)

| # | Question | Assumed default |
|---|---|---|
| 1 | `window_days` per track | npm/pip 7d, cargo 14d, brew/bottles 7d |
| 2 | Canary fraction | fixed 0.5 |
| 3 | Distro-track granularity v1 | repo-level where PM metadata identifies it (`apt/<repo>`); `*/all` fallback |
| 4 | Docker/OCI track | after Sprint 25 registry (sequencing already agreed) |
| 5 | brew/bottles in v1 tracks? (new) | yes — it's the same LangMetadata/LangArtifact machinery the language PMs use |

## Slice entry points (spec §10 build order, unchanged)

1. `deferral_state` table + policy storage + first-observed on miss — files:
   `monopsony-db/src/db.rs` (migration + CRUD), new `monopsony-proxy/src/deferral.rs`,
   miss-path hook near `proxy.rs:580`. **Observability only — no gating yet.**
2. Serve-time gate (ring, artifact-only, no curation) + metrics.
3. Fence mode + admin API (`monopsony-admin/src/deferral.rs`, `language_pms.rs` pattern) + audit log.
4. Metadata curation: pip simple → cargo → npm packuments (extend `body_rewrite.rs`).
5. Dashboard panel + ring assignment (SvelteKit, `LanguagePmPanel` pattern).
6. Client-ring awareness (canary early-serve; rides inventory/heartbeat plane — no new transport).
7. Fleet verification matrix (spec §9: defer real npm version on .58, .20 install resolves to
   prior version; promote → install proceeds; fence drill → 403 + clean rollback; one pip case).

Slices 1–3 = paid-server MVP (spec estimate 1–2 d/slice).

## Gates

- Workspace suite green + clippy `-D warnings`; new crate tests own their coverage.
- Fleet items run on the standard bench (.58 server / .20 client) per
  `references/fleet-bench-verification.md` rules (verify deployed binary before trusting results;
  delete bench seeds — deferral rows ARE state, clean `deferral_state` after the drill).
