# Hosting Roadmap Triage — Severity-Ranked Remediation Proposals

**Date:** 2026-11-14 · **Status:** Rides 1–4 LANDED uncommitted on the bump tree
(2026-11-14 — see "Post-implementation: updated 1.1.0 stage list" below); S5,
S4, S6 deferred · **Source:**
`docs/plans/2026-11-12-single-port-hosting.md` §1.5 + §2/§3 notes (task t_8260622e's
un-carded findings). **Tree state:** citations verified against the working tree at
2026-11-14 with the 1.1.0 bump staged but unsigned; no code, config, or git changes
made by this doc.

Severities are **deployment-risk** terms: CRITICAL = breaks a default deployment or
causes resource exhaustion under realistic load; WARNING = degraded or unsafe under a
realistic (esp. public) posture; INFO = documentation/robustness debt. Effort SWAG:
XS < 1 h, S ≤ 0.5 d, M 1–2 d, L 3 d+.

---

## Finding 1 — Tracker default :3130 collides with the dashboard default :3130

**Premise correction up front:** the dispatch text said the tracker binds "3129 per
config". Verified false: the tracker reads **no config file** (CLI flags only —
`server.toml.example` has no `[tracker]` section and `dist/init/` ships no tracker
unit template), its default is `0.0.0.0:3130`, and it never touches :3129 (that is
the server's mirror router). The design doc's statement stands as written.

### Evidence

- Tracker default: `crates/monopsony-tracker/src/main.rs:1240-1242`
  ```rust
  /// Address to bind (default 0.0.0.0:3130)
  #[arg(long, default_value = "0.0.0.0:3130")]
  addr: SocketAddr,
  ```
- Dashboard default: `crates/monopsony-server/src/main.rs:49-51`
  ```rust
  /// Address for the admin dashboard API
  #[arg(long, default_value = "0.0.0.0:3130")]
  dashboard_addr: String,
  ```
- Collision failure UX is a **context-free** exit: the tracker bind is a bare `?`
  (`crates/monopsony-tracker/src/main.rs:1482`:
  `let listener = tokio::net::TcpListener::bind(cli.addr).await?;`) — the operator
  sees `Address already in use (os error 98)` with neither address nor likely holder
  named. Contrast the landed client-side prior art (DEF-1 fix),
  `crates/monopsony-client/src/proxy.rs:1079-1095`, which names the address, the
  likely holders ("a second monopsony-client daemon, or a local monopsony-server
  whose mirror router defaults to the same port"), and the remedy.
- Shipped example compose co-hosts the collision: `docker-compose.yml:70-82`
  (commented tracker block) maps `3130:3130` on the host while the server block
  publishes 3128/50051/9090/3129 (lines 42-50) — publishing the dashboard later, or
  running server+tracker on one VM with all defaults, collides.
- `docs/container-deploy.md:178` hardcodes the collision port in prose: "The tracker
  runs on port 3130."
- `:3132` is referenced nowhere as a bound port anywhere in `crates/`, `dist/`, or
  `docs/` (verified 2026-11-14; the design doc §2.2/§3.2 already uses :3132 as its
  example tracker port).

### Real collision surface

The tracker is a **separate binary**; the server never spawns it. Collision requires
co-hosting server + tracker on one host/network namespace — exactly the posture-(b)
"full server on one VM" shape the hosting design targets, and exactly what
`docker-compose.yml` invites. The LAN homelab default (posture (a), no tracker) is
unaffected. Failure is loud (startup exit), not silent — that caps this at WARNING,
not CRITICAL.

**Severity: WARNING** (default-vs-default co-host fails at startup with a
non-self-diagnosing error; workable today only by remembering to pass `--addr`).

### Options

1. **Distinct default port `0.0.0.0:3132` — RECOMMENDED.** Makes the naive co-host
   work by construction instead of failing diagnosably; aligns the default with the
   port the hosting design already documents for posture (b)/(c); :3132 is free.
   Upgrade hazard to name in CHANGELOG: an operator re-launching a tracker that
   relied on the old default moves to :3132, so announce targets break unless
   `--addr` is pinned or consumers were given an explicit `--tracker-url` (servers
   always pass `--tracker-url` explicitly — `main.rs:714-729`, `Option<String>`, no
   baked-in default — so the break surface is the tracker process itself, not fleet
   clients).
2. Startup collision self-diagnosis (named AddrInUse error on the tracker bind).
   Good hygiene, prior art is copy-shaped (~15 lines), but it leaves the naive
   co-host *broken-then-explained* rather than working. Worth bundling as a rider
   since Finding 1's edit touches that bind line's neighborhood anyway — fixes the
   context-free error for custom overrides too.
3. Config validation warning — rejected: the tracker has no config file to validate,
   and no single process sees both binds, so "validation" degenerates into docs.

**Pick: option 1**, with the option-2 rider on the same bind site. Rationale:
defaults are the contract for the naive deployment this project targets; the design
doc's §1.5-2 already concluded "co-hosting requires moving one" — moving it *in the
default* is the only variant that requires nothing from the operator.

**Effort SWAG:** S (one `default_value` string + `docs/container-deploy.md:178` +
compose comment + CHANGELOG entry + a clap-default test); ~0.5 d with the
AddrInUse rider.

**Test plan:**
- Unit: assert clap default parses to `0.0.0.0:3132` (default-value literal test in
  the tracker crate, same shape as existing CLI tests).
- Rider integration: bind the addr once, then run the bind helper again → error text
  contains "address already in use", names the address, and names the tracker as the
  likely second process.
- Grep gate: zero remaining tracker-:3130 references in `dist/` and
  `docs/container-deploy.md` (roadmap history at `docs/sprint-roadmap.md:100` is an
  immutable historical record — leave it).
- `docker compose config` still validates after the comment/port edits.

---

## Finding 2 — Mirror file serving buffers whole files in RAM (both paths)

The design doc flagged the local-hit path (§1.5-3). Verification shows the
**upstream-miss path is the bigger spike**: it collects the entire upstream response
before a size check.

### Evidence

- Local hit, `crates/monopsony-mirror/src/mirror.rs:1461-1477` (`serve_mirror_file`):
  ```rust
  match tokio::fs::read(&canonical_file).await {
      Ok(data) => {
          ...
          .header("Content-Length", data.len())
          .body(axum::body::Body::from(data))
  ```
  Whole file in a `Vec<u8>` before the response exists. Confirmed as the doc
  described.
- Upstream miss, `mirror.rs:1533-1542` (`proxy_to_upstream`):
  ```rust
  let bytes = body.collect().await.unwrap_or_default().to_bytes();
  // Enforce max body size (4 GB for mirror files)
  const MAX_MIRROR_PROXY_SIZE: usize = 4 * 1024 * 1024 * 1024;
  if bytes.len() > MAX_MIRROR_PROXY_SIZE { ... 502 ... }
  ```
  The full upstream body is buffered **before** the cap fires — a 4 GiB upstream
  fetch allocates up to 4 GiB of RAM to then be rejected. First-time fetches of
  large artifacts (any cold `/:distro/*path` GET) take this path.
- The metadata signature verify path needs full bytes only for small metadata docs
  (`mirror.rs:1544-1553` → `verify_metadata_before_serve`, `mirror.rs:1580+`;
  InRelease/Release/APKINDEX/db files — KB to low MB). Sync-time verified copies
  bypass serve-path verification via the hit path — the trust anchor is sync, not
  serve.

### What sizes actually flow

Repo metadata (KB–MB); language artifacts / Homebrew bottles ~1–30 MB typical with
>100 MB outliers; distro ISOs and bundle artifacts multi-GB. Public serving
(postures (b)/(c)) means **per-request memory = file size**, so a 10-request burst
of 2 GB files is ~20 GB RSS — an OOM under entirely normal mirror traffic.

**Severity: WARNING on LAN; CRITICAL for public posture** (linear memory
amplification under concurrency → origin OOM; production-posture blocker per the
design doc).

### Does the stack support streaming on this path?

**Yes, with in-repo prior art.** axum 0.7 + hyper 1.x (root `Cargo.toml:27,30`):
`Body::from_stream` is shipped in six-plus sites — `oci_registry.rs:1345` and a
*range-request* stream at `:1369`, `brew_shim.rs:400,585`, `proxy.rs:1053,1250`,
`docker.rs:307`, client `proxy.rs:622`. `monopsony-mirror` already depends on
`http-body-util` (`Cargo.toml:21`); adding `tokio-stream` (workspace dep, root
`Cargo.toml:88`) to the crate is the only Cargo change.

### Options

1. **Stream both paths with a bounded buffer — RECOMMENDED.**
   - Hit: `tokio::fs::File::open` → `tokio_stream::wrappers::ReaderStream`
     (64 KiB chunks) → `Body::from_stream`; `Content-Length` from
     `file.metadata().await.len()` so the hit path keeps an exact length header
     instead of `transfer-encoding: chunked`.
   - Miss: hyper body `into_data_stream()` → `Body::from_stream`; keep the existing
     header passthrough minus `transfer-encoding`; pass upstream `Content-Length`
     through when present. Replace the post-collect 4 GiB check with a **counted
     stream** that aborts with 502 past the cap.
   - Signature verify: buffer **only** when the path is a known metadata document
     (InRelease/Release/Release.gpg/APKINDEX/`*db.tar.gz`/Packages*) **and** ≤ ~2 MiB;
     everything else streams — consistent with the shipped trust model where sync
     verification, not serve-time buffering, is the anchor (`mirror.rs:1544-1546`).
   - Out of scope, note only: Range requests are unsupported on this handler today;
     streaming does not add them (OCI registry's range stream at `:1369` is prior art
     if ever needed).
2. Size-cap guard (413 above N MB) — rejected as the primary fix: it refuses the
   real multi-GB artifacts mirrors exist to serve ("the pool must always grow");
   acceptable only as an emergency stopgap.
3. Status quo — rejected: public multi-GB serving stays an OOM vector.

**Effort SWAG:** M (1–2 d: two handlers + Cargo.toml + tests; header-semantics
parity is the actual risk, not the streaming itself).

**Test plan (monopsony-mirror tests; memory-bound asserts are flaky, so gate on
behavior + a grep):**
1. Hit path, 8 MiB temp file → 200, byte-exact body, `Content-Length` exact,
   `X-Mirror: hit`.
2. Hit path, 0-byte file → 200 + `Content-Length: 0` (zero-byte trap class —
   cf. CacheStorage empty-object precedent).
3. Miss path against a local hyper mock upstream that streams in 4 chunks → byte-
   exact body, `X-Mirror: miss`, upstream `Content-Length` passthrough preserved,
   response not buffered (assert the mock's chunks are consumed incrementally via a
   channel signal before the upstream finishes, or rely on the grep gate below).
4. Pinned-keys metadata doc with a bad signature → blocked response (verify path
   still buffers and still enforces); oversized "metadata" (> 2 MiB) → streams
   unbuffered.
5. Upstream larger than the cap → 502 without proportional RSS (counted-stream
   abort).
6. Existing path-traversal and SSRF tests stay green.
7. Grep gate: no `fs::read` / `collect().to_bytes()` left in `serve_mirror_file` /
   `proxy_to_upstream`.

---

## Finding 3 — Cloudflare gRPC toggle is useless for the monopsony control plane

### Evidence

- The gRPC listener is TLS-terminated **at the server** with client-cert auth on
  every connection: `crates/monopsony-control/src/grpc.rs:2446` ("gRPC server
  starting (mTLS required)"), `:2478-2479` rustls server config with revocation
  checking, `:2480-2486` `TlsAcceptor` + **`http2_only()`** connection builder, and
  the accept loop wraps every accepted TCP stream in TLS (`:2500+`).
- Cloudflare terminates TLS at the edge for all proxied traffic, so the CF→origin
  hop has CF as the only TLS client: no client certificate reaches :50051, and the
  rustls acceptor rejects the handshake. CF's Network→gRPC toggle (Free tier) only
  matters where the origin speaks gRPC without needing client certs; CF additionally
  documents that public-hostname Cloudflare Tunnels do not support gRPC at all
  (hosting doc §2.7, sources cited there).
- h2c: **zero** occurrences in `crates/` (verified) — there is no cleartext-h2 serving
  mode, and no monopsony-side gRPC toggle exists to remove; the "toggle" is a
  Cloudflare dashboard setting, not a repo surface.

### Does any deployment shape benefit?

No. Posture (a) has no CF in the path. Postures (b)/(c) die at the TLS hop
regardless of the toggle **and regardless of h2c**: terminating TLS strips the
client certificate, which *is* the control plane's auth model — an h2c upgrade
would swap the authenticated peer from "the fleet client" to "Cloudflare" and buy
nothing. The shipped, correct envelope is `/ws/tunnel`, which carries the full mTLS
handshake as opaque WS frames to the untouched :50051 accept path.

**Severity: INFO** (guidance debt; the failure mode is a loud client-side TLS
handshake failure, no silent breakage).

### Options

1. **Document-as-unsupported — RECOMMENDED.** One short section in
   `docs/container-deploy.md` + `docs/bare-metal-install.md` and a line in the
   `server.toml.example` `[grpc] ws_url` comment block: "Cloudflare Network→gRPC
   toggle: leave off — monopsony's control plane requires client certificates at the
   TLS accept path, which CF termination strips; use `wss://…/ws/tunnel`." Pair it
   with the S8 note below (CF 100 MB upload cap → keep :3131 off CF) so the CF
   posture has one authoritative passage.
2. h2c upgrade support — rejected: does not restore client-cert identity; adds a
   second serving mode (`http2_only()` vs cleartext h2) to maintain and a new
   unauthenticated-by-cert listener shape to harden.
3. Remove the toggle — not applicable: nothing in-repo exists to remove.

**Effort SWAG:** XS (< 1 h, docs only).

**Test plan:** none (docs). Grep gate that the note exists in both doc files.

---

## Sweep — remaining §-level findings from the hosting design

| # | Finding | Evidence | Severity | Recommendation | Effort | 1.1 train |
|---|---------|----------|----------|----------------|--------|-----------|
| S1 | Tracker endpoint advertisement broken (bind-derived URL, hardcoded port, wrong `/mirror/` prefix) | **RESOLVED** — `[mirror] public_base_url` landed: `announce.rs:69-72` field, `:157` normalized base, `:235` skip-when-unset warn, test `:428-441`; documented `server.toml.example:117-124`. Posture (b)'s "one code change needed" is done. | — | Close out; no action. | — | — |
| S2 | `docker_routes()` unwired (`/v2` pull-through has no mount call site) | `docker.rs:60-66` def + doc example only; **owned**: mount-or-cut scheduled at Sprint 25 S5 (`docs/sprint-roadmap.md:403,609-626` — needs SSRF gate on the client-controlled registry-host segment + Docker Hub token-dance spike) | INFO | No action this round; inventory note stands (route `/v2` → :3131). | per S5 plan | waits (S25) |
| S3 | XFF leftmost extraction spoofable behind CF (CF preserves + appends attacker XFF) | `auth.rs:648-657` leftmost; hosting doc §2.4 | WARNING | Operator-config remediation is documented and sufficient (final proxy overwrites XFF from `CF-Connecting-IP`; `trusted_proxies` = final proxy only). Put that config into the install docs as mandatory for posture (b). Code fix (shared real-IP helper + direct-peer-aware mirror limiter) waits. | docs XS; code M | docs → 1.1.0; code → 1.1.1+ |
| S4 | `POST /:distro/sync` is unauthenticated when no private mirror exists | handler `mirror.rs:1407-1420`; key required only when `any_private` (`main.rs:990-992`); rate-limit keyed on direct peer `mirror.rs:2479-2493` (collapses to one global bucket behind CF — caps abuse but also caps legit triggers) | WARNING | Require `X-Monopsony-Key` for sync **triggers** regardless of mirror privacy (GET file serving stays public). Bandwidth-abuse vector on origin + upstream. | S | 1.1.1 |
| S5 | TOML bind keys are documentation-only (§1.1 trap) — operators set `[dashboard] listen_address` etc. and observe no effect; binds are CLI-flag-only | `server.toml.example` bind keys vs `main.rs` one-off section reads; hosting doc §1.1 | WARNING | Startup warn when bind-shaped keys are present in parsed config (S) + installer/init templates emit CLI flags (design doc ordering item 4, M). | S warn / M installer | warn → 1.1.0 (optional); installer → later |
| S6 | `[tls] access_control` CIDRs read but not enforced | `grpc.rs:2458` `let _ = access_control; // wired in future: tonic interceptor` | WARNING | Knob-without-enforcement class (same lesson as the hash_policy defect): a knob that exists without an enforcement site is documentation, not a control. Warn at startup when configured ("not yet enforced") now; wire the tonic interceptor as its own slice. | S warn / M wire | warn → 1.1.1; wire → later |
| S7 | **NEW — compose tracker block is broken**: passes `--database-url postgres://…` which is not a tracker flag | `docker-compose.yml:76` vs tracker CLI (`--db` SQLite path, `main.rs:1244-1245`; `rusqlite::Connection::open` `:1399-1402`) — uncommenting fails at clap arg parse; `:78` also maps host :3130 (Finding 1) | WARNING | Fix the block: `command: ["--db", "/data/tracker.db", "--addr", "0.0.0.0:3132"]`, add a data volume, map `3132:3132`. | XS | 1.1.0 (same train as Finding 1) |
| S8 | CF request-body cap (100 MB Free/Pro) breaks OCI pushes | hosting doc §2.7 | INFO | Keep :3131 off CF; fold into Finding 3's docs passage. | XS (docs) | 1.1.0 (docs train) |
| S9 | Path-routing the mirror router is prefix-fragile (distro slug vs route-prefix collisions) | hosting doc §2.2 caveat | INFO | Design already recommends subdomain-per-surface; no action beyond the docs. | — | — |
| S10 | Six open operator questions (hosting doc §6: mirror URL shape, tracker visibility, legacy-code pairing, rate-limit accounting, cloudflared vs open-443, `always_require_login` on tailnet) | hosting doc §6 | INFO | Still open; defaults assumed there remain the working answers. Needs operator decisions, not code. | — | — |

---

## Sequencing vs the pending 1.1.0 release

The 1.1.0 bump is staged (unsigned) in the working tree. Rule applied: **nothing
that needs a test campaign rides the train** — the train carries the version bump
plus XS/S changes with self-contained verification.

**Rides 1.1.0:**
1. Finding 1 — tracker default → `0.0.0.0:3132` + AddrInUse named-error rider +
   CHANGELOG entry + `docs/container-deploy.md:178` fix (S).
2. S7 — compose tracker block fix, same commit family (XS).
3. Finding 3 + S8 — CF gRPC-unsupported / OCI-off-CF docs passage (XS).
4. S3 docs-only half — the mandatory CF-Connecting-IP XFF rewrite section for
   posture-(b) installs (XS).
5. S5 startup warn for TOML bind keys — *optional*; drop from the train without
   hesitation if timing slips (S). **DEFERRED to 1.1.1+ (rider verdict,
   2026-11-14):** not genuinely XS — the server binary has no full
   `ServerConfig` consumer (§1.1) and no existing validation path sees the
   bind keys, so the warn would need a NEW raw-TOML presence-check parse
   site in server `main.rs` (~30 lines + tests) rather than a hook into an
   existing path; and every key it would warn about ships UNCOMMENTED in
   `server.toml.example`, so stock installs would warn on every startup —
   that always-on-noise decision deserves its own slice.
6. S4, S6 warns — only if review bandwidth allows; both are behavior-adjacent and
   fine to defer.

**Waits for 1.1.1:**
1. Finding 2 — mirror streaming (M). Highest user-facing value of the set, but
   multi-GB serving behavior deserves its own test pass plus a bench run
   (large-file serving + concurrency) rather than a release-eve landing. LAN
   posture is unaffected today, so deferring costs little.
2. S4 — sync-trigger key requirement (S; behavior change → CHANGELOG).
3. S6 — access_control warn (S), then the interceptor wire (M) as its own slice.
4. S3 code half — shared real-IP helper + direct-peer-aware mirror limiter (M).
5. S2 — stays with Sprint 25 S5 (mount-or-cut), unchanged by this triage.

**Verification note (what this doc checked against the tree):** tracker CLI
defaults and bind site; server CLI defaults; mirror hit/miss buffering paths; axum/
hyper workspace versions + streaming prior art; monopsony-mirror Cargo deps;
announce `public_base_url` landed state; docker.rs call sites + roadmap ownership;
grpc.rs TLS/http2 accept path + `access_control` stub; absence of h2c anywhere;
compose tracker block flags; `dist/init/` template inventory; `:3132` availability.

---

## Post-implementation: updated 1.1.0 stage list

Rides 1–4 landed on the bump tree (uncommitted) on 2026-11-14: Finding 1 +
AddrInUse rider + S7 compose fix, Finding 3 + S8 docs, S3 docs half. S5
deferred (verdict on the rides list above); S4/S6 deferred per this doc.
This section REPLACES step 1's path list + diffstat expectation in
`context/version-bump-1.1.0.md` §7 (morning command block): the operator's
`git add` must now include the rider paths below. No bump file was touched
beyond Finding 1's scope; CHANGELOG.md carries the rider's two bullets
INSIDE the existing `[1.1.0]` entry.

### Extra paths the bump commit's `git add` must include

```
crates/monopsony-tracker/src/main.rs              (rider code: 3132 default + bind_tracker_listener AddrInUse rider + 2 tests)
crates/monopsony-tracker/README.md                (tracker CLI default doc ×3)
docker-compose.yml                                (S7: tracker block --db + 3132 + tracker-data volume)
dist/server.toml.example                          ([grpc] ws_url CF-gRPC-off comment block)
docs/container-deploy.md                          (:178 port fix + new CF gRPC/OCI/XFF section)
docs/bare-metal-install.md                        (CF Tunnel Notes expansion + mandatory XFF rewrite section)
README.md                                         (architecture diagram :3130 → :3132)
docs/plans/2026-11-14-hosting-roadmap-triage.md   (NEW untracked file — include if the operator wants this doc in the commit; otherwise it stays untracked)
```

`CHANGELOG.md` is already on the bump list — the rider adds 2 bullets to the
same `[1.1.0]` entry (Changed: port move + upgrade hazard + bind rider;
Fixed: compose tracker block), so no separate staging line is needed.

### Updated diffstat expectation (rider-only deltas; bump files unchanged)

| File | Rider delta |
|------|-------------|
| `crates/monopsony-tracker/src/main.rs` | +67 / −2 (default literal, `bind_tracker_listener` helper + AddrInUse rider, 2 new tests) |
| `crates/monopsony-tracker/README.md` | 3 lines updated (port ×3; custom-bind example now `127.0.0.1:3132`) |
| `README.md` | 1 line (`:3130` → `:3132` in the architecture diagram) |
| `docker-compose.yml` | +10 / −4 (tracker block rewrite + `tracker-data` volume) |
| `dist/server.toml.example` | +6 comment lines (`[grpc]` ws_url block) |
| `docs/container-deploy.md` | +34 / −1 (tracker line fix + new Cloudflare section) |
| `docs/bare-metal-install.md` | +33 / −4 (XFF forwarding note, CF Tunnel Notes expansion, mandatory XFF rewrite section) |
| `CHANGELOG.md` | +2 bullets inside the existing 1.1.0 entry (cumulative stat vs HEAD is +32 because the bump's ~30-line 1.1.0 entry sits in the same uncommitted diff) |

`git diff --stat` over the eight rider paths reads **185 insertions / 18
deletions** cumulative vs HEAD — of which the bump itself contributes only
the CHANGELOG entry (~30 lines); everything else above is rider.

### Rider gate results (2026-11-14)

| Gate | Result |
|------|--------|
| `cargo test --workspace --locked` | RC=0 — **1582 passed / 0 failed / 3 ignored** (aggregate of all 30 `^test result:` lines; log `tmp/rider-gates/test.log`). Bump-prep baseline 1580 + the 2 new tracker tests = 1582, zero failures |
| `cargo clippy --workspace --all-targets` | **15 warnings = exact pre-existing baseline set** (pairing.rs ×6, grpc_client.rs ×4, shadow_executor.rs ×2, trust.rs ×1, control/grpc.rs ×1, tls.rs ×1) — zero new warnings; the tracker crate lints clean |
| `docker compose config` | RC=0 — the rewritten tracker block + new volume validate |
| `:3130` grep gate | zero live tracker-:3130 defaults remain — classification below |
| rc2 version-claim invariant | intact — `grep -c '1\.0\.0-rc2' Cargo.lock` = 0; rpm spec `Version:`, PKGBUILD `pkgver`, deb control `Version:`, FreeBSD `PORTVERSION`, `dist/install.sh` `VERSION_FALLBACK` all at 1.1.0 (the rider touched none of the bump's files) |

Remaining `:3130` references, classified (each with its reason):

| Location | Reason |
|----------|--------|
| `crates/monopsony-server/src/main.rs:50,176-177` | the DASHBOARD's default — the other side of Finding 1's collision; intentionally unchanged |
| `crates/monopsony-server/src/main.rs:1576-1585` | login-request test fixtures on the dashboard port |
| `crates/monopsony-client/src/main.rs:3037,5504-5506` | doc comment + URL-parse fixtures naming the dashboard/pairing port |
| `crates/monopsony-common/src/config.rs:1118` | doc example URL on the dashboard port |
| `crates/monopsony-control/src/tracker.rs:265-272` | TrackerClient URL-normalization test fixtures; the client takes `base_url` explicitly and servers always pass `--tracker-url` (server main.rs:714-729, `Option<String>`, no baked-in default) — no default surface exists |
| `dist/install.sh:1429,2136,2328` + `dist/server.toml.example:459-461,486` | dashboard refs (installer flag string, CF-tunnel prose, final-routing hint; `[dashboard]` section defaults + admin-API rate-limit prose) |
| `docs/bare-metal-install.md` + `docs/pairing-security.md` | dashboard/pairing port prose |
| `docs/sprint-roadmap.md:100` | immutable historical record (and gitignored) — left per this doc's grep gate |
| `docs/plans/*` (multiple) | historical plan/bench records, not current-state docs |
| `docs/research/public-mirror-network.md` | Phase 1–3 research/design record — describes the tracker as it shipped in Phase 2; historical |
| tracker `main.rs`, `docker-compose.yml`, `docs/container-deploy.md` (post-edit) | the migration-note mentions themselves ("moved from :3130") — intentional |
