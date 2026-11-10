# Peer-Exchange Blacklist Propagation — design (3 approaches; operator pick recorded)

Date: 2026-10-31 · Lane: t_1301e15a · Status: **design ready — Approach A picked (recorded on t_1301e15a, 2026-10-31); implementation queued behind the operator's commit-script run**

Post-survey revision (2026-10-31, lane t_e98ce5ad): folded in findings from the two completed
survey lanes — t_a71ed84b (mirror signing / peers endpoint) and t_e1e4c07f (tracker blacklist
storage / announce protocol). Adds prerequisite 4 (client peers-schema repair), hardens
prerequisite 1 with the tracker auth caveats, adds the auto-ban blast-radius amplification to
the honesty section, and points at in-tree signing prior art.

## Problem

The tracker's blacklist (`blacklist` table, `crates/monopsony-tracker/src/main.rs:265-276`) is
tracker-local SQLite. A banned mirror is filtered from `GET /discover` (:622-626) and nothing else:

- Peer mirrors never learn about the ban — they keep advertising the bad peer via
  `GET /.well-known/monopsony/peers` (mirror.rs:1028-1049) and keep using it as a cross-check
  anchor (`cross_check.rs`).
- Clients in discovery v2 (`crates/monopsony-client/src/discovery.rs`) merge tracker + peers +
  mDNS results; the peers/mDNS sources are blacklist-blind, so a banned mirror stays reachable
  through any path that bypasses the tracker.
- Auto-blacklist (≥3 `hash_mismatch` reports / 24 h, main.rs:341-363) only helps clients that
  still ask THIS tracker — tracker-local enforcement against a decentralized discovery layer.

## Current state (verified against tree at HEAD 59dfe1b)

| Fact | Location |
|------|----------|
| `blacklist` table: mirror_id XOR operator_npub, reason, source `manual`/`auto`, `expires_at` (NULL = permanent) | tracker main.rs:265-276 |
| `is_blacklisted()` checked ONLY inside `handle_discover` | tracker main.rs:310-338, 622-626 |
| `POST /blacklist`, `GET /blacklist`, `DELETE /blacklist/{id}` are all in the **open router — unauthenticated**; auth middleware wraps only `/announce` | tracker main.rs:815-838 |
| Auto-blacklist rows are permanent (`expires_at = NULL`), **duplicate-triggerable** (no dedup), and reachable via the **unauthenticated `/report`** route (threshold 3-in-24h hardcoded) | tracker main.rs:341-363, 679-695 |
| `AnnounceRequest` carries `operator_npub` + `operator_signature` but `handle_announce` **never verifies the signature**; operators auto-created from the announced npub | tracker main.rs:54-56, 501-517 |
| jwt auth mode verifies against a **hardcoded dev HS256 secret** (`b"tracker-dev-secret"`) — not a production authority until the secret is configurable | tracker auth.rs:154 |
| `TrackerAnnouncer` sends **neither** npub nor signature → the announce path is unsigned end-to-end today | monopsony-control/src/announce.rs:89-99 |
| Peer exchange serves only local configs (`PeerEntry { mirror_id, endpoint, expires_at }`, `source: "local"`), unsigned | mirror.rs:1012-1049 |
| Client `DiscoveredMirror` requires mandatory `transport` + `score`; mirror `PeerEntry` serves neither ⇒ `query_peer_exchange` **fails deserialization today** — the client's `/peers` leg of discovery v2 is dead in practice (warn + skip) | client discovery.rs:10-20, 30-34 vs mirror.rs:1021-1049 |
| Ed25519 canonical-JSON sign/verify machinery exists, tested (sorted keys, sig/pk excluded, hex sig 64 B / pk 32 B) — **unwired** (`pub mod manifest`, zero consumers; the actual `/manifest.json` handler serves its own unsigned struct) | monopsony-mirror/src/manifest.rs:36-108; lib.rs:4; mirror.rs:973-995 |
| Server already holds an operator Ed25519 identity key (`<shadow_dir>/signing.key`) | server main.rs:696-698 via `shadow_signing::load_or_generate_signing_key` |
| Tracker depends on `ed25519-dalek` but has no signing-key management | tracker Cargo.toml:22 |
| Client discovery fetches `/.well-known/monopsony/peers` and merges into results | client/src/discovery.rs:70-72, 120-139 |
| Roadmap backlog line for this item | sprint-roadmap.md:644 (gitignored, local) |

## Security observations — prerequisite fixes in EVERY approach

1. **`POST /blacklist` is unauthenticated today** (tracker main.rs:832, open router). Anyone
   reachable on :3130 can ban any mirror. That is a censorship DoS locally — and once entries
   propagate it becomes *remote poison-a-fleet*. No propagation design may ship before the write
   path authenticates and signs: require the existing tracker auth middleware (token/jwt) on
   `POST /blacklist` + `DELETE`, and verify an Ed25519 operator signature over the canonical
   entry JSON against the originator npub. Note the auth anchor's own weakness: jwt mode
   verifies against the hardcoded dev HS256 secret (auth.rs:154), so token mode (shared key)
   is the only real authority today; jwt needs a configurable secret before it counts.
2. **`operator_signature` is accepted but never verified** in `handle_announce`, and the announce
   client never sends it. Wire verification (require + verify when the tracker runs token/jwt
   mode; verify-when-present in open mode) or drop the field — accepted-but-ignored signature
   fields manufacture false confidence. Mirror-id spoofing is trivial until this lands; operator
   -level bans are the only stable subject until announce signing exists.
3. **`/.well-known/monopsony/manifest.json` is unsigned in practice** — manifest.rs machinery
   exists and is tested but unwired. Same canonical-JSON helper this design reuses; wiring it is
   a free-rider fix, tracked separately (not in this card's scope).
4. **The client `/peers` leg fails deserialization today** (survey t_a71ed84b, verified in tree):
   the mirror serves `PeerEntry { mirror_id, endpoint, expires_at }` (mirror.rs:1021-1049) but
   the client's `DiscoveredMirror` requires mandatory `transport` and `score` fields
   (discovery.rs:10-20) — `query_peer_exchange` errors on every response, so discovery v2's
   peers source silently yields nothing (warn + skip, discovery.rs:68-78). Approach A's client
   consumption rides on this leg, so repairing it is a **required A prerequisite**: either the
   mirror extends `PeerEntry` with transport/score (it knows both for its own mirrors), or the
   client uses a dedicated relaxed peers struct (`#[serde(default)]`) instead of reusing
   `DiscoveredMirror`. Independent bug regardless of which approach is picked.

## Shared design (identical under all approaches)

### Entry format (proposed wire shape)

```json
{
  "subject": "mirror:us-west-arch-001",      // or "operator:<npub-hex>"
  "action": "ban",                            // or "retract"
  "reason": "cross-check divergence x3",
  "originator": "<64-hex Ed25519 public key>",
  "seq": 17,                                  // monotonic per originator
  "created_at": 1793500000,
  "expires_at": null,                         // null = until retracted (manual); set = TTL (auto)
  "signature": "<128-hex over canonical entry JSON>"
}
```

Signing reuses `manifest.rs` exactly: canonical JSON = serde value with recursively sorted
object keys, `signature` excluded, Ed25519 over the UTF-8 bytes, hex-encoded. No new crypto
scheme, matching the parent lane's directive. Entry types live in `monopsony-common` (client,
mirror, and tracker all consume; per the module-home rule, client does not depend on
monopsony-mirror). Clone-able in-tree prior art (survey t_a71ed84b): `PinAttestation`
(monopsony-common/src/pin_store.rs) and the oracle `SignedDelta` sign_snapshot/verify_delta
pattern (with its distinct freeze-alarm on expiry).

### Revocation semantics

- A **retract** is an entry with `action = "retract"`, signed by the **same originator npub**
  that issued the ban. Nobody retracts another operator's entry; an authority that loses
  confidence in an originator publishes its own ban of the compromised *operator* subject.
- Receivers keep, per `(originator, subject)`, the entry with the **highest seq**. A replayed
  stale signed `ban` loses to a newer signed `retract` without any clock trust. `expires_at`
  still bounds temporary (auto) bans.
- **Tombstone retention**: receivers retain retractions for a window (default 7 days) so a
  retraction outlives gossip lag — otherwise an offline peer re-syncing from an old snapshot
  resurrects the ban.
- Compacted view for serving: bans whose `(originator, subject)` max-seq entry is `retract` or
  expired are dropped from what we advertise onward.

### Trust authorization (pinned authorities — parent lane's house style)

Receivers accept an entry only if `originator` is in the operator-pinned set:

```toml
# server.toml
[peer_blacklist]
trusted_operators = ["<npub-hex>", ...]   # empty ⇒ v1 default: trust ONLY the
                                          # configured tracker's key (approach A/C)
                                          # or enforce local-only (no remote entries)
```

Pinned keys, apt `signed-by` model — no keyserver, no TOFU, no web-of-trust. The tracker's
operator key (once the tracker signs, approaches A/C) is the natural default pin: the tracker
already runs manual + auto-blacklist and its key is the authority the fleet already implies.

### Enforcement points

| Consumer | Enforcement |
|----------|-------------|
| Tracker `GET /discover` | already filters (exists) |
| Mirror `get_peers` (`.well-known/monopsony/peers`) | drop blacklisted peers from the served list |
| Mirror cross_check | skip blacklisted peers as comparison anchors (a poisoned anchor corrupts the divergence signal) |
| Mirror announce loop | n/a for others' bans; own-ban ⇒ operator-visible warning + metric |
| Client discovery v2 | filter ranked results |

## Approach A — piggyback on peer exchange (signed gossip)

`PeerList` gains an optional, size-capped `blacklist: Vec<BlacklistEntry>` (only entries the
serving mirror currently holds, newest-seq first, default cap 64) plus an envelope signature by
the serving mirror's key (transport integrity, manifest.rs envelope style). Each entry keeps its
originator signature, so trust travels through gossip hops.

Propagation: tracker bans mirror M → every mirror polling the tracker holds the entry → their
`/peers` responses carry it → peers-of-peers and clients merge it. After the first fetch the
system works through gossip even during a tracker outage (until tombstones age out).

*Post-survey correction:* the "clients merge it" leg currently fails deserialization
(prerequisite 4) — the merge code exists (discovery.rs:68-78) but has never received a
successfully parsed peers payload. The schema repair is part of A's cost, not optional.

- **Pros**: no new endpoint; rides the fetch cadence that already exists (cross_check and client
  discovery both already talk to `/peers` or `/discover`); genuinely decentralized — the
  blacklist survives tracker loss; mirrors can originate their own entries (e.g. a mirror that
  observes poisoning publishes a ban even if its tracker disagrees).
- **Cons**: eventual consistency (gossip convergence delay); `/peers` payload grows (capped);
  every receiver implements merge + pinned-originator policy; freeloader surface — the peers
  endpoint is on the public mirror router, so unsigned additions would be injectable by anyone
  (mitigated by per-entry signature verification, which is mandatory anyway).

## Approach B — dedicated signed snapshot endpoint

New `GET /.well-known/monopsony/blacklist.json` on the mirror router: a manifest.rs-style signed
snapshot — `{version, server_id, generated_at, generation: u64, entries: [...], signature,
public_key}`. Full list each time (bans number in tens, not thousands; cap enforced), cacheable
with ETag/304, polled on the discovery/sync cycle alongside `/peers`.

- **Pros**: clean separation from peer discovery; simplest to verify (one signature) and cache;
  `generation` counter makes change-detection trivial; client-only consumers don't parse the
  peers payload.
- **Cons**: new endpoint + a new fetch in every cycle; the publisher must hold the full merged
  set — the same merge logic A needs, so it saves no complexity; two transports for one data
  model if clients also keep peers support. In the small.

## Approach C — tracker-relayed (hardened single authority)

The tracker becomes the sole blacklist authority: prerequisite auth fix lands (`POST /blacklist`
behind auth middleware + entry signature verification), the tracker gains signing-key management
(`ed25519-dalek` dep already present) and serves a signed export — `GET /blacklist` becomes the
signed canonical document (or `?since=` incremental). Mirrors poll it on the existing announce
cadence and enforce locally (exclude from `get_peers`, skip in cross_check); clients keep
getting tracker-side filtering via `/discover` and can optionally fetch the export in discovery
v2.

- **Pros**: least new machinery — the tracker is already the announce/discover hub mirrors poll
  on a timer; strongest consistency (no gossip lag, no merge conflicts); enforcement logic is a
  filter, not a merge.
- **Cons**: **not decentralized** — tracker outage freezes enforcement state (last-known set
  persists but nothing new spreads), and the tracker operator becomes a single censor for every
  consuming mirror; mirrors cannot originate entries; contradicts the card's framing
  ("decentralized blacklist") unless it is only the authority anchor inside A.

## Tradeoff summary

| | A: peers gossip | B: snapshot endpoint | C: tracker-relayed |
|---|---|---|---|
| New endpoints | 0 | 1 | 0 (extend `/blacklist`) |
| Tracker outage tolerance | high (gossip continues) | high (peers carry it) | none (stale set) |
| Convergence | eventual (gossip delay) | eventual (poll delay) | immediate on poll |
| Mirror-originated bans | yes | yes | no |
| Client consumption | via `/peers` merge (requires peers-schema repair — leg dead today) | direct fetch | via `/discover` (works today) + optional export |
| New code mass | M (merge + policy + envelope + schema repair) | M (endpoint + same merge) | S (sign + poll + filter) |
| Censorship resistance | highest | high | lowest |

## Recommendation

**A with C's authority anchor folded in**: the tracker stays the primary pinned originator (its
auth + signing fix is the prerequisite anyway), and the peers endpoint carries signed entries so
the blacklist survives tracker outages and reaches clients through the peer path that already
exists (after the prerequisite schema repair). B is redundant transport — same data model,
second endpoint. Stated plainly: if the operator's deployment reality is "one tracker I
control," C alone delivers most of the practical benefit for roughly a third of the work and A
can be layered on later without a format change (entries are identical; only the transport
differs). The pick is about how decentralized the operator wants this to actually be.

## Open questions (assumed defaults, each a one-line flip)

1. Default authority set when `trusted_operators` is empty — **default**: tracker's key only
   (A/C) or ignore remote entries entirely (B); never trust-by-default.
2. Client-side enforcement scope — **default**: discovery v2 filter only; no proxy-level hard
   block in v1.
3. Gossip/snapshot entry cap — **default**: 64, newest-seq first.
4. Tombstone retention — **default**: 7 days.
5. cross_check skips blacklisted peers — **default**: yes.
6. Tracker `GET /blacklist` visibility (reason strings are mildly sensitive) — **default**:
   stays open, matching today; flip to auth if the operator wants reasons private.
7. Announce signature verification strictness by tracker auth mode — **default**: require+verify
   in token/jwt mode, verify-when-present in open mode.
8. Peers-schema repair direction (prerequisite 4) — **default**: client-side relaxed struct
   (`#[serde(default)]` on a dedicated peers entry type), since the mirror cannot honestly fill
   transport/score for arbitrary remote peers; flip to mirror-side extension if the operator
   wants the richer wire format fleet-wide.

## What this does NOT defend (honesty section)

- **A compromised pinned authority**: whoever holds a trusted key can ban anything (fleet-wide
  censorship). Mitigation is minimal pinning + `expires_at` on bans, not a protocol fix.
- **Sybil originators**: unpinned keys get zero effect by design; this is pinned-authority
  trust, not reputation-weighted consensus.
- **Subject instability under unsigned announces**: until announce signing lands (prerequisite
  2), a banned mirror can re-announce under a fresh mirror_id; operator-level bans are the
  stable subject, mirror-level bans are best-effort.
- **Partition consistency**: this is an AP system; last-writer-wins per `(originator, subject,
  seq)`; two partitions may temporarily enforce different sets.
- **Automatic poisoning verdicts**: entries are operator/authority judgments; cross-check
  divergence stays report-only (parent lane sign-off), and nothing here auto-bans.
- **Auto-ban blast-radius amplification**: the tracker's auto-blacklist (3 hash-mismatch
  reports / 24 h → permanent ban, threshold hardcoded, no dedup) is triggerable through the
  **unauthenticated `/report`** route today (survey t_e1e4c07f). Tracker-local now, but under
  ANY propagation approach an unauth report flood gains fleet-wide reach. Prerequisite 1 gates
  the blacklist write path; the `/report` path needs its own auth or source-discipline fix in
  the same slice, or the auto path stays disabled until it does.

## Implementation estimate (post-pick)

Prereq slice (any approach): tracker auth on `/blacklist` writes + entry signature verification
+ announce npub/signature wiring, plus the client peers-schema repair (prerequisite 4 — needed
for A's client path, existing dead leg regardless) — S (~half a day, tests included). A: M
(~1–1.5 d: common entry types + verify helper, peers endpoint extension, merge + enforcement in
mirror, client discovery filter). B: M (~1 d). C: S–M (~0.5–1 d: tracker signing key + signed
export + poll/filter).

Anticipated touch points: `monopsony-common` (entry type + canonical-json/verify helper, moved
or re-exported from a shared home), `monopsony-tracker` (auth fix, signing key, export),
`monopsony-mirror` (`/peers` or snapshot endpoint, merge + enforcement, cross_check filter),
`monopsony-client` (discovery filter + peers-schema repair), `monopsony-server` main wiring,
`dist/server.toml.example` (`[peer_blacklist]`). Gates per the corrected parent note: workspace
tests green (baseline 1348/0/3 from the landed integrity tree), clippy = the 15-location
pre-existing set (reconcile by file:line, not count), fmt on touched files. No commits
(user-signed whitelist flow).

## Decision request

Pick one: **A** (peers gossip, tracker-anchored — recommended), **B** (signed snapshot
endpoint), or **C** (tracker-relayed only). Any pick also settles open questions 1–8 or flips
defaults. Implementation is dispatched separately after the pick.

Recorded pick (kanban t_1301e15a, 2026-10-31): **Approach A**, open questions at documented
defaults.
