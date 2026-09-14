# Wave A/B context pack — shared facts (read ONCE before your slice spec)

## Mission context (one paragraph)
Monopsony = Rust package-cache proxy. Sprint 19 Phase 1 (HTTP CONNECT tunnels)
landed commit b22ebf2; fleet-verified. Now Wave A = Windows PM shims
(server-side metadata sources so winget/choco/scoop work through the cache)
and Wave B = backlog hardening. All shims mount on the MIRROR router (:3129).

## Environment rules (hard)
- HARD 600s cap: work linearly, write immediately, verify ONCE at the end.
- NO COMMITS. Working tree only. Never git add/commit/stash/reset.
- Reading whitelist = your slice spec + the files it names. Nothing else.
- Skip-and-report: out of time → list items under not_done.
- Patch-tool lint errors "async fn not permitted in Rust 2015" = FALSE
  POSITIVES (standalone linter can't see workspace edition 2021). cargo is
  the only arbiter.
- Keep each write_file/patch under ~8K tokens.

## Repo facts
- Workspace at /home/societus/projects/monopsony. axum 0.7, hyper 1, tokio,
  reqwest (mirror crate already deps reqwest+json), sqlx 0.8 postgres.
- Baseline: `cargo test --workspace` = 509 passed, 0 failed. Do not regress.
- Parallel agents are editing OTHER files; if `cargo check` fails on a file
  you don't own, wait 30s, retry once, then report verbatim — don't fix it.

## The shim mount decision (applies to A1/A2/A3 — already made, follow it)
Mirror router (crates/monopsony-mirror/src/mirror.rs `create_mirror_router`)
has catch-all `/:distro/*path`. Shims nest as STATIC prefixes which matchit
prioiritizes over params — no conflict:
```rust
axum::Router::new()
    .route("/", ...)
    .nest("/scoop", scoop_shim::router())          // <- new, BEFORE with_state? No:
    ...
```
CAVEAT: `.nest()` inside create_mirror_router must happen there (state types
differ). The exact integration is ONE hub-owned edit after the agents land —
agents build `pub fn router(state) -> axum::Router` self-contained modules and
unit tests; the hub wires + adds integration tests. Agents do NOT edit
create_mirror_router.

## Shim pattern contract (all three shims identical in shape)
Each shim module (in crates/monopsony-mirror/src/) exposes:
- `pub fn router(state: MirrorState) -> axum::Router` — routes under its
  prefix, self-contained state (no shared middleware; the mirror router's
  auth layer wraps the merged tree at the hub edit).
- A sync function `pub async fn sync(state: &MirrorState) -> anyhow::Result<()>`
  pulling upstream metadata into mirror_dir-backed storage under a reserved
  key prefix (`scoop/`, `choco/`, `winget/`).
- Storage via existing `state.storage` CacheStorage? NO — mirror crate uses
  plain files under `state.mirror_dir` (check MirrorState fields in mirror.rs
  first ~80 lines). Follow the mirror file-serving pattern exactly.
- Upstream fetches via `reqwest` (already a dep). ALL https upstreams.
- SSRF: reuse `crate::url_validation::validate_url_host` for any
  user/host-derived URL before fetching.

## PM backend configure actions (A-agents also touch monopsony-common)
crates/monopsony-common/src/package_manager/{scoop,choco,winget}.rs are ~100
lines each, trait `PackageManagerBackend::proxy_config(&self, proxy_addr) ->
ProxyConfigAction`. The Windows-relevant addition per plan §4:
- scoop: `RunCommand` running `scoop bucket add monopsony http://<server>:3129/scoop/main`
  (bucket add is idempotent-ish; check for existing bucket first via list).
- winget: RunCommand `winget source add -n monopsony -a http://<server>:3129/winget/ -t Microsoft.Rest`
  (remove existing source first if present: `winget source remove monopsony`).
- choco: RunCommand `choco source add -n monopsony -s http://<server>:3129/choco/api/v2/`
  (+ `choco source remove monopsony || true` first).
- These run on the CLIENT (Windows) — gate with `#[cfg(windows)]` where the
  existing backends already gate Windows-specific paths; keep proxy_config
  cross-platform returning Multiple(actions).
- get_available_backends() already includes all three — no registry changes.

## Storage keys + content types (per plan + repo convention)
- scoop bucket JSON files: `scoop/bucket/<bucket>/...` content-type
  application/json. `url:`/`hash:` REWRITTEN to `http://<proxy>/...` fetch
  path (see slice spec).
- choco nupkg + metadata: `choco/feed/...`.
- winget manifest json: `winget/manifests/...`.

## Verification (each agent, ONCE, exact command)
cargo test -p monopsony-mirror 2>&1 | tail -4
(+ cargo check -p monopsony-common if you touched it — run BOTH before reporting)

## Report honestly: files_changed, real verify output tail, not_done, deviations.
