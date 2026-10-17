# Centralized Updates & OS Release Readiness

One dashboard for the whole fleet (50+ containers, mini-PCs, workstations, business
server, hypervisors/NAS). Reports **update counts** ("X security / Y other") and
**OS-release readiness** ("Debian 12 → 13 ready to upgrade"), with a copy-paste
upgrade guide rendered per machine.

Scope: all package managers Monopsony already implements (20 backends — apt, dnf,
mport, pacman, pkg, pkg_add, pkgin, apk, winget, choco, scoop, npm, pip, cargo,
zypper, xbps, emerge, nix, brew). Order of implementation is a non-issue; we cover
as many PMs as exist at each phase.

---

## Proven failure (why not just "apt-get upgrade" everywhere)

Monopsony already ships push-update machinery for *application* binaries — the
shadow package system with `CheckShadowUpdates`/`ApplyShadowUpdate`, a managed
pipeline (pre_update → snapshot → atomic swap → health_check → rollback), signed
recipes, and a reputation-gated server-global catalog. That is **not** an OS-release
upgrade:

- A shadow recipe has an **atomic fallback**: fail by `cp -rf new /old` + restart +
  health check. Rollback is a file swap.
- An **OS upgrade has no atomic rollback.** You migrate the running kernel, hand off
  Ceph metadata (`pveadm ceph reset`), rejoin quorum, reboot into new initramfs — and
  if the client dies mid-way you have bricked a node that `cp -rf` cannot resurrect.

Conflating them is how an OS upgrade becomes "just another apt-get". This feature does
**not** extend the shadow pipeline to whole-OS installs; it adds a *separate*
assessment + guidance layer on top of shared delivery plumbing (gRPC push, dashboard
assignment). The server **renders instructions, never executes commands** — this is
the same §4b trust boundary the update proxy relies on.

---

## Decisions log

| # | Decision | Rationale | Flippable default |
|---|----------|-----------|-------------------|
| 1 | Advisory-only for Phases 1–4; no auto-push / "1-key push" | Matches §4b: the update proxy is client-facing code and there is no safe runtime flip on untrusted networks. Push intent must be operator-proven. | Keep advisory until a robust security/privacy enhancement exists (public servers stay off). |
| 2 | **Vendor tooling does readiness; we parse it.** No bespoke cluster/space detection modules. | Proxmox VE ships `pve8to9` which auto-FAILs if any cluster node cannot be upgraded and limits the batch until quorum is lost — it answers exactly "is this ready?" Ubuntu has `do-release-upgrade --dry-run`. Parsing WARN→review / FAIL→not-ready lets every platform reuse its own authoritative preflight instead of us re-implementing it. | None. |
| 3 | **Readiness lives on the client; version availability on the server.** | Only the machine can introspect its own OS (cluster membership, free space). The server is the source of truth for *which distro versions are supported/available* so a client never silently adopts an unknown major. They merge into the verdict. | None. |
| 4 | **Guided recipes reuse the existing shadow `RecipeScript` parser** (display-only repurpose) — no new TOML parser. | The recipe schema already parses install ordering, health checks, config paths; we render it as a checklist rather than executing it. Zero new parsing code. | None. |
| 5 | **Server renders the guide for display only.** Later phases may let an opt-in client *apply its own generated guide locally* (dual-gated), never server-driven. | Keeps blast radius on LAN-only; no OOB management, no single-key push. The safety lock is a client-side opt-in + operator proof-of-presence gate, not anything the server does. | Advisory→opt-in-apply stays gated behind Phase 5. |
| 6 | **Update counts (security vs other) come from the PM's own "upgradable" listing** (`apt list --upgradable`, `dnf list --updatable`), reported by client at heartbeat/inventory. | Reuses each backend already on disk; no new query logic beyond mapping existing upgradable-list output into a count + severity split (security = anything in the distro's security channel / held for security). | None. |
| 7 | **One doc, all PMs.** Order does not matter — implement across every implemented backend at each phase. | User directive: coverage order is irrelevant; breadth (all our backends) matters more than sequencing. | Coverage scope = current backend set; new backends fold into the next phase for free. |

---

## Trust / architecture model

- **Client introspects itself**, produces a structured verdict, sends it up the gRPC
  stream (the same `ReportInventory`/heartbeat channel already exists — extend with
  readiness fields). Server never runs anything on untrusted networks (§4b).
- **Server curates** supported-version availability and authored recipes; merges the
  client's local version into "upgrade available + ready" and renders a guide.
- **Display only through Phase 4.** A later opt-in apply path (dual-gated: server flag
  + client opt-in, default off) would let a client run its *own* rendered guide locally;
  it never executes a command the server chose for it. This is the "push" half and stays
  behind a safety lock — no out-of-band management, no single-key push.

### Reporting surfaces (real, not new UI from scratch)

The dashboard already renders: `GET /api/v1/clients` (client list), per-client package
inventory (`GET /api/v1/clients/:id/inventory`), and the shadow catalog via
`db.list_shadow_catalog()`. We **extend** those — counts + readiness flags + rendered
guide attach to existing rows rather than building a new page.

### New gRPC messages (proto additions)

```proto
// Client -> Server: update counts + OS availability + readiness verdict
message UpdateReport {
    string client_id = 1;
    int32 total_updates = 2;   // from apt/dnf upgradable listing
    int32 security_updates = 3;
    int32 other_updates = 4;

    // OS-release availability (server-merged with local version):
    repeated OsReleaseUpgrade releases = 5;

    // Readiness verdict + vendor-parsed reasons:
    OsReleaseUpgrade selected_release = 6;   // the one we'd recommend, or "" if none
    repeated string readiness_notes = 7;     // e.g. "in cluster: yes (review)", "free space: 82%"
}

message OsReleaseUpgrade {
    string distro = 1;         // e.g. "debian"
    string from_version = 2;   // client's current, e.g. "12"
    string to_version = 3;     // available, e.g. "13"
    bool ready = 4;            // true only if vendor preflight WARN/FAIL all clean
    repeated string readiness_notes = 5;
}

// Server -> Client: rendered guided-recipe steps (display only)
message GuidedRecipeStep {
    int32 order = 1;
    string group = 2;         // "package", "sources", "services", "reboot"
    string action = 3;        // e.g. "apt update && apt full-upgrade -y"
    bool blocking = 4;        // true notes that must be reviewed/cleared first
}

message GuidedRecipe {
    repeated GuidedRecipeStep steps = 1;
}
```

---

## Phased implementation (each slice ships + verifies on the test fleet independently)

### Phase 1 — Update counts across all implemented PMs (+ dashboard surface)

- **Client:** add an OS/distro upgradable-listing capability. Each Linux backend emits a
  count from its own `upgradable` command; map to security vs other (security = distro's
  held-for-security channel). Report via `UpdateReport` at heartbeat/inventory.
- **Server:** accept + persist counts per client; merge into the dashboard client list.
- **Dashboard:** show "X security / Y other updates" on each client row.
- Reuse: backend upgradable-listing wiring (apt/dnf) — new small module, no parser reuse needed here.

Estimate: ~2–3 days. Verify: counts correct for apt + dnf hosts; dashboard renders; heartbeats carry them.

### Phase 2 — OS-release availability (server-curated supported versions)

- **Server:** author a curated release list (`debian`, `ubuntu`) → which `(distro)` majors are
  currently available/supported (e.g. Debian 12→13). This is the source of truth so a client
  never adopts an unknown major blindly.
- **Client:** report local distro + version; merge with server list → `OsReleaseUpgrade` rows.
- **Dashboard:** "Debian 12 → 13 ready" appears for eligible clients, absent for none-available.

Estimate: ~1–2 days. Verify: Debian 12 client sees the 13 row; a synthetic "unsupported major" does not appear.

### Phase 3 — Vendor preflight parsing (readiness via WARN/FAIL)

- **Client:** run each platform's own preflight and parse output:
  - Debian/Ubuntu → `do-release-upgrade --dry-run` readiness summary (parse the "not ready for"/warnings).
  - Proxmox VE → vendor tooling (`pve8to9`) — it auto-FAILs if any cluster node can't upgrade and caps the batch until quorum is lost. Parse WARN→review / FAIL→blocking-not-ready; **do not** re-implement cluster detection (the tool already answers it).
  - Other distros → their equivalent "prepare"/dry-run preflight where one exists; absence = report `ready` with a note that vendor parsing isn't implemented for this backend yet.
- **Client:** fill live variables into the verdict ("in cluster: yes", "free space: 82%").
- **Dashboard:** readiness flag + annotated reasons per release row (why it's *not* ready).

Estimate: ~3–4 days; vendor parsing is a new module but no re-implemented detection. Verify: a PVE client in-cluster reports the cluster WARN correctly against your real `pve8to9` output on a test node; Ubuntu dry-run parses cleanly.

### Phase 4 — Authored guided recipes (rendered checklists, display only)

- **Server:** author per-distro/per-version recipes (`debian-release`, `ubuntu-do-release-upgrade`,
  `proxmox-ve`) encoding correct/safe step ordering so a wrong sequence ("stop corosync before ceph")
  can never drift. Reuse the existing shadow `RecipeScript` parser (display-only). Live variables are
  filled from Phase 3's verdict at render time.
- **Dashboard:** paste-ready checklist per ready client — e.g. for PVE:
  1. apt update & dist-upgrade
  2. change apt sources to `<new distro slug lines>`
  3. stop corosync, pmxcfs, ceph services
  4. apt update & upgrade again
  5. confirm completion and reboot
- **Nothing executes.** Display-only through Phase 4.

Estimate: ~2–3 days. Verify: rendered guide matches the intended safe ordering for a PVE + Ubuntu example; no server-side command execution path exists in this feature.

### Phase 5 (deferred — gated) — Opt-in local apply ("the push half")

- **Not built in this doc.** A dual-gated client opt-in to *apply its own rendered guide locally*,
  operator-proven, with a safety lock (no OOB management, no single-key push). Stays behind §4b;
  re-scoped at kickoff.

Estimate: TBD / backlog tail. Verify: gated feature — no executor until scope decision.

---

## What this does NOT defend

- **Auto-push / "1-key push."** No server-driven command execution anywhere in Phases 1–4. The
  §4b threat model (malware push over an untrusted update proxy) is preserved; the update proxy
  remains client-facing code with no safe runtime flip on untrusted networks.
- **Out-of-band management.** No OOB control plane. Push intent would be operator-proven, not a broadcast.
- **Public exposure.** Names-only/full metrics + push inherit the private-trust-boundary gate;
  any server advertising `public = true` stays advisory-only.

## Open questions (assumed defaults — flip cheaply)

1. **Coverage now = current backend set** (20 PMs). New backends auto-fold into Phase 1 for free. *(default: yes)*
2. **PVE cluster detection via vendor tool only** — we parse WARN/FAIL, don't re-implement quorum logic. *(default: confirmed; do not add bespoke corosync module unless a future PVE release removes `pve8to9`-style preflight)*
3. **Phase 5 apply stays gated.** *(default: advisory only until security hardening exists)*
