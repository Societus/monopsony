# Monopsony → CheckMK (Raw/CRE) monitoring

Sprint 27. Gives CheckMK visibility into monopsony-server + its clients with
**zero agent software on any client** and no Prometheus middleman. All
per-client data comes from the server's own `/metrics` endpoint (Prometheus
text format), which already carries `client_id`-labeled series.

Design decisions + grounding: `docs/plans/sprint27-client-health-and-monitoring.md` §6.
Key facts (verified against docs.checkmk.com, 2026-06 rev):

- The Prometheus special agent targets a Prometheus **server** query API —
  a bare exporter endpoint is not enough, so it is not used.
- Dynamic host management (per-client auto-hosts) is **commercial-only**.
  On Raw, all clients aggregate as services on the monopsony-server host.
- The **spool directory** accepts arbitrary local-check output from scripts —
  Raw-native, gives native graphs/perfdata.

## Setup

1. **Install CheckMK Raw** on the ingest hypervisor (VM or LXC) and create a
   site, e.g. `omd create mon` (see checkmk.com/download for the current
   Raw package for your distro), then `omd start mon`.

2. **Add the monopsony-server as a host** in Setup > Hosts with the "no
   agent" option (we do not install the CheckMK agent on it).

3. **Active liveness checks:** add active HTTP checks for
   `http://<monopsony-server>:9090/metrics` and the health endpoint —
   these alert if the server process or metrics exporter dies.

4. **Per-client services via the spool script:**
   - Copy `monopsony_spool.py` to the site, e.g.
     `~/local/share/monopsony_spool.py` under `/omd/sites/mon`.
   - Run it every minute as the site user:
     ```
     * * * * * MONOPSONY_METRICS_URL=http://<monopsony-server>:9090/metrics \
         /omd/sites/mon/local/share/monopsony_spool.py >/dev/null 2>&1
     ```
   - The script writes `~/tmp/check_mk/spool/monopsony_health`
     (atomic replace) containing:
     - `Monopsony stale clients` — fleet aggregate (WARN when > 0 stale)
     - `Monopsony client <client_id>` — one service per client with
       `age` (heartbeat seconds) + `failed` (windowed update failures)
       perfdata; WARN at `STALE_WARN_SECS` (default 900, mirror of the
       server's `stale_threshold_secs`) or any failure in window.
   - Tune thresholds via env: `STALE_WARN_SECS`, `FAILED_WARN`.
   - Run a **service discovery** (WATO) after the first spool write; the
     spool services appear and are kept updated on every check cycle.

## Self-test without a CheckMK site

```
MONOPSONY_METRICS_URL=file:///path/to/sample_metrics.txt \
CHECKMK_SPOOL_DIR=/tmp/spool python3 monopsony_spool.py
```
