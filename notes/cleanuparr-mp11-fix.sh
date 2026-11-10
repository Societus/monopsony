#!/bin/sh
# Cleanuparr CT 1934 — remove redundant mp11 (/downloads/tv) mount + its stale compose volume line.
# Run as root on the PVE host (ingest). ~1 min Cleanuparr downtime; tailscale + monopsony-client
# auto-start with the CT. Backups: 1934.conf.bak-<date> and docker-compose.yml.bak-<date>.
#
# Why (verified 2026-11-02):
#  - /z2pool/library/downloads/completed/tv is EMPTY since Feb 16 2026, owned root:root (0:0)
#    on the host -> shows as 65534:65534 in-CT (unmapped uid). It is bind-mounted as mp11
#    INSIDE the /downloads tree, so the orphan cleaner sees an unclaimed empty dir and tries
#    to move it -> "Device or resource busy" every 5-min scan (~288 ERR/day).
#  - rmdir only succeeds on an empty dir = built-in safety: if it ever has content, the
#    script aborts before starting the CT.
set -e
HOSTDIR=/z2pool/library/downloads/completed/tv

echo "== backups =="
cp /etc/pve/lxc/1934.conf /etc/pve/lxc/1934.conf.bak-$(date +%Y%m%d)
cp /z2pool/servers/docker/cleanuparr/docker-compose.yml /z2pool/servers/docker/cleanuparr/docker-compose.yml.bak-$(date +%Y%m%d)

echo "== stop CT 1934 =="
pct stop 1934

echo "== remove mp11 line from /etc/pve/lxc/1934.conf =="
sed -i '/^mp11:/d' /etc/pve/lxc/1934.conf

echo "== remove empty root-owned host dir (rmdir refuses if non-empty) =="
if [ -d "$HOSTDIR" ]; then rmdir "$HOSTDIR"; fi

echo "== remove /downloads/tv volume line from compose (REQUIRED: a podman-level nested bind would recreate the EBUSY) =="
sed -i '\#^- /downloads/tv:/downloads/tv$#d' /z2pool/servers/docker/cleanuparr/docker-compose.yml

echo "== start CT 1934 =="
pct start 1934
sleep 20

echo "== recreate container without the tv volume, re-attach supervisor =="
pct exec 1934 -- sh -c 'rc-service cleanuparr stop; sleep 2; podman-compose -f /var/lib/cleanuparr/docker-compose.yml up -d; sleep 8; podman ps --format "{{.Names}} {{.Status}}"; rc-service cleanuparr start'

echo
echo "== done. verify =="
echo "grep mp11 /etc/pve/lxc/1934.conf           # expect: no output"
echo "pct exec 1934 -- ls /downloads             # expect: complete incomplete orphaned only (no tv)"
echo "pct exec 1934 -- podman inspect cleanuparr --format '{{range .Mounts}}{{println .Destination}}{{end}}' | grep tv   # expect: nothing"
echo "pct exec 1934 -- curl -s -o /dev/null -w '%{http_code}' http://127.0.0.1:11011/health   # expect: 200"
echo "wait 5-10 min (one scan cycle), then: pct exec 1934 -- sh -c 'grep -c \"Device or resource busy\" /var/lib/cleanuparr/logs/cleanuparr-\$(date +%Y%m%d).txt'  # expect: count stops growing"
