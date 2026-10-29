# Phase 2 grounding — service scripts (systemd + OpenRC templates)

**Source of truth:** `docs/design/shadow-packages.md` §3 invariant #2 and §7 scope
lock #2: **systemd + OpenRC only for v1**; every other init system is its own later
phase. Default "no package" template set ships baked into the client binary; location
substitution into the rendered script is part of rendering an install.

## `os_family` → template mapping (the Phase 2 dispatch table)

| os_family | Init | Template | Install path | Enable |
|---|---|---|---|---|
| debian / arch / fedora (systemd) | systemd | `systemd/*.service.template` | `/etc/systemd/system/<unit>` | `systemctl daemon-reload && systemctl enable --now <unit>` |
| alpine (OpenRC) | OpenRC | `openrc/*.openrc.template` | `/etc/init.d/<name>` (chmod 755) | `rc-update add <name> default && rc-service <name> start` |
| any systemd host, containers | systemd | `quadlet/*.container.template` | `/etc/containers/systemd/<name>.container` | `systemctl daemon-reload` (generator runs on boot/start) |

`os_family` detection must reuse the existing OS-info machinery (`OsInfo` in
`monopsony-common/src/models.rs`, and the Sprint 31 item-6 design
`docs/plans/2026-10-23-sprint31-item6-nonsystemd-detect-design.md` — non-systemd
*detection* was designed there; this phase implements the two v1 renderers, not the
other detectors).

## Location substitution contract

**Reconciled with the landed tree (2026-10-28):** the default "no package" template
set ALREADY ships in the client binary —
`crates/monopsony-client/src/shadow_service.rs` (`SYSTEMD_UNIT_TEMPLATE` /
`OPENRC_SCRIPT_TEMPLATE` via `include_str!("templates/…")`) with
`detect_init_system` (os-release content, systemd default), `ServiceSpec`, and a
fail-loud `{{var}}` renderer. Service rendering deliberately does NOT reuse the
Phase 1 step engine: recipes run services, they don't build them, and the variable
sets differ (unit_name/description/exec_start/working_dir vs
version/target_dir/arch/os/libc). Grounding rule unchanged in spirit: ONE renderer
per variable vocabulary, fail loud on unknown placeholders, and a rendered file
containing a literal `{{` is a bug.

These fixture `.template` files remain the grounded examples of rendered output for
real apps; the remaining Phase 2 work is executor/CLI wiring (render + write +
enable per init system, persistence semantics, quadlet path for containers).

## Runtime persistence semantics (design doc: on-demand, unless-stopped)

In systemd terms the two modes map to:

- **on-demand** — no `[Install]` section (never starts at boot); `Restart=no`. Starts
  only when something activates it.
- **unless-stopped** — `[Install] WantedBy=multi-user.target` + `Restart=always`:
  starts at boot, restarts on failure, and — matching Docker's `unless-stopped` — a
  manual `systemctl stop` keeps it stopped until the next boot.

Grounding rule: validate the quadlet fixture on a real podman host (bench rule — probe
the mechanism on the live OS before coding against it) before baking the template set
into the binary.
