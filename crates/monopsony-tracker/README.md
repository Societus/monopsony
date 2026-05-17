# monopsony-tracker

Decentralized mirror discovery tracker for Monopsony. Lightweight HTTP service where mirrors announce themselves and clients discover nearby mirrors. SQLite-backed, single binary.

## What It Does

The tracker is a discovery service (similar to BitTorrent's tracker protocol). Mirror operators announce their servers; clients query for nearby mirrors based on distro, geo-location, and quality scoring.

## Running

```bash
# Basic (open auth, default port 3130)
monopsony-tracker

# With token auth
monopsony-tracker --auth-mode token --api-keys "secret1,secret2"

# With JWT auth
monopsony-tracker --auth-mode jwt --jwt-issuers "trustbroker"

# With V4V platform mode
monopsony-tracker --v4v-enabled

# Custom bind and DB path
monopsony-tracker --addr 0.0.0.0:3130 --db /var/lib/monopsony/tracker.db
```

### CLI Flags

| Flag | Default | Description |
|------|---------|-------------|
| `--addr` | `0.0.0.0:3130` | Bind address |
| `--db` | `tracker.db` | SQLite database path |
| `--stale-timeout` | `7200` | Purge mirrors older than N seconds |
| `--v4v-enabled` | `false` | Enable V4V bounty/governance tables |
| `--auth-mode` | `open` | Auth mode: `open`, `token`, or `jwt` |
| `--api-keys` | - | Comma-separated API keys (token mode) |
| `--jwt-issuers` | - | Comma-separated trusted JWT issuers (jwt mode) |

## API Reference

### Mirrors

**POST /announce** -- Register/update a mirror.

```json
{
  "mirror_id": "mirror-1",
  "distro": "archlinux",
  "transport": ["http"],
  "endpoints": ["http://mirror1.example.com:3129"],
  "capacity_bytes": 500000000000,
  "bandwidth_bps": 104857600,
  "packages": 12000,
  "lat": 33.5,
  "lon": -112.0,
  "operator_npub": "npub1..."
}
```

**GET /discover?distro=archlinux&lat=40.7&lon=-74.0&limit=10** -- Discover mirrors. Returns scored and ranked results. No auth required.

**GET /health** -- Health check. Returns `ok`.

### Reports

**POST /report** -- Report a mirror issue (hash mismatch, slow response, etc).

```json
{
  "mirror_id": "mirror-1",
  "report_type": "hash_mismatch",
  "details": "SHA256 mismatch for package xyz"
}
```

Report types: `hash_mismatch`, `slow_response`, `unreachable`, `stale_data`, `other`.

**GET /reports** -- List recent reports.

### Blacklist

**POST /blacklist** -- Manually blacklist a mirror or operator.

```json
{
  "mirror_id": "mirror-1",
  "reason": "serving malicious packages",
  "expires_at": 1735689600
}
```

Either `mirror_id` or `operator_npub` is required.

**GET /blacklist** -- List blacklist entries.

**DELETE /blacklist/{id}** -- Remove a blacklist entry.

Auto-blacklist triggers when a mirror accumulates 3+ `hash_mismatch` reports within 24 hours.

### Reputation

**GET /reputation/{npub}** -- Get operator reputation score (0-100, default 50).

**POST /reputation/dispute** -- Open a dispute against an operator.

**POST /reputation/attest** -- Attest to resolve a dispute.

### Operator Identity

Mirrors identify their operator via a Nostr-style `npub` (Ed25519 public key hex). Operators are auto-created on first announce. Reputation events (announce, report, dispute, attest) adjust the score.

## Scoring Algorithm

Mirrors are scored on a 0-100 scale using weighted factors:

| Factor | Weight | Metric |
|--------|--------|--------|
| Proximity | 40% | Haversine distance (geo-location) |
| Freshness | 30% | Time since last sync |
| Reliability | 20% | Uptime duration |
| Capacity | 10% | Bandwidth |

NAT type applies a penalty: Public/FullCone = 0, RestrictedCone = -5, PortRestrictedCone = -10, Symmetric = -20, Unknown = -8.

Blacklisted mirrors are excluded from discovery results entirely.

## Graceful Shutdown

The tracker handles SIGINT (Ctrl+C) with `with_graceful_shutdown`. In-flight requests drain before the server exits.

## Architecture

```
monopsony-tracker/
  src/
    main.rs       -- Router, CLI, handlers, schema, scoring
    auth.rs       -- Auth middleware (open/token/jwt)
    report.rs     -- Report protocol handlers
    reputation.rs -- Reputation scoring and disputes
    stun.rs       -- NAT type detection integration
    v4v.rs        -- Conditional V4V schema (bounty/governance)
```

Database: SQLite via rusqlite. In-memory for tests, file-based for production.

## V4V Mode

When started with `--v4v-enabled`, the tracker creates additional tables for the Value-for-Value platform:

- `bounty_pool` -- Sponsor-funded audit bounties
- `bounty_delegations` -- ARK-style delegation to champions
- `bounty_proposals` -- Audit bounty proposals
- `audit_badges` -- Audit completion tracking

These tables are dormant and not used until post-v1.0.
