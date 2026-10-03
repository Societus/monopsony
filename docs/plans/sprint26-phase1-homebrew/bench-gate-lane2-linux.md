# Lane 2 — .20 Linux regression (checklist item 8) — fully independent

Shared background: read `bench-gate-context-pack.md` in this directory FIRST.
You own ONE checklist item: **#8 Linux PM regression** through the SAME .58
server the brew lane is testing. Do NOT touch server config, do NOT restart
the server, do NOT touch m2mini — a parallel lane owns those. If a request
fails with connection refused once, wait 30 s and retry once (transient
probes/restarts on the other lane are possible; anything longer, record and
move on).

## Host facts

- .20 = Arch Linux client, address `10.10.20.20`. **SSH works as
  `root@10.10.20.20` with key auth** (verified: `uname` returned
  `6.8.12-20-pve` — it is an unprivileged PVE container). The `martin@` user
  does NOT authenticate — use root.
- The paired monopsony-client daemon + pacman/npm/pip/cargo config from the
  Phase F gate (2026-10-01) should still be in place. Verify before testing:
  - daemon: `pgrep -af monopsony-client`
  - client config: find the toml (`/etc/monopsony/client.toml` or
    `~/.config/monopsony/client.toml`)
  - proxy config files the PMs read: `/etc/pacman.d/monopsony.conf`-style
    include (check `/etc/pacman.conf` for the hook), npmrc/pip.conf/cargo
    config under root's home.

## Item 8 proof (per checklist: pacman green + npm/pip/cargo MISS→HIT spot check)

1. **pacman**: `pacman -Sy` through the chain (or `-Syu --noconfirm` if -Sy
   alone is degenerate) → exits 0, no signature errors. Then pick one small
   package NOT in pacman's cache and install it
   (`pacman -S --noconfirm <small-pkg>`); confirm the .58 log shows the
   package URL fetched through the proxy, then RE-move/re-install or
   `pacman -Sc` the local pkg cache and re-install to observe the second
   fetch → .58 log shows no upstream re-fetch (HIT). Evidence: log lines.
2. **npm spot check**: from .20, `npm cache clean --force` then
   `npm install --no-save <tiny-pkg>` (e.g. `is-odd`) twice with a cache clean
   between → first run MISS in .58 log, second run HIT (no registry.npmjs.org
   upstream line on the second).
3. **pip spot check**: same pattern — `pip download --no-cache-dir -d /tmp
   idna` twice (clear /tmp between) → MISS then HIT on
   files.pythonhosted.org/simple routes in the log.
4. **cargo spot check**: `cargo new /tmp/regtest && cd /tmp/regtest && cargo
   add bitflags@2.6.0 && cargo fetch` — clean `~/.cargo` registry cache for
   that crate between runs → MISS then HIT on index.crates.io/static.crates.io.
5. Report each as a sub-verdict (pacman / npm / pip / cargo) with the log
   evidence lines from .58 (`grep -E '<host-pattern>' /tmp/monopsony-server.log
   | tail`).

## Cleanup (binding — from fleet history)

- Remove any packages you installed (`pacman -Rns --noconfirm <pkg>`) and
  /tmp artifacts. Do NOT delete ledger high-water rows or repo state that
  pre-existed — only what YOUR test created.
- No seeds to reset: you are read-mostly on cache state (installs warm normal
  cache entries, which is fine to leave).

## Report

Item 8 sub-verdict table + evidence lines + what you cleaned up + any
BLOCKED-gate notes. FACTS from tool output only.
