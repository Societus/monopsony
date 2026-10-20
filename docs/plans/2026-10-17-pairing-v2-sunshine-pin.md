# Pairing v2 — Sunshine-style admin approval with client PIN

Date: 2026-10-17
Companion artifacts:
- Red-team findings with file:line evidence: `context/2026-10-17-pairing-redteam-sunshine-pin.md`
- Roadmap entry: `docs/sprint-roadmap.md` → Sprint 28

## Problem / proven failure

Current pairing (all claims verified against the tree, 2026-10-17):

1. `POST /api/v1/pair/init` (`crates/monopsony-admin/src/pairing.rs:105`) mints a
   6-char code and returns it in the JSON body to ANY caller — no auth on the
   minting endpoint.
2. The pairing listener binds `0.0.0.0:50052` plaintext; the code is the only
   gate (pairing.rs:345). Holding it = DB registration + mTLS client cert +
   full gRPC control (ProxyConfigActions, RESYNC, UPDATE_CONFIG, RESTART) —
   root-equivalent takeover.
3. `/ws/pair` relay (`crates/monopsony-admin/src/ws_tunnel.rs:56`) is
   deliberately unauthenticated ("the pairing code itself is the credential") —
   same exposure class through reverse proxies.
4. Client certs hard-code `not_after = 2036-01-01`
   (`crates/monopsony-control/src/tls.rs:376`); no serial tracking, no CRL, no
   revoke. A compromised pairing is a decade-long credential; the only remedy
   is CA rotation — the UnknownIssuer outage class already debugged once.
5. No rate limiting anywhere on the admin API (:3130) — the Phase 1
   token-bucket covers the mirror router (`monopsony-mirror/public_rate_limit.rs`)
   only. Login has no lockout/attempt limit.
6. `PairRequest` carries no nonce or public key (`proto/monopsony.proto:130-136`)
   — the cert goes to whoever holds the pairing connection.
7. One serialized session (second init → 409); DB registration happens before
   the client completes (the m2mini state-asymmetry bug class: server believes
   pairing succeeded while the client hangs).
8. No dashboard UI for pairing; the SPA holds no login session for write ops.

The production pairing path pairs clients over the public internet before
lockdown, so the pairing phase is the exposed window — not a LAN-only
hypothetical.

## Decisions log

| # | Decision | Rationale |
|---|----------|-----------|
| D1 | Sunshine model: client-initiated request, admin approves on auth-gated dashboard by entering a client-displayed PIN | Kills unauth code minting; binds admin intent to one specific request (user directive) |
| D2 | PIN generated CLIENT-side, displayed locally; request carries only a salted hash; approval row-bound (request_id + PIN), 3-strike invalidation | Wire never carries the PIN → MITM learns nothing; loose "match any request" lookup invites mis-binding (red-team F1) |
| D3 | PairRequest v2 carries a client ephemeral pubkey; cert delivered only on proof of possession | Closes the approve→collect TOCTOU race; Sunshine's actual property (F2) |
| D4 | Client certs 90d + silent renewal over mTLS; serials in DB; revoke API + gRPC accept-path check | 2036 certs are unusable lifetimes; renewal IS the tiered self-service re-pair (F3) |
| D5 | `pairing_mode` config: `pin` (new default) / `legacy-code` (kept, trusted-LAN quick mode) / off | Fleet/automation + bench need the fast path; legacy gets firewall guidance, not deletion |
| D6 | Enrollment keys (use-count + expiry, Tailscale-auth-key pattern) for fleet scale | 20–200 clients cannot be click-approved one-by-one; also the Ansible path |
| D7 | Approval requires login even under TailnetAutoTrust (default on); approver identity logged | A compromised tailnet node otherwise self-approves (F6) |
| D8 | Pending requests in-memory only, cap 50, TTL 15 min, per-IP limits; identical claimed identity + same source replaces its earlier pending request (dedup); DB write only at approved collection | Flood/DB-bloat DoS (F4, F9); fixes the register-before-complete asymmetry (problem 7); dedup stops the admin-facing duplicate-row confusion attack (F4) |
| D9 | Observed identity (peer IP / tailnet node) displayed above claimed identity; XFF honored only from configured trusted proxies | Self-attestation is forgeable; XFF spoofing behind CF/Caddy (F5) |

## Trust / architecture model

New flow:

1. Client generates an ephemeral keypair + 6-char PIN (charset
   `ABCDEFGHJKLMNPQRSTUVWXYZ23456789`), displays the PIN locally.
2. Client submits a pairing request: {claimed hostname/os/PMs, ephemeral
   pubkey, salted PIN hash} → unauthenticated but rate-limited intake
   (TCP :50052 v2 frame or `/api/v1/pair/request`; `/ws/pair` relay equivalent).
3. Server stores the request as pending (in-memory, capped, TTL) and surfaces
   it on the dashboard with the OBSERVED source.
4. Admin (Session auth; escalated per D7) enters request_id + PIN → approve.
   Three wrong PINs invalidate the request.
5. Client polls / long-polls; on approval the server issues the cert bound to
   the request's ephemeral key; the client proves possession, receives the
   cert bundle + client_id; DB registration happens HERE, not before.
6. The daemon renews over mTLS inside the renewal window — no human involved.

Topology matrix:

- LAN / tailnet serve: trusted perimeter; `legacy-code` or `pin` both
  acceptable; firewall :50052 guidance stands.
- Funnel / public IP / reverse proxy: `pin` mode (default); D7 escalation;
  trusted-proxy XFF config required for observed-IP display.
- Enrollment key: any topology; key = pre-approval; issuance still key-bound.

## Phased implementation

### Phase 0 — immediate risk reduction, no protocol change [~1d]
- Token-bucket per-IP rate limiting on the admin router (reuse the
  `public_rate_limit.rs` pattern from monopsony-mirror), plus a global bucket
  (XFF is not yet trusted-proxy-gated — per-IP alone is evadable by header
  rotation until Phase 2).
- Login lockout: 5 fails / 15 min per username+IP.
- Wire real session validation into `auth_middleware` — today ANY non-empty
  session cookie passes (auth.rs checks existence only; the `SessionStore` is
  never consulted, TODO at auth.rs:610). Lockout is theater until this lands
  (red-team F7: "the approval gate is only as strong as that session").
- `dist/install.sh`: replace hardcoded `VERSION="0.1.0"` with a tag-derived
  version (proven failure: re-run fetches nonexistent v0.1.0). Resolution
  order: `--version` flag > `MONOPSONY_VERSION` env > latest release tag from
  the repo API > pinned fallback.
- Release CI on the PUBLIC GitHub mirror (Societus/monopsony) — Forgejo is a
  local server behind forward auth and cannot host `curl | bash` downloads.
  `DOWNLOAD_BASE` repoints to GitHub Releases. The asset-name contract is
  currently broken both ways: installer requests
  `monopsony-server-v${V}-${platform}.tar.gz`, CI publishes
  `monopsony-${V}-${platform}.tar.gz` (tag already carries `v`). Fix to the
  per-binary contract: `monopsony-{server,client}-v${V}-${platform}.tar.gz`
  for linux x86_64/aarch64 + darwin arm64/x86_64.
Gate: rate-limit + lockout + session-validation tests green; installer
downloads a real client tarball from a scratch tag on the GitHub mirror.

### Phase 1 — cert lifecycle [~2-3d]
- tls.rs: 90-day client cert validity (configurable), serial assignment.
- DB: `client_certs` table (serial, client_id, issued_at, expires_at, status)
  migration + CRUD.
- Revoke API + gRPC accept-path check (cached revoked-serial set). The check
  must demonstrably cover `/ws/tunnel`-relayed connections too — the WS relay
  carries the same mTLS handshake bytes, so the check belongs in the TLS
  accept path, with a test exercising the WS route (red-team F3).
- Daemon renewal over mTLS (re-issue inside the 30-day window).
Gate: renewal + revoke proven on the .58 bench (direct gRPC AND via
`/ws/tunnel`); UnknownIssuer regression suite still green.

### Phase 2 — pairing protocol v2, server [~3-4d]
- proto: PairRequest v2 fields (ephemeral pubkey, PIN hash); new
  PendingPairInfo / ApprovePairRequest / collect-challenge messages.
- pairing.rs rewrite per D2/D3/D8: intake (rate-limited, in-memory pending,
  cap/TTL, dedup-replace on same claimed identity + source), `GET
  /api/v1/pair/pending` (auth), `POST /api/v1/pair/approve` (auth, row-bound,
  3-strike, idempotent per request — repeat approve of the same request is a
  no-op, not an error), key-bound issuance, registration-at-collection.
  Lifecycle semantics (red-team F8): approved-but-uncollected requests expire
  on a shorter clock than pending TTL; PIN single-use after successful
  collection. PIN hash salt is CLIENT-generated random (sent alongside the
  hash) — the request cannot wait on a server nonce, and equal PINs must not
  produce equal hashes.
- `/ws/pair` relay caps; XFF trusted-proxy handling; D7 escalation +
  approver audit log.
- `pairing_mode` config; legacy path preserved.
Gate: integration tests — happy path, wrong-PIN×3, race (second collector
rejected), flood (cap enforced), TTL expiry, dedup (resubmission replaces,
does not duplicate).

### Phase 3 — client pair v2 [~2d]
- grpc_client.rs: v2 flow (keypair + PIN generation/display, submit, poll,
  possession proof, chain verification), TOFU CA-fingerprint pinning
  (store + warn on change).
- `--code` legacy flag retained for legacy-mode servers.
Gate: fresh client pairs against a pin-mode server on the bench (manual
approve); legacy path still pairs against a legacy-mode server.

### Phase 4 — dashboard UI [~2-3d]
- SPA login session for write ops (currently absent).
- Clients page: pending list (observed ≥ claimed), approve-with-PIN dialog, deny.
- Client list: cert status (valid / expiring / revoked), revoke action.
- Approval audit view.
Gate: the full approve loop driven purely from the dashboard on the bench.

### Phase 5 — enrollment keys [~1-2d]
- Authed API: create/list/revoke keys (use-count, expiry, label; stored hashed).
- Client `pair --enrollment-key` (pre-approved; issuance still key-bound).
- Dashboard key manager.
Gate: three clients paired non-interactively with a 3-use key; the fourth
attempt is rejected.

### Phase 6 — installer + setup [~2d]
- install.sh client path: binary download default (curl + openssl prereqs
  only, no cargo), guided pairing showing the PIN flow (non-fatal decline →
  clean installed-not-paired state), PM config + init placement deferred to
  `monopsony-client setup --interactive`.
- `setup --interactive`: PM discovery (get_available_backends) → selection →
  ProxyConfigActions → init standard/custom placement; idempotent, re-runnable.
- Uninstall: also runs proxy-config removal.
Gate: fresh LXC install → pair → proxy one package end-to-end, from the
doc-following user's perspective.

### Phase 7 — docs, skills, threat model [~1-2d]
- `docs/pairing-security.md`: topology matrix, what the PIN does and does not
  defend, side-channel assumption, firewall guidance.
- Update AGENTS.md pairing notes + the `monopsony-client-server-flow` skill.
Gate: a fresh session reads the docs only and runs the full flow without
re-derivation.

Runnable-early: Phase 0 has no dependencies (pure risk reduction). Phases 6–7
partially parallelize once Phase 3 lands. P2 depends on P1 serials; P4 depends
on P2; P6 depends on P3.

## What this does NOT defend

- Phase 0 residual: until trusted-proxy XFF gating (Phase 2, D9), a direct
  attacker can send a forged `X-Forwarded-For` matching the admin's IP and
  fail 5 logins, locking the (admin, admin-IP) pair for 15 min — a bounded,
  reversible admin-lockout DoS. Accepted for the interim; the global rate
  bucket bounds the abuse volume.
- A stolen admin session: the attacker submits their own request, knows their
  own PIN, approves it themselves. The PIN binds admin intent to a request; it
  is not an admin second factor.
- A social-engineered side channel: if the admin accepts PINs over chat from
  whoever asks, this degrades to today's shared-code scheme. The PIN assumes a
  verified out-of-band channel (in-person, known voice).
- A compromised tailnet node in serve mode with D7 disabled: that node IS the
  admin.
- Client-endpoint compromise after pairing: handled by revocation (Phase 1),
  not by pairing itself.
- MITM at pair time swapping the CA: mitigated, not eliminated, by TOFU
  pinning + key-bound issuance; full elimination needs out-of-band CA
  verification — documented, not forced.

## Open questions (defaults assumed; flip cheaply)

1. Keep `legacy-code` as the default mode until the Phase 4 dashboard loop
   exists, then flip default to `pin`. (Assumed: yes.)
2. PIN format: 6-char unambiguous alphanumeric (32^6 ≈ 1.1e9) vs Sunshine-style
   4-digit numeric. (Assumed: 6-char.)
3. Pending TTL 15 min / cap 50. (Assumed: as stated.)
4. Cert validity 90d, renewal window 30d. (Assumed: as stated.)
5. Enrollment keys: use-count only, or also hostname-prefix pinning?
   (Assumed: use-count only for v1.)
6. Does legacy mode stay long-term or get a deprecation date?
   (Assumed: stays, documented as trusted-perimeter-only.)
