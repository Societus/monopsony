# Deferral Gate & Ring Workflow — Paid-Server Update Retention

**Date:** 2026-10-01 · **Status:** SPEC (not built) — approved direction: "spec first, build later"
**Motivation:** enterprise-style update retention for a paid public server — audit updates before fleets get them, hold back day-zero releases (the Microsoft patch-Tuesday ring model). Decided 2026-10-01 after the macOS Content Caching investigation concluded Apple's plane is un-interceptable; this is the Monopsony-native answer and applies to **every PM surface Monopsony controls**.

**Grounding (verified in tree):**
- Serve-time per-PM gate exists: `resolve_pm_enabled` → 503 (proxy.rs:554) — deferral gate extends this exact pattern.
- Artifact identification exists: `retention.rs::parse_artifact_path` → (ecosystem, name, version) for npm/pip/cargo.
- Metadata rewrite exists: `body_rewrite.rs::rewrite_pypi_simple` / `rewrite_cargo_config` on HIT — enables index *curation*, not just blocking.
- Policy storage pattern exists: `server_settings` JSONB + `language_pms.rs` GET/PATCH admin route.
- Audit signal exists: ClamAV/YARA/entropy scan pipeline + CVE lookup (Sprints 13–18); tamper alerts + pins (Sprint 20 Ph3); high-water ledger (Ph4).

---

## 1. Concepts

| Term | Meaning |
|---|---|
| **Track** | A (pm, channel) pair the policy can address: e.g. `npm/all`, `pip/all`, `cargo/all`, `brew/bottles`, `apt/<repo>`, `dnf/<repo>`, `docker/<namespace>`. Ships v1 with the four language PMs + brew post-B1; distro/docker tracks are config-identical but land later. |
| **Deferral policy** | Per-track: `{ mode, window_days, require_audit_clean, max_hold_days }`. Modes: `off`, `ring`, `fence` (definitions below). |
| **Ring** (default) | Soft hold: deferred versions are hidden from indexes where rewriting is possible, and artifact requests get `503 + Retry-After + X-Monopsony-Deferred: <iso-deadline>`. Clients with pinned lockfiles that *demand* the version can be allowed through after `max_hold_days` (escape valve so builds never hard-fail). |
| **Fence** (strict) | Hard hold: deferred artifact requests fail `403 Deferred` with no time-based release; only admin promotion (or mode change) releases. For incident response ("nobody pulls this until I say so"). |
| **Promotion** | Admin action moving a (track, name, version) from deferred → available, optionally recording an audit note. Immediate, takes effect on next request (no cache invalidation needed — the gate is serve-time). |
| **Audit-clean** | The scan pipeline result for the artifact is clean (no findings, no quarantined status) — checked at promotion time and continuously if `require_audit_clean`. |
| **First-observed** | Monopsony-clock timestamp when (track, name, version) was first seen in upstream metadata or requested through the cache. **The deferral clock is `now − first_observed`, not upstream release date** — uniform across PMs (npm has `time` in packuments; pip/cargo indexes have no dates), honest about what we can know, and attacker-resistant (a re-pushed same version doesn't reset our clock). |

## 2. Data model

**New table `deferral_state`** (PostgreSQL, migration):

```sql
CREATE TABLE deferral_state (
    id            UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    track         TEXT NOT NULL,           -- 'npm/all', 'pip/all', ...
    name          TEXT NOT NULL,
    version       TEXT NOT NULL,
    version_ord   TEXT,                    -- normalized via existing Sprint 20 Ph4 normalizer (nullable: non-semver)
    first_observed TIMESTAMPTZ NOT NULL,
    status        TEXT NOT NULL DEFAULT 'deferred',  -- deferred | available | fenced | promoted
    promoted_at   TIMESTAMPTZ,
    promoted_by   TEXT,                    -- admin identity
    promote_note  TEXT,                    -- audit note
    audit_state   TEXT NOT NULL DEFAULT 'unknown', -- unknown | clean | flagged | quarantined
    UNIQUE (track, name, version)
);
CREATE INDEX idx_deferral_state_track_status ON deferral_state (track, status);
```

- Populated lazily: first time a version appears in a track's *metadata response* or an artifact *miss*.
- `version_ord` reuses the Sprint 20 Ph4 `version_ord` normalizer (proven) so "latest", "newer than", and floor logic are consistent codebase-wide.

**Policy in `server_settings`** (existing JSONB pattern, key `deferral_policy_v1`):

```json
{
  "tracks": {
    "npm/all":  { "mode": "ring", "window_days": 7,  "require_audit_clean": true,  "max_hold_days": 14 },
    "pip/all":  { "mode": "ring", "window_days": 7,  "require_audit_clean": true,  "max_hold_days": 14 },
    "cargo/all":{ "mode": "ring", "window_days": 14, "require_audit_clean": false, "max_hold_days": 30 }
  },
  "default_track": { "mode": "off" }
}
```

**Rings of clients** are a *client attribute*, not a policy container: `clients.ring` (`canary` | `stable` | `unrestricted`, default `unrestricted`). A track in `ring` mode serves a version early **only** to `canary`-ring clients once `window_days * canary_fraction` has elapsed (default fraction 0.5). Ring assignment rides the existing client inventory/heartbeat plane — no new transport.

## 3. Serve-time gate (the core)

Both hooks live beside the existing `resolve_pm_enabled` check (proxy.rs:554) and share its semantics: DB-backed resolve, cached with janitor-cadence refresh, fail-open on DB error *only when mode=off* (see §8).

**Miss path (upstream fetch):**
1. After PM + track resolution, look up / insert `deferral_state` for (track, name, version).
2. Compute disposition: `available` (promoted, or past window and audit-clean) → proceed; else `deferred`.
3. Ring mode: if client ring = canary and elapsed ≥ canary threshold and audit-clean → serve (log promotion-by-ring).
4. Deferred in ring mode after `max_hold_days` → serve with `X-Monopsony-Deferral-Released: max-hold` (escape valve; logged + counted).
5. Still deferred → `503` + `Retry-After: <seconds to earliest release>` + `X-Monopsony-Deferred: <iso deadline>` + JSON body naming track/name/version. **Never fetch from upstream** — the whole point is that the bytes never transit until released.
6. Fence mode → `403` + `X-Monopsony-Fenced: true`, no Retry-After (release is manual).

**Hit path (serving from cache):** identical disposition check before `serve_cache_hit`. Cache presence does NOT bypass the gate — an artifact can be cached (prefetched, scanned) while still deferred; promotion flips it to servable with zero cache work.

**Metadata curation (ring mode, where rewrite is possible):**
- pip simple-index + cargo config rewrites already exist (`body_rewrite.rs`) — extend them to filter deferred versions from the served index, so `pip install <pkg>` resolves to the newest *released* version instead of 404-ing on a deferred one.
- npm packuments: add a packument rewrite (drop `versions[deferred]` + prune `dist-tags` entries pointing at them). Larger bodies (KB–MB) but buffered rewrite is already the pattern there.
- When curation applies, `pip install pkg==<deferred-ver>` (explicit pin) hits the artifact gate → 503 ring / 403 fence. Lockfile users get the documented escape valve (max_hold) rather than mystery failures.
- **Curate-only-when-configured**: rewriting is on by default for tracks with rewriters; a per-track `curate_metadata: false` fallback gives plain artifact-gating for anything unrewritable.

## 4. Admin API (extends the `language_pms` pattern)

| Route | Verb | Purpose |
|---|---|---|
| `/api/v1/admin/deferral` | GET | Policy + state summary (counts by status/track, oldest deferred, upcoming releases) |
| `/api/v1/admin/deferral/policy` | PATCH | Update per-track policy (validated; mode/window/max_hold bounds) |
| `/api/v1/admin/deferral/state?track=&status=` | GET | Paged deferred/promoted list, filterable |
| `/api/v1/admin/deferral/promote` | POST | `{track, name, version, note}` → status=promoted (requires audit-clean if configured; 409 with audit findings otherwise) |
| `/api/v1/admin/deferral/fence` | POST | `{track, name?, version?}` — fence a version, a whole name, or a track (incident mode) |
| `/api/v1/admin/deferral/release` | POST | Reverse of fence/promote → back to deferred or available |
| `/api/v1/admin/clients/:id/ring` | PATCH | `{ring: canary\|stable\|unrestricted}` |

All mutations append to an **append-only audit log** (`deferral_audit_log` table: actor, action, target, note, timestamp) — this is the paper trail that makes the paid feature defensible. Mutations are admin-session-authenticated like existing admin routes.

**CLI parity:** `monopsony admin deferral status|promote|fence|release --track npm/all --name x --version y` reuses the RPC/HTTP surface.

## 5. Dashboard (SvelteKit panel)

- **Deferral panel** on the admin dashboard: per-track policy cards (mode, window, counts), deferred-versions table with release deadline countdown, promote/fence buttons, audit-note prompt on promote.
- **Ring view:** clients grouped by ring with assignment control.
- Rides existing tiered auth; a11y-clean per repo standard.

## 6. Metrics (Prometheus, existing exporter)

- `deferral_blocked_requests_total{track,mode}` — gate hits (503/403)
- `deferral_released_max_hold_total{track}` — escape-valve fires (watch this: high = window too aggressive)
- `deferral_promoted_total{track}` / `deferral_fenced_total{track}`
- `deferral_curated_metadata_responses_total{track}`
- `deferral_first_observed_lag_seconds` (histogram, metadata-seen → artifact-requested) — future intel value

## 7. Interactions with landed machinery (no regressions)

- **Enablement gate (per-PM on/off)** runs first; deferral only applies to enabled PMs. Orthogonal axes.
- **Scan pipeline**: artifact scan results write `audit_state`; quarantine status wins over deferral (quarantined = never served regardless of promotion).
- **Retention/floors**: floors protect *cache storage*; deferral gates *serving*. A deferred artifact can be evicted while deferred (it'll re-fetch on promotion) — retention stays the storage authority.
- **Pins/integrity**: pin capture unchanged; promoted artifacts serve with pins exactly as today.
- **Ledger (Ph4)**: deferral never *downgrades* a client below its high-water mark (ledger still rejects); the two compose as belt-and-suspenders.
- **Prefetch**: prefetching a deferred version is allowed and encouraged (warm + scan before release) — flag `X-Monopsony-Prefetch-Deferred` in logs so operators can see pre-staged bytes.

## 8. Failure semantics (red-teamed)

- DB unreachable + mode=off → fail-open (serve; deferral is a feature, not a dependency). DB unreachable + mode=ring/fence → **fail-closed for tracks with a policy** (503 + `Retry-After: 60`, logged loudly). A paid retention feature that fail-opens on DB blips is a broken promise; a hard hold that fail-opens is a security incident.
- Clock: server clock only (`first_observed` is our clock; no trust in upstream dates).
- Version normalization failure (non-semver) → treated as immediately-eligible but still `fence`-able by exact string match; never crashes the gate.
- Re-pushed same version upstream (yank-and-replace attack): `first_observed` unchanged; content hash change triggers pin/pipeline mismatch handling as today — deferral clock is not resettable by upstream churn.
- Promotion privilege: admin session required; promotion of quarantined artifacts is refused server-side (409), not just hidden in UI.

## 9. Test plan

- **Unit:** policy parse/validate; disposition computation (window/canary/max-hold/fence); metadata curators (packument/simple-index filtering) with fixture documents; version_ord reuse; fail-open/fail-closed matrix.
- **Integration (axum tests, existing harness):** miss-path gate 503 semantics + headers; hit-path gate (cached-but-deferred); promotion flips serve with no cache invalidation; fence 403; escape valve after max_hold; curation hides deferred from pip simple index; lockfile pin gets 503 ring; scan-flagged promotion refused.
- **Fleet verification (bench .58/.20):** defer a real npm version → client install resolves to prior version; promote → install proceeds; fence during incident drill → 403 with clean rollback. Repeat one pip case.

## 10. Slices (build order when scheduled)

1. `deferral_state` + policy storage + first-observed tracking on miss (no gating yet) — observability first.
2. Serve-time gate (ring mode, artifact-only, no curation) + metrics.
3. Fence mode + admin API (promote/fence/release) + audit log.
4. Metadata curation (pip → cargo → npm packuments).
5. Dashboard panel + ring assignment.
6. Client-ring awareness (canary early-serve).
7. Fleet verification matrix.

Estimated 1–2 days/slice; slices 1–3 are the paid-server MVP.

## 11. Open questions

1. **Default `window_days` per track** — proposal: npm/pip 7d, cargo 14d (slower ecosystem cadence), brew 7d; needs a product call at kickoff.
2. **Canary fraction** — fixed 0.5 or configurable per track?
3. **Distro-track granularity** — `apt/<repo>` vs `apt/all` for v1? (Repo-level matches how Debian/Ubuntu think about stability; proposal: repo-level where the PM metadata identifies it.)
4. **Docker/OCI track** — defer to after the personal-registry work (Sprint 25) so the deferral gate and the registry share tag/version plumbing.
5. **Public-server coupling** — when a public deployment happens, deferral policy should be admin-API-only (no client-visible policy detail beyond headers). Confirm at Sprint 24 kickoff.
