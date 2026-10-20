# Pairing Security — Monopsony v2 (Sunshine model)

How clients establish trust with a Monopsony server, what each pairing mode
protects against, and where the boundaries are.

## Pairing modes

Configured per-server in `[dashboard] pairing_mode`:

| Mode | Flow | Use when |
|------|------|----------|
| `pin` (default) | Client generates a PIN + ephemeral keypair, submits a request; admin approves from the dashboard by entering the PIN; client collects the cert with a possession proof | Default for new installs; required for funnel/public exposure |
| `legacy-code` | Server mints a 6-char code; client presents it on plaintext port 50052 in a single exchange | Trusted LAN/tailnet quick path only |
| `off` | Pairing disabled | Locked-down fleets; add clients via enrollment keys before locking |

All modes issue **90-day client certificates** (configurable validity). The
daemon auto-renews inside the final 30 days over the existing mTLS connection —
no human involvement. Certificates are individually revocable by serial
(dashboard or `client_certs` table); revocation is enforced at the TLS accept
path, covering both direct gRPC and `/ws/tunnel`-relayed connections.

## The three client-side flows

```
monopsony-client pair --server HOST:3130 --pin            # v2 Sunshine
monopsony-client pair --server HOST:50052 --code ABCDEF   # legacy (LAN only)
monopsony-client pair --server HOST:3130 --enrollment-key mopk-...  # fleet
```

### pin (v2 Sunshine)

1. Client generates an ephemeral Ed25519 keypair and a 6-char PIN
   (`ABCDEFGHJKLMNPQRSTUVWXYZ23456789`, no ambiguous glyphs) and **displays
   the PIN locally**.
2. Client submits `{claimed hostname/os, ephemeral pubkey, SHA-256(salt+PIN)}`
   to `POST /api/v1/pair/request` — unauthenticated but rate-limited. The wire
   never carries the PIN.
3. The request appears on the dashboard with the **observed** source IP (from
   the socket, or `X-Forwarded-For` only from configured trusted proxies).
4. Admin enters `request_id` + PIN → approve. Three wrong PINs invalidate the
   request. Pending requests expire after 15 min; approved-but-uncollected
   after 5 min. Repeat approves are idempotent.
5. Client polls, then collects: signs `request_id` with the ephemeral private
   key (proof of possession). The server issues the cert only to the holder of
   that key. Registration happens **at collection**, not before.
6. Client TOFU-pins the CA fingerprint (`ca.fingerprint` in the cert dir) and
   warns on any change.

### enrollment-key (fleet / automation)

`POST /api/v1/pair/enroll` with a `mopk-…` key (stored server-side as SHA-256
only), ephemeral pubkey, and a possession signature over
`monopsony-enroll-v1`. Single round-trip: validate key → verify signature →
issue cert → bump use count → audit. Keys are bounded by use-count and expiry
and revocable from the dashboard. Same 90-day certs, same renewal, same
revocation.

## Topology matrix

| Topology | Recommended mode | Notes |
|----------|------------------|-------|
| LAN / tailnet (trusted perimeter) | `pin` or `legacy-code` | `legacy-code` is acceptable here; still firewall :50052 to the local segment |
| Funnel / public IP / reverse proxy | `pin` only | D7: approval requires dashboard login even under TailnetAutoTrust. Configure `trusted_proxies` so observed-IP display and rate limiting cannot be spoofed by a forged `X-Forwarded-For` |
| Fleet 20–200 clients | enrollment keys | Ansible-friendly; the fourth client on a 3-use key is rejected |
| Offline/air-gapped | enrollment key w/ short expiry | Generate, distribute out-of-band, let it lapse |

**Firewall guidance:** port 50052 (legacy pairing listener) binds plaintext
and accepts any connection holding the current code. If you never use
`legacy-code`, do not expose 50052 anywhere — block it at the host firewall.
The admin API (3130) and gRPC (50051) should be behind mTLS/auth as deployed.

## What the PIN defends — and what it does not

**Defends:**
- Unauthenticated code minting (the v1 failure: `POST /pair/init` returned the
  code to anyone). v2 has no server-side code to steal.
- Blind request flooding binding certs to attacker machines: every issuance
  requires a human who can see the client's PIN over a verified channel.
- Wrong-target approval: the PIN is row-bound to one request; the dashboard
  shows observed IP next to claimed hostname so the admin can spot a mismatch.
- MITM at pair time learning anything useful: the PIN hash is salted
  client-side; the cert is only handed to the ephemeral-key holder.

**Does NOT defend:**
- **A stolen admin session.** The attacker submits their own request, knows
  their own PIN, and approves it. The PIN binds admin intent to a request; it
  is not an admin second factor. Protect the dashboard session (the login has
  lockout + rate limiting; keep it that way).
- **A social-engineered side channel.** If the admin accepts PINs over chat
  from whoever asks, this degrades to the old shared-code scheme. The PIN
  assumes a verified out-of-band channel: in person, known voice, or an
  established device relationship.
- **A compromised tailnet node in serve mode with `always_require_login`
  unset.** That node IS the admin (TailnetAutoTrust). Set
  `always_require_login = true` when the tailnet itself is not fully trusted.
- **Client-endpoint compromise after pairing.** That is what revocation is
  for (dashboard → client → revoke), not pairing.
- **MITM swapping the CA at first pairing.** Mitigated, not eliminated, by
  TOFU pinning + possession proof; full elimination needs out-of-band CA
  fingerprint verification. Documented, not forced.

## Operational notes

- Pairing requests, approvals, denials, and enrollments are auditable:
  `GET /api/v1/pair/audit` (approver attribution is currently `system`/
  `enrollment-key`; session-username attribution is a known gap).
- Enrollment keys are shown once at creation. Lose the plaintext → revoke and
  create a new key.
- The m2mini/state-asymmetry bug class (server believes pairing succeeded
  while the client hangs) is closed by registration-at-collection: a pending
  or approved request that never collects simply expires; no DB row exists
  until the client proves possession.
- Renewal is silent: 90-day certs renew inside the final 30 days over mTLS.
  If a client goes offline past expiry, delete its certs and re-pair (or
  issue an enrollment key and run `pair --enrollment-key`).
