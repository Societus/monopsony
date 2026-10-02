# Slice 1 — HomebrewBackend (monopsony-common)

**Read first:** `docs/plans/sprint26-phase1-homebrew/context-pack-common.md`
(shared pack — wire facts, cache classes, code anchors, pitfalls).

**Scope:** ONE new file `crates/monopsony-common/src/package_manager/brew.rs`
plus the 7 registration edits in `mod.rs`. Nothing else.

## brew.rs

`HomebrewBackend` implementing `PackageManagerBackend`:

- `kind()` → `PackageManagerKind::Homebrew`
- `is_available()` → `which::which("brew").is_ok()`
- `list_installed()` → run `brew list --formula --versions` and
  `brew list --cask --versions` (absolute `/opt/homebrew/bin/brew` first, plain
  `brew` fallback — non-interactive PATH quirk; if both fail → empty Vec, not
  an error, matching sibling backends' container behavior). Lines are
  `name version`; strip tap prefixes (`homebrew/cask/`) if present. PackageRef:
  `installed_at: 0`, `manager: PackageManagerKind::Homebrew`.
- `upstream_url_patterns()` → exactly two (from the pack, § URL patterns):
  - `formulae.brew.sh/api/.*` → LangMetadata, priority 100, desc "Homebrew API metadata"
  - `ghcr.io/v2/homebrew/core/[^/]+/manifests/[^/]+$` → LangArtifact, priority 110,
    desc "Homebrew bottle manifests"
- `url_to_cache_path()` → `Some(format!("brew{}", url.path()))` (both shapes).
- `proxy_config(addr)` → `ProxyConfigAction::WriteFile {
    path: PathBuf::from("~/.homebrew/brew.env"),
    content: "HOMEBREW_API_DOMAIN=http://{addr}/brew/api\nHOMEBREW_BOTTLE_DOMAIN=http://{addr}/brew/bottles\n"
  }`
- `remove_proxy_config()` → `RemoveFile { path: "~/.homebrew/brew.env" }`.

## mod.rs registration (7 edits, per the checklist skill)

mod / pub use / enum variant `Homebrew` / as_str "brew" / parse_kind "brew" /
get_all_backends / get_backend. Verify with `cargo check -p monopsony-common`
— exhaustive-match breakage from the new variant surfaces here.

## Tests (in brew.rs, ~10)

- patterns match formula/cask/JWS URLs; reject non-brew URLs (gitlab ghcr
  false-positive check: `https://ghcr.io/v2/some/other/repo/manifests/x`
  must NOT match).
- `url_to_cache_path`: formula JSON → `brew/api/formula/jq.json`; ghcr
  manifest → `brew/v2/homebrew/core/jq/manifests/1.8.2-1`; "not a url" → None.
- `proxy_config`: path + both env lines + `http://` (status-check contract).
- `remove_proxy_config` → RemoveFile on the same path.
- inventory parser unit test: `parse_installed_output("jq 1.8.2\nwget 1.25\")`
  → two PackageRefs (expose the parser as a plain fn to test it without brew).
- kind/availability non-panicking.
- A cask line with tap prefix strips to bare name.

## Verify

```
cargo test -p monopsony-common brew 2>&1 | tail -5
cargo clippy -p monopsony-common --all-targets 2>&1 | grep -cE '^error'   # 0
cargo check --workspace 2>&1 | tail -3                                     # sibling crates compile (exhaustive matches)
