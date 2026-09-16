# Slice 5 (wave 2) — dnf primary.xml.gz + apt InRelease parsers + Valid-Until

## Read first (ONLY these)
1. docs/plans/sprint20-phase4/context-pack-common.md
2. crates/monopsony-client/src/index_parse.rs (wave-1 landed — EXTEND, don't fork)
3. crates/monopsony-client/src/proxy.rs lines ~560-870 (pm_kind_from_url,
   wrap_with_ledger_check, ledger_index_verdict_response — two touch points only)
4. crates/monopsony-common/src/version_ord.rs — doc-comment + `normalize`
   signature + `NormalizedVersion { epoch, version, release }` only (do not
   re-derive the algorithm)

## File set (strict)
- crates/monopsony-client/src/index_parse.rs (extend)
- crates/monopsony-client/src/proxy.rs (ONLY: pm_kind_from_url + the
  ParsedIndex match inside wrap_with_ledger_check's spawn_blocking + the
  proxy.rs tests module)
- NO Cargo.toml edits: quick-xml stays OUT (hand-rolled tag scanning, the
  winget_shim YAML-subset parser precedent). flate2 is already a client-crate
  dep — use it for primary.xml.gz.

## Design — index_parse.rs

### dnf/rocky primary.xml(.gz)
- Shape: `<metadata ...><package type="rpm"><name>x</name>...
  <version epoch="0" ver="1.2" rel="1"/>...<checksum type="sha256">HEX</checksum>`.
- Hand-rolled scanning: split the decompressed text on `<package ` (also accept
  `<package>`). Per package record:
  - name = text of the first `<name>...</name>`.
  - version = attributes of the FIRST `<version` tag: `epoch`, `ver`, `rel` →
    string `"{epoch}:{ver}-{rel}"`, DROPPING the epoch prefix when the attr is
    absent or "0" (so ordinary strings like `1.2-3` compare naturally through
    `version_ord::normalize`).
  - sha256 = text of the FIRST `<checksum` element whose `type="sha256"`
    attribute matches; skip other types (sha/md5 occur first in real files);
    lowercase hex. Absent → None.
- `ParsedIndex` gains `Dnf { entries: Vec<IndexEntry> }` (same IndexEntry type).

### apt InRelease / Release
- Clearsigned text: first line `-----BEGIN PGP SIGNED MESSAGE-----`, armor
  header block, blank line, then cleartext with `Origin:`/`Suite:`/`Date:`/
  `Valid-Until:` and hash blocks: `SHA256:` followed by lines
  ` <hash> <size> <path>`. Plain `Release` files have the identical body
  without armor — parse both the same way.
- `ParsedIndex` gains `Apt { entries: Vec<IndexEntry>, valid_until: Option<i64> }`.
  - entries come ONLY from SHA256-block lines whose path ends `.deb`:
    sha256 = the hash field; (name, version) from the basename via the
    pacman-style split (`name_v v r arch.deb` → name = pieces before
    3rd-from-last `-`, version = `ver-rel`). Ambiguous split → SKIP the line,
    never guess. Non-.deb lines ignored (Sources files etc.).
  - valid_until: parse `Valid-Until:` as UTC with chrono, trying
    `%a, %d %b %Y %H:%M:%S UTC` then the same with `GMT` (Debian uses both);
    unparseable → None, never an error.

### Detection (content, never extension — mirror the wave-1 magic rule)
Decode order inside `parse_index`/`decode_container`:
1. zstd/gzip magic → decompress (existing code), then sniff the DECODED bytes.
2. Decoded/raw bytes sniff:
   - trimmed start is `-----BEGIN PGP SIGNED MESSAGE-----` OR the first 4 KB
     contain both `Valid-Until:` and a `SHA256:` block line → apt parser.
   - trimmed start (after optional BOM/whitespace) is `<` and `<metadata`
     appears in the first 512 bytes → dnf parser.
   - else the existing tar path (pacman/apk unchanged).
- Caps: decompressed size through the existing `read_limited`
  MAX_DECOMPRESSED_BYTES; add `MAX_XML_BYTES = 256 MB` applied to the
  decompressed buffer and `MAX_XML_PACKAGES = 100_000` records scanned.
- Garbage XML (unterminated tags) → `IndexError`, never panic (match the
  wave-1 hostile-input contract). Non-UTF-8 text → IndexError.

## Design — proxy.rs (two touch points ONLY)

### 1. `pm_kind_from_url` — add BEFORE the None fallback
- path ends `/primary.xml.gz` or `/primary.xml`, or last segment is
  `primary.xml.gz`/`primary.xml` → `Some("dnf")`
- last segment is `InRelease` or `Release` → `Some("apt")`
  (`Release.gpg` deliberately falls through to None → pass-through; it is a
  signature, not an index).
- The archlinux/.pkg.tar/.db and alpine/APKINDEX checks stay first — zero
  behavior change for pacman/apk.

### 2. `wrap_with_ledger_check`'s spawn_blocking entries match
Currently matches `Pacman`/`Apk` arms. Add:
- `ParsedIndex::Dnf { entries }` → same as the existing arms
  (`entries.clone()`).
- `ParsedIndex::Apt { entries, valid_until }` → `entries.clone()` PLUS
  native staleness: `if let Some(vu) = valid_until`, and `now > vu`, push
  `("apt:valid-until", format!("expired-at={vu}"), format!("now={now}"))`
  onto the downgrades vec — the EXISTING policy split then applies verbatim
  (reject → 503, warn → `X-Monopsony-Downgrade` header + tamper event).
  Reuse the downgrade machinery; do NOT invent a new header name.
- future `valid_until` → nothing beyond the entries. dnf has no native expiry
  in primary.xml — the max_index_age_secs fallback already covers it; do not
  invent a dnf sentinel.

## Tests (extend the existing tests modules; fixtures in-memory)
index_parse.rs tests:
1. dnf: 2-package primary.xml (one epoch attr "0", one epoch "2", sha256 +
   sha checksums interleaved) → gzip → parse → versions `"1.2-3"` and
   `"2:9.15-1"`, sha256 picks the type="sha256" element.
2. dnf: unterminated-tag XML → Err, no panic.
3. apt: InRelease text with armor header, `Valid-Until` now+3600, SHA256
   block with 2 `.deb` lines + 1 non-.deb line → entries from basenames only,
   valid_until ≈ expected (±60s).
4. apt: past Valid-Until → parse still Ok (staleness is the proxy's call),
   valid_until value correct.
5. apt: PGP-armor sniff routes to the apt parser regardless of filename.
6. `looks_like_index` gains: `/rocky/9/BaseOS/x86_64/os/repodata/primary.xml.gz`,
   `/debian/dists/bookworm/InRelease`, `/debian/dists/bookworm/Release` →
   true; `/repo/Release.gpg` and `/foo.xml.gz` (not primary) → false.
proxy.rs tests module (name them with a `pm_kind` prefix so the filter finds
them):
7. primary.xml.gz → `Some("dnf")`; InRelease → `Some("apt")`; Release.gpg →
   None; `core.db` still pacman; `APKINDEX.tar.gz` still apk. Build a
   `hyper::Uri` from full URLs.

## Do NOT
- Do not touch ledger.rs, main.rs, lib.rs, any Cargo.toml, or tests/ files.
- Do not alter pacman/apk parse behavior or their tests (they must stay green).
- Do not restructure wrap_with_ledger_check or ledger_index_verdict_response.
- lib.rs already declares the module — nothing to register.

## Verify (ONCE, from /home/martin/projects/monopsony)
`cargo test -p monopsony-client index_parse -- --nocapture && cargo test -p monopsony-client --lib pm_kind -- --nocapture`

## Reporting
done / not_done / deviations + verbatim verify tail. Floor if time runs
short: dnf parser + its tests. apt Valid-Until + proxy touch points may go
not_done — never half-edit.
