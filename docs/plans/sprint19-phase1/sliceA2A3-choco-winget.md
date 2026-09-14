# Slice A2 — choco NuGet v2 feed shim
# Slice A3 — winget Microsoft.Rest source shim

Read `wave-ab-context-pack.md` first. Your slice is named in the dispatch
brief. COPY THE VALIDATED PATTERN: read
crates/monopsony-mirror/src/scoop_shim.rs — it is the template
(pub fn router(MirrorState) -> axum::Router, sync-time rewriting, plain-file
storage under mirror_dir, SSRF via url_validation, unit tests + router
oneshot tests, canonicalize traversal protection).

## A2 — choco (files: NEW choco_shim.rs, lib.rs mod line, choco.rs configure)
Serve a NuGet v2 feed so `choco source add -n monopsony -s http://<server>:3129/choco/api/v2/` works.
- Routes (v2 subset choco actually needs):
  - `GET /api/v2/Packages()` — package list (Atom/XML feed; findPackagesById
    filter param `Id=...&semVerLevel=2.0.0` optional subset)
  - `GET /api/v2/FindPackagesById()?id=<pkg>` — versions feed (XML)
  - `GET /api/v2/package/<id>/<version>` — .nupkg download (serve file,
    application/zip)
  - `GET /api/v2/$metadata` — static EDMX stub (choco tolerates minimal)
- Storage: `<mirror_dir>/choco/feed/<id>/<version>/` holding the .nupkg +
  a sidecar nuspec.json extracted at sync (id, version, title, deps,
  checksums, projectUrl).
- sync(state, package_id, upstream_base, proxy_base): v1 = explicit package
  list (config-driven, not full-catalog): fetch
  `<upstream>/api/v2/Packages()?$filter=(Id eq '<id>')` … OR simpler v1:
  fetch `https://community.chocolatey.org/api/v2/package/<id>/<version>` for
  each (id,version) the operator lists; store nupkg (zip — parse the embedded
  .nuspec via the `zip` crate ONLY IF already a workspace dep — else defer
  nuspec parsing, store raw, and record id/version from the request). SSRF:
  validate upstream host. Nupkg served as-is — CACHED via the url-rewrite
  trick? NO for A2: nupkg is served directly from mirror_dir (it IS the
  cache). Installer script URLs inside non-internalized packages:
  document in module docs as pass-through (no rewrite) — honest caveat from
  the plan.
- XML generation: hand-build the Atom entries with format!/xml escape
  (escape & < > in id/title). No XML crate needed for v1.
- choco.rs configure: RunCommand powershell:
  `if (choco source list -r | Select-String '^monopsony') { choco source remove -n monopsony }; choco source add -n monopsony -s http://<mirror-addr>/choco/api/v2/`
  (derive mirror addr from proxy_addr like scoop.rs does — read its landed
  code and mirror the derivation). remove: `choco source remove -n monopsony; exit 0`.
- Tests: XML feed generation unit tests (correct Id/Version/entry shape,
  escaping), router oneshot (FindPackagesById 200 + content-type, package
  download 200 application/zip, traversal != 200), sync skipped-if-network.

## A3 — winget (files: NEW winget_shim.rs, lib.rs mod line, winget.rs configure)
Serve a `Microsoft.Rest` winget source at `http://<server>:3129/winget/`.
- Routes (winget REST contract, JSON):
  - `GET /winget/` or `/winget/information` — source info JSON:
    {"Data":{"SourceIdentifier":"monopsony","ServerSupportedVersions":["2.0.0"]},"ContinuationToken":null}
  - `GET /winget/manifestSearch` — POST body {"Query":{...},"Inclusions":[...]}
    → matched manifests (search local synced index)
  - `GET /winget/packageManifests/<packageId>` — versions + installers
  - Manifest JSON: winget's REST shape (Packages[].PackageIdentifier,
    Versions[].PackageVersion, Installers[].InstallerUrl/InstallerSha256/
    InstallerType/Architecture). Storage: `<mirror_dir>/winget/manifests/...`
    (synced from the winget-community repo's manifests dir on GitHub —
    same GitHub tree-API sync as scoop_shim; only manifests for a
    configured package list v1, not the whole 8k-package repo).
- Rewriting at sync: InstallerUrl → `<proxy_base>/<original-url>` (absolute-URI
  through the existing cache proxy — same trick as scoop). InstallerSha256
  PRESERVED verbatim (winget verifies it client-side; Phase 3 also pins it —
  include it unchanged in served manifests).
- winget.rs configure: RunCommand powershell:
  `winget source remove monopsony 2>$null; winget source add -n monopsony -a http://<mirror-addr>/winget/ -t Microsoft.Rest`
  (+ `--trust` if winget requires it for http sources — check: it does warn
  on http; add --trust to the add command). remove:
  `winget source remove monopsony; exit 0`.
- Tests: manifest JSON shaping, search filtering, InstallerUrl rewrite +
  Sha256 preserved, router oneshot (information endpoint, manifestSearch 200
  with fixture index, traversal), sync skipped-if-network.

## Shared for both
- Verify ONCE: cargo test -p monopsony-mirror 2>&1 | tail -4 && cargo check -p monopsony-common 2>&1 | tail -2
- Baseline after A1+hub: 21 mirror tests green. NO COMMITS.
- The hub wires your `.nest("/choco"|"/winget", ...)` — do NOT edit mirror.rs.
- Skip-and-report. Deviations allowed where upstream reality differs —
  document in report + module docs.
