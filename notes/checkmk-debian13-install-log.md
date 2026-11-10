# Checkmk server install log — CT 1933 "checkmk" on hypervisor `ingest` (Debian 13 trixie)

Written for the recipe reader (`recipes/shadow/checkmk-server.toml`), not as a diary.
Verified live 2026-10-25. Checkmk Community (Raw) **2.5.0p16**, OMD site **cmk**.

## Execution order (what was done, in order)

1. CT 1933 existed already (Debian 13, 2 cores / 8 GB / 50 G rootfs, unprivileged,
   nesting=1, `/dev/net/tun` passed). Interfaces: eth0 = 192.168.11.33/24 (INGRESS,
   gw 192.168.11.1 pfSense), eth1 = 10.3.0.33/24 (container net), tailscale0 = 100.95.194.7.
2. `/opt/omd` is a **zfs bind mount**, not CT-local storage:
   `mp0: /z2pool/servers/docker/checkmk,mp=/opt/omd` — site data therefore lands on
   z2pool and is covered by the scheduled backup. The checkmk .deb installs its
   skeleton INTO that mount; nothing needed re-routing.
3. Firewall was already in place inside the CT (`/etc/nftables.conf`, policy drop):
   input allows SSH (clear LAN), web 80/443 (192.168.11.0/24 + 10.3.0.0/24),
   agent ports 5667/6556 from both nets; output allows DNS, active-check egress
   (5666/6556/SNMP/ping) to both nets, and monopsony ports to 100.124.155.69.
4. **Egress gap fixed**: the CT had NO http/https outbound. Two live nft inserts in
   `inet filter output`:
   - `tcp dport { 80, 443 } ip daddr != { 192.168.11.0/24, 10.3.0.0/24, 100.124.155.69 } accept`
   - `ip daddr 192.168.15.0/24 tcp dport { 5666, 6556 } accept` (monitoring egress
     to ingest itself + plex + homelable, which live on the 192.168.15.0/24 uplink LAN)
   **PERSISTENCE NOT YET WRITTEN** to /etc/nftables.conf (gate-timed out; see OPEN ITEMS).
5. Unprivileged-port fix (port 80 bind): `sysctl -w net.ipv4.ip_unprivileged_port_start=0`
   + persisted in `/etc/sysctl.d/99-unprivileged-ports.conf`. The system apache2 needs
   this to bind :80 inside an unprivileged LXC.
6. DNS quirk: technitium (10.3.0.11) SERVFAILs on `download.checkmk.com` (plain
   `checkmk.com` resolves fine). Worked around with `/etc/hosts` pin
   `45.133.11.29 download.checkmk.com` inside the CT. Remove once technitium resolves it.
7. Downloaded `check-mk-community-2.5.0p16_0.trixie_amd64.deb` (337 MB) from
   download.checkmk.com straight into the CT, installed with
   `DEBIAN_FRONTEND=noninteractive apt-get install -y /tmp/checkmk.deb`.
   Zero manual dependency work (deps pulled from trixie main).
8. Site creation: `omd create cmk` (generated cmkadmin password printed at creation).
   OMD per-site config changes that matter (`omd config cmk set ...`):
   - `APACHE_TCP_PORT 5000` and `APACHE_TCP_ADDR 127.0.0.1` — site apache stays on
     its private loopback port (the DEFAULTS; an initial experiment with port 80
     collided with the system apache and was reverted).
   - **Architecture**: the .deb also installs a SYSTEM apache2 (systemd unit) which
     owns `*:80` and proxies `/cmk/` → `127.0.0.1:5000`. Wire it with
     `omd update-apache-config cmk` after any site/port change. The nft rules for
     80/443 exist for this system apache.
9. `omd update-apache-config cmk` + `systemctl restart apache2` + `omd start cmk`
   → `omd status cmk` = running (nagios core = the community event core).
10. Agent on the server CT itself:
    `apt-get install -y /opt/omd/versions/<ver>/share/check_mk/agents/check-mk-agent_<ver>-1_all.deb`
    (arch-all, **zero dependencies** — installs offline everywhere). Listener is a
    systemd **socket unit** (`check-mk-agent.socket`, *:6556). On several CTs the
    postinst enable didn't take: `systemctl enable --now check-mk-agent.socket` fixes it.
11. Agent on the ingest hypervisor itself (same deb via `pct exec 1933 -- cat <deb>`
    → /tmp on ingest → dpkg -i). Hypervisor nft is accept-all; no rule needed there.
12. **Fleet agent rollout (Debian/Ubuntu CTs)** — for each of
    1900, 1901, 1902, 1903, 1905, 1910, 1911, 1913, 1914, 1951, 1952, 1953:
    `pct push` the deb → `dpkg -i` → `nft insert rule inet filter input ip saddr
    { 10.3.0.33, 192.168.11.33 } tcp dport 6556 accept` (only where an input chain
    with policy drop exists; skipped cleanly where the CT has no nft ruleset) →
    verify `ss -tln | grep 6556` → `systemctl enable --now check-mk-agent.socket`
    on the ones that came up without a listener.
13. Host registration via REST API (`http://127.0.0.1/cmk/check_mk/api/1.0`, basic
    auth cmkadmin): POST `/domain-types/host_config/collections/all`
    `{folder:"/", host_name, attributes:{ipaddress}}` for
    cloudflared 192.168.11.200 · postgresql 10.3.0.4 · caddy 10.3.0.2 · redis 10.3.0.3 ·
    crowdsec 10.3.0.6 · vaultwarden 10.3.0.10 · technitium-dns 10.3.0.11 ·
    transmission-bt 10.3.0.13 · searxng 10.3.0.14 · jellyfin 10.3.0.51 ·
    sonarr 10.3.0.52 · radarr 10.3.0.53 · plex 192.168.15.113 ·
    homelable 192.168.15.246 · ingest 192.168.15.90 · checkmk 127.0.0.1 (itself).
14. Service discovery: `su - cmk -c 'cmk -I'` (all hosts; synchronous; writes
    autochecks). NOTE: `omd su cmk -c '...'` quoting silently swallowed the command
    once — use `su - cmk -c` or `omd su cmk` interactively. The REST discovery
    endpoints (`/domain-types/service_discovery_run/actions/start/invoke`) left the
    background job stuck at "initialized" in this setup — the CLI path is reliable.
15. Activation: GET `/domain-types/activation_run/collections/pending_changes` →
    copy the `ETag` response header → POST
    `/domain-types/activation_run/actions/activate-changes/invoke` with `If-Match`
    header + body `{"redirect":false,"force_foreign_changes":false}`. (2.5 API:
    create-host moved to the collections/all endpoint; activate REQUIRES If-Match —
    a stale/absent ETag returns 428/412, and the 412 body helpfully prints the
    expected ETag.)
16. Verified via livestatus (`unixcat tmp/run/live`): 16 hosts monitored, all
    state 0. ingest discovery includes zpool status, PVE processes, IPMI sensors,
    every zfs filesystem. plex + homelable show PING-only until their agents land
    (see OPEN ITEMS — gate-timed-out batch).

## Package set (and why)

- `check-mk-community` (server deb): the whole OMD stack; apache2 + deps from trixie.
- `check-mk-agent` (arch-all deb, from the server version's agents/ dir): every
  Debian/Ubuntu CT + the hypervisor. Self-contained; dpkg -i works offline.
- No other packages added on CTs (no python3 requirement for the agent).

## Where state really lands

- `/opt/omd/` = zfs dataset `z2pool/servers/docker/checkmk` (bind mount, backed up).
  - `versions/<ver>/` — one dir per installed version (side-by-side; old versions
    remain until `omd cleanup` / manual rm).
  - `sites/cmk/` — the site: etc/ (config), var/ (rrd, logs, autochecks),
    tmp/ (site tmpfs entry added to /etc/fstab by omd create).
- System apache config: `/etc/apache2/` inside the CT (omd update-apache-config writes it).
- nft: `/etc/nftables.conf` (+ live ruleset); sysctl: `/etc/sysctl.d/99-unprivileged-ports.conf`.

## UPDATE PROCEDURE (recipe automates this)

1. `pre_update`: `omd stop cmk` (|| true).
2. Script: read latest version from the `releases/latest` redirect Location
   (strip v-prefix; exit 0 if equal/unknown) → download
   `https://download.checkmk.com/checkmk/<V>/check-mk-community-<V>_0.trixie_amd64.deb`
   → `apt-get install -y` (new version lands side-by-side in /opt/omd/versions/)
   → `omd update cmk --force` (migrates the site to the new version) → cleanup deb.
3. `post_update`: `omd start cmk`.
4. Health check: TCP 127.0.0.1:5000 (site apache). Failure → executor rollback +
   re-run post_update.
- Independent of the app: plain `apt upgrade` keeps apache2/python deps current;
  checkmk versions NEVER update through distro repos (download.checkmk.com only).
- DNS caveat: update downloads rely on the /etc/hosts pin for download.checkmk.com
  (technitium SERVFAIL). If the pin goes stale, fix technitium or re-pin.

## OPEN ITEMS

- [x] **nft persistence inside CT 1933** (done 2026-10-25): both egress rules (http/https
      to non-LAN, 192.168.15.0/24 agent egress) PLUS four MagicDNS DNS rules
      (100.100.100.100 / fd7a:115c:a1e0::53 udp+tcp 53 — required for tailscale serve cert
      provisioning) are in /etc/nftables.conf, syntax-checked and reloaded.
- [x] **Agent on plex (1950) + homelable (1958)** (done 2026-10-25). Gotcha hit: plex runs
      a `plexprotect` nft table (priority filter+10, "LAN default-deny on new inbound") and
      pfSense SNATs cross-LAN traffic, so the checkmk CT's real source is rewritten to a
      15.x address — allowance had to be `tcp dport 6556 accept` inside plexprotect (live +
      /etc/nftables.conf). homelable needed no rule. Post-`cmk -I`, a core reload via
      `su - cmk -c 'cmk -O'` was required (CLI discovery writes autochecks but creates NO
      WATO pending change — REST activation 422s "no changes"). plex now 27 services,
      homelable 14, all state 0.
- [x] **Tailscale serve** (done 2026-10-25): `tailscale serve --bg 80` in CT 1933;
      https://checkmk.tail0964.ts.net/cmk/ verified 200 from the workstation (tailnet).
      Root `/` 302-redirects to /cmk/ (/etc/apache2/conf-available/checkmk-root.conf).
      LE cert provisioning required the MagicDNS egress rules above.
- [x] **monopsony-client systemd unit on CT 1933** (done 2026-10-25): unit deployed
      (ExecStart = plain `daemon`), enabled, stale legacy `pair --code` process killed.
      HOWEVER the daemon cannot connect — transport-error retry loop. Server row
      `checkmk` (c4970a13) exists, last_seen 2026-10-18 (a week stale, i.e. the old manual
      daemon was already failing before this work). TCP to 100.124.155.69:50051 from the CT
      is fine → matches the known stale-binary transport-error case. FRESH BUILD STAGED at
      ingest:/tmp/monopsony-client-new (glibc, working tree, built 2026-10-23) — swap
      (`install` over /usr/bin/monopsony-client after `systemctl stop monopsony-client`,
      backup first, then start + verify "Connected to server") — swap completed
      2026-10-26 after operator approval; daemon connected immediately.
- [x] **Recipe registration** (done 2026-10-26): `checkmk-server` v2.5.0p16 registered
      (id 27ed4eed), server row verified (config_paths forwarded), `package check` green
      ("up to date" = expected upstream blindness). Daemon connected after the binary swap
      (fresh glibc build; the old binary had been failing since Oct 18).
- [x] **API user monitoring visibility** (done 2026-10-26): `thequietagent` saw 0 hosts /
      0 services — root cause was contact visibility, NOT API shape. Fixed via contact
      group `checkmk-watch` (member: thequietagent) + root-folder `contactgroups` with
      recurse_use, then activate. Also: REST collections need repeated `columns=` params
      (name-only stubs otherwise) and LQL state values are STRINGS (numeric returns empty
      silently). Full details in the checkmk-ops skill.
- [x] **checkmk_watch.py poller/assessor** (done 2026-10-26): poll → diff (NEW/ONGOING/
      RESOLVED) → stable issue numbers → snooze → local-LLM assessment (ornith,
      fail-open) → markdown report. Cron `checkmk-watch-30m` (job 8eef0ac54074, deliver
      matrix, no_agent, empty stdout = silent). Master: ~/workspace/checkmk-handoff/;
      installed copy ~/.hermes/scripts/ (apply changes to both). First live run surfaced
      57 real findings incl. z2pool degradation (known), ~10 filesystems >90%, a failed
      crowdsec-cloudflare-bouncer, and Checkmk-agent-TLS warnings on every host.
- [x] **ICMP egress gap on CT 1933** (done 2026-10-26): host ping checks to 192.168.15.x
      (plex/homelable/ingest) failed — output chain allowed only tcp 5666/6556 to that
      subnet. icmp echo-request allow added (live + persisted).
- [ ] **Matrix direct-send**: matrix.json access_token still placeholder. Cron delivery
      works regardless (scheduler posts stdout); --send stays skipped until a real token
      is filled in.
- [ ] **Snooze/tune the recurring noise classes** (first cron runs will repost them):
      ~16× Check_MK Agent TLS warnings (register agents via cmk-agent-ctl OR snooze),
      ~6× page-table Memory CRITs on small CTs (checkmk threshold tuning candidates —
      ornith's assessment flags them as threshold artifacts), media-library filesystems
      at 80-97% (capacity planning, known).
- [ ] **Alpine CTs (10)**: 1904 authentik, 1912 transmission, 1915 forgejo,
      1916 matrix, 1931 prowlarr, 1934 cleanuparr, 1955 lingarr, 1956 tanoshi,
      1957 radicale, 1970 forgejo-runner. No dpkg, no inetd/xinetd (busybox only,
      `/usr/bin/nc` exists). Pattern to try: generic agent
      `/opt/omd/versions/<ver>/share/check_mk/agents/check_mk_agent.linux` →
      `/usr/bin/check_mk_agent` (+ `waitmax`), listener via a tiny OpenRC service;
      apk installs need the operator-sanctioned temporary nft flush. Per-CT nft fragments
      live in `/etc/nftables.d/<name>.nft`. Remember pfSense SNAT: allow by port, not by
      source IP, on any target with a default-deny input table.
- [ ] **CT 1932 (agent0)**: stopped — deliberately not monitored. Start+agent if it
      returns to service.
- [ ] **p17 update**: upstream checkmk 2.5.0p17 exists (installed: p16). Run manually via
      the recipe (`package update checkmk-server`) once the client daemon is connected —
      restarts the site; operator approval per skill rule.
