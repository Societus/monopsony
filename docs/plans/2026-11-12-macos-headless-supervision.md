# macOS headless supervision — design doc (client daemon under launchd)

**Date:** 2026-11-12
**Status:** DESIGN (proposal only — nothing implemented; no files touched outside this doc)
**Provenance:** live bench facts from m2mini (M2 Pro, macOS 26.3.1 / Darwin 25.3.0, arm64), 2026-11-11 session, all verified before this doc was written. Tree citations checked against the working tree at HEAD + the DEF round's uncommitted changes. **Sprint number intentionally NOT claimed** — plan docs informally reserve numbers and 34 is already reserved (server-mediated self-update); the operator assigns the next free number at roadmap entry.

---

## 1. Problem / proven failure

The client daemon must supervise under launchd on headless macOS hosts (no GUI login — CI nodes, kiosk Macs, build farms). Two failure classes are live-proven on the bench; the design below is built on top of them, not around them.

### 1.1 User-domain and GUI-domain bootstrap is refused over ssh (macOS 26.3.1)

- `launchctl bootstrap gui/<uid> <plist>` from an ssh session: **fails, EIO 5**, even as root. `bootstrap user/<uid>`: same refusal.
- Bisected exhaustively on 2026-11-11: label conflicts, plist content, ownership, and domain health are all ruled out. launchd's own log exposes **no readable rejection reason**. Current macOS simply refuses to let an ssh-originated process materialize a service in a user/gui domain, full stop.
- Corollary from the same bench: `gui/<uid>` domains exist only for **console-login users** — a headless box has no live gui domain to bootstrap into even locally.
- The legacy `launchctl load -w` path reaches a *user* LaunchAgent only inside that user's console session (the skill's user-domain template `templates/com.monopsony.client.launchd.plist` documents its kickstart proof as console-session-only for this reason).

**Consequence:** any installer path that installs the client as a user LaunchAgent on a headless host produces a service that never runs and cannot be started over ssh. This is an OS behavior, not a bug we can fix — the design must target the system domain for headless hosts.

### 1.2 The default-port crash-loop shape (DEF-1) and its in-tree fix

- A "client daemon crash-loop at the default proxy port" is **not** a double-bind: the client has exactly one production `TcpListener::bind` (comment block at `crates/monopsony-client/src/proxy.rs:1079-1084`). It is a **cross-process port collision** — a second monopsony-client daemon, or a local monopsony-server whose mirror router defaults to the same port. On m2mini a legacy GUI-domain daemon has held `127.0.0.1:3129` since 2026-10-31.
- Diagnostic traps that cost a full misdiagnosis (both fixed or documented now):
  - non-root `lsof -i` **cannot see another user's socket** — use `netstat -anv` on macOS;
  - the daemon used to print `Local proxy listening on …` **before** the real bind, claiming success for a bind that failed.
- **Fix landed in tree (uncommitted, part of the DEF round under review):**
  - the production bind (`proxy.rs:1085-1100`) now maps `ErrorKind::AddrInUse` to a self-diagnosing error naming the address, the two likely holder classes, and the remedy: *"local proxy bind <addr> failed: address already in use — another process is listening there (a second monopsony-client daemon, or a local monopsony-server whose mirror router defaults to the same port); stop that process or pass a distinct --proxy-addr"*;
  - the pre-bind false claim is removed; the **single source of truth** is the post-bind line `info!("Client proxy listening on {}", listener.local_addr()?)` (`proxy.rs:1141`);
  - guarded by test `bind_conflict_error_names_address_and_remedy` (`proxy.rs:1233`) which asserts the message contains the bind address and the conflict wording.
- m2mini workaround in production: the CI daemon's args carry `--proxy-addr 127.0.0.1:0` (ephemeral port), sidestepping the held 3129.

**Consequence:** templates shipped for hosts that may carry a legacy daemon must pin a distinct proxy addr, and the service layer must rely on the post-bind log line as the health signal.

---

## 2. Ground truth this design stands on

| # | Fact | Source |
|---|------|--------|
| F1 | `bootstrap` into `user/<uid>` and `gui/<uid>` domains from ssh fails EIO 5 even as root; cause not surfaced in launchd logs | bench 2026-11-11 (bisected) |
| F2 | The ONLY working headless pattern is a **system-domain LaunchDaemon**: `/Library/LaunchDaemons/com.monopsony.client-ci.plist`, root:wheel 644, `launchctl bootstrap system`, `launchctl kickstart -k` restart-proven | bench 2026-11-11; live on m2mini since 2026-11-11 |
| F3 | That daemon carries `--proxy-addr 127.0.0.1:0` (DEF-1: legacy GUI daemon holds 3129) | bench; live plist |
| F4 | `detect_init()` returns `launchd` unconditionally on Darwin | `dist/install.sh:1071-1073` |
| F5 | `install_service_launchd()` exists for the **server only**: render → `chown root:wheel` → `chmod 644` → `bootstrap system` → `enable` → `kickstart -k`, with legacy `load -w` fallback | `dist/install.sh:1588-1607` |
| F6 | `TEMPLATE_FILES` carries 8 templates; the only plist is `com.monopsony.server.plist` — there is **no client launchd template in `dist/init/`** | `dist/install.sh:1140-1149` |
| F7 | The server plist's domain-critical properties: root:wheel 0644 under `/Library/LaunchDaemons` (= system domain), `UserName monopsony`, `RunAtLoad`, `KeepAlive`, `ThrottleInterval 5`, `RUST_LOG=info` env, log paths under `/var/log/monopsony/` | `dist/init/com.monopsony.server.plist` |
| F8 | Client daemon CLI surface: `monopsony-client [--cert-dir DIR] daemon [--proxy-addr ADDR] [--server ADDR] …` — `--proxy-addr` defaults to `127.0.0.1:3129`; **there is NO `--config` flag on `daemon`** | `crates/monopsony-client/src/main.rs` (`Daemon` variant, `run_daemon`) |
| F9 | The daemon **requires paired certs** and exits with a pair instruction when absent — pair before loading the service | `run_daemon` cert load (main.rs) |
| F10 | `@PLIST_EXTRA_ARGS@` renders one `<string>` per arg via sed `r blockfile` + `d`; `@INSTALL_DIR@` is platform-resolved | `dist/install.sh:1430-1445` (`render_template`) |
| F11 | Precedent bug: `dist/init/monopsony-client.service` ships `daemon --config /etc/monopsony/client.toml`, an argument the daemon does not accept — field-validated crash-loop | `dist/init/monopsony-client.service`; `monopsony-installer-init` skill pitfall |
| F12 | macOS dir conventions already in the installer: `/etc/monopsony`, `/var/lib/monopsony`, `/var/log/monopsony`, `monopsony` user via dscl with free-ID scan 300-399 | `setup_user_and_dirs_macos` (`dist/install.sh:396-438`) |
| F13 | Lifecycle dispatch for launchd exists server-side: stop/disable → `bootout`/`unload -w`, restart → `kickstart -k system/com.monopsony.server`, uninstall → rm plist | `dist/install.sh:1677-1691, 1969-1971` |

---

## 3. Decisions log (settled here so debates don't re-litigate)

| # | Decision | Rationale |
|---|----------|-----------|
| D1 | **System-domain LaunchDaemon is the shipped headless service shape** for the client. | The only bench-proven pattern (F2). User/gui domains are refused over ssh (F1) — shipping them as the default would install dead services on exactly the hosts this lane targets. |
| D2 | **`detect_init()` is NOT changed.** The init system is `launchd` either way; the *domain* is a new, orthogonal dimension handled by a new flag, not a new init-system verdict. | Keeps `MONOPSONY_INIT_SYSTEM=launchd` pin semantics intact (F4) and avoids conflating two decisions. |
| D3 | **One new template: `dist/init/com.monopsony.client.plist`** (system-domain client daemon), mirrored in `template_embedded()` and `TEMPLATE_FILES`. The existing user-domain LaunchAgent template **stays skill-side** (`monopsony-installer-init/templates/`) as the documented manual path for console-session hosts — not installer-managed in this design. | Minimal surface: ship the proven pattern; the user-domain path has no headless validation and would need its own console-session battery. |
| D4 | **Args in the plist follow the validated CLI surface verbatim**: `<INSTALL_DIR>/monopsony-client --cert-dir <CERT_DIR> daemon --proxy-addr <ADDR> [--server <ADDR>]`. No `--config` (F11 precedent). | Every template arg must round-trip against the shipped CLI (F8) — the systemd unit's invalid arg is the cautionary tale. |
| D5 | **Proxy-addr policy (detail in §4.2):** template pins a concrete `--proxy-addr` at render time (default `127.0.0.1:3129`), installer warns when the target port is already held, `127.0.0.1:0` is the documented escape hatch for hosts with a legacy daemon. | Predictable addr for `configure` consumers by default; proven workaround preserved (F3). |
| D6 | **Client LaunchDaemon runs as root by default** (no `UserName` override beyond what the render sets — root is the system-domain default), with an explicit `--cert-dir` under `/var/lib/monopsony/client` and logs under `/var/log/monopsony/`. | Matches the proven client-ci pattern (F2); root avoids the `$HOME`-derived state-path permission traps documented in the B4 bench lessons (root-owned `~/.config/monopsony` poisoning). Dedicated dscl user is a hardening follow-up (open question Q2). |
| D7 | **Pair-before-load is an installer precondition**, not a template concern: the service setup step refuses (with the pair command hint) when the cert dir has no certs. | The daemon exits without certs (F9); loading a plist for a cert-less daemon guarantees a KeepAlive crash-loop. Same ordering rule already documented for the CI daemon ("pair before loading"). |
| D8 | **Reuse `@PLIST_EXTRA_ARGS@` unchanged** for the client plist's variable tail (`--server` addr when configured); `--cert-dir` and `--proxy-addr` are first-class render tokens handled like the server's `--config` path. | No new render mechanism needed (F10); keeps CF-tunnel extras capability for free even though the client daemon is local-only today (non-goal, §7). |

---

## 4. Design

### 4.1 Installer/init proposal

**New template `dist/init/com.monopsony.client.plist`** (system domain), shape modeled on the server plist (F7):

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>com.monopsony.client</string>

    <key>ProgramArguments</key>
    <array>
        <string>@INSTALL_DIR@/monopsony-client</string>
        <string>--cert-dir</string>
        <string>@CLIENT_CERT_DIR@</string>
        <string>daemon</string>
        <string>--proxy-addr</string>
        <string>@CLIENT_PROXY_ADDR@</string>
@PLIST_EXTRA_ARGS@
    </array>

    <key>RunAtLoad</key>
    <true/>

    <key>KeepAlive</key>
    <true/>

    <key>ThrottleInterval</key>
    <integer>5</integer>

    <key>WorkingDirectory</key>
    <string>/var/lib/monopsony</string>

    <key>StandardOutPath</key>
    <string>/var/log/monopsony/client.log</string>
    <key>StandardErrorPath</key>
    <string>/var/log/monopsony/client-error.log</string>

    <key>EnvironmentVariables</key>
    <dict>
        <key>RUST_LOG</key>
        <string>info</string>
    </dict>
</dict>
</plist>
```

New render tokens: `@CLIENT_CERT_DIR@` (default `/var/lib/monopsony/client/certs`), `@CLIENT_PROXY_ADDR@` (default `127.0.0.1:3129`). Both are simple value substitutions added to `render_template` alongside the existing `@INSTALL_DIR@` rule; `@PLIST_EXTRA_ARGS@` is reused as-is for the optional `--server <addr>` tail (D8). Rendering stays POSIX-safe sed with output redirect — no `sed -i` (BSD sed).

**Installer wiring:**

- `install_service_launchd_client()`: clone of the server function's proven sequence (F5) with the client label — render to `${LAUNCHD_DIR:-/Library/LaunchDaemons}/com.monopsony.client.plist`, `chown root:wheel`, `chmod 644`, `launchctl bootstrap system` → `launchctl enable system/com.monopsony.client` → `launchctl kickstart -k system/com.monopsony.client`, legacy `load -w` fallback.
- **Cert preflight (D7):** before rendering, check `@CLIENT_CERT_DIR@` contains the cert set; when absent, print the exact pair command hint (matching the daemon's own error text, F9) and skip service install — clean state, not half-paired with a dangling listener.
- **Domain flag:** `--launchd-domain {system,user}`, **default `system`**. `user` renders into `~/Library/LaunchAgents` without a `UserName` key (the skill's LaunchAgent shape) and the installer prints the console-session caveat: bootstrap/kickstart must run inside the user's console session; over ssh it will fail (F1). This flag exists so a console host *can* choose user-domain, not because headless needs it.
- **CLI surface for the client install:** the installer's client mode currently targets Linux daemons; macOS client install gets the same treatment as server install — binary into `@INSTALL_DIR@` (`mkdir -p /usr/local/bin` first on Apple Silicon), then `install_service_launchd_client`.

**Lifecycle coverage** (dispatch without lifecycle support strands uninstall — skill rule). Every existing launchd case (F13) gains a client-label twin:

| Verb | Addition |
|------|----------|
| `service_stop_and_disable` | `launchctl bootout system/com.monopsony.client \|\| launchctl unload -w <plist>` |
| `service_restart` | `launchctl kickstart -k system/com.monopsony.client` |
| `do_uninstall` | bootout/unload + `rm` the client plist |
| `print_info` status | `launchctl print system/com.monopsony.client` + tail of `/var/log/monopsony/client.log` |

**`dist/init/README.md`** table gains the client plist row; CI artifact glob (`cp dist/init/* dist-out/`) already covers new files automatically; `--check-templates` must pass after the embedded copy is synced via `--emit-templates`.

**Field-validation checklist** (gate for any implementer, before the live battery):

1. `grep -r '@INSTALL_DIR@\|@PLIST_EXTRA_ARGS@\|@CLIENT_CERT_DIR@\|@CLIENT_PROXY_ADDR@'` over both renders (plain + any extras variant) — zero leftover tokens.
2. `xmllint --noout` on the rendered plist; then `python3 -c "import plistlib; …"` structural asserts — plain-mode `ProgramArguments` must be exactly `[<bindir>/monopsony-client, --cert-dir, <certdir>, daemon, --proxy-addr, <addr>]` (+ extras when set); `KeepAlive` true; `ThrottleInterval` 5; log paths present.
3. **CLI-surface round-trip (D4):** extract every `ProgramArguments` element after the binary and confirm each parses as an argument the shipped CLI accepts (`monopsony-client --help`; `daemon --help`). This is the check that would have caught the `--config` unit bug (F11). Automate it: diff the plist arg set against `--help` output in the validation script rather than eyeballing.
4. `MONOPSONY_INIT_SYSTEM=launchd bash -c 'source dist/install.sh; detect_init'` echoes the pin (on Linux hosts — the macOS branch itself is Darwin-gated and stays covered by the m2mini battery).
5. Lifecycle smoke with the unit absent: `service_stop_and_disable` / `service_restart` for the client label must warn and return, never abort (set -e safety).
6. Emit round-trip: `--emit-templates` output diffs clean against `dist/init/` (excluding README.md).
7. `bash dist/install.sh --check-templates` passes (byte-identical embedded vs canonical).

**Live battery (m2mini, per the skill's five-lane recipe):** recon → source installer → `setup_user_and_dirs` idempotence → **pair first** (D7; enrollment-key flow, `--proxy-addr` irrelevant to pairing) → `install_service_launchd_client` → assert: rendered file token-free, registered under system domain, service UP, post-bind log line `Client proxy listening on <addr>` present in `/var/log/monopsony/client.log` → `service_restart` yields a PID change → `service_stop_and_disable` leaves it down/unregistered/process-gone → CF-mode-style extras render to /tmp only. Boot-persistence is already proven for this plist class on this host (the client-ci daemon has survived unattended since 2026-11-11); a dedicated reboot pass is still owed to the *installer-managed* path (see verification matrix).

### 4.2 Template policy: proxy-addr pinning

**Problem:** any host that ever carried a GUI-session client daemon may still have that daemon holding `127.0.0.1:3129` (m2mini: holding since 2026-10-31). A template that hardcodes the default port turns the service install into a guaranteed crash-loop (DEF-1) — and before the in-tree fix, into a *silent* one.

**Policy (D5), three layers:**

1. **Render-time pin with a preflight check.** The installer, before rendering, probes whether the configured proxy addr is already bound on the local host (`netstat -anv` on macOS — non-root `lsof -i` cannot see other users' sockets, which is exactly the legacy-daemon case). If held, the installer **warns and names the remedy**, and proceeds only with an explicitly provided `--proxy-addr` override; with no override it aborts the service-install step with the same self-diagnosis text the daemon itself now emits. Never render a plist whose bind is known-doomed.
2. **Explicit override flag.** `--proxy-addr` on the installer's client path flows into `@CLIENT_PROXY_ADDR@`. This includes the ephemeral form: `--proxy-addr 127.0.0.1:0` is a legal, documented value.
3. **Ephemeral-port escape hatch, with a documented trade.** `127.0.0.1:0` guarantees startup on any host but makes the port unknowable in advance — anything that must *point at* the daemon (PM `configure` actions, scripts) cannot predict it; the consumer must read the post-bind line (`Client proxy listening on <addr>`, F-log) from the service log. Suitable for CI/bench hosts where humans or log-reading scripts consume the addr; not suitable as a fleet default. **Default stays the pinned `127.0.0.1:3129`.**

Rule of thumb for future templates: *a template may hardcode a default port only when the installer preflights it; a host class known to carry legacy daemons must pin a distinct addr.* (This generalizes the existing installer-init rule "templates shipped for hosts that may carry a legacy daemon must pin a distinct proxy addr".)

### 4.3 How the self-diagnosing AddrInUse error surfaces in service logs

With the service supervised (KeepAlive + ThrottleInterval 5), a port collision produces a **visible, self-describing crash-loop** instead of a silent one:

- stderr of every failed start lands in `StandardErrorPath` (`/var/log/monopsony/client-error.log`). Each crash iteration appends one block:
  `Error: local proxy bind 127.0.0.1:3129 failed: address already in use — another process is listening there (a second monopsony-client daemon, or a local monopsony-server whose mirror router defaults to the same port); stop that process or pass a distinct --proxy-addr`
  (verbatim from `crates/monopsony-client/src/proxy.rs:1088-1094` — the exact text the fix ships; test-asserted to contain the bind address and the conflict wording at `proxy.rs:1233`).
- The **healthy signal** is the post-bind line in `StandardOutPath`: `Client proxy listening on 127.0.0.1:3129` (`proxy.rs:1141`). Because the pre-bind false claim was removed, *presence of a listening line now implies a successful bind* — this is the line the battery, monitoring, and operators should grep for, and its absence alongside AddrInUse blocks in the error log is the collision signature.
- **What the template header comment must document** (in the plist's XML comment, mirroring the user-domain template's doc style):
  1. the collision signature: crash-loop + `address already in use` naming the bind address in `client-error.log`;
  2. the two holder classes the message names (second client daemon / local server mirror router) and the two remedies (stop the holder, or set a distinct `--proxy-addr` via reinstall/override);
  3. the diagnostic gotcha that cost the original misdiagnosis: `lsof -i` as non-root cannot see another user's listener — use `netstat -anv` on macOS;
  4. `--proxy-addr 127.0.0.1:0` as the ephemeral escape hatch and its trade (§4.2).
- Operator-facing triage one-liner to put in the README: `grep -c "address already in use" /var/log/monopsony/client-error.log` (looping count = collision) paired with `grep "Client proxy listening" /var/log/monopsony/client.log` (bound addr when healthy).

### 4.4 Migration notes: the m2mini `com.monopsony.client-ci.plist`

The live plist **is** the pattern this template codifies (F2/F3). Migration to the installer-managed shape is deliberately conservative:

1. **Inventory first:** `plutil -p /Library/LaunchDaemons/com.monopsony.client-ci.plist` — record the live `ProgramArguments` (including the actual `--cert-dir` value), `UserName` presence/absence, log paths, and the daemon's post-bind log line. These recorded values become the render inputs so the migrated service is behavior-identical (same cert dir, same proxy addr).
2. **Label strategy — keep `com.monopsony.client-ci` on that host for now** (open question Q5). Rationale: booting a second label (`com.monopsony.client`) before the old one is booted out yields two daemons — one binds (ephemeral), one crash-loops. The installer-managed render can carry the `-ci` label via the same render inputs; a fleet-wide label normalization to `com.monopsony.client` belongs to a later pass, done as bootout-old → bootstrap-new in one step.
3. **Cutover order (one operator session):** stop + bootout the old label → `launchctl bootstrap system <new plist>` → `kickstart -k` restart proof → verify post-bind line + PID change → only then delete the old plist file. Never leave both loaded.
4. **Binary identity before verdicts:** verify the deployed binary by sha256/md5 against the built artifact, never `--version` (reports the build-tree Cargo.toml, mis-tags new code). The AddrInUse self-diagnosis text is itself a staleness probe: `strings /usr/local/bin/monopsony-client | grep -c "address already in use"` — 0 hits = pre-DEF-1 build.
5. **Do not "fix" the pinned `127.0.0.1:0` during migration.** The legacy GUI daemon still holds 3129 on that host; un-pinning re-introduces the crash-loop this workaround exists for. The addr can change only when the legacy daemon is removed (operator action, §7).
6. After migration the host exercises the standard battery paths (restart proof, stop/disable, restore), making m2mini the canonical validation host for the template going forward.

### 4.5 Phased implementation (each slice ships and verifies independently)

| Phase | Scope | Effort | Exit gate |
|-------|-------|--------|-----------|
| P1 | Template + embedded copy + `TEMPLATE_FILES` + two render tokens + `install_service_launchd_client()` + lifecycle dispatch twins + README row; field-validation checklist items 1-7 all green (static, host-independent) | S | `--check-templates` clean; renders valid in both modes; CLI round-trip check passes |
| P2 | Installer client path on macOS: cert preflight + `--proxy-addr` preflight/override + `--launchd-domain` flag (system default) | S | Preflight aborts on held port with self-diagnosis text; override flows into the render |
| P3 | Live battery on m2mini (§4.1 battery list) + migration of `client-ci` per §4.4 + one reboot pass proving unattended boot persistence of the installer-managed service | M | Service UP post-reboot with no console login; restart proof; log signals verified |
| P4 | Docs pass: `docs/bare-metal-install.md` macOS client section + template header comment content (§4.3 items 1-4) + `monopsony-installer-init` skill updated with the new lane facts | S | Skill + README carry the collision-signature triage and the pinning policy |

Dependencies: P2 depends on P1 (tokens exist); P3 depends on P1+P2; P4 depends on P3 (facts proven before documenting). No phase touches the four dirty DEF-round files — P1/P2 are `dist/`-only; the tree's uncommitted client fix is a *prerequisite* consumed by this design, not work this plan performs. **Landing order note:** the DEF-1 fix must land (operator sign-off) before P3, so the bench client binary actually carries the self-diagnosis the docs describe.

---

## 5. Verification matrix (what proves what)

| Claim | Proven by | Status |
|-------|-----------|--------|
| System-domain LaunchDaemon survives + restarts headless | live `client-ci` plist since 2026-11-11, kickstart-proven | PROVEN (F2) |
| AddrInUse self-diagnosis text + post-bind-only listening line | in-tree uncommitted fix + test `bind_conflict_error_names_address_and_remedy` | LANDED-UNCOMMITTED (gate: operator review) |
| User/gui-domain ssh bootstrap refusal | 2026-11-11 bisection | PROVEN (F1) |
| Rendered plist validity + token freedom | P1 static checklist | PENDING (this plan) |
| Installer-managed install/restart/stop on real macOS | P3 battery | PENDING (this plan) |
| Unattended boot persistence of the installer-managed service | P3 reboot pass | PENDING (this plan) |
| User-domain flag path on a console host | not scheduled — manual path only this design | OUT OF SCOPE (§7) |

---

## 6. What this does NOT defend / solve (honesty section)

- **It does not fix the EIO 5 refusal.** The root cause inside launchd is unknown and its logs are unreadable on the subject; this design *routes around* the refusal (system domain) and will silently keep working only for as long as Apple permits system-domain bootstraps from ssh. A macOS major that gates *that* would need a new design — the spike methodology in `references/macos-client-platform.md` (probe live on the bench OS before designing around flags) applies to any future revisit.
- **It does not remove the legacy GUI daemon on m2mini** — that daemon (holding 3129 since 2026-10-31) is operator-owned state; the design only avoids colliding with it.
- **It does not solve the reboot reachability chain:** FileVault locks ssh until one console login, and Tailscale (on this bench) does not start until GUI login. The service itself is boot-persistent; the *box* is unreachable after reboot until a console login. Headless uptime and headless reachability are different guarantees; this design delivers only the former.
- **It does not validate the user-domain (`--launchd-domain user`) path on a console host** — that path ships documented-but-unproven; its kickstart proof requires a console session by definition (F1 corollary).
- **It does not add CF-tunnel mode for the client daemon.** The five-localhost-bind CF shape is a server-surface concept; the client daemon is local-only today (non-goal, D8 context). `@PLIST_EXTRA_ARGS@` keeps the door open without building anything.
- **It does not decide the client.toml-vs-server.json resolution question** (see Q6): templates in this design do not depend on client.toml at all, so either answer leaves them correct.

---

## 7. Open questions for the operator (each with an assumed default)

| # | Question | Assumed default (flip cheaply) |
|---|----------|-------------------------------|
| Q1 | Which sprint number owns this? (Plan docs reserve numbers; 34 is taken by server-mediated self-update.) | None claimed in this doc; operator assigns at roadmap entry. |
| Q2 | Client LaunchDaemon identity: root (proven, D6) vs dedicated dscl `monopsony` user (hardening, needs cert-dir/state ownership work + `UserShell /usr/bin/false` home handling)? | **root**, matching the live client-ci pattern; dedicated user as a later hardening slice. |
| Q3 | Template proxy-addr default: pinned `127.0.0.1:3129` (predictable for `configure`, preflight-guarded) vs ephemeral `127.0.0.1:0` (crash-proof, log-consumed)? | **pinned 3129 with preflight abort** (§4.2); ephemeral stays an explicit override. |
| Q4 | Console-user detection to auto-select `--launchd-domain`: ship an `UNVERIFIED` probe (e.g. console-ownership check) or always default system until benched? | **always system**; no unverified mechanism ships (plan-doc conventions: recalled mechanisms are marked UNVERIFIED or left out). |
| Q5 | m2mini label during migration: keep `com.monopsony.client-ci` or normalize to `com.monopsony.client` in the same cutover? | **keep `-ci`** (§4.4 item 2 — avoids twin-daemon windows); normalize in a later single-cutover pass. |
| Q6 | Daemon server resolution: the macOS bench note says the daemon ignores `client.toml`'s `[server]` table, while the current tree's `run_daemon` reads `config.server.address` when `--server` is absent. Which behavior is live on the bench build? | Templates rely only on `--server` flag or cert-dir `server.json` (both bench-proven), so the answer doesn't block P1-P3; re-verify on the bench before any template starts depending on client.toml. |
| Q7 | Remove the legacy GUI daemon on m2mini (freeing 3129) before or after migration? | **after** — migration proceeds with the `:0` pin untouched; freeing 3129 is a separate operator action that then allows re-pinning to the default. |
