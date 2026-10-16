# Sprint 27 — Client health & CheckMK integration (planned)

**Status:** Part A LANDED (server-only staleness detection + metrics + config knob, 995 tests
green, clippy clean). Part B still awaiting transport decision (see §6).

**Part A as landed:**
- `monopsony-control/src/client_health.rs` — pure drift decision (`is_stale`, boundary:
  age == threshold is stale; clock-step-back clamped at 0), `StaleTracker` episode state
  (counter bumps once per fresh→stale transition), `ClientHealthMonitor` 60s DB sweep
  (interval = threshold/4 clamped [5,60]).
- Heartbeat receipt (unary + control-stream paths) pins the client's age gauge to 0 and
  closes an open stale episode immediately.
- Metrics: `monopsony_client_last_heartbeat_age_seconds{client_id}`,
  `monopsony_clients_stale_total` (episodes), `monopsony_clients_stale_current`.
- Config: `[metrics].stale_threshold_secs` (default 900, 0 disables).
- Fixed in passing: control-stream per-message `MonopsonyGrpc` reconstruction dropped
  `with_telemetry` (config bypass) — now carried through.
- Fixed in passing: `streaming_tests.rs` pre-existing compile break from Sprint 26 Wave A
  (`run_proxy` 8th `hit_counter` arg not updated in the test harness).

**Goal:** give the CheckMK monitor visibility into monopsony-server + clients without turning
every client into a CheckMK agent, and surface *stale / failed* clients as the actually-useful
failure signals instead of raw liveness.

---

## 1. Why (checkmk.io findings)

Monopsony already defines a full Prometheus metric set (`monopsony-common/src/metrics.rs`)
and serves it at `/metrics` via `run_metrics_server()` (server/main.rs:816). Those are wired,
not stubbed — this is *observability plumbing*, not discovery. What CheckMK needs to see is:

| CheckMK ingestion path | Edition gate | Fits "clients stay off the monitor list"? |
|---|---|---|
| **SNMP v2c** (CMG pull) | all incl. Raw | No — docs say SNMP is slow + incomplete; wrong tool for cache/package metrics at volume. |
| **OVS HTTP push agent** (`/metrics` → Agent Controller :6559) | Ultimate only | Best fit conceptually, but paywalled. |
| **Active checks (HTTP poll / port check)** | all incl. Raw | Yes — zero client software; CheckMK polls a service/port on each host. Lowest-effort liveness. |

Verdict: wire Prometheus + label it so the server is one monitored host (active-check friendly),
and let OVS push be the path only if running Ultimate. No client-side agent regardless of edition.

## 2. What's already true (verified against code)

- `run_metrics_server()` live in `server/main.rs:816`; `/metrics` renders the global Prometheus
  exporter handle from `monopsony-control/src/metrics.rs`. Not a stub.
- Server aggregate metrics exist and are called throughout: cache hit/miss, bytes served/cached,
  prefetch counts, mirror sync, gRPC latency, connected/registered clients (`metrics.rs`).
- Per-client state today = `clients.last_seen` only. `db.heartbeat()` bumps it; no drift logic exists.
- Client heartbeat cadence = every 60s (client main.rs:1346); server unary + streaming control stream both exist.

## 3. Part A — DO NOW (server-only, no proto/client change)

**Goal:** expose staleness so CheckMK can alert on a hung client without any client rewrite.

1. **Drift threshold in the gRPC layer** (`monopsony-control/src/grpc.rs` `handle_heartbeat`):
   compare `Utc::now() - req.last_seen_age` against a config-tunable threshold; maintain a
   counter of stale clients (the request carries nothing today, so derive from stored row + now).
2. **Prometheus gauges/counter** (reuse existing metric names in `metrics.rs`, add one):
   - `monopsony_client_last_heartbeat_age_seconds{client_id}` — how long since the last heartbeat.
   - `monopsony_clients_stale_total` counter bump when a client is now past threshold.
3. **Config knob** (`server.toml`): `stale_threshold_secs` (default 900, = 15× heartbeat interval).
4. **Tests**: unit test the drift decision + a gRPC handler-level stale-count test (950-test suite
   must stay green; workspace has `-D warnings`).
5. **CheckMK wiring**: label by client so one host aggregates all clients OR each is its own host —
   decide at landing based on fleet size (§7).

## 4. Part B — DEFERRED (needs a design decision, see §6)

**Goal:** report *failed updates* and richer health from the client. Today nothing reports update
outcomes back to the server: shadow `update_history` records intent only; auto-update failures are
swallowed (`client/main.rs:1546`).

Two candidate transports — trade-off below, awaiting user pick:

| Option | Description | Pro | Con |
|---|---|---|---|
| **A. Extend existing stream/unary** (add fields to `HeartbeatRequest`/`ClientMessage`, e.g. `failed_updates{since}` + `last_update_attempt_ts`) | Reuses the 60s heartbeat cadence; no new RPC surface, no extra failure mode | Fewer moving parts; low blast radius | Fields must be added to proto + DB table `client_update_failures` + client counters (3 crates); counter windowing needed so it doesn't grow unbounded |
| **B. New optional RPC** (`report_update_outcome`) | Clean, explicit contract; easy future extension (push notifications, paid features) | Bigger blast radius; another failure mode to handle; more moving parts | Unblocks structured push + paid-server telemetry later |

Recommended default: **A**, because staleness is already carried by `last_seen`, and adding a
counter windowed per heartbeat interval keeps the DB bounded. B only if Part A lands cleanly AND
push/paid features are in this sprint's critical path (per §6).

### 4a. Field shape must fit Sprint 24 privacy levels now (not at Sprint 24)

Sprint 24 ships `privacy_level = counts-only | names-only` for exactly this telemetry. So Part B
fields should be tiered by client config so they slot under that policy without re-architecting:

| Tier | What the field carries | Example value |
|---|---|---|
| **counts-only** (`privacy_level = counts-only`, default on public) | error *rate* per host bucket only, no package identity | `failed_updates_total{host="x"}=3` (windowed) — but NOT which packages failed |
| **names-only** (`privacy_level = names-only`) | same, plus the failing package name(s) | `failed_updates_total{name="foo-1.0", host="x"}=2` |

Design rule: the *transport* stays lean (Option A, windowed heartbeat field), but the payload is
gated by the client's `privacy_level`. Public deployments default to counts-only; names/full are
unavailable until a public-server hardening gate lands (§5). This pre-binds Part B to Sprint 24 so
the privacy tier exists before the data does.

### 4b. Security/privacy policy — full metrics + push limited to private/tailnet servers only

Per user decision (Sprint 27, Part B): **rich telemetry and any server→client push are permitted on
private or tailnet deployments only.** Public deployments must be gated down to `counts-only` until a
robust security/privacy enhancement lands.

**Threat model driving this:** the update proxy is a client-facing code path; a server (or a client
it trusts) could *push* known malware / malicious metadata through that channel without user request —
a supply-chain attack vector over the telemetry/push surface on untrusted networks. On private/tailnet
servers the trust boundary is acceptable (operator-controlled); on public servers it is not.

Consequences:
- Feature-flag `rich_telemetry_enabled` (default **off**): gates names-only/full metrics + push notifications.
  - Private/tailnet deployments set it **on**.
  - Public deployments leave it **off** → only counts-only, no push-to-client ever.
- **Hard gate on public exposure:** a server advertising `public = true` may not enable
  `rich_telemetry_enabled`, and Phase 6 push notifications require the private/tailnet trust boundary
  (same as this policy). This hard-gates Sprint 24's JWT trust broker + Phase 6 push against public
  deployments.
- The flag is *not* a per-request runtime toggle; it's a deployment-class gate evaluated at
  startup/config, because the failure mode is code execution on clients — there is no safe "on-demand"
  flip for that vector.

## 5. Security / privacy notes

- Staleness exposes hostname + last_seen — both already flow server-side over mTLS+API-key, so **no
  new privacy surface** vs today; safe to push to CheckMK.
- Part B update-failure counts are operational only (not sensitive). If exposed, keep client label
  hashed/derived rather than raw hostname.

## 6. Open question for user

Part A is approved. Part B transport (A extend-existing vs B new-RPC) needs a call — see table in §4.

## 7. Landing / gating

- Workspace suite must stay green + clippy `-D warnings` clean; target ~950 tests maintained.
- CheckMK edition check before choosing OVS-push vs active-check ingestion.
