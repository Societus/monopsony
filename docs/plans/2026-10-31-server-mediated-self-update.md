# Server-mediated self-update distribution

Date: 2026-10-31 · Status: PLAN (decisions D1–D4 settled by operator, open questions carry defaults)
Sprint: 34 (roadmap entry: `docs/sprint-roadmap.md`)

## Problem / proven failure

Two defects, verified against the tree on 2026-10-31:

1. **Self-update asset matching is dead against real releases.**
   `check_for_update` (`crates/monopsony-client/src/self_update.rs:469`) lists
   assets from `api.github.com/repos/societus/monopsony/releases/latest`, but
   `select_asset_for_platform` (`self_update.rs:553`) and every rule in
   `DEFAULT_MONOPSONY_RECIPE` (`crates/monopsony-common/src/shadow_types.rs:16`)
   match `monopsony-x86_64-unknown-linux-gnu.tar.gz`-style names. The release CI
   (`.github/workflows/release.yml`, runs on the public GitHub mirror
   Societus/monopsony because Forgejo sits behind forward-auth and cannot host
   downloads) actually publishes:
   ```
   monopsony-{server,client}-v<VERSION>-{linux-x86_64,linux-aarch64,darwin-arm64,darwin-x86_64}.tar.gz
   SHA256SUMS (+ SHA256SUMS.sig when GPG secrets are configured)
   ```
   No rule can ever match; the selection also has no server/client split and no
   darwin rules. `apply_update` (`self_update.rs:601`) additionally hardcodes
   `monopsony-server.bak.*` backup names for both binaries.

2. **Every host performs its own WAN download.** Each server and each client
   independently queries GitHub and downloads its own tarball — the exact
   duplication the caching proxy exists to eliminate. There is no server-side
   artifact store, no serving channel, and no fleet version visibility
   (`HeartbeatRequest` carries no client binary version; `HeartbeatResponse`
   carries only `server_time`).

Goal: the server pulls each release **once** (WAN), verifies it, stores it
content-addressed, and serves every client over the LAN — the same
single-point-of-cache economics as package caching. Operator framing: "the
same fashion that the single point of a server's cache saves duplicated
downloads."

## Decisions log (settled — do not re-litigate)

| # | Decision | Rationale |
|---|----------|-----------|
| D1 | Serve binaries over **gRPC streaming on the existing mTLS control plane** — new `CheckUpdate` + `DownloadUpdateAsset` RPCs modeled on `DownloadShadowAsset` (`proto/monopsony.proto:27`, handler `grpc.rs:1573`) | No new credentials or ports; rides ws_tunnel/Cloudflare for free; paired-client auth and cert revocation apply automatically |
| D2 | Server pulls from **GitHub Releases on the public mirror** (where release CI actually publishes), verifying `SHA256SUMS` (+ GPG `.sig` when a key is pinned) | Forgejo cannot host downloads (forward auth); CI does not publish artifacts there. GPG-signing secrets already wired in release CI |
| D3 | Rollout policy: **server auto-fetches on new release; clients apply only after admin approves a rollout** (dashboard/CLI). Per-client manual `self-update apply` always available | Fleet control with minimal toil; mirrors the shadow staging approval philosophy |
| D4 | Server's own binary **is in scope**: server self-applies from the already-downloaded local blob via the managed pipeline. Order: distribute-first, self-apply last | Same code path, one WAN download; ordering keeps the distribution plane alive while clients converge |
| D5 | Store artifacts in the existing **`ShadowCache`** (`crates/monopsony-shadow/src/shadow_cache.rs:64-146`) — `store_blob` + `index_asset(package, version, platform)` | Content-addressed dedup + orphan GC already solved; index shape fits `(monopsony-client, 1.0.1, linux-x86_64)` exactly |
| D6 | Rollout approval pushes to clients via the **pending-assignment pattern** (`take_pending_assignment_command`, `grpc.rs:770`; table precedent `pending_shadow_assignments`, `db.rs:639`) — new `pending_update_assignments` table + a Command variant | Proven push channel over ControlStream; status tracking (pending → delivered → applied/refused) for the dashboard |
| D7 | GPG verification of `SHA256SUMS.sig` via **rpgp** with a pinned public key in `server.toml [update]` (armored, mirrors.toml `trusted_keys` precedent). Key configured ⇒ sig **required and must verify** (fail-closed). No key ⇒ sha256-only + warning metric | Same trust model as mirror integrity (no keyserver, no TOFU); fail-closed only when the operator opted into pinning |
| D8 | Server→client update manifest is **Ed25519-signed with the existing shadow signing key**; clients verify via the existing trust key plumbing (`trust.rs`, `[shadow_trust]`) | Extends an established trust root; keeps Phase-3 "tracker-shaped edges" open (a future tracker/mirror can serve the same signed manifest) |
| D9 | Distinct new RPCs rather than overloading the shadow ones | Shadow RPCs are keyed by `package_id` recipe semantics; self-update has different fields (binary kind, platform, rollout state) |

## Architecture

```
GitHub Releases (public mirror, D2)
        │  1× per release (poller, [update].check_interval + admin "check now")
        ▼
┌─ monopsony-server ────────────────────────────────────────────────┐
│ update_distributor.rs                                             │
│   verify SHA256SUMS (+rpgp GPG, D7) → ShadowCache.store_blob (D5) │
│   → index_asset(monopsony-{client,server}, ver, platform)         │
│   → DB: update_releases / update_artifacts / pending assignments  │
│ Serving: CheckUpdate + DownloadUpdateAsset (mTLS gRPC, D1/D9)     │
│   manifest Ed25519-signed (D8), rollout-gated (D3)                │
│ Self-apply last (D4): managed pipeline from LOCAL blob            │
└───────────────────────────────────────────────────────────────────┘
        │  per client, over LAN mTLS (ws_tunnel-transparent)
        ▼
monopsony-client daemon
   heartbeat carries monopsony_version → fleet skew on dashboard
   rollout approved → staged two-phase apply (below) → self-restart
   unpaired / server unreachable → GitHub-direct fallback (P0-fixed)
```

**Client apply safety (self-swap).** The managed pipeline cannot health-check
a binary swap of the *running* daemon from inside itself. Two-phase staging:
the daemon downloads + verifies sha256 + stages the new binary, then spawns
`monopsony-client self-update finalize` as a **fresh non-daemon process** and
exits; the finalize process performs the snapshot → swap → health check
(process/`--version`) → commit/rollback exactly like the existing managed
pipeline, then restarts the unit (`systemctl restart` / OpenRC
`rc-service restart` / `launchctl kickstart`) or exits for systemd
`Restart=always` to pick up. Old binary kept as `.bak`;
`self-update rollback` CLI restores it. Init-system detection follows the
installer's existing `detect_init()` families — the bench fleet exercises
all three (ct/1952 systemd glibc, ct/1904 OpenRC musl, m2mini launchd).

**Version order.** `semver::Version` comparison only (the fixed
`parse_version` at `self_update.rs:438`); build metadata ignored. The server
never pushes a version older than a client's current.

## Phased implementation

Each phase ships and verifies independently on the bench (server 10.10.20.58;
clients ct/1952 Debian linux-x86_64, m2mini darwin-arm64). Config-driven
`[update] repo` lets the bench point at a scratch GitHub repo with synthetic
releases instead of touching real tags.

### P0 — Fix the asset contract + version plumbing (prerequisite bugfix, ~0.5d)

- `select_asset_for_platform`: new matching against the real contract —
  kind-aware (`monopsony-server-` / `monopsony-client-`), versioned names,
  `linux-{x86_64,aarch64}` (musl static; libc no longer required to match),
  `darwin-{arm64,aarch64→arm64 remap,x86_64}`. Signature gains `binary_kind`.
- Split `DEFAULT_MONOPSONY_RECIPE` into `DEFAULT_MONOPSONY_SERVER_RECIPE` and
  `DEFAULT_MONOPSONY_CLIENT_RECIPE`: per-binary hooks (stop/start
  `monopsony-server` vs `monopsony-client`), health checks (server: port
  50051; client: process), per-binary `chmod`/backup names.
- `apply_update`: parametrize binary name (drop hardcoded `monopsony-server.bak`).
- Tests: fixture list of all 8 real asset names + `SHA256SUMS` (from the
  release.yml contract) → assert correct selection for every
  binary×platform; negative tests (wrong kind, unknown platform).
- Files: `crates/monopsony-client/src/self_update.rs`,
  `crates/monopsony-common/src/shadow_types.rs`, server wrapper
  `crates/monopsony-server/src/self_update.rs`, both `main.rs` CLI arms.
- Gate: `cargo test -p monopsony-client` + workspace check (shared-struct rule).

### P1 — Server-side distributor: fetch once, verify, store (~1.5d)

- New `crates/monopsony-server/src/update_distributor.rs`:
  - Release listing via GitHub API (reuse `--github-token` /
    `MONOPSONY_GITHUB_TOKEN` already wired in server main.rs:115); choose
    latest by semver honoring `include_prerelease` (default true — fleet
    ships rcs).
  - Download all 8 tarballs + `SHA256SUMS` (+`.sig`); sha256-verify every
    asset; rpgp GPG verify per D7; atomic store into ShadowCache +
    `index_asset`; DB rows.
  - Retention: keep `retain_versions` (default 3) newest complete releases;
    never prune the server's own running version.
- DB (inline `CREATE TABLE IF NOT EXISTS` pattern, `db.rs`):
  `update_releases` (version, prerelease, fetched_at, changelog_url,
  assets_complete) and `update_artifacts` (release FK, package, platform,
  asset_name, sha256, size, blob_indexed).
- Config `server.toml [update]`: `enabled`, `repo` (default
  `societus/monopsony`), `check_interval` (default 6h), `include_prerelease`
  (default true), `retain_versions` (3), `trusted_gpg_key` (optional armored
  key / path). Document in `dist/server.toml.example`.
- Background poller spawned alongside the mirror sync task (main.rs:900
  precedent); `server_settings` runtime toggles.
- Admin REST (route registration precedent `monopsony-admin/src/lib.rs:201`):
  `GET /api/v1/updates` (latest release + per-platform completeness),
  `POST /api/v1/updates/check` (fetch now), plus rollout endpoints in P2/P4.
- Gate: unit tests with a mocked GitHub API (wire-format fixtures captured
  from the real API shape), sha256-mismatch fail path, GPG fail-closed path
  (rpgp test keys — synthetic, throwaway), retention prune test.

### P2 — gRPC serving surface (~1d)

- proto additions (`MonopsonyControl`):
  - `CheckUpdate(CheckUpdateRequest{current_version, binary, platform}) →
    CheckUpdateResponse{latest_version, update_available, rollout_approved,
    asset{name, sha256, size}, changelog_url, manifest_sig}` — manifest body
    (version+asset digest+size) Ed25519-signed (D8).
  - `DownloadUpdateAsset(DownloadUpdateAssetRequest{binary, version,
    platform}) → stream UpdateAssetChunk{data, offset, total_size, sha256}` —
    ShadowCache blob → 1 MiB chunks; ~1 min of work to keep the stream
    identical in shape to `ShadowAssetChunk` (grpc.rs:1573 precedent).
- Rollout gating: `pending_update_assignments` table (client_id, version,
  status pending/delivered/applied/refused, created_at) — `POST
  /api/v1/updates/rollout {version, client_ids|all}` creates rows;
  `take_pending_assignment_command`-style push on the next control-stream
  yield; `CheckUpdate.rollout_approved` true iff a pending row exists for the
  caller. Clients report outcome via existing report RPC shape (applied /
  refused + from/to version) → dashboard state.
- `HeartbeatRequest.monopsony_version = 7` (string) — stored on the client
  row; `HeartbeatResponse` unchanged (CheckUpdate is the version channel).
- Gate: control-crate handler tests with an in-memory ShadowCache + fake
  assignment rows; streaming round-trip test asserting sha256 of reassembled
  bytes; signature verify test on the manifest.

### P3 — Client rework: server-first source, staged apply (~1.5d)

- `self_update.rs`: source enum `Server { grpc } | GitHub`; paired clients
  prefer `CheckUpdate` + `DownloadUpdateAsset` (verify manifest signature when
  a trust key is configured — fail closed per `trust.rs` policy); fallback to
  GitHub direct when unpaired or the server is unreachable (P0-fixed path).
- Refactor `apply_update_managed` to accept an asset **provider**
  (`Url(String)` | `ServerStream(bytes)` | `LocalBlob(path)`) instead of
  downloading from a URL inside phase 1 — the pipeline phases, snapshot and
  rollback logic stay untouched.
- Daemon integration: piggyback on heartbeat cadence — when
  `rollout_approved`, run staged two-phase apply (architecture above);
  `client.toml [update] policy = "rollout"|"manual"` (default rollout;
  `"manual"` = per-client opt-out, dashboard shows it as opted-out).
- CLI: `self-update check` reports both channels (server latest vs GitHub
  latest); `self-update apply` gains `--from-server`; new
  `self-update finalize` + `self-update rollback` (hidden/internal,
  documented).
- Gate: client-crate tests with a mock control server (tonic in-proc):
  check→download→verify→stage flow; manifest-signature negative test;
  finalize swap+rollback test using temp dirs (existing managed-pipeline test
  harness).

### P4 — Server self-apply + dashboard fleet view (~1d)

- `POST /api/v1/updates/self-apply {version}` — managed pipeline against the
  **local** blob (`LocalBlob` provider). Confirm-guard: warns while any active
  paired client reports a version < target (operator can override). Order per
  D4: rollout → clients converge → self-apply last. Note: a server running
  version N serves N+1 blobs fine — the serving path is data, not code.
- Dashboard (SvelteKit section, rust-embed): updates panel (latest release,
  per-platform asset completeness, rollout button with client multiselect,
  self-apply with guard) + fleet version column on the clients page (from
  heartbeat-reported version) + skew badge when a client lags >1 release
  (Sprint 27 health precedent).
- Gate: dashboard build + `cargo test -p monopsony-admin`; live bench pass.

### Bench verification pass (dispatcher, after each phase or bundled)

1. Scratch GitHub repo with synthetic releases (v0.9.9 → v1.0.0-rc.1) or a
   real rc cut; server `[update] repo` pointed at it.
2. Verify: poller fetches once (server WAN counter), all 8 assets stored +
   indexed, sha256 + GPG paths exercised (good key, wrong key, missing sig
   with key pinned = fail-closed).
3. ct/1952: heartbeat reports version; rollout approve → daemon stages,
   finalizes, restarts, reports applied; dashboard reflects it; md5 the
   deployed binary against the CI artifact (never `--version`).
4. m2mini (darwin-arm64): same flow through launchd; confirm hook behavior.
5. Server self-apply last: guard warning fires while ct/1952 lags; self-apply
   succeeds; post-restart serving still works (N serving N+1 blobs).
6. Negative: client refused/opted-out shows in dashboard; rollback restores
   prior binary; unpaired client falls back to GitHub direct.

## What this does NOT defend (honesty section)

- **Unpinned upstream trust.** Without `trusted_gpg_key` configured, D7
  degrades to sha256-of-GitHub-served-bytes — i.e. trust in GitHub's TLS and
  the mirror account. Pinning the release GPG key closes this; the default
  config warns (metric + dashboard banner) until a key is pinned.
- **Server root compromise = fleet-wide compromise.** The Ed25519 manifest
  key lives on the server; an attacker with root signs malicious manifests
  for every client. Same trust root as shadow recipes today; mitigation is
  key rotation + cert revocation, not this design.
- **Daemon crash-loop after a bad-but-verified update.** The finalize phase
  health-checks, but a defect that passes the immediate check and breaks
  later leaves the client down until `self-update rollback` / systemd
  start-limit intervention. No automatic watchdog-across-restarts in v1.
- **Clients on versions predating this feature** bootstrap via the
  GitHub-direct fallback (or the installer); they cannot see the server
  channel until upgraded once.
- **Windows clients** — release CI publishes no Windows assets; out of scope
  until that changes.
- **WAN-side bandwidth for huge fleets**: one server still downloads all 8
  assets per release (~100–200 MB); at 20–200 clients this is the intended
  trade. Cross-server/tracker serving is future work (D8 keeps the manifest
  portable for it).

## Open questions (each carries an assumed default — flip cheaply)

1. **`include_prerelease` default `true`** — the fleet ships rc builds; flip
   to `false` for stable-only servers. Default stays true.
2. **Rollout granularity** — default: per-client multiselect + "all"
   shortcut (not percentage/canary waves; canary % is a P5 candidate).
3. **macOS finalize restart mechanics** — default: `launchctl kickstart` when
   a launchd unit is detected, else exit-and-KeepAlive; to be proven on
   m2mini in the bench pass (probe live, per the macOS mechanism rule).
4. **Heartbeat piggyback vs separate CheckUpdate cadence** — default:
   rollout decision rides the existing heartbeat/control-stream; a separate
   client-side `check_interval` only governs the manual/unpaired path.
5. **Keep GitHub fallback on paired clients?** — default yes (resilience
   when the server is down during an emergency update); flip = server-only.
