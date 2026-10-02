# Slice 3 — Client wiring + admin list + retention + dashboard (integration slice)

**Read first:** `docs/plans/sprint26-phase1-homebrew/context-pack-common.md`.

**Scope:** four small edits, no new files except tests. Each is independently
verifiable. This slice touches 4 crates lightly — verify each with per-crate
commands, workspace last.

## 3a. Client: `parse_lang_pm` gains brew

`crates/monopsony-client/src/lang_pm_config.rs` L16:
```rust
"brew" => Some(PackageManagerKind::Homebrew),
```
Add tests: `parse_lang_pm("brew")` → Some(Homebrew); `parse_lang_pm("BREW")` →
Some (case-insensitive); existing tests untouched. Everything else in the
client is generic (status/apply dispatch through `get_backend`) — zero
main.rs edits expected; if `run_lang_pm_command` hardcodes the 3-PM list
anywhere, extend it there (it doesn't — grounded: dispatch is via
parse_lang_pm → get_backend).

## 3b. Admin: SUPPORTED_LANGUAGE_PMS → 4

`crates/monopsony-admin/src/language_pms.rs` L28:
```rust
pub const SUPPORTED_LANGUAGE_PMS: [&str; 4] = ["npm", "pip", "cargo", "brew"];
```
Update the pinned-shape test (`default_map_serializes_to_pinned_shape`) to
include brew. Verify: `cargo test -p monopsony-admin 2>&1 | tail -3`.

## 3c. Retention: parse_brew_path

`crates/monopsony-proxy/src/retention.rs` in `parse_artifact_path` (~L76):
```rust
"brew" => parse_brew_path(&segments),
```
```rust
/// brew bottles: `["brew", "bottles", <name>, <version>, "<file>.tar.gz"]`.
fn parse_brew_path(segments: &[&str]) -> Option<(String, String)> {
    if segments.len() < 5 || segments[1] != "bottles" {
        return None; // api/, v2/ shapes = metadata, not candidates
    }
    Some((segments[2].to_string(), segments[3].to_string()))
}
```
Tests:
- `brew/bottles/jq/1.8.2/jq-1.8.2.arm64_tahoe.bottle.1.tar.gz` →
  ("jq", "1.8.2")
- `brew/api/formula/jq.json` → None
- `brew/v2/homebrew/core/jq/manifests/1.8.2-1` → None
Verify: `cargo test -p monopsony-proxy retention 2>&1 | tail -3`.

## 3c-2. Scrubber + CVE ecosystem

- `crates/monopsony-proxy/src/scrubber.rs`: no walk-prefix change needed —
  verify by reading how the janitor walk enumerates candidates (the known
  scrubber gap about `.cache-meta` noise is pre-existing, out of scope).
  ONLY IF the scrub walk has a hard-coded ecosystem list does this need an
  edit — check and report; default is no-op.
- `crates/monopsony-proxy/src/scan_cve.rs` `infer_ecosystem`: add
  `else if path_str.contains("brew") { "Homebrew" }` arm before the fallback.
  Test: a `brew/bottles/...` path infers "Homebrew".

## 3d. Dashboard: LanguagePmPanel brew row

`dashboard/src/lib/components/LanguagePmPanel.svelte`:
- L5 `type PmName = 'npm' | 'pip' | 'cargo' | 'brew';`
- L10 `const PMS: readonly PmName[] = ['npm', 'pip', 'cargo', 'brew'];`
- Panel description text mentions Homebrew bottles + API caching.
Verify: `cd dashboard && npx svelte-check --output human 2>&1 | tail -5` (0
errors, 0 warnings — a11y clean per convention).

## 3e. Docs touch

`docs/TODO-planned-backends.md` (if exists — check): mark Homebrew
implemented. AGENTS.md test-count line: do NOT update (hub does after the
workspace run). monopsony-site updates are OUT of scope (user-run, post-merge).

## Verify (order matters — cheapest first)

```
cargo test -p monopsony-client lang_pm 2>&1 | tail -3
cargo test -p monopsony-admin 2>&1 | tail -3
cargo test -p monopsony-proxy retention 2>&1 | tail -1
cargo test -p monopsony-proxy scan_cve 2>&1 | per-filter tail
cd dashboard && npx svelte-check 2>&1 | tail -3
```

Final workspace gate is HUB-run: `cargo test --workspace` +
`cargo clippy --workspace --all-targets` zero errors + `cargo fmt --check`.
