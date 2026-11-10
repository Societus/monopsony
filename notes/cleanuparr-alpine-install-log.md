# Cleanuparr on Alpine Linux — Install Log & Shadow-Recipe Source

> **UPDATE 2026-10-25 (later — status change): the musl-native build is ABANDONED.**
> The app crashed the runtime (SIGABRT, native abort, no managed stack) repeatedly during
> dashboard-driven download-client testing; two silent deaths before, then a confirmed
> crash-loop (supervisor respawns at 20:11/20:12/20:14/20:20 UTC). Root cause not diagnosed —
> the abort bypassed the CLR dump handler entirely (no crash report even with
> DOTNET_EnableCrashReport=1), pointing at native/musl code. Diagnosis artifacts: strace,
> crash env, and the retired unit were removed; the native tree remains at
> `/opt/cleanuparr-musl` (inactive) and the DB backup at
> `/var/lib/cleanuparr/backups/pre-container-db-*.tgz`.
>
> **CANONICAL DEPLOYMENT NOW:** official container `ghcr.io/cleanuparr/cleanuparr:latest`
> (v2.10.12, glibc) via **podman 5.7.0 on the same Alpine CT**, driven by **podman-compose
> from the canonical definition `/var/lib/cleanuparr/docker-compose.yml`** (§11, added
> 2026-10-28 — digest-pinned image, `--network host`, full bind list incl. `/downloads`,
> `/nzb`, `/plexroot/*` which the original run line lacked). Config on the z2pool bind
> `/var/lib/cleanuparr:/config` (gatekeeper account + 2FA + DataProtection keys carried
> over — same app version, no migration needed), `-e PUID=999 -e PGID=991 -e
> TZ=America/Phoenix`, `restart: unless-stopped`. Boot persistence + keep-alive via
> supervise-daemon OpenRC unit running the **start-or-attach wrapper
> `/usr/local/bin/cleanuparr-container`** (plain `podman start -a` FAILS against an
> already-running container — crun exec.fifo — which is exactly the state compose leaves;
> respawn_delay 5, unlimited). Image bundles Apprise (venv at /opt/apprise-venv in-image) — §4 obsolete.
> The musl sections below are retained as historical reference (the FLM bundle-extraction
> technique in §3 remains useful if a musl build is ever revisited).
>
> **Podman-on-Alpine reality config (2026-10-25):** `/etc/containers/containers.conf` now
> carries `[engine] cgroup_manager="cgroupfs"` + `events_logger="file"` and
> `[containers] cgroups="disabled"` (container inherits the CT cgroup — correct for a
> single-container unprivileged CT). The apk defaults assume a systemd host. Residual: podman
> still logs one benign `conmon cgroupfs sandbox` warning per start — enabling controllers on
> the delegated root (`cgroup.subtree_control`) returns EBUSY because the no-internal-process
> constraint applies to the CT's delegated root (all CT processes live in it). Not worked
> around: the warning is cosmetic; a boot-time PID-migration dance would be required and is
> not justified unless a cgroup item causes a real problem.

**Purpose:** Executable-form source for the Cleanuparr shadow package recipe. Every action in
execution order, verified live on ct/1934 (`cleanuparr`, Alpine 3.23.7, unprivileged LXC).
Generate/iterate the recipe from THIS log — the log is the source of truth, the recipe is its
executable form. Written for the recipe reader, not as a diary.

**Deployment shape (verified green 2026-10-25):**
- App: Cleanuparr v2.10.12, **musl-native build** (framework-dependent), served by OpenRC
- UI: **tailnet-only** via `tailscale serve --bg 11011` → `https://cleanuparr.tail0964.ts.net` (verified 200 externally)
- Data: SQLite in `/var/lib/cleanuparr` (z2pool bind mount), user `cleanuparr` 999:991
- Monopsony client paired (v2 PIN), daemon under OpenRC, heartbeat verified server-side
- CheckMK: **server does not exist yet** (ct/1933 is a bare Debian box) — agent install deferred

---

## 0. LXC prerequisites (outside the container)

- Unprivileged Alpine 3.23 LXC, `features: nesting=1`, `dev0: /dev/net/tun` (Tailscale),
  3 cores / 2 GB RAM / 16G rootfs (local-zfs), net0 on vmbr2 (10.3.0.34/24, gw 10.3.0.1, DNS 10.3.0.11).
- Userns +100000: in-container 999:991 ↔ host **100999:100991** (the same ids that own the arr
  data on z2pool — chosen deliberately so the scanner shares arr file ownership).
- Bind mounts (`/etc/pve/lxc/1934.conf`):
  - `mp0: /z2pool/servers/docker/cleanuparr,mp=/var/lib/cleanuparr` — app data (host-owned 100999:100991)
  - `mp1 /downloads`, `mp10 /nzb`, `mp11 /downloads/tv` — malware-scan dirs
  - `mp2-9 /plexroot/<lib>` — media library visibility (anime, conferences, documentaries,
    liveaction, movies, hentai, videocourses, westernanimation)
- Tailscale installed & paired BEFORE app setup (`tailscale serve` needs it).

## 1. System packages (apk) — and why each is needed

```sh
apk add --no-cache nftables dotnet10-runtime aspnetcore10-runtime sqlite \
                    python3 py3-pip tzdata curl unzip ca-certificates git dotnet10-sdk
```

| Package | Why | Update path |
|---|---|---|
| `dotnet10-runtime` + `aspnetcore10-runtime` | app targets .NET 10; Alpine community packages = musl build (verified 10.0.13) | `apk upgrade` (independent of app) |
| `dotnet10-sdk` + `git` | **build-time only** — upstream ships no musl asset (see §3) | `apk upgrade` |
| `python3` + `py3-pip` | Apprise notification CLI (pinned apprise==1.9.6 in venv) | `apk upgrade`; venv pip separately |
| `nftables` | fleet drop-policy firewall | `apk upgrade` |
| `sqlite` | CLI/debug only (app bundles its own musl `libe_sqlite3.so` from NuGet RID assets) | optional |
| `tzdata`, `curl`, `unzip`, `ca-certificates` | release download, timezone, TLS | `apk upgrade` |
| ~~`gcompat`~~ | **NOT NEEDED — do not install.** The official glibc release binary boots under gcompat then deadlocks (CLR up, diagnostic socket up, threads blocked on bundle reads + futex, zero CPU — verified via /proc + strace). Dead end. | — |

Runtime facts: Alpine 3.23 community carries `dotnet10-runtime`, `aspnetcore10-runtime`,
`dotnet10-sdk` (10.0.110) — no Microsoft CDN needed. `mcr.microsoft.com/dotnet/aspnet:10.0-alpine`
proves .NET-10-on-musl is upstream-supported.

## 2. User/group + directories

```sh
addgroup -g 991 cleanuparr
adduser -S -D -H -u 999 -G cleanuparr -s /sbin/nologin -g cleanuparr cleanuparr
chown -R 999:991 /var/lib/cleanuparr && chmod 775 /var/lib/cleanuparr
ln -sf /usr/share/zoneinfo/America/Phoenix /etc/localtime
```

**Config-path mechanics (from source, `ConfigurationPathProvider`):** native (non-container) runs
use `$CLEANUPARR_CONFIG_PATH` if set, else `<AppContext.BaseDirectory>/config`;
`DOTNET_RUNNING_IN_CONTAINER=true` forces `/config`. **The wrapper exports the env var — config
never lands in the app dir, and the app dir can stay root-owned.** (The `/config →
/var/lib/cleanuparr` symlink created early on is NOT load-bearing; the env var is.)

## 3. App acquisition — musl build pipeline (required EVERY version)

**Why build:** the official `Cleanuparr-<ver>-linux-amd64.zip` is a glibc-linked single-file
apphost — dead under musl. Alpine's apk SDK produces a native musl build. Frontend `wwwroot/` is
reused from the official zip (no node/npm).

Build env (all REQUIRED):
```sh
export DOTNET_CLI_TELEMETRY_OPTOUT=1 DOTNET_NOLOGO=1 DOTNET_SYSTEM_GLOBALIZATION_INVARIANT=1
export HOME=/root DOTNET_CLI_HOME=/root
mkdir -p /run/user/0 && export XDG_RUNTIME_DIR=/run/user/0
# XDG_RUNTIME_DIR: NuGet first-run mutex does lstat("/run/user/0") — no logind in LXCs → EOVERFLOW crash without it
```

```sh
# 3a. source at the pinned tag
git clone --depth 1 --branch v2.10.12 https://github.com/Cleanuparr/Cleanuparr.git /tmp/Cleanuparr-src

# 3b. extract FLM.*.dll from the OFFICIAL release zip's single-file bundle
#     FLM.QBittorrent 1.0.3 + FLM.Transmission 1.0.3 (csproj pins at v2.10.12 — re-check per tag)
#     are PRIVATE GitHub-Packages deps. Neither a fine-grained PAT nor a gh OAuth (gho_) token is
#     accepted by the NuGet feed (403 both, live-verified), and Flaminel's FLM repos are private.
#     SOLUTION: parse the .NET single-file bundle and repack the exact released assemblies:
#       signature = 64-byte marker; header offset = u64 stored BEFORE the signature;
#       v6 header: major,minor u32/u32, count i32, 7-bit-len bundle_id, 40-byte v2 block;
#       entries: off u64, size i64, comp i64, type u8 (1=asm,4=runtimeconfig…), 7-bit-len path.
#     Extractor verified 407/407 entries, FLM DLLs valid PE (script pattern: /tmp/extract_bundle.py)
python3 /tmp/extract_bundle.py <zip-extract-dir>/Cleanuparr /tmp/bundle-dlls

# 3c. craft a local NuGet feed — FLAT layout REQUIRED (hierarchical dirs are not seen by restore):
#     /opt/nuget-local-feed/FLM.QBittorrent.1.0.3.nupkg  (+ FLM.Transmission.1.0.3.nupkg)
#     nupkg = zip of: [Content_Types].xml, _rels/.rels, <Id>.nuspec (id/version, NO deps),
#             lib/net10.0/<Id>.dll     (TFM net10.0 per Cleanuparr.Api.csproj)

# 3d. nuget.config at repo root (clear-first, local feed + nuget.org)
cat > /tmp/Cleanuparr-src/nuget.config <<'EOF'
<?xml version="1.0" encoding="utf-8"?>
<configuration>
  <packageSources>
    <clear />
    <add key="local-feed" value="/opt/nuget-local-feed" />
    <add key="nuget.org" value="https://api.nuget.org/v3/index.json" />
  </packageSources>
</configuration>
EOF

# 3e. clean stale restore state, then publish
cd /tmp/Cleanuparr-src/code/backend
find /tmp/Cleanuparr-src -type d \( -name obj -o -name bin \) -exec rm -rf {} + 2>/dev/null || true
rm -rf /opt/cleanuparr-musl.new
dotnet publish Cleanuparr.Api/Cleanuparr.Api.csproj \
  -c Release -r linux-musl-x64 --self-contained false \
  -p:PublishSingleFile=false -p:Version=2.10.12 -m:1 \
  -o /opt/cleanuparr-musl.new
cp -r <zip-extract-dir>/wwwroot /opt/cleanuparr-musl.new/
# atomic-ish swap (this is backup_old):
mv /opt/cleanuparr-musl /opt/cleanuparr-musl.old && mv /opt/cleanuparr-musl.new /opt/cleanuparr-musl
chmod 755 /opt/cleanuparr-musl/Cleanuparr && chown -R root:root /opt/cleanuparr-musl
```

Expected output layout: `Cleanuparr` (78 KB musl ELF apphost), `Cleanuparr.dll`, `FLM.*.dll`,
`libe_sqlite3.so` (musl RID build from NuGet), `wwwroot/`, rest of managed deps.

## 4. Apprise notifications

```sh
python3 -m venv /opt/apprise-venv
/opt/apprise-venv/bin/pip install apprise==1.9.6
ln -sf /opt/apprise-venv/bin/apprise /usr/local/bin/apprise
```
Do NOT `export PATH` inside the OpenRC unit — the unit is sourced into OpenRC's shell and clobbers
helper paths (`ebegin: not found`, `mountinfo: not found` → start fails). Symlink binaries instead.

## 5. Service wiring

Wrapper `/usr/local/bin/cleanuparr-app` (env lives HERE, not in the unit):
```sh
#!/bin/sh
export CLEANUPARR_CONFIG_PATH="/var/lib/cleanuparr"
export XDG_RUNTIME_DIR="/tmp"               # named mutexes; /run/user/<uid> absent in LXCs
export DOTNET_ROOT="/usr/lib/dotnet"
export DOTNET_GCHeapHardLimitPercent="50"   # GC misreads host RAM through the LXC (saw 515GB)
exec /opt/cleanuparr-musl/Cleanuparr
```

`/etc/init.d/cleanuparr` (supervised — auto-respawns on any silent death):
```sh
#!/sbin/openrc-run
name="cleanuparr"
description="Cleanuparr download manager"
command="/usr/local/bin/cleanuparr-app"
directory="/opt/cleanuparr-musl"       # content root: wwwroot is resolved via CWD
command_user="cleanuparr:cleanuparr"
supervisor="supervise-daemon"
respawn_delay=5
respawn_max=0
pidfile="/run/cleanuparr.pid"
output_log="/var/log/cleanuparr.log"
error_log="/var/log/cleanuparr.log"

depend() { need net; use dns; }
```
NOTE: with supervise-daemon, the pidfile holds the **supervisor's** pid, not the app's — to
kill/inspect the app itself use `pgrep -f /opt/cleanuparr-musl/Cleanuparr`. Spawned once after
the app died silently (no OOM, no log line — one-off native crash), which left a plain
start-stop-daemon unit dead with no auto-recovery.

```sh
chmod 755 /etc/init.d/cleanuparr
rc-update add cleanuparr default
touch /var/log/cleanuparr.log && chown cleanuparr:cleanuparr /var/log/cleanuparr.log
rc-service cleanuparr start   # ~10s; verify: curl http://127.0.0.1:11011/health → 200 + UI title
```
App listens on :11011 (default), binds [::] — exposure controlled by firewall + tailscale serve.

**Alpine fleet gotcha:** OpenRC's nftables init loads **`/etc/nftables.nft`**, NOT
`/etc/nftables.conf`. Keep both identical (`cp /etc/nftables.conf /etc/nftables.nft`) or your
rules silently don't load (bit on 1934; 1904's loaded rules are the stock Alpine set for the
same reason).

## 6. Exposure — tailscale only (operator directive)

```
tailscale serve --bg 11011     # → https://cleanuparr.tail0964.ts.net (tailnet only)
cp /var/lib/tailscale/tailscaled.state /var/lib/cleanuparr/backups/   # state backup
```
Nothing for the UI on the subnet; first external request provisions the cert (slow once, then 200).

**Secure Core / egress-VPN hardening (2026-10-25, after VPN-server rotation outage):**
- `eth0` MTU is **1400** (was 8196), persisted in `/etc/network/interfaces`. The egress VPN is
  Proton Secure Core (double hop); at default MTU, large TLS ClientHellos (Go/ML-KEM ~1.8KB)
  hit an MTU blackhole in the tunnel and tailscaled's control dial exceeded its internal
  deadline on every attempt (backend stuck NoState/doLogin) while longer-budget curl probes
  succeeded — classic per-dial-deadline loss. MTU 1400 cut the controlplane dial 8s → 1.35s and
  the node went Running/Online immediately.
- `dnsmasq` on 127.0.0.1 (enabled, `/etc/dnsmasq.conf`: no-resolv, server=10.3.0.11, cache
  2000) with `/etc/resolv.conf` = 127.0.0.1 → 10.3.0.11 → 100.100.100.100. Reason: MagicDNS
  (100.100.100.100) forwards public names through the tunnel where the Secure Core upstream
  intermittently SERVFAILs; technitium (LAN) is reliable and resolves ts.net names too.
- NOTE: tailscaled may rewrite resolv.conf back to 100.100.100.100-only after a control-plane
  sync. If tunnel DNS flakiness returns, the durable fix is adding 10.3.0.11 as a Global
  nameserver in the tailnet admin DNS page (affects all devices — operator decision).
- A live-only `ip -6 route replace blackhole ::/0 metric 2048` was added during diagnosis
  (no working IPv6 upstream; AAAA answers blackholed). Not persisted; harmless either way.

## 7. Firewall (fleet drop-pattern)

`/etc/nftables.d/cleanuparr.nft` — input drop with: lo, established, rate-limited icmp,
`192.168.11.33 tcp 5666` (checkmk), `udp dport 41641` + `iifname "tailscale0"`, log+drop;
forward drop; **output accept**. `/etc/nftables.conf` = flush + include, same content copied to
`/etc/nftables.nft` (see §5 gotcha). `rc-update add nftables default`.

Outbound targets — all verified reachable from 1934 after peer firewalls:

| Peer | CT | IP:port | Verified response |
|---|---|---|---|
| sonarr | 1952 | 10.3.0.52:8989 | 307 |
| radarr | 1953 | 10.3.0.53:7878 | 307 |
| lidarr | 1954 | 10.3.0.54:8686 | CT STOPPED throughout — pending |
| lingarr | 1955 | 10.3.0.55:9876 | 200 |
| sabnzbd | 1912 | 10.3.0.12:8080 | 303 |
| transmission | 1913 | 10.3.0.13:9091 | 409 |
| prowlarr | 1931 | 10.3.0.31:9696 | 200 |

Peer rule file `/etc/nftables.d/<svc>.nft`: input drop with lo/established/icmp-limited,
`ip saddr 10.3.0.0/24 accept`, `ip saddr 192.168.11.0/24 accept`, checkmk 5666, explicit
`ip saddr 10.3.0.34 tcp dport <port> accept`, tailscale 41641 + tailscale0, log+drop; forward
drop; output accept. Debian peers: `/etc/nftables.conf` + `systemctl enable --now nftables`.
Alpine peers: write BOTH `/etc/nftables.nft` and `.conf`, `rc-update add nftables default`.

**podman peer caveat (1913 transmission — container rebuilt, lessons below):** `flush ruleset`
wipes the `inet netavark` table → published ports blackhole (conmon listener accepts TCP, relays
to the dead container IP — established-but-silent). Post-reboot netavark does NOT self-repopulate.
Fix pattern: `podman network reload transmission` AND forward-chain accepts for the podman subnet
(real subnet 10.88.0.0/16 — read `podman0` addr per host, don't assume 10.89):
```
ip saddr 10.3.0.0/24 ip daddr 10.88.0.0/16 accept
ip saddr 192.168.11.0/24 ip daddr 10.88.0.0/16 accept
ip saddr 10.88.0.0/16 accept
```
Also allow the swarm port through input: `tcp/udp dport 51413 accept` (public peers arrive from
anywhere via pfSense forward). Governed by `transmission-container.service` (systemd, enabled).

**TRANSMISSION CONTAINER — canonical definition (2026-10-25 rebuild):**
```sh
podman run -d --name transmission \
  --restart unless-stopped \
  -p 9091:9091 -p 51413:51413 -p 51413:51413/udp \
  -v /var/lib/transmission/config:/config \
  -v /var/lib/transmission/init-override/run:/etc/s6-overlay/s6-rc.d/init-transmission-config/run:ro \
  -v /downloads:/downloads \
  -e PUID=999 -e PGID=991 -e TZ=America/Los_Angeles \
  docker.io/linuxserver/transmission:latest
```
- **Mount level matters:** appdata is `/var/lib/transmission/config` (NOT the parent). The
  original deployment's compose line was `/var/lib/transmission/config/:/config`.
- **Auth mechanism (critical):** the LSIO init script forces `rpc-authentication-required=false`
  at EVERY boot unless legacy `USER` + `PASS` env vars are set (it then writes username +
  plaintext password; the daemon hashes on load). The daemon itself honors a valid-format hash
  from disk — the disarming observed during recovery was the init else-branch writing false,
  plus the daemon's shutdown flush clobbering concurrent settings.json edits (edit only while
  STOPPED). The original password lived only in the original `podman run` env (lost).
- **Current state (2026-10-25 final):** auth ARMED with an operator-supplied password hash
  (`{hex40 + salt8` ssha1 format). The init-override mount (bind-mounted over the image's init
  script) prevents the init from touching credentials; the daemon accepts the hash from disk and
  preserves it across boots. Verified: no-creds 401, wrong-password 401, hash survives restart.
- **Credential change procedure (VERIFIED — follow exactly):** ① `podman stop transmission`
  (the running daemon flushes in-memory settings on SIGTERM and will CLOBBER concurrent edits);
  ② edit `/var/lib/transmission/config/settings.json` while stopped; ③ `podman start`.
- **ssha1 format (from 4.1.3 source, `crypto-utils.cc`):** `{` + hex(sha1(password + salt)) +
  salt, where salt = 8 printable chars from `[0-9a-zA-Z./]`, NO closing brace. 48 chars after
  `{`. Generator:
  ```sh
  read -rs PW
  SALT=$(tr -dc '0-9a-zA-Z./' < /dev/urandom | head -c8)
  HASH="{$(printf '%s%s' "$PW" "$SALT" | openssl dgst -sha1 -hex | awk '{print $NF}')$SALT}"
  echo "$HASH"
  ```
- The legacy `USER`/`PASS` env route also remains available (recreate without the override
  mount, with those env vars) — but the hash-on-disk route above is what's deployed.

## 8. Monopsony client (paired, daemon under OpenRC)

- Binary `/usr/local/bin/monopsony-client` = byte-identical relay of ct/1904's (md5
  999e728dccc9d2f18aaf466cf9bbe2de, `pair --pin` present = current working-tree build).
- `/etc/monopsony/client.toml` (35 lines, six required tables) + `/root/.config/monopsony/mirror-verify.key.hex`
  copied from 1904 (fleet-shared operator key); certs are per-client, delivered by pairing.
- Paired via v2 PIN: `pair --server monopsonytest.tail0964.ts.net:3130 --pin` detached; PIN
  R64356; approved via dashboard API (POST /api/v1/auth/login → session cookie → POST
  /api/v1/pair/approve {pin,request_id} — the pending list GET needs no auth, approve does).
  **client_id c27fe72d-31a2-415d-9e27-7cc6bdae738f**, cert expires 2027-01-23, auto-renews,
  CA TOFU-pinned, mirror-verify key delivered by pairing.
- OpenRC `monopsony-client` enabled+started → `gRPC connection established` / `Connected to
  server` / `Heartbeat sent`; proxy listening 127.0.0.1:3129.
- Server row verified: id+hostname+last_seen match (psql clients table).
- Proxy registration (`proxy on --pm …`) NOT done: apk is not a supported PM backend yet.

## 9. UPDATE PROCEDURE (the recipe's executable core)

New upstream release vX:
1. `rc-service cleanuparr stop`
2. Download `Cleanuparr-vX-linux-amd64.zip`, extract
3. Re-run the §3 pipeline at tag vX: clone, re-extract FLM DLLs from the NEW zip (csproj pins may
   bump — re-check), repack local feed at the new pins, publish `-p:Version=X` into
   `/opt/cleanuparr-musl.new`, overlay the new zip's `wwwroot/`, swap dirs (old kept as `.old`)
4. `chown -R root:root /opt/cleanuparr-musl && chmod 755 /opt/cleanuparr-musl/Cleanuparr`
5. `rc-service cleanuparr start` → `/health` 200 + log banner `Cleanuparr vX`
6. **Files replaced per update:** `Cleanuparr` apphost, all managed `*.dll` incl. `FLM.*.dll`,
   bundled `libe_sqlite3.so` (musl RID asset from NuGet), `wwwroot/`,
   `Cleanuparr.staticwebassets.endpoints.json`, BuildHost-* dirs. **Never touched:**
   `/var/lib/cleanuparr` (cleanuparr.json, SQLite DBs, DataProtection-Keys, logs) — env-driven,
   outside the app dir by design. Release-notes watchlist: config-schema migrations (app
   self-migrates), new/renamed env vars, apprise version bump
7. **Rollback:** swap `/opt/cleanuparr-musl.old` back + restart (equivalent of `backup_old`)
8. Independent of app updates: `apk upgrade` (dotnet10-*, nftables, python3, tzdata), apprise venv
   pip, monopsony-client (self-update), tailscale

## 10. SHADOW RECIPE DRAFT (gates 1–5 pending: validate → detect → register → check → update)

```toml
[package]
name = "cleanuparr"
homepage = "https://github.com/Cleanuparr/Cleanuparr"
description = "Servarr download-queue manager (musl-native Alpine build)"

source_type = "github_release"

[install]
method = "script"
service = "cleanuparr"
target_dir = "/opt/cleanuparr-musl"
config_paths = ["/var/lib/cleanuparr"]
backup_old = true
max_backups = 3
pre_update = "rc-service cleanuparr stop"
post_update = "sh /usr/local/lib/cleanuparr-update.sh"   # §3 pipeline at detected tag + swap + start
on_failure = "rc-service cleanuparr start"

[install.health_check]
type = "http"
url = "http://127.0.0.1:11011/health"
timeout_secs = 120

[asset_selector]
tag_pattern = ""

[version]
detect_command = "grep -ioE 'Cleanuparr v[0-9.]+' /var/log/cleanuparr.log | tail -1"
detect_pattern = "(v\\d+\\.\\d+(\\.\\d+)*)"

[notifications]
changelog_url = "https://github.com/Cleanuparr/Cleanuparr/releases"
notes = "Updates REQUIRE the musl rebuild pipeline (no upstream musl asset). FLM.* private deps are repacked per version from the official release bundle."
```

## 11. COMPOSE-CANONICAL REBUILD (2026-10-28)

**What was wrong:** the container run line carried only `/var/lib/cleanuparr:/config`. The
CT-level PVE mounts (mp1 `/downloads`, mp10 `/nzb`, mp11 `/downloads/tv`, mp2-9
`/plexroot/<lib>`) are visible to the CT but were never passed into the container — the app
was blind to every scan/library dir. Config was never at risk (it lives on the z2pool bind;
verified intact: cleanuparr.db, users.db, events.db, DataProtection-Keys, jwt-key.bin).

**Canonical definition:** `/var/lib/cleanuparr/docker-compose.yml` (on mp0 → z2pool, covered
by zfs backup layers). podman-compose 1.5.0 (apk). Key fields: `container_name: cleanuparr`
(pins the name the OpenRC unit and tailscale serve rely on), `network_mode: host`,
`restart: unless-stopped`, image pinned `:latest@sha256:d0212e4c…` (the known-good v2.10.12
digest — bump the line to update), 12 volumes: `/config` + `/downloads`, `/downloads/tv`,
`/nzb`, 8× `/plexroot/<lib>`.

**Rebuild procedure (verified live 2026-10-28):**
```sh
rc-service cleanuparr stop            # stop supervisor (container keeps running)
podman stop cleanuparr && podman rm cleanuparr   # clean SIGTERM: app flushes state first
podman-compose -f /var/lib/cleanuparr/docker-compose.yml up -d
rc-service cleanuparr start
```
Future image updates: bump the image line in the compose file, then `podman-compose -f …
up -d` alone (compose stops+recreates; the wrapper re-attaches automatically — see below).
`rc-service stop`/`start` around it is optional but gives a clean supervisor cycle.

**OpenRC unit change (required companion):** `podman start -a` FAILS against an
already-running container (crun `exec.fifo` error) — which is the state compose leaves after
`up -d`. The old unit went into a 5s respawn loop of failed starts. Fixed with wrapper
`/usr/local/bin/cleanuparr-container` (unit command changed to it, everything else
identical):
```sh
#!/bin/sh
state=$(podman inspect -f '{{.State.Running}}' cleanuparr 2>/dev/null)
if [ "$state" = "true" ]; then
    exec podman attach --sig-proxy=false cleanuparr
fi
exec podman start -a cleanuparr
```
Both branches tested live: attach (compose-started) and start (cold/boot path — CT reboot
hits this one). `podman stop` alone still gets undone ~5s later by the supervisor; a REAL
stop is `rc-service cleanuparr stop` first, as before.

**Observed on first scans (config operating, not a defect):**
- DownloadCleaner immediately began moving non-category items `/nzb/*` →
  `/downloads/orphaned/*` (the operator's configured orphan policy; the dirs were finally
  visible). `/nzb/incomplete` is spared (recognized category dir).
- Recurring cosmetic ERR: `Failed to handle orphaned entry: /downloads/tv —
  IOException: Device or resource busy` — `/downloads/tv` is a MOUNTPOINT (mp11) inside the
  `/downloads` bind; the app can't move a mountpoint, logs ERR, skips it. mp11 is empty,
  `nobody`-owned (unmapped uid at the PVE layer), and redundant for the container deployment
  — removing `mp11` from `/etc/pve/lxc/1934.conf` (PVE-side, needs a brief CT stop) would
  silence it. Operator decision; not done.

## 12. mp11 REMOVAL — /downloads/tv (2026-11-03, verified green)

**What changed:** `mp11` deleted from `/etc/pve/lxc/1934.conf` (backup `1934.conf.bak-20261103`
on the PVE host); `/downloads/tv` volume line deleted from the compose (backup
`docker-compose.yml.bak-20261103`); `/z2pool/library/downloads/completed/tv` deleted (empty
since Feb 2026, host-root-owned → unmapped in-CT, displayed 65534:65534). Container recreated
with **11 binds**. EBUSY orphan errors stopped: today's count froze at 146 with zero new lines
after restart. uid/gid confirmed correct throughout — app writes logs as 999:991
(= host 100999:100991).

**Procedure that worked:**
1. `pct stop 1934` → `sed -i '/^mp11:/d' /etc/pve/lxc/1934.conf` → `rmdir` host dir (empty-only
   safety) → delete the compose volume line → `grep`-verify both files → `pct start 1934`
2. `podman rm -f cleanuparr && podman-compose -f /var/lib/cleanuparr/docker-compose.yml up -d`
3. verify: health 200, 11 mounts, no `/downloads/tv` in CT or container, EBUSY count frozen

**Gotchas (each hit live, in order):**
1. **sed `^`-anchor missed the indented YAML line** (`      - /downloads/tv:...`) — the "fresh"
   container mounted tv again via the surviving line (podman auto-creates missing bind sources).
   Grep-verify every deletion.
2. **`podman-compose up -d` does NOT recreate on config change** — it tried `podman start` on
   the stale container and failed against the deleted bind (`crun: cannot stat /downloads/tv`).
   `podman rm -f` + `up -d` is the recreate path.
3. **Host-root-owned files on the /config bind crash-loop the container.** The image entrypoint
   chowns /config contents to PUID:PGID; a file created as PVE-host root (the .bak backup) is
   unmapped inside the CT → `chown: Operation not permitted` → Exit 1 flap. Fix: `chown
   100999:100991` any host-created file on the bind. Audit:
   `find /z2pool/servers/docker/cleanuparr -maxdepth 2 -printf '%u %p\n' | awk '$1<100000'`.

**Remaining known noise:** the `Bar Rescue S10E09` torrent (May 2026) still lists a `.nfo`
that's gone from disk → `file does not exist or insufficient permissions` every scan until the
torrent is removed/re-verified in transmission (operator handles client-side).

## OPEN ITEMS
- [ ] Recipe gates 1–5 (skill §4): recipe validate → detect by hand → register WITH `--recipe` → server-row check → `package check` → `package update` (operator approval — restarts the service)
- [ ] Lidarr (1954): firewall rules when next started (stopped throughout this deploy)
- [ ] CheckMK: ct/1933 has NO server installed (bare Debian: no OMD, no packages) — operator decision pending; the `192.168.11.33 5666` input rule is already in place on 1934 + all peers
- [ ] Cleanuparr dashboard config (download clients transmission+sabnzbd, arrs, API keys, scan dirs /downloads + /nzb) — dashboard-driven, operator does in UI
- [ ] Monopsony apk proxy backend (monopsony-pm-backend work) — client paired, package proxy not wired for apk
