# Mailbox passthrough mode — local installs for egress-restricted CTs

**Date:** 2026-11-03 · **Status:** PLAN only (draft; not started, no code)
**Scope:** a new client transport class that distributes Monopsony-managed packages,
shadow updates, and self-update artifacts through a shared bind mount instead of the
mTLS gRPC hop. Sprint 34 candidate.
**Roadmap:** `docs/sprint-roadmap.md` §Planned → "Sprint 34" (roadmap is gitignored
by design; this doc is the committed record).
**Parent plan:** `docs/plans/2026-10-31-server-mediated-self-update.md` (D1–D9 decisions,
`update_distributor.rs`, ShadowCache substrate). This plan **extends** that pipeline with
a second transport; it does not fork it.

## TL;DR

CTs with default-deny firewalls (the CT 1933 incident: firewall had to be flushed just to
install the client) have no path to Monopsony. Mailbox mode gives them one over shared
storage: the server stages **signed manifests** into a per-client inbox; the client
verifies, pulls **content-addressed blobs** from a shared read-only store, seeds its
package manager's own cache, and acks. Same keys, same signing, same staging/rollback
machinery as gRPC clients — only the transport hop is swapped. The security core is a
**provenance invariant** (blob may enter the store only with server-verified lineage),
not per-blob cryptography. The GC strategy (§GC) is the engineering centerpiece: three
per-class policies, ten invariants, and admission control instead of pin-breaking.

## Problem / proven gap

- Egress-restricted CTs cannot pair, heartbeat, or fetch. The only deployment path today
  is a firewall flush (done for CT 1933), which defeats the isolation the operator built.
- Bind mounts are already this fleet's isolation primitive (one trunk branch per CT);
  a storage-mediated channel adds **zero** firewall rules.
- Shadow builds that need network access fail closed in restricted CTs; this mode makes
  that policy explicit and gives it a sanctioned workaround (vendor packs), not a hole.

## Decisions log (settled — do not re-litigate)

| # | Decision | Rationale |
|---|---|---|
| D1 | Mailbox clients are a distinct class: slower loop (poll), batch manifests, ack-based liveness. Online CTs stay on gRPC — no feature parity pressure. | The mode exists only where the network cannot go. |
| D2 | Mailboxes carry **signed manifests referencing hashes**; payloads live once in a shared content-addressed blob store. Clients verify sha256 + signature after reading. | No duplication; integrity independent of filesystem trust. |
| D3 | Directionality enforced at the mount: client mounts `blobs/` **ro**, `epochs/` **ro**, own `mailboxes/<id>/` rw (inbox ro, outbox rw). | Kernel-enforced unidirectional gateway; the client physically cannot forge an inbox manifest. |
| D4 | **Provenance invariant (security core):** a blob enters the store only with server-verified lineage — mirror-synced (checksum-verified, epoch recorded) or recipe-pinned (signed recipe + hash verified). The server serves a blob to a client only if that client has an issued manifest referencing it (lineage gate at serve time). | Closes the "anyone with the mount can `ls blobs/`" hole without crypto. |
| D5 | The **ShadowCache is the unified substrate**: shadow blobs, self-update tarballs, git bundles, staged PM packages, mailbox distribution — one store, one GC. The **HTTP proxy cache stays separate** (hot path, SSD, JSON sidecars, existing janitor). | One integrity story where durability matters; no hot-path performance coupling to z2pool. |
| D6 | **No per-blob cryptography.** Rejected: derived per-client verification keys (integrity is already keyless — content addressing + signatures; encryption breaks content-addressing and protects public artifacts from nobody). | Model 1 rejected. |
| D7 | **No middleman broker.** Rejected: mTLS-reachable broker = the existing gRPC mode. Concurrency comes from immutability + pins, not locks. | Model 2 rejected. |
| D8 | Auth = request signatures from existing client keys (mTLS-equivalent). Revocation = storage-admin: drop mount, revoke key, release pins. Pairing = bind-mount provisioning + enrollment key (`mopk-…`). | Storage replaces TLS as the trust anchor; crypto unchanged. |
| D9 | **Fail-closed installs.** Seeded PM installs run with network fetch disabled (`apt-get --no-download`, `dnf --cacheonly`, pacman with pre-synced db). A mismatch aborts loudly; it never silently falls back to upstream. | Silent fallback would dissolve the mode's guarantee. |
| D10 | The server never unpacks archive content. Blobs are opaque verified bytes; parsing happens client-side (where the PM already does it). | Server-side archive parsing is pure attack surface. |

## Object model

```
z2pool/software/monopsony/            # server-only write access
├── blobs/<sha256>                    # immutable, content-addressed (ShadowCache root)
├── epochs/<epoch_id>/                # mirror-sync metadata manifest (signed, KB–MB)
└── mailboxes/<client-id>/
    ├── inbox/manifest.<seq>.signed   # server → client (client ro)
    ├── outbox/request.<seq>.signed   # client → server (client rw, server consumes)
    └── status.json                   # ack/heartbeat/liveness, client-written
```

| Object | Size | Who writes | Meaning |
|---|---|---|---|
| **Blob** | KB–GB | server (verified ingest only) | immutable payload bytes |
| **Epoch** | KB–MB | server (one per mirror sync) | repo metadata snapshot; the coherence unit; provenance record |
| **Manifest** | KB | server | a job: blob refs + hashes, epoch ref, target PM cache paths, expiry, Ed25519 signature |
| **Request** | KB | client (signed) | what the client wants; server treats as untrusted input (sig + schema + size caps) |
| **Status** | bytes | client | ack (blob hashes actually applied), liveness, optional access report |

Flow: client exports PM db snapshot → outbox. Server diffs against catalog, stages blobs
(verified ingest), issues manifest (pin acquisition, §GC). Client verifies signature +
every hash, applies staged repo metadata **then** seeds PM cache, runs the PM
fail-closed, acks with applied hashes. Server releases pins, updates liveness, GC
cascades. Shadow path: recipe-pinned blobs / git bundles (`shadow_git_mirror.rs` export)
replace the mirror-epoch provenance class; Build recipes in restricted CTs require
vendored dependency packs staged as blobs or fail closed with explicit error text.

## GC strategy (bolted down)

The unified store is GC-critical: it holds issued-but-unacked fleet work and the only
copy of old-version packages laggard clients still resolve against. Operator rules
adopted verbatim: **manifests/epochs are cheap and outrank blobs**; an epoch lives until
every blob it references is gone; manifests get their own retention policy because
old-version pulls depend on them.

### Invariants

| # | Invariant |
|---|---|
| I1 | **Provenance gate (ingest):** no blob without verified lineage (mirror epoch or recipe pin). Enforced at `ShadowCache.store_blob`; anything else is a bug. |
| I2 | **Immutability:** blob bytes at `blobs/<sha256>` are never modified after rename. All mutation is create/delete; races reduce to create-vs-read (atomic rename) and GC-vs-read (I4 + ro-mount hash-verify). |
| I3 | **Pin before serve:** issuing a manifest inserts manifest row + blob pin rows in **one DB transaction**. GC reads the same tables. There is no window in which an issued manifest references unpinned blobs. |
| I4 | **No refcount-invisible deletion:** a blob is evicted only when zero pins reference it at check time; the DB row delete is the commit point (file unlink follows). |
| I5 | **Epoch outlives blobs:** an epoch (and its metadata files) is deleted only when *every* blob it references is gone **and** no unacked manifest references it. Epochs are never pressure-evicted (KB-scale). While any referenced blob exists, the epoch exists — old-version clients can always resolve coherently. |
| I6 | **Manifest retention is independent:** *acked* manifests are audit records on a time policy (default 180d); their deletion never affects epochs (blob-driven) or blobs (pins die with ack/release, not audit expiry). *Unacked* manifests live until acked or explicitly released — they **are** the pins. |
| I7 | **DB-first deletion + orphan sweep:** delete row → unlink file → nightly orphan sweep reclaims files with no DB row older than a 24h grace. Crash direction is safe: a crash leaves an orphan file, never a dangling row or a phantom epoch. |
| I8 | **Quarantine ≠ deletion:** scrubber hash-mismatch moves bytes to `.quarantined/`, keeps the DB row, pins, and epoch refs intact; blob is unservable but present. Epoch cascade treats quarantined blobs as existing. Purging quarantined bytes is an operator action. |
| I9 | **Mailbox-blind LRU fix:** the server cannot observe client reads (ro mount). Blob `last_accessed` updates at **issue time** and **ack time** (ack lists applied hashes); optional client-reported access list in `status.json`. Without this, mailbox-served blobs look cold and evict prematurely. |
| I10 | **Admission control instead of pin-breaking:** when the pinned set would exceed the store budget, the server **refuses to issue new manifests** (staging pauses, alert fires). Issued pins are never auto-invalidated. Release paths: signed ack · admin client-revoke (storage-admin op per D8) · explicit manifest-cancel API. |

### Per-class policies

| Class | Policy | Trigger | Notes |
|---|---|---|---|
| Distro blobs | staleness/LRU under pressure | class budget exceeded | matches existing janitor distro behavior (upstream moves → old blobs die) |
| Language/shadow/self-update blobs | size-weighted LRU on `last_accessed`, forever by default | class budget exceeded | append-only class per the established disk-pressure rule; no TTL (would resurrect upstream-dependency problems) |
| Epochs | pure cascade: referenced-blob set empty **and** no unacked manifest refs | never pressure-driven | I5; cascade runs after every blob pass |
| Manifests (acked) | time retention, default 180d | age | audit trail only (I6) |
| Manifests (unacked) | live until ack / release | — | they are the pins |

### Sweep algorithm (single nightly pass, off-peak — z2pool constraint)

1. **Blob pass** per class policy; skip pinned; delete DB row first, then unlink (I7).
2. **Epoch cascade** — recompute per-epoch referenced-blob sets from `epoch_blobs`;
   delete epochs satisfying I5 (rows, then metadata files).
3. **Orphan sweep** — on-disk blobs/epochs with no DB row, age > 24h → delete.
4. **Report** — pinned bytes, evicted bytes per class, epoch count, oldest pin age,
   admission-control trips → Prometheus (`metrics.rs` pattern) + dashboard panel.

### Edge cases (specified, tested)

| Case | Behavior |
|---|---|
| Blob shared by N epochs | evictable by blob policy regardless of epochs (I5 is epoch-outlives-blobs, not blobs-pinned-by-epochs — otherwise old epochs pin dead weight forever); deletion decrements all referencing epochs, which cascade independently |
| Client offline indefinitely | pins hold; admission control eventually pauses new staging; alert at 30d pin age; **no auto-release** — operator resolves via revoke/release |
| Epoch with zero blobs (metadata-only sync) | immediately cascade-eligible once no unacked manifest references it |
| Manifest references a missing blob | impossible via I3 + issue-time validation (issuance re-verifies every referenced blob exists); defensive path returns a loud error, never a partial manifest |
| GC vs active client read | safe: client verifies hashes; worst case is a failed read → job retry. (Linux unlink-while-open keeps the inode alive until the fd closes.) |
| Concurrent issue vs GC | serialized by DB transactions on the pin tables (I3/I4) — no lock broker needed |
| Scrubber quarantines a pinned blob | pin + epoch intact (I8); next client request fails verification loudly; operator decides |

### Schema (extends the self-update plan's DB work — reconcile in P0, don't duplicate)

- `store_blobs` (sha256 PK, size, class, provenance_kind, provenance_id, created_at,
  last_accessed_at, state: `live|quarantined`)
- `sync_epochs` (epoch_id PK, mirror_id, upstream_url, created_at, signed metadata manifest)
- `epoch_blobs` (epoch_id, sha256) — the join that drives the I5 cascade
- `mailbox_manifests` (manifest_id PK, client_id, epoch_id, issued_at, acked_at, expires_at)
- `mailbox_manifest_blobs` (manifest_id, sha256) — the pin rows
- `mailbox_requests` (audit of validated requests)

Refcounts are **derived** (indexed joins), never maintained counters — nothing to drift.

### Config knobs (`[store]`, `[store.gc]`)

store root path (default `z2pool` mount); per-class budgets; gc sweep interval;
`manifest_retention_days` (180); `pin_alert_age_days` (30); orphan grace (24h);
`issue_block_when_pinned_bytes_over` (budget fraction, default 0.9).

### GC test matrix (the gate for P2)

Pin-acquisition race (issue ∥ GC, repeated) · shared-blob multi-epoch cascade ·
epoch survives while one referenced blob lives, dies when last blob goes ·
ack releases pins + touches liveness · admission control trips at budget ·
orphan sweep after simulated crash between row-delete and unlink ·
quarantine keeps pins + epoch · blind-LRU: un-acked mailbox blob not evicted,
acked blob ages correctly · manifest retention deletes acked only ·
crash-replay: kill sweep mid-pass, re-run, verify invariants hold.

## Phased implementation

Bench: server 10.10.20.58; mailbox-client test on a firewalled test CT (reproduce the
CT 1933 constraint; do **not** touch the production checkmk CT). Gates per repo
convention: workspace green, zero new clippy warnings, hermetic tests only (tempdir
stores, std-TcpListener patterns). No commits — operator signs.

### P0 — Schema + store re-root (~1d)
Migrations (schema above, reconciled with self-update plan tables); `[store]` config;
ShadowCache root configurable and re-rooted to the shared mount; provenance columns.
Files: `crates/monopsony-db/src/db.rs`, `crates/monopsony-common/src/config.rs`,
`crates/monopsony-shadow/src/shadow_cache.rs`. Gate: `cargo test -p monopsony-db` +
migration round-trip tests.

### P1 — Epoch recording at sync (~1d)
Mirror sync assigns epoch ID, writes signed metadata manifest to `epochs/`, links synced
blobs (`epoch_blobs`). Files: `crates/monopsony-mirror/src/mirror.rs`,
`crates/monopsony-shadow/src/shadow_cache.rs`. Gate: `cargo test -p monopsony-mirror`.

### P2 — GC engine (~2d) — **the gate phase**
Per-class policies, pins, admission control, orphan sweep, scrubber interaction,
metrics; full test matrix above. Files: `shadow_cache.rs` (GC core),
`crates/monopsony-server/src/main.rs` (sweep wiring). Gate: `cargo test -p
monopsony-shadow` + workspace; GC matrix green.

### P3 — Mailbox transport, server side (~2d)
Outbox watcher (poll ~15s); request validation (signature + schema + size caps);
job generator (PM-db-snapshot diff → manifest issuance = pin acquisition, lineage gate
at serve per D4); ack processing (release pins, liveness touch, status update).
Files: `crates/monopsony-server/src/mailbox.rs` (NEW), `main.rs`. Gate: hermetic
mailbox tests (tempdir mailboxes, synthetic signed requests).

### P4 — Client mailbox mode (~2–3d)
New transport behind the existing client trait: inbox poll → verify → stage; `SeedCache`
proxy-config action (apt/pacman/dnf paths) + fail-closed run modes (D9); PM db snapshot
export to outbox; ack writer. Files: `crates/monopsony-client/src/mailbox.rs` (NEW),
`proxy.rs`, `main.rs`. Gate: `cargo test -p monopsony-client` (convention: 480+ total,
hermetic).

### P5 — Shadow / git-bundle path (~1–2d)
Bundle export in `shadow_git_mirror.rs`; recipe-pinned provenance class; offline Build
policy: vendor packs as blobs or explicit fail-closed error. Gate: `cargo test -p
monopsony-client` (shadow executor), `cargo test -p monopsony-shadow`.

### P6 — Provisioning, dashboard, docs (~1–2d)
Mailbox client class provisioning (enrollment key + bind-mount spec + server config);
dashboard panel (mailbox client skew/ack state, GC stats from P2); docs: AGENTS.md
note, `docs/pairing-security.md` key-handling delta. Gate: docs review; dashboard build.

**Est. total: ~8–11d.** Sequencing: P0→P1→P2 are the foundation (GC before transport —
nothing may issue manifests into a store whose GC isn't proven); P3–P5 parallelizable
after P2; P6 last.

## What this does NOT defend (honesty section)

- **Runtime compromise** of a mailbox CT — crowdsec/authentik/nftables/Checkmk lane.
- A malicious client requesting *other tracked* content: lineage-gated, but content is
  public — that request succeeding is harmless by design. Blob store has ~zero
  confidentiality (D6); if a genuinely private artifact ever needs staging, it stays
  out of the shared store.
- **z2pool availability coupling**: the mailbox plane dies with the degraded pool. L0 +
  weekly PBS cover restore, not uptime.
- Old-version resolution is bounded by blob retention: once a blob is evicted under its
  class budget, its epoch cascades (I5) and laggards must re-sync. Budget sizing is the
  retention lever.
- Vendor packs make offline Build *possible*, not safe: recipe trust enforcement applies
  unchanged; a Build recipe without vendored deps fails closed in restricted CTs.

## Open questions (each carries an assumed default — flip cheaply)

1. **Epochs dir shared ro vs metadata staged into each inbox** — default: shared ro
   (less duplication; epoch metadata is public-within-LAN under D4/D6).
2. **Epochs for proxy-fetched language artifacts** — default: no (they revalidate
   upstream; epochs apply to mirror syncs + staged jobs only).
3. **GC sweep cadence** — default: nightly, off-peak.
4. **Manifest retention** — default 180d.
5. **Poll interval** — default 15s server-side watch, 5min client-side poll.
