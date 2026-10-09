# Sprint 26 Handoff — jumpstart for the next dev session

**Written:** 2026-10-03, end of session. Read this + `docs/plans/2026-10-01-kickoff-backlog-and-macos.md` (authoritative sprint plan) + `docs/sprint-roadmap.md` (local-only backlog) and you're current.

## Where the sprint stands

Sprint 26 = macOS lane. Sequence and status:

| Phase | What | Status |
|---|---|---|
| P1 | Homebrew B1 backend (server caching) | **DONE + gated.** `a7dfab6` + bench gate 9/9 proven on m2mini/.58; gate fixes in `fcc3d18` |
| P3 / B2 Tier 1 | macOS system-proxy CONNECT forwarding | **DONE, committed** `a9def65` (`proxy on/off/status --system` via networksetup → local client proxy 127.0.0.1:3129, forwarded-NOT-cached) |
| P3 / B2 Tier 2 | cask caching via HOMEBREW_ARTIFACT_DOMAIN | **NO-GO, closed.** Proven on m2mini (brew 7.0.7): env var is honored for bottles, NEVER contacted for casks (NO_FALLBACK=1 bottle fetch errored while identical-env cask fetch came from origin). Do not re-spike. Recorded in kickoff plan + `brew.rs` comment |
| P3.5 / 3.5b | Content Caching orchestration (scripted `AssetCacheManagerUtil` + discovery escape hatches) | **Client lane CODE COMPLETE (later Oct 3).** `setup mac [--status\|--disable]` + `content_cache.rs`; unprivileged status live-verified on m2mini. Root acceptance (activate) still needs the console — see "pick ONE" below. 3.5b (TXT/Parents/mDNS) untouched. Kickoff plan §113–132 |
| P4 / B4 | SwiftUI menu-bar client + Darwin CI lane | **Designed, not started.** Kickoff plan §134–144 |

CI note: workspace `-D warnings` clippy had been red since Sep 15 (per-crate local gates never compiled all targets). Fixed in `bb979f9` (pushed; Forgejo run 10 was still running at session end — confirm green first thing). Backlog item for a **macos-arm64 Forgejo runner on m2mini** is in `docs/sprint-roadmap.md` — x86_64 CI structurally cannot compile `cfg(target_os = "macos")` code.

## Recommended next session: pick ONE

1. **Phase 3.5 console acceptance** (implementation DONE later this session): run `monopsony-client setup mac` at the m2mini **console** (one sudo prompt; martin has admin). Verify: `setup mac --status` shows `ON ✓ — shared content only`, `AssetCacheManagerUtil settings` shows `AllowPersonalCaching: false`, and a second Apple device on the LAN shows cache hits (System Settings → General → Sharing → Content Caching). Then pick 3.5b or B4.
2. **Phase 3.5b** — TXT-record/Parents/mDNS-reflector orchestration (needs a second Mac or VLAN to prove anything; heavier bench requirements).
3. **Phase 4 / B4** — SwiftUI menu-bar app skeleton on m2mini (`swift build`, MenuBarExtra, thin shell over daemon; CLT already present, Rust 1.98.1 verified). Bigger lift, user said GUI is wanted.

## Open items (in priority order)

- **Verify CI run 10 green** on `bb979f9` (was `running`). API: `https://base.societus.club/api/v1/repos/societus/monopsony/actions/tasks?limit=3`
- ~~**m2mini client rebuild**~~ — **DONE (later Oct 3).** Sources rsynced, `cargo build --release --offline` green (17s); `target/release/monopsony-client` is the only client copy on the bench (no deployed binary elsewhere — verified). `setup mac --status` + `proxy status --system` proven through the fresh binary unprivileged. Remaining: `--system` acceptance at the **console** (networksetup may prompt; ssh-only verification is impossible for the OS-proxy flip).
- **`.58` budget decision** — `budget_bytes` left at 1 GiB (backup: `/etc/monopsony/server.toml.item7-backup`, original 1 MiB). For any future bottle-HIT benching keep 1 GiB; otherwise consider restoring 1 MiB baseline or picking a production number.
- **Quarantined blob kept on purpose** as forensic evidence: `.quarantined/brew/bottles/oniguruma/4.3.1/` on .58. Delete whenever.
- Backlog (roadmap, local-only): macos-arm64 CI runner, scrubber whole-root-walk scope defect (walks metadata pages + `tls/ca.key` expecting `.meta`), docker thin-sidecar full-shape parity, unit-file cleanup of empty version dirs after evictions.

## Environment cheat sheet (hard-won; trust these)

- **.58 staging server**: `ssh monopsony@10.10.20.58` (key auth; passwords never worked). Server runs `~/monopsony-staging-server --config /etc/monopsony/server.toml --database-url postgres://…` via `setsid nohup`; health `http://10.10.20.58:3128/health`. Use `-o StrictHostKeyChecking=accept-new` if host-key errors. Deploy: scp → `/tmp/ms-fixN` → verify sha → kill `ps -o pid= -C monopsony-staging-server` → swap → restart (recipe in AGENTS.md-adjacent history).
- **m2mini**: `martin@10.10.2.127` (LAN IP; `.local`/Tailscale names unreliable). `martin-ci` for non-interactive ssh. **Never reboot** (Tailscale+FileVault need console login; LuLu blocks sshd-spawned outbound HTTPS until GUI approval). brew at `/opt/homebrew/bin/brew` (not on non-interactive PATH). `/opt/homebrew` owned by martin; martin-ci lacks admin/sudo.
- **AssetCacheManagerUtil verbs (bench-verified, macOS 26.3.1):** `canActivate`, `status`, `settings` run UNPRIVILEGED; `isActivated` needs root (rc=77) BUT `-j isActivated` works unprivileged — rc=1 + JSON on stdout when deactivated, NSLog line on stderr; capture BOTH streams, never treat rc as the signal. Root-only: `activate`, `deactivate`, `reloadSettings` (unprivileged → "must run as root", rc=77). Status verb is `isActivated`, NOT `isActive` (earlier handoff/kickoff text said `isActive` — wrong).
- **.20 Linux test client**: `root@10.10.20.20` (Arch; glibc).
- **Local gates = the EXACT CI command**: `cargo clippy --workspace --locked -- -D warnings` + `cargo fmt --all -- --check` + `cargo test --workspace` (977 passing after Phase 3.5). NEVER per-crate `-p` runs as the final gate.
- Patch-tool lint output lies about edition ("Rust 2015 async fn") — cargo is authoritative; don't chase it.

## This session's provenance (for the record)

- Gate fixes `fcc3d18` deployed to .58 mid-session (binary `763be64c`); all 9 bench-gate items proven across lane-1 (m2mini), lane-2 (Linux), and hub re-verification. 4 product defects + 1 latent bug fixed: pip gzip serve rewrite, brew gzip JSON decode, `record_spool_meta` at all spool-commit sites, full-shape shim sidecars, `/pypi/packages` strip path bug.
- Untracked scratch (do not commit): `docs/plans/sprint26-phase1-homebrew/gate-blocked-commands.txt`, `make-commits.sh`.
- `docs/sprint-roadmap.md` is **gitignored by design** (internal-only) — never try to commit it; preflight guards in future commit scripts should assert it stays OUT of diffs.
