#!/usr/bin/env python3
"""Sprint 27: monopsony → CheckMK Raw/CRE spool-dir bridge.

Runs on the CheckMK site host (cron, every minute). Fetches the monopsony
server's Prometheus endpoint, parses the client-labeled series, and writes
one local-check line per client plus fleet aggregates into the site's spool
directory. No agent on any monopsony client; no Prometheus server needed.

Environment:
  MONOPSONY_METRICS_URL  default http://127.0.0.1:9090/metrics
  CHECKMK_SPOOL_DIR      default: auto-detected OMD site spool dir
  STALE_WARN_SECS        default 900 (mirror of stale_threshold_secs)
  FAILED_WARN            default 0 (any windowed failure -> WARN)

Local-check line format: <status> "<item>" <perfdata> <details>
status: 0 OK, 1 WARN, 2 CRIT, 3 UNKNOWN.
"""

import os
import re
import sys
import tempfile
import urllib.request
from pathlib import Path

METRICS_URL = os.environ.get("MONOPSONY_METRICS_URL", "http://127.0.0.1:9090/metrics")
STALE_WARN = int(os.environ.get("STALE_WARN_SECS", "900"))
FAILED_WARN = int(os.environ.get("FAILED_WARN", "0"))

LABEL_RE = re.compile(r'^(\w+)\{([^}]*)\}\s+([-\d.eE+]+)$')
BARE_RE = re.compile(r'^(\w+)\s+([-\d.eE+]+)$')


def spool_dir() -> Path:
    explicit = os.environ.get("CHECKMK_SPOOL_DIR")
    if explicit:
        return Path(explicit)
    # Auto-detect: OMD sites live under /omd/sites/<site>; the site user's
    # spool dir is <home>/tmp/check_mk/spool.
    for cand in sorted(Path("/omd/sites").glob("*/tmp/check_mk/spool")):
        if cand.is_dir():
            return cand
    print("no spool dir found; set CHECKMK_SPOOL_DIR", file=sys.stderr)
    sys.exit(2)


def parse_metrics(text: str) -> dict:
    """Return {metric_name: {"__bare__": v, "<label value>": v}}."""
    out: dict = {}
    for line in text.splitlines():
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        m = LABEL_RE.match(line)
        if m:
            name, labels, val = m.group(1), m.group(2), float(m.group(3))
            for kv in labels.split(","):
                if "=" in kv:
                    k, v = kv.split("=", 1)
                    if k.strip() == "client_id":
                        out.setdefault(name, {})[v.strip().strip('"')] = val
                    break
            continue
        m = BARE_RE.match(line)
        if m:
            out.setdefault(m.group(1), {})["__bare__"] = float(m.group(2))
    return out


def status_of(value: float, warn: float, crit: float) -> int:
    if value >= crit:
        return 2
    if value >= warn:
        return 1
    return 0


def build_lines(metrics: dict) -> list:
    lines = []

    # Fleet aggregate: clients currently stale.
    stale_n = int(metrics.get("monopsony_clients_stale_current", {}).get("__bare__", 0))
    st = 1 if stale_n > 0 else 0
    lines.append(
        f'{st} "Monopsony stale clients" stale={stale_n};1;1; '
        f"{stale_n} client(s) past the staleness threshold"
    )

    # Per-client: heartbeat age + windowed update failures on one service.
    ages = metrics.get("monopsony_client_last_heartbeat_age_seconds", {})
    failed = metrics.get("monopsony_client_failed_updates", {})
    for client_id in sorted(set(ages) | set(failed)):
        age = ages.get(client_id, -1)
        fails = int(failed.get(client_id, 0))
        st = 0
        if age >= STALE_WARN:
            st = max(st, 1)
        if fails > FAILED_WARN:
            st = max(st, 1)
        lines.append(
            f'{st} "Monopsony client {client_id}" '
            f"age={age:.0f};{STALE_WARN};{STALE_WARN * 2};0 failed={fails};{FAILED_WARN};{FAILED_WARN * 2 + 1};0 "
            f"heartbeat age {age:.0f}s, {fails} failed update(s) in window"
        )
    return lines


def main() -> int:
    target = spool_dir()
    try:
        with urllib.request.urlopen(METRICS_URL, timeout=10) as resp:
            metrics = parse_metrics(resp.read().decode())
    except Exception as e:  # noqa: BLE001 - any fetch failure is one UNKNOWN line
        lines = [f'3 "Monopsony metrics fetch" - fetch failed: {e}']
    else:
        lines = ['0 "Monopsony metrics fetch" - endpoint OK'] + build_lines(metrics)

    payload = "\n".join(lines) + "\n"
    fd, tmp = tempfile.mkstemp(dir=str(target), prefix=".monopsony-", text=True)
    try:
        with os.fdopen(fd, "w") as f:
            f.write(payload)
        dest = target / "monopsony_health"
        os.replace(tmp, dest)
    except BaseException:
        os.unlink(tmp)
        raise
    print(payload, end="")
    return 0


if __name__ == "__main__":
    sys.exit(main())
