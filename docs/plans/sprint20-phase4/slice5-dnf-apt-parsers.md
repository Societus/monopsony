# Slice 5 (wave 2) — dnf primary.xml.gz + apt InRelease parsers + Valid-Until

## Read first (ONLY these)
1. docs/plans/sprint20-phase4/context-pack-common.md
2. crates/monopsony-client/src/index_parse.rs (wave-1 landed — EXTEND, don't fork)

## File set
- crates/monopsony-client/src/index_parse.rs (extend)
- crates/monopsony-client/Cargo.toml (only if a dep is truly missing; quick-xml
  is NOT in the workspace — prefer hand-rolled tag scanning like the
  winget_shim YAML-subset parser did, or justify the dep)

## Design
### dnf/rocky primary.xml(.gz)
- Decompress gzip → XML. Relevant shape:
  `<metadata ...><package type="rpm"><name>x</name><version epoch="0" ver="1.2" rel="1"/><...><checksum type="sha256">HEX</checksum>`
- Extract (name, `epoch:ver-rel` as version string, sha256). Cap sizes like
  slice 3. Detection: `looks_like_index` gains `.xml.gz` path ending + content
  sniff `<metadata` prefix, OR repomd.xml `<location href="...primary.xml.gz"/>`
  handling stays OUT of scope (the proxy sees the primary.xml fetch itself).
### apt InRelease / Release
- Clearsigned format: first line `-----BEGIN PGP SIGNED MESSAGE-----`,
  header block with `Valid-Until: <RFC1123-ish timestamp>`, blank line, then
  hash blocks (`SHA256:` section listing ` <hash> <size> <path>` lines).
- Extract: Valid-Until (stored where? repo_seen extension — add columns
  `valid_until INTEGER` via ALTER TABLE guard, or a side table; your call, keep
  migrations additive), per-file sha256 for paths ending in `.deb` → these map
  to pool payload URLs (repo_key + path basename matching for slice-4 D
  cross-check extension).
- Detection: path ends `InRelease` or `Release` + content sniff.
### Staleness (L4 native)
- apt: if now > Valid-Until → policy split (reject=503, warn=header
  `X-Monopsony-Index-Expired`) — this is plan §6 "apt Valid-Until native".
- dnf: repomd `expires`/`valid-until` tag if present, same handling; absent →
  max_index_age_secs fallback.

## Verify (ONCE)
`cargo test -p monopsony-client index_parse -- --nocapture` (new tests + wave-1
tests green; fixtures built in-memory)
