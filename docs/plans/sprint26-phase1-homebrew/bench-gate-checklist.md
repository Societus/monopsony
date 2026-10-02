# B1 fleet gate — m2mini verification checklist (run AFTER hub wiring + workspace green)

Precondition: `cargo test --workspace` green, clippy --all-targets 0 errors,
hub wiring landed (routes mounted, enablement arm, mod decl). Deploy fresh
server binary to bench .58; client binary + `monopsony` CLI to m2mini.

## Setup (m2mini, user-run where sudo-marked)

1. `monopsony-client proxy on --pm brew --server 10.10.20.58:3128` — expect
   `✓ brew: wrote ~/.homebrew/brew.env`.
2. `cat ~/.homebrew/brew.env` — exactly two lines, both `http://10.10.20.58:3128/brew/...`.
3. `brew config | grep -i domain` — API/BOTTLE domains point at the proxy
   (brew reads brew.env natively; no shell restart needed).

## Gate items (all must pass; per-item verdict table in the report)

| # | Item | Proof |
|---|------|-------|
| 1 | Formula metadata MISS→HIT | `rm -rf ~/Library/Caches/Homebrew/api/*` (user-run); `brew info jq`; server log shows formulae.brew.sh fetch (MISS) then `brew info jq` again → X-Cache: HIT; cache_entries row class=lang_metadata |
| 2 | Bottle full chain | `brew install --force <small formula>` (fresh, not installed); wire order in .58 hits: formula JSON → ghcr manifest (MISS, 200-direct) → blob (MISS via reqwest, 307 followed); brew sha256 verification passes (no fallback Warning); `X-Monopsony-Bottle` from cache on second install after `brew uninstall` + cache-key change |
| 3 | Second-machine HIT (proxy-level) | Same formula from a clean local brew cache (`rm -rf ~/Library/Caches/Homebrew/downloads/*` user-run) → all three routes X-Cache: HIT, zero upstream fetches in server log |
| 4 | brew upgrade | `brew upgrade` on an outdated formula serves bottles from cache |
| 5 | Enablement | dashboard LanguagePmPanel shows brew row (default off until first request → auto source); toggle off → blob route 404s → brew falls back to origin silently (install still succeeds); toggle on → cache resumes |
| 6 | Retention | seed 2 formula versions across majors, force budget_bytes below working set → evict log lists brew blob, floor keeps current major |
| 7 | Scrubber | corrupt a throwaway brew blob (> min-age) → scrub pass quarantines + tamper_alerts row; PURGE blob+sidecars after |
| 8 | Linux PM regression | .20 pacman -Syu green through the same server; npm/pip/cargo MISS→HIT spot check |
| 9 | Off state | `monopsony-client proxy off --pm brew` → brew.env removed → `brew config` shows stock domains |

## Traps (from the skills — binding)

- 404 stubs lie: always test with REAL staged content (origin-fallback
  absorbs mirror failures silently).
- Fresh-download evidence = changed cache-key hash prefix in
  ~/Library/Caches/Homebrew, plus the server hit log — not brew's output.
- `/opt/homebrew` lock-dir ownership: run brew as the prefix owner (martin).
- Non-interactive ssh needs `/opt/homebrew/bin/brew` absolute.
- Verify the DEPLOYED binary contains the shim: sha256 vs local build before
  any gate item; a green /health on :3128 proves nothing.
- Never leave corrupted blobs or forced budgets behind — restore + restart +
  health-check after items 6/7.
- `softwareupdate` items (B3 forwarding) are OUT of scope for this gate.
