# Slice 1 (wave 1) — monopsony-common: version normalizer + vercmp

## Read first (ONLY these)
1. docs/plans/sprint20-phase4/context-pack-common.md

## File set
- crates/monopsony-common/src/version_ord.rs (NEW)
- crates/monopsony-common/src/lib.rs (one `pub mod version_ord;` line)
- crates/monopsony-common/Cargo.toml (NO new deps — std only)

## Design
Pure functions, no I/O, heavily tested. Reference semantics: alpm-version vercmp
(epoch:ver-rel) and Debian version comparison notes; implement, don't import.

### API
```rust
/// Normalize a raw version string: strip leading `v`/`V`, strip pacman `-rN`
/// release suffix into a separate release component, keep epoch.
pub fn normalize(raw: &str) -> NormalizedVersion { epoch: u64, version: String, release: u64 }

/// Total-order compare: epoch, then version (Debian-style chunk compare —
/// alternating non-digit/digit chunks; tilde `~` sorts BEFORE everything
/// including empty), then release.
pub fn vercmp(a: &NormalizedVersion, b: &NormalizedVersion) -> std::cmp::Ordering;

/// Sortable canonical string for the ledger's version_ord column:
/// format!("{:016}:{ver}:{rel:06}", epoch, ...) — display/debug only;
/// ordering decisions always go through vercmp.
pub fn order_key(v: &NormalizedVersion) -> String;
```

### Rules (bake these; tests enforce)
- Epoch: leading `N:` where N is digits → numeric epoch (default 0).
- Pacman release: trailing `-rN` → release=N. Other distros' `-1` style
  trailing `-N` (all digits) also counts as release. Leading `-` never.
- `v`-prefix strip: only when followed by a digit.
- `+reallyX.Y` prefixes: strip the `+really` marker (keep the real version).
- Unknown/messy input never panics: unparseable chars compare as their byte
  runs (Debian rule: non-digit chunks compare byte-wise with `~` special-cased).

### Tests (in-file #[cfg(test)], ≥12)
- epoch ordering beats version: "1:0.9" > "2.0"
- tilde: "1.0~rc1" < "1.0"
- pacman: "5.2.37-1" vs "5.2.38-1" release handling; "-r7" form
- v-prefix: "v2.3.0" == "2.3.0"
- +really: "1.2+really1.3" normalizes sanely (document choice)
- numeric vs lexicographic chunks: "1.10" > "1.9"; "1.001" == "1.1"
- empty/garbage inputs return cleanly
- order_key round-trips format

## Do NOT
- Do not touch config.rs, client files, or any PM backend.

## Verify (ONCE)
`cargo check -p monopsony-common && cargo test -p monopsony-common version_ord -- --nocapture`
