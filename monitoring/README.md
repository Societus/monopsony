# Monopsony monitoring pack (Sprint 24)

Grafana dashboard + Prometheus alert rules for the metrics endpoint
monopsony-server already exposes (default `0.0.0.0:9090`, path `/metrics`;
`--metrics-addr` flag to move it). Metrics-only — no code changes.

Everything here is grounded against the metric inventory actually emitted by
the tree: **every metric name and label value used in these files exists in
`crates/`** (grounding table below). Metrics we wish existed but don't are
listed in [Metric gaps](#metric-gaps), not invented.

```
monitoring/
├── grafana/monopsony-overview.json   # dashboard: 24 timeseries + 5 stat panels
├── prometheus/alerts.yml             # 23 alert rules in 5 groups (promtool-validated)
└── README.md                         # this file
```

## Import the dashboard

1. Grafana → **Dashboards → New → Import**, upload
   `grafana/monopsony-overview.json` (or paste the JSON).
2. Select your Prometheus datasource in the `Prometheus` dropdown — the
   dashboard ships with a templated datasource, so it imports cleanly into
   any Grafana ≥ 9 (schemaVersion 39).
3. Template variables: **Distro** and **Client** auto-populate from the live
   series (`label_values`), both default to *All*.

## Wire the alert rules

1. Scrape the exporter — add to `prometheus.yml` (the alert
   `MonopsonyTargetDown` assumes job label `monopsony`; align either side):

   ```yaml
   scrape_configs:
     - job_name: monopsony
       static_configs:
         - targets: ['monopsony.example.internal:9090']
   ```

2. Load the rules file:

   ```yaml
   rule_files:
     - /etc/prometheus/alerts/monopsony-alerts.yml   # copy alerts.yml here
   ```

3. Validate and reload:

   ```sh
   promtool check rules /etc/prometheus/alerts/monopsony-alerts.yml
   curl -X POST http://prometheus:9090/-/reload      # needs --web.enable-lifecycle,
   # or: kill -HUP <prometheus-pid>                  # or restart the service
   ```

4. Optional: route by the `severity` label — `critical` pages, `warning`
   tickets, `info` is context only.

The metrics port also serves `/health` (JSON: status, database, uptime) and
`/stats` (JSON: cache size/files, client count, uptime) — useful for
blackbox/uptime checks that don't need Prometheus.

## Alerts

| Alert | Sev | Fires when | Meaning | First response |
|---|---|---|---|---|
| `MonopsonyTargetDown` | critical | scrape fails 3m | Server down or :9090 blocked | Check the server unit; curl :9090/metrics from the Prometheus host |
| `MonopsonyCacheHitRateLow` | warning | hit rate < 50% over 30m (with traffic) | Slow SLO burn: cold cohort, pattern gap, or upstream churn | Compare miss growth vs prefetch; check URL-pattern coverage |
| `MonopsonyCacheHitRateCritical` | critical | hit rate < 20% over 5m (with traffic) | Fast burn: eviction, cache loss, traffic shift | Check `cache_entries_current` + evictions |
| `MonopsonyCacheEvicting` | info | any retention eviction in 30m | Disk-budget retention deleting blobs | Resize cache budget before hit rate degrades |
| `MonopsonyScrubQuarantine` | critical | scrubber quarantined a blob | **Tamper/corruption alert**: stored content no longer matches its hash; blob withheld (fail-safe) | Identify blob in scrub report, re-fetch from upstream, fsck the disk |
| `MonopsonyMirrorHashMismatchFailClosed` | critical | sync hash mismatch, fail-closed policy | Refused upstream content under `distro` | Re-sync; compare against upstream checksums manually if it repeats |
| `MonopsonyMirrorHashMismatchFailOpen` | warning | hash mismatch served under fail-open | Suspect content may be reaching clients | Verify the file vs upstream; tighten policy if recurring |
| `MonopsonyMirrorSignatureVerifyFailed` | critical | metadata signature `fail`/`fail_open` | Repo metadata failed signature verification | Refresh trusted keys, re-sync, cross-check a second upstream |
| `MonopsonyMirrorSignatureNoKeys` | warning | signature check skipped (`no_keys`) | Distro metadata integrity unattested | Install upstream signing keys in mirror trust config |
| `MonopsonyCrossMirrorDivergence` | warning | peer cross-check `divergence` | Two mirrors disagree — one is stale or tampered | Diff disputed paths vs upstream; re-sync the laggard |
| `MonopsonyMirrorSelfBan` | critical | self-ban detected | **This** mirror banned by a trusted tracker blacklist originator | Stop serving, audit recent content, contact originator |
| `MonopsonyMirrorSyncFailure` | warning | any sync `error` in 15m | One failed sync; stale-good-copy still serving | Read sync error in logs (reachability/space/throttle), re-trigger |
| `MonopsonyMirrorSyncFailing` | critical | ≥5 sync errors in 2h | Sync path broken, not flaky | Incident: upstream status, disk capacity, key expiry |
| `MonopsonyMirrorSyncStalled` | warning | no successful sync in 26h | **Index staleness**: served index aging past one daily cycle | Manual sync; retune the 26h window if cadence changed |
| `MonopsonyClientsStale` | warning | any client past staleness threshold 5m | Client(s) missed ≥15 heartbeats (default 900s threshold) | `topk` the heartbeat-age gauge; ping host, check client unit |
| `MonopsonyClientsStaleCritical` | critical | ≥50% of connected clients stale | Shared cause: control plane, LAN, bad rollout | Check server gRPC logs + recent client rollouts before walking the fleet |
| `MonopsonyClientHeartbeatAgeHigh` | warning | oldest heartbeat > 1800s | Escalation past stale: silently gone for 2× threshold | Named by `topk(5, heartbeat_age)`; host up? daemon running? |
| `MonopsonyClientUpdateFailures` | warning | per-client failed-update gauge ≥ 3 | Repeated failed update attempts on one client | Check client update logs + attempt-age gauge |
| `MonopsonyClientUpdateAttemptsStalled` | info | no update attempt in 48h | Quiet-by-policy is fine; quiet-by-hang is not | Confirm the client's `[update]` policy intends this |
| `MonopsonyUpdateHashVerifyFailClosed` | critical | update asset sha256 refused (`mismatch`/`unlisted`) | **Update verification failing closed**: distributor refused a release asset | Compare upstream release page vs logs; suspect channel if unlisted assets repeat |
| `MonopsonyUpdateSignatureFailClosed` | critical | SHA256SUMS signature refused | Update metadata unsigned/mismatched — never bypass to get unstuck | Check for upstream key rotation; refresh trusted key |
| `MonopsonyUpdateSignatureNoKey` | warning | signature check ran with no trusted key | Update channel has no signature gate | Install the update-signing public key |
| `MonopsonyUpdatePollStalled` | warning | zero poll outcomes in 12h | Update-distributor loop wedged or dead | Check distributor logs; confirm poll interval; restart if loop died |

## Metric grounding table

What the exporter actually emits. `labels` shows every value the code can
produce. "—" = no labels.

| Metric | Type | Labels (values) | Emitted by |
|---|---|---|---|
| `monopsony_cache_requests_total` | counter | `result` = hit \| miss | monopsony-proxy/src/proxy.rs |
| `monopsony_cache_bytes_served_total` | counter | — | monopsony-common/src/metrics.rs (helper `cache_bytes_served`) |
| `monopsony_cache_bytes_cached_total` | counter | — | same |
| `monopsony_proxy_connect_bytes_total` | counter | — | CONNECT tunnel relay |
| `monopsony_cache_entries_current` | gauge | — | cache entry count setter |
| `monopsony_cache_evicted_total` | counter | — | monopsony-proxy/src/proxy.rs (retention over budget) |
| `monopsony_scrub_verified_total` | counter | — | monopsony-proxy/src/proxy.rs (scrub pass, count per report) |
| `monopsony_scrub_quarantined_total` | counter | — | monopsony-proxy/src/proxy.rs (scrub quarantine) |
| `monopsony_clients_connected_current` | gauge | — | server |
| `monopsony_clients_registered_total` | counter | — | server |
| `monopsony_packages_cached_current` | gauge | `manager` | server (per-PM) |
| `monopsony_packages_prefetched_total` | counter | `manager` | prefetch |
| `monopsony_packages_purged_total` | counter | `manager` | purge |
| `monopsony_grpc_requests_total` | counter | `method` | monopsony-control/src/grpc.rs |
| `monopsony_grpc_request_duration_seconds` | histogram | `method` (+`_bucket`/`_sum`/`_count`) | same |
| `monopsony_client_last_heartbeat_age_seconds` | gauge | `client_id` | monopsony-control/src/client_health.rs (sweep + heartbeat ingress) |
| `monopsony_clients_stale_total` | counter | — | same (fresh→stale episodes) |
| `monopsony_clients_stale_current` | gauge | — | same |
| `monopsony_client_failed_updates` | gauge | `client_id` | Sprint 27 Part B (windowed count) |
| `monopsony_client_last_update_attempt_age_seconds` | gauge | `client_id` | same (skipped when client never attempted) |
| `monopsony_mirror_sync_total` | counter | `distro`, `result` = success \| error | monopsony-mirror/src/mirror.rs |
| `monopsony_mirror_sync_duration_seconds` | histogram | `distro` (+buckets) | same |
| `monopsony_mirror_sync_bytes_total` | counter | `distro` | same |
| `monopsony_mirror_sync_files_total` | counter | `distro` | same |
| `monopsony_mirror_served_total` | counter | `distro` | mirror router |
| `monopsony_mirror_served_bytes_total` | counter | `distro` | same |
| `monopsony_mirror_hash_verify_total` | counter | `distro`, `result` = pass \| mismatch_fail_closed \| mismatch_fail_open \| no_checksum* | mirror.rs sync + serve paths |
| `monopsony_mirror_signature_verify_total` | counter | `distro`, `result` = pass \| fail \| fail_open \| no_keys \| unsupported | mirror.rs metadata verification |
| `monopsony_mirror_cross_check_total` | counter | `distro`, `result` = match \| divergence \| unreachable \| skipped | monopsony-mirror/src/cross_check.rs |
| `monopsony_mirror_blacklist_self_ban_total` | counter | — | announce loop own-ban detection |
| `monopsony_scan_total` | counter | `status` | scan subsystem |
| `monopsony_scan_cve_lookup_total` | counter | `result` = found (unlabeled base series too) | scan subsystem |
| `monopsony_scan_policy_decision_total` | counter | `decision` | scan subsystem |
| `monopsony_update_releases_fetched_total` | counter | `result` = new \| already_cached \| none | monopsony-server/src/update_distributor.rs |
| `monopsony_update_hash_verify_total` | counter | `result` = pass \| mismatch_fail_closed \| unlisted_fail_closed | same |
| `monopsony_update_signature_verify_total` | counter | `result` = pass \| no_key \| missing_sig_fail_closed \| fail_fail_closed | same |
| `monopsony_update_retention_pruned_total` | counter | — | same (keep-N retention) |

\* `no_checksum` is declared in the metric's doc comment
(monopsony-common/src/metrics.rs, `mirror::HASH_VERIFY_TOTAL`) but **no call
site emits it yet** — a series with that label value won't appear until code
emits it. It is intentionally not alerted on.

Note on scope: the exporter renders metrics recorded **in the server
process**. The client-crate metrics (`monopsony_client_proxy_requests_total`,
`monopsony_client_bytes_saved_total`, `monopsony_client_grpc_connections_total`)
are incremented inside monopsony-client, which installs no Prometheus
recorder — they exist as names but are exported nowhere today (see gaps).

## Assumptions (tunable, not code constants)

- **Hit-rate SLO**: warning < 50% (30m), critical < 20% (5m). Both carry a
  traffic floor so an idle server never pages.
- **Staleness threshold**: alerts assume `[metrics].stale_threshold_secs`
  default **900s** (client_health.rs default; 15× the 60s heartbeat). The
  1800s heartbeat-age alert is 2× that default.
- **Mirror sync cadence**: `MonopsonyMirrorSyncStalled` uses a 26h success
  window (one missed daily cycle) and a 7d history guard so never-synced
  distros stay quiet. Widen the window for weekly-cadence distros.
- **Update poll cadence**: `MonopsonyUpdatePollStalled` uses 12h ≈ 2× the
  default 6h poll interval.
- **Scrape job label** `monopsony` — must match your scrape_configs.

## Metric gaps

Metrics this pack would have used but that **do not exist** in the tree —
each with the file that would own the instrumentation. Adding one? Update
this pack in the same change.

| Gap | Why it hurts | Would live in |
|---|---|---|
| Per-PM cache hit/miss (`result` breakdown, or a `manager` label on `monopsony_cache_requests_total`) | Hit-rate alerting is fleet-wide only; can't tell apt traffic from brew traffic | `crates/monopsony-proxy/src/proxy.rs` + `crates/monopsony-common/src/metrics.rs` |
| Registry fetch counters for language artifacts (crates.io / PyPI / npm pulls, per registry) | The dashboard's "registry activity" ask has no ground truth — panel omitted rather than invented | `crates/monopsony-proxy/src/proxy.rs` or `crates/monopsony-proxy/src/prefetch.rs` |
| Tracker-side metrics: announce volume, blacklist bans served, peer-list requests | `monopsony-tracker` exposes no `/metrics` at all; blacklist health is invisible from the tracker's side (only the mirror's own-ban counter exists) | `crates/monopsony-tracker/src/main.rs` (needs an exporter) + `crates/monopsony-control/src/announce.rs` |
| Mirror index age gauge (`monopsony_mirror_index_age_seconds{distro}`) | "Index staleness" is currently *inferred* from sync outcomes (26h window); a direct age gauge would make it exact | `crates/monopsony-mirror/src/mirror.rs` (sync completion path) + `crates/monopsony-common/src/metrics.rs` |
| Quarantine backlog gauge (blobs currently quarantined) | Only the event counter exists; can't trend the withheld population or alert when it never drains | `crates/monopsony-proxy/src/proxy.rs` (scrub path) + `crates/monopsony-common/src/metrics.rs` |
| Update-distributor poll age gauge (`monopsony_update_last_poll_age_seconds`) | Poll-stall alert relies on counter absence over a fixed 12h window; a direct age gauge tracks interval changes automatically | `crates/monopsony-server/src/update_distributor.rs` |
| `no_checksum` emission on `monopsony_mirror_hash_verify_total` | Label value declared in the doc comment but never emitted; files without upstream checksums are silently uncounted | `crates/monopsony-mirror/src/mirror.rs` |
| Client-crate metric exposure (proxy requests, bytes saved, gRPC connections) | Recorded in monopsony-client but the client installs no recorder/exporter — three metrics exported nowhere | `crates/monopsony-client/src/main.rs` (install recorder + small endpoint, or push gateway) |
