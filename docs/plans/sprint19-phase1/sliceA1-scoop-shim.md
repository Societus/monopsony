# Slice A1 — scoop bucket shim (validate-the-pattern slice)

FIRST read `wave-ab-context-pack.md`, then this. Smallest shim — its shape is
the template A2/A3 copy.

## Task
Server-side scoop bucket server: serve bucket JSON from
`http://<server>:3129/scoop/<bucket>/` with `url:`/`hash:` fields rewritten to
Monopsony fetch paths, so `scoop bucket add monopsony http://...:3129/scoop/main`
gives Windows clients cached installs.

## Files (yours ONLY)
1. crates/monopsony-mirror/src/scoop_shim.rs (NEW — all logic + tests)
2. crates/monopsony-mirror/src/lib.rs (add `pub mod scoop_shim;` — one line)
3. crates/monopsony-common/src/package_manager/scoop.rs (configure action)

## Design
- `pub fn router(state: MirrorState) -> axum::Router` serving:
  - `GET /:bucket/*path` → bucket JSON files from `<mirror_dir>/scoop/bucket/<bucket>/<path>`
    (plain file serving, same pattern as serve_mirror_file — read it first).
  - `GET /:bucket` → bucket root manifest if present else 404.
- Rewriting happens at SYNC time (not serve time — serve stays dumb/static):
  `pub async fn sync(state: &MirrorState, bucket: &str, upstream: &str) -> anyhow::Result<()>`
  fetches the upstream bucket git-dir listing or JSON per upstream layout
  (scoop buckets are git repos of .json files). For v1: support the common
  case — a bucket URL whose files are fetchable via raw https (e.g. GitHub
  repo). Write each .json under the storage path, with TWO fields rewritten
  in the JSON: `url` and any `url.<arch>` keys →
  `http://<proxy_addr_configurable>/scoop-fetch/<bucket>/<pkg-file-name>`
  — v1 SIMPLEST: rewrite to `http://<mirror-host>:3128/<original-upstream-url>`
  (absolute-URI through the caching proxy — this is the EXISTING cache path,
  zero new machinery; the URL is the cache key). `hash` left as-is (Phase 3
  pins it).
  The mirror host:port for rewrites comes from a new field on MirrorState or a
  param — check MirrorState struct; if it lacks a public addr, add
  `pub proxy_base: String` (default `http://127.0.0.1:3128`) set at
  construction; sync() uses it.
- SSRF: validate upstream URL host via crate::url_validation::validate_url_host
  before any fetch. Private IPs rejected (mirror crate has no bypass flag —
  keep it that way).
- Tests (in scoop_shim.rs #[cfg(test)]): rewrite logic unit tests with fixture
  JSON (url rewritten, hash untouched, arch-variant urls rewritten, non-url
  fields untouched); router test using axum's oneshot (Router + tower ServiceExt)
  serving a temp-dir-backed file; sync test optional if network-hermetic
  impossible — skip and note.
- lib.rs mod line. NO edits to mirror.rs/create_mirror_router (hub wires).

## scoop.rs configure action (monopsony-common)
In proxy_config: after existing actions, append
`RunCommand { program: "powershell", args: ["-NoProfile","-Command", &format!("if (scoop bucket list | Select-String -Quiet '^monopsony') {{ scoop bucket remove monopsony }}; scoop bucket add monopsony http://{proxy_addr_mirror}:3129/scoop/main")] }`
— proxy_addr param is the SERVER addr string; derive mirror base by replacing
:3128 with :3129 if present, else use as-is. Windows-only in practice; keep
the action unconditional (client executes on Windows where scoop exists).
remove_proxy_config: RunCommand removing bucket `monopsony` (best-effort,
`|| exit 0` semantics via powershell `-Command "...; exit 0"`).

## Verify ONCE
cargo test -p monopsony-mirror 2>&1 | tail -4 && cargo check -p monopsony-common 2>&1 | tail -2
Baseline monopsony-mirror tests must all still pass + your new ones.

Report: files_changed / verify tail / not_done / deviations. NO COMMITS.
