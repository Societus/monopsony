# Sprint 26 Phase 1 — B1 Homebrew fleet gate: PASSED (9/9) — 2026-10-16

Completes the gate left open 2026-10-02 (5/9 then) and 2026-10-15 (items 1–4).
Items 5–9 closed this session. Bench: .58 (10.10.20.58, server) + m2mini
(10.10.2.127, client, brew 7.0.9 / macOS 26.3.1).

## Session-start findings

- **.58 server was DOWN** (died ~2026-10-16 00:51 per old log tail — no systemd,
  manual process). Restarted: `RUST_LOG=info nohup ~/monopsony-staging-server
  -c /etc/monopsony/server.toml > ~/server-run-20261016.log 2>&1 &`.
  Binary sha256 prefix `3d0fa275dd65f20b` matches the Oct 15 provenance-verified build.
- **Item 5's blocker dissolved**: the admin write API does NOT require the
  server host to run tailscale — without tailscaled on .58 the dashboard falls
  back to username/password SESSION auth. `POST /api/v1/auth/login` (JSON
  `{username, password}`) → session cookie → `PATCH /api/v1/admin/language-pms`
  works over plain LAN. The Oct 15 session's 415 was just a wrong content-type.

## Per-item verdicts

| # | Item | Verdict | Evidence |
|---|------|---------|----------|
| 1 | Formula metadata MISS→HIT | ✅ 2026-10-15 | `brew info jq` → DB row `formulae.brew.sh/api/formula/jq.json` class=lang_metadata, cached that session |
| 2 | Bottle full chain + install from cache | ✅ 2026-10-15 | manifest row `ghcr.io/v2/homebrew/core/jq/manifests/1.8.2-1` class=lang_artifact; `brew uninstall jq && brew install jq` → jq-1.8.2 runs; brew sha256 passed untouched |
| 3 | Refetch HIT (zero upstream) | ✅ 2026-10-15 | single ghcr manifest row on second install; blob + sidecars on .58 |
| 4 | brew upgrade from cache | ✅ 2026-10-15 | `brew upgrade ada-url` served from cache, no upstream fetch |
| 5 | Enablement toggle | ✅ 2026-10-16 | session login → PATCH off → flat blob route `/brew/bottles/jq-1.8.2.arm64_tahoe.bottle.1.tar.gz` 404 → live `brew fetch --force jq` on m2mini: "Warning: Bottle missing, falling back to the default domain…" → ghcr origin, exit 0 → PATCH on → 200. Bonus: the enabled probe 200'd from a cold LRU (server restart) — list_keys formula fallback proven |
| 6 | Retention eviction + floors | ✅ 2026-10-16 (with notes) | budget forced 1073741824 → 28572634 (server.toml, backup `.bak-gate6`), restart → `Retention: evicting 1 lang-artifact blob(s) over budget (28572634 bytes)` at 00:08:41; ada-url bottle (version `4.0.0_1` — unparseable → eviction priority) left the cache; floors held: jq 1.8.2, oniguruma 6.9.10/5.4.0, node, sqlite, simdjson, simdutf all intact. Budget restored, toml diff-identical to backup, server restarted healthy |
| 7 | Scrubber quarantine | ✅ 2026-10-16 | synthetic `gatetest` blob (1 byte appended) → 60 s pass: `checksum mismatch — blob quarantined … expected=ca67c64d… actual=a9b241bb…` → `.quarantined/` move + `Tamper alert recorded: id=11` + tamper_alerts row (verified via psql) → purged blob+sidecars+quarantine after |
| 8 | Linux PM regression | ✅ 2026-10-16 | .20: `pacman -Sy` exit 0 (`.sig` 404 = documented pre-existing); expac MISS (Cached: 14596 B + .sig) → local pkg cache wiped → reinstall exit 0 with Cached-count unchanged (HIT); npm is-odd ×2 exit 0 zero upstream (cache-served); pip idna MISS (index+wheel cached) → second download zero upstream; cargo bitflags ×2 exit 0 zero upstream. expac + /tmp artifacts cleaned |
| 9 | Off state | ✅ 2026-10-16 | `proxy off --pm brew` → brew.env removed → `brew config` shows no domain overrides → `proxy on` restores the two-line env file |

## Notes / observations for follow-up

1. **Retention sidecar residue (small wart)**: hand-seeded sidecars whose blob
   was evicted can linger (the `.cache-meta` ghost kept claiming its size in
   later walks). Real-commit entries evicted during the window (ada-url) were
   removed cleanly including sidecars, so this may be specific to
   hand-seeded/synthetic entries; worth one targeted unit test if it ever
   recurs. Self-healed after budget restore + restart (no re-plans).
2. **Scrubber noise (pre-existing)**: `sidecar unreadable` errors for
   `pins/*`, `tls/*`, and (observed this session) `.cache-meta/*` paths walked
   as keys — hundreds per pass. Harmless but buries real signal; candidate for
   a walk-scope fix.
3. **Unparseable-version eviction priority confirmed live**: `4.0.0_1`
   (brew rebuild suffix) ranked ahead of parseable old-majors — matches
   retention.rs's "unparseable ranks below everything" rule.
4. The brew flat blob path shape is `/brew/bottles/{filename}` (single
   segment) — deeper shapes 404 by design. m2mini live traffic during the
   window (merve/uv/wget/tree) re-warmed cache entries — bench is actively
   used, keep arithmetic time-boxed.

## Leftover state (post-cleanup)

- server.toml restored (diff-identical to pre-test backup), server restarted
  ×2, /health + brew shim 200 + admin 200.
- Synthetic seeds purged: gatetest (blob+sidecars+quarantine),
  oniguruma 4.3.1 (blob+sidecars+Oct 3 `.quarantined` leftover).
- m2mini: brew.env ON (two lines → 10.10.20.58:3128), brew enabled=true.
- tamper_alerts rows id=11/12 remain as audit trail (gate evidence).
- Server log for this session: `.58:~/server-run-20261016.log`.

**Gate verdict: Sprint 26 Phase 1 (Homebrew B1) — PASSED 9/9. Sprint 26 Phase 1 complete.**
