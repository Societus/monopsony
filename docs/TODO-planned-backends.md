# Planned Package Manager Backends

The marketing website (monopsony.dev) lists these backends as "planned."
Track implementation status here.

## Currently Implemented (15 backends)

| Backend  | Platform           | File                              | Status |
|----------|--------------------|-----------------------------------|--------|
| apt      | Debian/Ubuntu      | package_manager/apt.rs            | ✅ Live-verified |
| dnf      | RHEL/Rocky/Alma    | package_manager/dnf.rs            | ✅ Live-verified |
| pacman   | Arch Linux         | package_manager/pacman.rs         | ✅ Live-verified |
| apk      | Alpine Linux       | package_manager/apk.rs            | ✅ Live-verified |
| winget   | Windows            | package_manager/winget.rs         | ⚠️ Cross-compiled, untested on real Windows |
| choco    | Windows            | package_manager/choco.rs          | ⚠️ Cross-compiled, untested on real Windows |
| scoop    | Windows            | package_manager/scoop.rs          | ⚠️ Cross-compiled, untested on real Windows |
| mport    | MidnightBSD        | package_manager/mport.rs          | ⚠️ Backend runs, cache untested (VPN intermittent) |
| pkg      | FreeBSD            | package_manager/pkg.rs            | ⚠️ Cross-compiled, untested on FreeBSD |
| pkg_add  | OpenBSD            | package_manager/pkg_add.rs        | ⚠️ Cross-compiled, untested on OpenBSD |
| pkgin    | NetBSD/Minix       | package_manager/pkgin.rs          | ⚠️ Cross-compiled, untested on NetBSD |
| zypper   | openSUSE           | package_manager/zypper.rs         | ✅ Implemented, unit-tested |
| xbps     | Void Linux         | package_manager/xbps.rs           | ✅ Implemented, unit-tested |
| emerge   | Gentoo             | package_manager/emerge.rs         | ✅ Implemented, unit-tested |
| nix      | NixOS              | package_manager/nix.rs            | ✅ Implemented, unit-tested |

## All Planned Backends Implemented

All 4 backends listed on monopsony.dev as "planned" are now implemented:

### 1. zypper (openSUSE) — ✅ Done

- **Platform:** openSUSE Tumbleweed / Leap
- **URL patterns:** `download.opensuse.org`, `download.suse.de`
- **Proxy config:** `/etc/zypp/zypp.conf` — `proxy = http://monopsony:3128`
- **List installed:** `rpm -qa --queryformat` (preferred), `zypper se --installed-only` (fallback)
- **Package format:** RPM

### 2. xbps (Void Linux) — ✅ Done

- **Platform:** Void Linux (glibc and musl variants)
- **URL patterns:** `*.voidlinux.org`, `*.repo.voidlinux.org`
- **Proxy config:** env vars `http_proxy`/`https_proxy` via `/etc/profile.d/monopsony-proxy.sh`
- **List installed:** `xbps-query -l`
- **Package format:** XBPS binary packages

### 3. emerge (Gentoo) — ✅ Done

- **Platform:** Gentoo Linux
- **URL patterns:** `distfiles.gentoo.org`, `gentoo.osuosl.org`
- **Note:** Gentoo builds from source — the proxy caches source tarballs
  (distfiles), not binary packages.
- **Proxy config:** `/etc/portage/make.conf` with http_proxy + FETCHCOMMAND
- **List installed:** parses `/var/db/pkg/` directory tree
- **Package format:** Source tarballs + ebuilds

### 4. nix (NixOS) — ✅ Done

- **Platform:** NixOS, and any system with nix-daemon
- **URL patterns:** `cache.nixos.org`, `*.cachix.org`, custom substituters
- **Proxy config:** systemd drop-in override for nix-daemon service
  (`/etc/systemd/system/nix-daemon.service.d/monopsony-proxy.conf`)
- **List installed:** `nix-store --query --requisites` (preferred), `nix profile list` (fallback)
- **Package format:** NAR (Nix Archive) files + `.narinfo` metadata
- **Special considerations:** Content-addressed model — NAR files are immutable
  by hash, making them ideal cache candidates.

## After Implementation

When a backend is implemented:
1. Add the backend file to `crates/monopsony-common/src/package_manager/`
2. Register it in `mod.rs` (PackageManagerKind enum, from_str, get_backend)
3. Add to `get_all_backends()`
4. Update the website platform strip from "Planned" to active
5. Update test count in AGENTS.md and on the website
6. Write integration test with a real or mock repository
