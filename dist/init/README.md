# Init System Templates

Canonical service templates for every init system the installer supports.
`dist/install.sh` renders these (token substitution) during `--server` installs
and keeps embedded fallback copies of the same files so the curl|bash one-liner
works standalone.

## Files

| File | Init system | Deployed to |
|------|-------------|-------------|
| `monopsony-server.service` | systemd | `/etc/systemd/system/monopsony-server.service` |
| `monopsony-client.service` | systemd (client daemon) | `/etc/systemd/system/monopsony-client.service` |
| `monopsony-server.openrc` | OpenRC (Alpine, Artix, Gentoo) | `/etc/init.d/monopsony-server` |
| `monopsony-server.runit` | runit (Void, Artix-runit) | `/etc/sv/monopsony-server/run` |
| `monopsony-server.runit-log` | runit log service | `/etc/sv/monopsony-server/log/run` |
| `monopsony-server.s6` | s6/s6-rc (Artix-s6) | `/etc/s6/sv/monopsony-server/run` |
| `monopsony-server.sysv` | SysV init.d (Devuan, legacy) | `/etc/init.d/monopsony-server` |
| `com.monopsony.server.plist` | launchd (macOS) | `/Library/LaunchDaemons/com.monopsony.server.plist` |

## Tokens

The installer replaces these at render time:

- `@INSTALL_DIR@` — binary install prefix (`/usr/bin` on Linux, `/usr/local/bin` on macOS)
- `@EXTRA_ARGS@` — extra CLI args, currently the Cloudflare-Tunnel localhost bind
  set; removed entirely when unused (always written with a leading space in templates)
- `@PLIST_EXTRA_ARGS@` — plist-only variant; replaced by one `<string>` element
  per extra arg, or the line is deleted when unused

## Sync with install.sh

The embedded fallback copies inside `dist/install.sh` must match these files
byte-for-byte. After editing either side:

```bash
bash dist/install.sh --emit-templates dist/init   # regenerate from embedded copies
bash dist/install.sh --check-templates            # verify sync (also runs in CI)
```

## Detection

Init system detection is capability-based (PID 1 identity, `/run/systemd/system`,
`/run/openrc/softlevel`, live s6-rc database `/run/s6-rc`, scan dirs, command
probes) — not keyed on distro name. Strong s6 signals (s6-svscan as PID 1 or a
live `/run/s6-rc`) outrank OpenRC's weakest heuristic, since a converted Gentoo
keeps OpenRC's early-boot scripts while s6 supervises services. See
`detect_init()` in `dist/install.sh`. Override for testing/pinning:

```bash
MONOPSONY_INIT_SYSTEM=openrc bash dist/install.sh --detect-init
```
