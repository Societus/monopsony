# Mirror Sync Integrity — GPG verification, cross-mirror comparison, hash-policy knob

Date: 2026-10-31 · Lane: t_1982088c (parent gate t_4a866c8f satisfied) · Status: IMPLEMENTED (uncommitted)

## Scope (3 items, one subsystem — monopsony-mirror sync path)

1. GPG signature verification for repo metadata before serving (Debian/Ubuntu
   Release/InRelease, Alpine APKINDEX, Arch databases).
2. Cross-mirror hash comparison against tracker-listed peer mirrors.
3. Configurable hash-verification policy (server.toml knob, fail-closed vs
   fail-open, per-mirror override).

## Verified wire facts (checked live 2026-10-31)

### Debian / Ubuntu
- `InRelease` is a clearsigned OpenPGP message over the Release text.
  Verified live: `https://deb.debian.org/debian/dists/bookworm/InRelease`
  begins with `-----BEGIN PGP SIGNED MESSAGE----- / Hash: SHA256`.
- `Release.gpg` (detached armored signature over the exact Release bytes)
  served alongside (HTTP 200 checked live).
- Primary source: apt-secure(8) (manpages.debian.org, bookworm): archives sign
  with `gpg --clearsign -o InRelease Release` and `gpg -abs -o Release.gpg
  Release`; key distribution is by pinned keyring — "ship your key in its own
  keyring package like Debian does with debian-archive-keyring".

### Alpine
- `APKINDEX.tar.gz` = two concatenated gzip members; member 1 is a tar holding
  one `.SIGN.<TYPE>.<keyfile>` entry (raw signature bytes), member 2 is the
  tar with `DESCRIPTION` + `APKINDEX` (live-checked on
  `dl-cdn.alpinelinux.org/alpine/v3.22/main/x86_64/`).
- **The signature covers the COMPRESSED bytes of member 2.** Proven live with
  openssl: `openssl dgst -sha1 -verify <official key> -signature <SIGN member>
  <member-2 gzip bytes>` → "Verified OK" on the real v3.22 index; hashing the
  decompressed tar or the APKINDEX file alone FAILS to verify.
- Digest comes from the member name (apk-tools v2.12.14 src/package.c
  `signature_type[]`): `RSA`→SHA1, `RSA256`→SHA256, `RSA512`→SHA512, `DSA`→DSA.
  Live v3.22 uses `.SIGN.RSA.*` (SHA1). Keys are SPKI PEM
  (`PEM_read_bio_PUBKEY`, apk-tools src/package.c:599).
- Negative controls (live openssl): corrupted member-2 payload → verify
  fails; corrupted signature → fails; foreign key → fails.

### Arch
- **Repo databases are unsigned upstream.** `core.db.sig` returns HTTP 404 on
  six independent official/tier-1 mirrors (geo.mirror.pkgbuild.com,
  mirror.pkgbuild.com, mirrors.kernel.org, mirror.rackspace.com,
  mirrors.mit.edu, arch.mirror.constant.com) — checked live 2026-10-31.
- Consequence: database-signature verification is impossible by upstream
  design. Package integrity comes from `%CSHA256%` entries inside the db
  (already parsed by repo_index.rs, now enforced at prefetch per policy) and
  from cross-mirror hash comparison for the db files themselves.
- Note: gitlab.archlinux.org is behind an Anubis bot-wall, so the
  pacman.conf(5) SigLevel text could not be fetched for citation; the six
  live 404s are the load-bearing evidence for this design decision.

## Design

### Trust model (choice points, for sign-off)
- **Pinned keys per mirror** in mirrors.toml (`trusted_keys = [...]`), each an
  inline armored key or a path to an armored key file. OpenPGP keys anchor
  Debian/Ubuntu; SPKI PEM RSA keys anchor Alpine. This is the apt `signed-by`
  keyring model — the operator decides which upstream keys to trust, nothing
  is fetched from keyservers, no TOFU. Alternatives rejected:
  - *TOFU*: worthless against a poisoned first fetch (the exact attack a
    mirror must resist).
  - *Shipped distro keyrings*: duplicates debian-archive-keyring's job and
    bloats the container; an operator pinning `signed-by` keyrings already
    has the armored keys to hand.
  - *gpgv shell-out*: adds a host dependency (gpg absent from the musl
    container image) and breaks the no-network unit-test gate. rpgp (pure
    Rust) verifies in-process.
- **No-keys behavior**: signature verification is SKIPPED (metric
  `no_keys`, logged once per sync); checksum sidecars still apply. Fail-closed
  on missing keys would break every existing mirror config — strictness is
  governed by the hash-policy knob, not by key presence. **Sign-off point.**
- **Arch**: no signature enforcement possible upstream (see wire facts);
  metric `unsupported` is emitted per sync so dashboards can see the gap.
  **Sign-off point** (alternatives: fail the Arch sync entirely — rejected as
  it hard-disables a working mirror class).
- **Cross-mirror divergence**: reported via metric + error log; NO
  auto-quarantine in v1 (task text specifies "report via metrics + log").
  **Sign-off point** for a future auto-quarantine policy.
- **Cross-check peer source**: the operator's own tracker; the private-IP
  SSRF guard is NOT applied to tracker-supplied peer endpoints (they are
  operator-controlled infra; blocking RFC1918 would disable the feature on
  every LAN). Risk accepted: read-only GET, response hashed and discarded,
  1 GiB size cap. **Sign-off point.**

### Hash-verification policy (item 3)
- Global default: server.toml `[mirror] hash_policy` (default
  **"fail-closed"** — recommendation for sign-off; the previous per-mirror
  field default already claimed fail-closed but the sync path ignored it and
  served anyway).
- Per-mirror override: mirrors.toml `hash_policy = "fail-open"` on a
  `[[mirror]]` entry (now `Option<String>`; absent = inherit global).
- **Enforcement fix** (the actual gap): `sync_single_file` previously
  swallowed `verify_file_hash` errors with a warn ("Future: make this
  configurable"); it now rejects the object under fail-closed — not indexed,
  not written, not served — and only warns under fail-open.
- Verification sources, in order: (1) anchor hash from VERIFIED metadata
  (Debian Release SHA256 block / repo db `%CSHA256%`), (2) upstream
  `.sha256`/`.sha256sum` sidecar files.
- Serve path: metadata documents fetched on cache miss are signature-verified
  before serving when keys are pinned (fail-closed → 502; fail-open → warn +
  serve). Locally-stored copies are the verified sync-time bytes.
- Metrics: `monopsony_mirror_hash_verify_total{distro,result}`
  (pass / mismatch_fail_closed / mismatch_fail_open),
  `monopsony_mirror_signature_verify_total{distro,result}`
  (pass / fail / fail_open / no_keys / unsupported),
  `monopsony_mirror_cross_check_total{distro,result}`
  (match / divergence / unreachable / skipped).

### Cross-mirror hash comparison (item 2)
- After each successful sync (legacy scrape + index/prefetch paths),
  `cross_check.rs` queries `{tracker}/discover?distro=X`, skips self (mirror_id
  prefix = server hostname), samples up to `cross_check_sample` locally-stored
  objects (newest-modified, depth ≤ 3 walk), fetches the same distro-relative
  paths from peers (round-robin, per-fetch timeout), and compares SHA-256s.
- Sequential fetches, bounded sample, per-fetch timeout — gentle on upstreams.
- Config: `[mirror] cross_check_enabled = true (default), cross_check_sample =
  4 (default), cross_check_timeout_secs = 30 (default)`. Cross-check only runs
  when tracker URLs are configured (--tracker-url).

### Metadata verification by distro (item 1)
- Debian/Ubuntu: fetch `dists/<suite>/InRelease` (clearsigned, self-contained)
  → verify with rpgp against pinned keys → the signed text anchors the
  Packages.gz checksum (Release `SHA256:` block) → index hash enforced per
  policy → Packages entries anchor package prefetch hashes. Fallback:
  `Release` + detached `Release.gpg` verification.
- Alpine: fetch `{suite}/main/{arch}/APKINDEX.tar.gz` → split gzip members
  (exact RFC1952 boundaries via flate2 Decompress total_in — flate2's
  GzDecoder over-reads and cannot report boundaries) → verify RSA-PKCS1v1.5
  over member 2's COMPRESSED bytes with the digest from the member name →
  parse APKINDEX (client-consistent checksum decoding: sha256 only when the
  `c:`-prefixed value decodes to exactly 32 bytes, never guess).
- Arch: db downloaded without signature (unsigned upstream, metric
  `unsupported`); prefetch verifies package bytes against `%CSHA256%`.
- Alpine was added to `repo_index_urls` (new distro support; suite default
  "latest-stable", overridable via mirrors.toml `suite`).

## Files

| File | Change |
|------|--------|
| crates/monopsony-mirror/src/integrity.rs | NEW: HashPolicy, TrustedKeys, OpenPGP clearsign/detached verify (rpgp), APKINDEX member-split + RSA verify (rsa crate), Release SHA256 extractor, fixture fixtures generator hook |
| crates/monopsony-mirror/src/integrity_tests.rs | NEW: round-trip + tamper + foreign-key + format-detection tests; `generate_fixtures` (--ignored) writes the checked-in fixtures |
| crates/monopsony-mirror/tests/fixtures/ | NEW: SYNTHETIC signed fixtures (throwaway test keys, NOT distro trust anchors): debian/{Release,InRelease,InRelease.bad,Release.gpg,release-key.pub.asc}, alpine/{APKINDEX.tar.gz,APKINDEX.tampered.tar.gz,alpine-test-key.pub}, README.md |
| crates/monopsony-mirror/src/cross_check.rs | NEW: tracker discover → peer fetch → sha256 compare, metrics; divergence-detection test with honest+poisoned axum peer servers |
| crates/monopsony-mirror/src/mirror.rs | EDIT: hash_policy → Option<String>, trusted_keys + suite fields, IntegritySettings, fail-closed enforcement in sync_single_file, anchor-hash prefetch, verified-metadata caching (write_atomic), serve-path metadata verification, cross-check hooks |
| crates/monopsony-mirror/src/repo_index.rs | EDIT: suite param, alpine index + APKINDEX parser + checksum decode |
| crates/monopsony-mirror/src/lib.rs | EDIT: module exports |
| crates/monopsony-mirror/Cargo.toml | EDIT: pgp 0.21, rsa 0.9 (sha1+sha2), sha1 0.10 (oid), base64, thiserror |
| crates/monopsony-common/src/config.rs | EDIT: MirrorIntegrityConfig ([mirror] section) |
| crates/monopsony-common/src/metrics.rs | EDIT: 3 new mirror integrity metric names + helpers |
| crates/monopsony-server/src/main.rs | EDIT: [mirror] section load → IntegritySettings wiring |
| dist/mirrors.toml.example | EDIT: trusted_keys/hash_policy/suite docs |
| dist/server.toml.example | EDIT: [mirror] section docs |

## Deliberately NOT in this slice (follow-ups)
- DB persistence of hash_policy/trusted_keys (mirror_configs columns +
  UpsertMirrorConfig proto): integrity config is operator-edited mirrors.toml
  only for v1; DB-loaded mirror rows inherit the server-wide default.
- Auto-quarantine of diverged objects (report-only per task text).
- Additional suites per distro (v1 indexes exactly one suite per mirror).

## Sign-off summary
Defaults presented for operator approval: **fail-closed** (global hash
policy), **cross-check ON** (4 files, 30 s timeout), **no-keys ⇒ skip
signature verification** (never break existing configs silently), **Arch
dbs ⇒ unsupported metric, no enforcement possible upstream**, **divergence ⇒
report only**. Every one is a one-line config flip; none require rebuilds
except moving the cross-check default to false.
