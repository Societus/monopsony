# Tray GUI Kickoff — Design Grounding (2026-11-10)

Design-only kickoff doc for the desktop tray GUI surface (macOS / Windows / Linux / BSD tray presence, roadmap future surface). No code, no git operations. Uncommitted per house convention — the operator signs everything. This lane is scope-locked to this single file; roadmap placement and sprint-number allocation are explicitly deferred to the orchestrator (when scheduled, run the roadmap numbering check before claiming a slot).

## 0. Method and source discipline

Three earlier attempts on this card stalled on live fetching (network channels for the official docs pages were blocked on this host). The operator's takeover brief made this a zero-network lane: claims are grounded ONLY in (a) pre-fetched text snapshots staged on this host 2026-11-10 (scratch copies under `~/.hermes/cache/scratch/tray-grounding/` — transient, pruned after 24 h; the URL below each quote is the durable citation), (b) four browser-gated pages the operator hand-fetched and posted to the card as `OPERATOR-SUPPLIED GROUNDING` (treated as verified), and (c) the current repo tree, checked at authoring time. Nothing is recalled from memory as a mechanism claim.

Provenance tags used on every mechanism claim:
- **[OP-<block>]** operator hand-fetched block (WIN-TOASTS, SNI-SPEC, MAC-NAP, MAC-NOTIFICATIONS).
- **[PF:<url>]** pre-fetched snapshot; quote verified against the staged text.
- **[TREE <path>:<line>]** verified against the checkout at authoring time.
- **[UNVERIFIED]** a mechanism claim with no fetched source; it is listed in §8 and gated behind a §7 spike. Never stated as fact, never used as design load-bearing.

### Tree rails the design must respect (audit at authoring time)

| Rail | Anchor |
|---|---|
| macOS menu-bar skeleton (SwiftUI `MenuBarExtra`, CLT-only SPM, zero third-party deps, bare executable output) | `clients/macos/` — MonopsonyMenuBarApp.swift:28-38 (`MenuBarExtra` + `.menuBarExtraStyle(.window)`), AppState.swift, Core.swift, SelfTest.swift, `Resources/com.societus.monopsony-client.plist`, README.md:11,26-27,32-35 |
| Shell-out-only design law (already stated) | clients/macos/README.md:3-5 — "a thin shell over the existing Rust monopsony-client daemon... this app only shells out to the CLI and to launchctl" |
| Daemon status CLI for tray consumption (`status --quiet`) | crates/monopsony-client/src/main.rs:141-146 — "Machine-quiet output: suppress the tracing log prefix so stdout carries only the key: value lines the menu bar app parses" (landed Sprint 26 Wave A) |
| LaunchAgent flow (`launchctl bootstrap/bootout gui/$UID`, modern syntax only) | clients/macos/README.md:42-65; AppState.swift:7-9,209-213; plist template substitutes daemon with `__CLIENT_BINARY__`, RunAtLoad + KeepAlive (README:54-55) |
| Polling strategy: refresh on open + 5 s while open, zero work while closed | clients/macos/README.md:111-115 |
| Parser degradation contract: unrecognized lines dropped, "it never fabricates state" | Core.swift:74-116; README.md:122-124 ("Status parsing tracks today's CLI output shape... unknown output falls back to raw lines") |
| Client local proxy listener exists — PM forward-proxy semantics only, NOT a status/IPC surface | crates/monopsony-client/src/proxy.rs:4-5 (requests to 127.0.0.1:3129 → server HTTP forward proxy 3128) |
| `update-scan` CLI verb, macOS-only cfg guard, deliberately absent from the menu (bench verdict 2026-10-31) | main.rs:220-222,1013-1014,5042-5043; README.md:87-92 |
| Self-update CLI + managed pipeline with server-first asset sources | main.rs:204-208 (`SelfUpdate` arm); self_update.rs:76-89 (`AssetProvider::{Url, ServerStream, LocalBlob}`; doc comment: pipeline phases, snapshot and rollback logic unchanged) |
| gRPC update serving + rollout gate | proto/monopsony.proto:31-34 (`CheckUpdate`, `DownloadUpdateAsset` streaming over the mTLS surface); 621-623 (per-caller rollout gate — "an update reaches a client only when assigned — no broadcast-by-default" — plus standard revoked-cert check); `HeartbeatRequest.monopsony_version` field 7 at :138-154 |
| Cert issue/expiry/revocation surfaces | crates/monopsony-db/src/db.rs:202-208 (`client_certs` serial table; comment: short-lived certs, red-team F3, replacing 10-year un-revocable certs); client main.rs:2913-2920 (`status` prints "Certificate expires: <rfc3339> (auto-renews inside 30 days)" via `trust::client_cert_expiry`); crates/monopsony-admin/src/clients.rs:303-306 (`EXPIRING_WINDOW_DAYS: i64 = 30`, valid/expiring/revoked badge constants, 90-day issued lifetime) |
| Server-side client-health staleness | crates/monopsony-control/src/client_health.rs:1-8 (staleness from `clients.last_seen`, `[metrics].stale_threshold_secs` default 900 s = 15x 60 s heartbeat); wiring server main.rs:858-862; control grpc.rs:421-423 |
| Prometheus exporter server | crates/monopsony-control/src/metrics.rs:1-3 |
| Swift CI lane | `swift-selftest` job in .forgejo/workflows/ci.yml:245-267 (macos-arm64, continue-on-error: swift build + `--selftest` + `cargo check -p monopsony-client`). Drift note: clients/macos/README.md:108-109 cites ".github/workflows/ci.yml" — stale pointer; the job lives in the Forgejo workflow (`.github/workflows/` holds only release.yml — verified by file listing) |
| Windows + FreeBSD cross-compile already configured for the client | .cargo/config.toml:5-10 (`x86_64-pc-windows-gnu` mingw linker, `x86_64-unknown-freebsd` linker) |
| Daemon init-template family (system-level services incl. client daemon) | dist/init/monopsony-client.service + server family (systemd, OpenRC, runit, s6, SysV, launchd plist) |
| No tray or notification implementation exists in any Rust crate | content search across `crates/` for `notify-send`, `StatusNotifier`, `org.freedesktop.Notifications`: 0 matches (authoring time) |

## 1. Shell decision matrix

### Option A — per-OS native thin shells (SwiftUI menu bar; Windows unpackaged desktop app; D-Bus SNI client; BSD deferred)

Capability and maintenance notes per OS, grounded:

- **macOS.** The menu-bar window pattern exists and is bench-proven in-tree: `MenuBarExtra` with `.menuBarExtraStyle(.window)` compiles and runs on the bench (macOS 13+ requirement stated in README.md:32-35; m2mini bench at 26.3.1) [TREE]. Notification posting on macOS requires a properly structured, code-signed `.app` bundle with a valid bundle identifier — "raw bare executables fail authorization and payload registration" [OP-MAC-NOTIFICATIONS]. Today's SPM build emits a bare executable [TREE README.md:26-27], so `.app` packaging is a load-bearing gap (§7 spike 1), not a polish item. Maintenance: Swift lane already paid — dedicated CI job [TREE ci.yml:245-267] and m2mini runner exist; zero third-party dependencies [TREE README.md:11].
- **Windows.** Toasts from an unpackaged desktop app are feasible via the Windows App SDK `AppNotificationManager` API, with an AppUserModelID (AUMID) required for attribution (shortcut or registry/activator registration) and COM activation registration needed to handle toast clicks and background activations [OP-WIN-TOASTS]. The AUMID assignment surfaces are documented: `System.AppUserModel.ID` on the app's shortcut (MSI's MsiShortcutProperty table applies it at install) and process-level `SetCurrentProcessExplicitAppUserModelID`, which "must" be called "during an application's initial startup routine before the application presents any UI" [PF:https://learn.microsoft.com/en-us/windows/win32/shell/appids]. Gap: NO fetched source grounds a tray/notify-icon API available to unpackaged WinUI 3 / Windows App SDK apps — [UNVERIFIED], gated behind §7 spike 5. The Rust daemon side is ready: windows-gnu cross-compile configured [TREE .cargo/config.toml:5-7].
- **Linux.** The tray contract is StatusNotifierItem over D-Bus: "As soon as a new instance of a StatusNotifierItem is created, the application must register the unique instance name to the StatusNotifierWatcher" [OP-SNI-SPEC], against the `org.kde.StatusNotifierWatcher` service name and its `RegisterStatusNotifierItem` method; essential item properties include Id, Title, IconName, and Menu [OP-SNI-SPEC]. Deployment caveat per the same block: vanilla modern GNOME does NOT render tray icons out-of-the-box — it requires an extension (e.g. "AppIndicator and KStatusNotifierItem Support") — a deployment caveat, not a blocker. Desktop notifications exist as a standard: the freedesktop Desktop Notifications Specification (v1.3, dated 18 August 2024) is "a standard for a desktop notifications service, through which applications can generate passive popups to notify the user in an asynchronous manner of events", with a D-Bus Protocol section (spec section 9) [PF:https://specifications.freedesktop.org/notification-spec/latest/]; the fetched snapshot is only the landing page, so method-level call details are [UNVERIFIED] (§7 spike 8). Which implementation route (direct Rust D-Bus vs Qt/GTK binding) is a design choice, not a grounded capability — gated to §7 spike 7.
- **BSD.** Only the service-scripting mechanism is grounded: rc.d scripts are "controlled with rc.conf(5)" via rc.subr(8), and the `rcvar` variable names the ON/OFF knob [PF:https://docs.freebsd.org/en/articles/rc-scripting/]. No tray mechanism, no notification mechanism, and no BSD GUI bench exists in the fleet. Tray work deferred by decision (D1 below).

Maintenance weight: each shell is presentation-only (the macOS reference is ~150-300 LOC of presentation + a pure-logic core), because all logic and protocol stay in the Rust daemon which already cross-compiles per target. Cost paid per added OS shell: one small native app + its CI job + its packaging step. The macOS lane shows the floor is low when the shell is dumb.

### Option B — one cross-platform shell (Electron/Tauri/Qt/Slint/egui-class framework)

Capability claims about tray/notification support inside these frameworks are NOT grounded by any source fetched this run — [UNVERIFIED], and the recommendation below deliberately does not rest on them. What the recommendation rests on is structural reasoning (stated as reasoning, not mechanism claims):

- Every OS integration point this program needs is an OS-native primitive: a LaunchAgent plist file, a registry Run key, a `.desktop` autostart file, SNI D-Bus registration, and each OS's own notification API. A cross-shell must either wrap each primitive behind a framework abstraction (one abstraction layer to maintain per OS, version-pinned against each OS's churn) or ship its own autostart/update machinery — duplicating the installer surface the repo already maintains (dist/install.sh + dist/init family) and forking a second source of truth for service state.
- Framework runtimes add per-OS binary weight and supply-chain surface against a repo precedent of zero-dependency toolchains (CLT-only SPM build; musl-static server/client binaries).
- The repo's cross-compile matrix already covers windows-gnu and freebsd targets for the shared daemon; a GUI framework multiplies that matrix into per-OS GUI build/test lanes with no bench coverage today except macOS.

### Recommendation D1

Per-OS native thin shells sharing ONE daemon-side contract (§2), sequenced macOS → Windows → Linux. macOS leads because its rails exist and are bench-verified; BSD is deferred (no GUI bench, no grounded tray mechanism, fleet-confirmed GUI-bench absence to be re-checked at triage). The macOS skeleton is the reference shape for the other shells: dumb view layer, CLI shell-out to the daemon, OS-native autostart primitive, state from parsed daemon output.

## 2. Status surface: daemon-CLI vs local-IPC

**Option A — daemon-CLI pull** (`status --quiet`, `proxy status`, direct reads of user-domain pairing/state files):
- Parsing stability: the contract is today's human-shaped text; the tree already carries its fragility and its mitigation. README.md:122-124 states parsing tracks today's CLI output shape with best-effort field pickup and raw-line fallback; the Swift parser is built to degrade without fabricating state [TREE Core.swift:74-77]. The drift risk is real (deployed-binary staleness is a known repo bug class), but the blast radius is a degraded menu line, not wrong data shown as confident data.
- Auth: none needed — the shell-out runs as the same user reading user-domain files; cert material stays daemon-side. server.json identity is already consumed this way, mirroring the daemon's own `resolve_server_addr` chain (`ws_url` → `grpc_addr` from `~/.config/monopsony/server.json`) [TREE Core.swift:10-35]; HIT-ratio state reads the daemon-persisted `~/.local/state/monopsony/hit-counts.json` [TREE README.md:71-75].
- Latency: process spawn per refresh; menu-open-only polling (5 s while open, none closed) is the bench-proven shape [TREE README.md:111-115]. Sufficient for a status panel; not a live event stream.

**Option B — local-IPC** (a new status socket, or piggy-backing the existing client proxy):
- No such surface exists: the only local listener is the PM forward proxy on 127.0.0.1:3129 [TREE proxy.rs:4-5], and content search found no status/IPC endpoint in the client crate. Building one means a new auth/trust boundary (any same-user process could connect), framing, and versioning — net-new maintenance with zero existing rails [TREE absence above].
- Its only wins are push latency and high-frequency data (live cache activity). Neither is a Wave B requirement.

### Recommendation D2

Daemon-CLI pull for the tray's v1 data, with the `--quiet` key:value output formalized as the frozen tray API (versioned with the binary, documented key list). A `--json` variant for the status verbs is a Wave B pre-spike (§7 spike 9) so new panels stop multiplying text-parse fragility rather than adding to it. Local IPC is designed only when a proven push-latency requirement appears (e.g. live activity graphs). Live-event notifications (§3) ride the daemon-surfaced-state + tray-poll model of §5, not an IPC stream.

## 3. Per-OS notification mechanisms (update-available + failure alerts)

### macOS

- Mechanism: `UNUserNotificationCenter`, authorization flow `UNUserNotificationCenter.current().requestAuthorization(options:completionHandler:)`; provisional authorization (`.provisional` option) sends notifications quietly to Notification Center without an interrupting prompt, letting the user decide later whether to keep them [OP-MAC-NOTIFICATIONS].
- Hard constraint: posting requires a properly structured, code-signed `.app` bundle with a valid bundle identifier; bare executables fail authorization and payload registration [OP-MAC-NOTIFICATIONS]. The tray/notification path MUST therefore be a signed `.app`; the existing MenuBarExtra skeleton is the right vehicle, and a bare daemon binary cannot post notifications itself [OP design consequence; TREE clients/macos/]. Consequence for the alert flow: update/failure events detected by the daemon reach the user through the tray app process, not through the daemon itself.
- App Nap: menu-bar or background-only apps "may" be put in App Nap when they lack visible windows (and active assertions); an app leaves nap automatically "when the user brings the app to the foreground or when the app receives an XPC message or an Apple event" [OP-MAC-NAP]. Delivery of standard user notifications is handled by system daemons and is generally not blocked by App Nap; the opt-out API is NSProcessInfo activity options (e.g. `beginActivityWithOptions:` with user-initiated / latency-critical flags) [OP-MAC-NAP]. Design consequence: notification delivery does not need an App Nap opt-out, but in-process timers (any poll loop that survives menu-closed) may need one — verify in §7 spike 2 rather than pre-adopting.

### Windows

- Mechanism: Windows App SDK notification APIs (`AppNotificationManager`) with Notification Center; unpackaged desktop apps can show toasts [OP-WIN-TOASTS]. An AUMID "must be set or registered (either via shortcut or registry/activator setup) so Windows can attribute the toast to the correct application", and COM activation registration is required to handle user clicks and background activations [OP-WIN-TOASTS].
- AUMID assignment surfaces (grounded separately): AUMIDs "are used extensively by the taskbar in Windows 7 and later systems to associate processes, files, and windows with a particular application"; explicit IDs are "strongly recommended" (system-assigned ones cannot be retrieved and heuristics cost performance) [PF:appids page, learn.microsoft.com/en-us/windows/win32/shell/appids]; assignment locations are the shortcut's `System.AppUserModel.ID` property (MSI can apply it at install time) and process-level `SetCurrentProcessExplicitAppUserModelID` "during an application's initial startup routine before the application presents any UI" [PF: same page].
- Packaged vs unpackaged: monopsony releases are unpackaged (tar.gz release assets + installer; no MSIX — repo release-asset contract; no Windows assets published today, cross-build only) [TREE .cargo/config.toml:5-7 + release asset naming per repo convention]. The unpackaged path is therefore THE design path; the packaged/MSIX path is not chosen and its differences are [UNVERIFIED].
- Install-time consequence (operator design consequence): the AUMID shortcut/registry step belongs to the installer, and a Start-Menu shortcut exists anyway, so the toast attribution rides the existing install step rather than a bespoke one.

### Linux

- Notifications: freedesktop Desktop Notifications Specification v1.3 (18 Aug 2024) — "a standard for a desktop notifications service, through which applications can generate passive popups to notify the user in an asynchronous manner of events"; explicitly excludes modal dialogs; use cases in-spec include transfer completions and low-resource warnings [PF:https://specifications.freedesktop.org/notification-spec/latest/]. The spec's section 9 is D-Bus Protocol [PF: same page, contents]; the fetched snapshot covered the landing page only, so the exact method signatures/capability negotiation are [UNVERIFIED] and gated to §7 spike 8. The client crate contains no notification code today [TREE grep, §0 rails table last row].
- Tray + notification coexistence: tray presence uses SNI (§1); the GNOME extension caveat from the operator block applies to the tray icon only, while the Notifications spec targets any desktop session with a notifications daemon — both are deployment-environment assumptions to verify per bench target, not code blockers.

### BSD

- Everything beyond the rc.d mechanism is [UNVERIFIED]: whether a freedesktop-compatible notifications daemon or libnotify-equivalent tooling is present on a given BSD install varies and was not grounded this run. Design stance: BSD support = daemon + rc.d service only; no BSD tray/notification work until a BSD GUI bench and a mechanism fetch both exist.

## 4. Per-OS autostart

### macOS — LaunchAgent

- Grounded: on user login, launchd scans the agent directories including `~/Library/LaunchAgents` ("User Agents ... Run on behalf of: Currently logged in user") [PF:https://www.launchd.info/]; job keys `RunAtLoad` ("launched once at the time the job is loaded. The default is false") and `KeepAlive` ("kept continuously running or to let demand and conditions control the invocation"; a true KeepAlive implies RunAtLoad) [PF:launchd.plist(5) man page via https://www.manpagez.com/man/5/launchd.plist/].
- In-tree rails: daemon agent template + modern `bootstrap`/`bootout gui/$UID` flow already implemented and bench-verified [TREE README.md:42-65, AppState.swift:209-213]. The tray app's own autostart is the same mechanism (a second agent entry pointing at the signed `.app`). The LoginItem alternative is [UNVERIFIED] (not fetched) and not chosen.

### Windows — Run key (HKCU default)

- Grounded: "Use `Run` or `RunOnce` registry keys to make a program run when a user logs on. The `Run` key makes the program run every time the user logs on, while the `RunOnce` key makes the program run one time, and then the key is deleted. These keys can be set for the user or the machine." The four keys are HKLM/HKCU × Run/RunOnce; values are command lines capped at 260 characters; execution order among multiple entries under one key is indeterminate [PF:https://learn.microsoft.com/en-us/windows/win32/setupapi/run-and-runonce-registry-keys].
- Design consequence: per-user install writes `HKCU\...\Run` (no elevation); an all-users install writing HKLM requires elevation and lands only behind the per-user path (open question Q3). The 260-char cap means the entry points at a fixed install path, not a long argument chain.
- StartupAppTask and the Task Scheduler route: NOT grounded by any fetched source this run — [UNVERIFIED]; neither is selected for v1. They are recorded here as options the kickoff matrix was asked to consider, held for a future grounding pass if the Run key proves inadequate (e.g. needs-runlevel or hidden-launch semantics).

### Linux — XDG autostart for the tray; system unit for the daemon

- Tray app: ".desktop file in one of the Autostart directories... automatically launched during startup of the user's desktop environment after the user has logged in"; default user directory `~/.config/autostart/` when `$XDG_CONFIG_HOME` is unset, system-wide `/etc/xdg/autostart/`; per-user masking via the `Hidden` key (Hidden=true in the most important directory silences same-named files in the others); Desktop Entry Specification format required [PF:https://specifications.freedesktop.org/autostart-spec/latest/].
- Daemon: stays a system-level service via the existing template `dist/init/monopsony-client.service` [TREE] — installed by the installer, not by autostart machinery. systemd user units are documented (units under `~/.config/systemd/user/`, `systemctl --user enable` for autostart; the user manager "is started after the first login of a user and killed after the last session of the user is closed" unless lingering via `loginctl enable-linger`) [PF:https://wiki.archlinux.org/title/Systemd/User] — recorded as the per-user-daemon alternative (Q4 default: system unit remains; a user unit is opt-in for SSH-only/headless users running daemon-side work, and it is NOT the tray's mechanism).

### BSD — rc.d knob

- Grounded: "rc.d scripts are controlled with rc.conf(5)" via rc.subr(8); the `rcvar` variable names the ON/OFF knob; a warning is emitted when rcvar is set but the knob variable is unset, and third-party scripts should provide the knob default themselves [PF:https://docs.freebsd.org/en/articles/rc-scripting/]. A FreeBSD daemon install therefore lands as an rc.d script + `monopsony_enable="YES"`-style knob. Gap flagged: dist/init currently ships no FreeBSD template [TREE §0 rails table] — backlog item for the installer lane, not this one. GUI-session autostart on BSD is [UNVERIFIED] and rides the deferred tray (D1).

## 5. Update-trigger path (tray invokes, daemon executes)

**Standing law (D5): the tray is display + trigger. The `monopsony-client` daemon/CLI is the executor. The tray process never elevates and never writes system paths.** This mirrors the shell-out law already bench-proven on macOS [TREE README.md:3-5].

- Self-update invocation: the CLI arm exists [TREE main.rs:204-208 `SelfUpdate { Check | Apply }`]. `apply` runs the managed safety pipeline (hooks → snapshot → download/extract → atomic binary swap → health check → commit or rollback); the Sprint 34 P3 rework made asset sources server-first via `AssetProvider::{Url, ServerStream, LocalBlob}` while "the pipeline phases, snapshot and rollback logic are unchanged" [TREE self_update.rs:76-89]. The tray's "Update available" action shells out to the same verbs — identical pattern to the Wave A system-proxy toggle [TREE README.md:94-95]. The exit-code surface (check/apply outcomes the menu can map to badge states) is recorded in the development skill but must be re-verified against the current binary before Wave B — §7 spike 10.
- Server-mediated gating: the tray must not contact GitHub or any upstream directly. Update availability comes from the mTLS gRPC pair `CheckUpdate` / `DownloadUpdateAsset`, which are gated per-caller: "an update reaches a client only when assigned — no broadcast-by-default", plus the standard revoked-cert check [TREE proto/monopsony.proto:31-34,621-623]; the client's version posture already travels in `HeartbeatRequest.monopsony_version` [TREE proto:154]. A GitHub-direct fetch remains a CLI fallback (`AssetProvider::Url`), not a tray job.
- No privilege escalation: the client binary lives in the user domain (macOS default `~/.local/bin/monopsony-client`, per `AppState.MonopsonyConfig.clientBinary`) [TREE README.md:36-40], and the managed pipeline swaps that binary with snapshot/rollback — no system paths, no elevation. The daemon's own cert renewal is internal and already automatic: status reports "Certificate expires: <rfc3339> (auto-renews inside 30 days)" [TREE main.rs:2913-2920].
- Shadow-package updates keep their consent gate: they remain in the staging + client-approval workflow (the `shadow` CLI surface with its view/accept subcommands [TREE main.rs:196-197 wiring `shadow_cli::PackageCommand`]). The tray may surface "recipe staged, review it" notifications, but install execution — including any elevation a recipe's `target_dir` would need — stays an explicit, interactive, human-approved action outside the tray's background process. Where a GUI must elevate at all, the only sanctioned precedent in-tree is the documented osascript admin wrapper for the macOS Content Caching toggle (exactly one password dialog per action, absolute binary path, no embedded credentials, status-only fallback) [TREE README.md:81-86, MonopsonyMenuBarApp.swift:107-135] — and it carries a pending bench approval; copying that pattern anywhere else requires its own bench verdict.
- macOS-specific carve-out: the menu's Update-scan button is deliberately absent per the 2026-10-31 bench verdict (SUS traffic through the system proxy is an uncached CONNECT tunnel; `softwareupdate --background` is throttle-gated so the button frequently no-oped); the `update-scan` CLI verb stays as the bench/diagnostic primitive [TREE README.md:87-92, main.rs:220-222,5042-5043]. Do not re-add it to the menu without a new verdict.
- Event origination (open question Q2, assumed default): the DAEMON polls `CheckUpdate` on its heartbeat cadence and exposes the answer as a status key; the tray reads it and fires the native notification. Alternative (tray shells out to `self-update check` on a timer) is simpler but adds process churn and GitHub-direct fallback drift; default rejected.

## 6. Trust-state panel data source

What a tray trust panel can show today, all rails verified:

| Panel row | Source | Anchor |
|---|---|---|
| mTLS cert expiry + auto-renew posture | `status` prints the parsed PEM expiry: "Certificate expires: <rfc3339> (auto-renews inside 30 days)" via `trust::client_cert_expiry`; 90-day issued lifetime, serial-tracked, individually revocable (red-team F3 design) | TREE main.rs:2913-2920; db.rs:202-208 |
| Server identity + pairing (host, tunnel-vs-direct, dashboard URL) | `~/.config/monopsony/server.json`, `ws_url` → `grpc_addr` — the Swift rail already mirrors the daemon's `resolve_server_addr` chain | TREE Core.swift:10-35,54-56 |
| Per-service proxy state rows | `proxy status` subsystem rows, parsed with ✓/✗ spine | TREE Core.swift:59-116; README.md:71 |
| Cache HIT ratio | daemon-persisted `~/.local/state/monopsony/hit-counts.json`, ratio line "cache hit ratio:" | TREE README.md:71-75; Core.swift:118-147 |
| Update posture (version + assigned-availability) | `CheckUpdate` answer surfaced by the daemon + `HeartbeatRequest.monopsony_version`; rollout is assigned-only with revoked-cert check at the TLS/RPC layer | TREE proto:31-34,154,621-623 |

Machine-readability gap: the cert line today is human prose inside status output [TREE main.rs:2919]; a Wave B contract should add a machine key (e.g. `cert days remaining:`) as part of the §7 spike 9 contract freeze, so the tray stops scraping a sentence.

What the tray does NOT read (D6): the server's Prometheus exporter and the client-health staleness plane. Staleness is derived server-side from `clients.last_seen` against `[metrics].stale_threshold_secs` (default 900 s) and exposed via the metrics exporter and dashboard badges (`EXPIRING_WINDOW_DAYS: i64 = 30` drives the valid/expiring/revoked badge) [TREE client_health.rs:1-8; control grpc.rs:421-423; server main.rs:858-862; metrics.rs:1-3; clients.rs:303-306]. Those are fleet-ops views; the trust panel is the LOCAL view (my cert, my server, my proxy rows). Scraping :9090 from each tray would add a second, unauthenticated read path per client and duplicate what the dashboard already owns. Server-health "is my server okay" for the tray therefore means: daemon connection state (already reflected in the Wave A icon state — connected/disconnected/error [TREE Core.swift:149-163; README.md:79-80]) plus the daemon-mediated CheckUpdate/heartbeat posture — not Prometheus.

## 7. Kickoff checklist — spikes required on real hardware before Wave B

macOS (bench: m2mini, macOS 26.3.1, runner + CI lane exist):
1. **Signed `.app` bundle packaging spike** (gates ALL macOS notification work): wrap the SPM executable as a bundle with a valid identifier, code-sign (identity TBD), test `requestAuthorization` + provisional delivery with a locally-built bundle. The bare-executable failure mode is documented [OP-MAC-NOTIFICATIONS]; the packaging recipe itself is [UNVERIFIED] until this spike runs.
2. **App Nap behavior** of a MenuBarExtra app with the poll-timer pattern: confirm whether any timer survives menu-closed gets napped (the design already claims zero work while closed [TREE README.md:111-115] — verify that claim's runtime truth, not just its code), and decide on NSProcessInfo activity assertions per [OP-MAC-NAP].
3. **LaunchAgent for a GUI app** (tray itself as agent entry) + KeepAlive interplay with the daemon agent — confirm GUI-domain app launched from an agent context behaves as expected on 26.3.1.
4. **Tray icon + `.app` interplay**: MenuBarExtra inside a LaunchAgent-bootstrapped bundle vs user-launched app.

Windows (bench recipe exists in the cross-platform-binary-deployment skill — Win11 VM; release CI publishes no Windows assets today, so bench takes the cross-built binary):
5. **Tray icon API for unpackaged apps** — the matrix's open question: no grounded API source was fetched (§3/[UNVERIFIED]); this spike exists precisely to ground it. Candidate mechanisms must be fetched-and-cited before selection.
6. **Toast end-to-end**: `AppNotificationManager` from the cross-built unpackaged binary; AUMID via Start-Menu shortcut property + `SetCurrentProcessExplicitAppUserModelID` call before UI [PF:appids; OP-WIN-TOASTS]; COM activation registration for click handling [OP-WIN-TOASTS].
7. **HKCU Run key write from the installer** against the real install path (260-char cap check) [PF:runkeys].

Linux (bench targets must be provisioned — none exists for GUI sessions today):
8. **Notifications round-trip** on a real desktop session (spec method/capability grounding beyond the landing page) [PF:notification-spec thin → UNVERIFIED until hardware].
9. **SNI registration** minimal implementation on KDE Plasma (native watcher) and on GNOME with the AppIndicator/KStatusNotifierItem extension [OP-SNI-SPEC]; decide direct-Rust-D-Bus vs toolkit binding per Q1 with the spike's evidence.

Contract spikes (OS-independent):
10. **`status --quiet` contract freeze**: enumerate the current key set (e.g. the "cache hit ratio:" line the Swift rail parses), decide the `--json` variant, add machine keys for cert days-remaining and update-available posture (§6). Freeze as the tray API with a version field.
11. **Exit-code surface re-verification** for `self-update check/apply` against the current binary (skill-recorded codes: 0/1/2/3/4/5 — re-verify in tree, do not inherit).

Cross-cutting:
12. **BSD: no Wave B work.** Record the deferral decision and its two preconditions (BSD GUI bench exists + tray/notification mechanism fetched and cited). Confirm "no BSD GUI bench in fleet" at triage.
13. **README CI-pointer drift fix** (`.github/workflows/ci.yml` → `.forgejo/workflows/ci.yml`) — trivial, but do it in the same lane that next touches clients/macos/ so the drift doesn't outlive its discoverer.

## Decision log (settled — do not re-litigate)

- **D1 — Shell architecture:** per-OS native thin shells + one shared daemon contract; order macOS → Windows → Linux; BSD deferred. Rationale: OS integration primitives are native; the cross-shell alternative was rejected on structural grounds (duplication of installer/service surface, zero-dependency precedent, no bench coverage), with framework capability claims explicitly left ungrounded.
- **D2 — Status surface:** daemon-CLI pull; `--quiet` key:value output frozen as the tray API; `--json` pre-spike; no local-IPC until a proven push requirement.
- **D3 — Notifications:** native per OS. macOS gate = signed .app bundle (bare binaries fail) [OP]. Windows gate = AUMID registered at install + COM activation for clicks [OP]. Linux = freedesktop Notifications daemon, method details verified on hardware. BSD = none.
- **D4 — Autostart:** LaunchAgent (macOS, existing rails), HKCU Run key (Windows), XDG autostart .desktop for the tray + system unit for the daemon (Linux), rc.d rcvar knob for the daemon (BSD); tray GUI itself deferred on BSD.
- **D5 — Update trigger:** tray displays and triggers; daemon/CLI executes through the managed pipeline and the mTLS assigned-only rollout gate; shadow installs keep the view/accept consent flow; no background elevation from the tray process — the osascript wrapper remains the only GUI-elevation precedent and needs its bench approval before reuse.
- **D6 — Trust panel data:** local facts (cert expiry from status, server.json identity, proxy rows, hit ratio, daemon connection state) + daemon-mediated CheckUpdate posture; no tray-side Prometheus scraping; staleness/badge planes stay server/dashboard-side.

## Open questions (each with a cheap assumed default the operator can flip)

- **Q1 (Linux tray implementation route):** direct Rust D-Bus SNI vs toolkit binding. Default: direct SNI from a Rust tray binary (zero-dependency precedent); flip if spike 9 shows the direct path too heavy. Capability of both routes is currently ungrounded — the spike decides, not recall.
- **Q2 (update-available origination):** daemon-surfaced status key (default) vs tray-shelled `self-update check` timer.
- **Q3 (Windows install topology):** per-user HKCU (default, no elevation) vs all-users HKLM (elevation-gated installer path).
- **Q4 (Linux daemon topology):** system unit stays (default); systemd user unit + linger as opt-in for headless per-user setups — never as the tray's launch mechanism.
- **Q5 (macOS tray autostart):** LaunchAgent second entry (default, grounded) vs LoginItem (uninvestigated, [UNVERIFIED]).
- **Q6 (notification dedupe):** where the "already told the user" state lives so tray restarts don't re-toast the same update event. Default: tray-side persisted last-notified key in the user-state dir; exact mechanism is a Wave B design item, not a mechanism claim.

## What this doc does NOT ground (honesty ledger)

Every claim below is [UNVERIFIED] — none is load-bearing in the recommendations; each is gated behind a §7 spike or a deferral:

- Windows tray/notify-icon API available to unpackaged apps (no source fetched; spike 5).
- Windows StartupAppTask and Task Scheduler autostart mechanics (matrix options considered, rejected for v1 without grounding).
- Windows packaged (MSIX) vs unpackaged notification behavior differences beyond the operator's unpackaged notes.
- Linux freedesktop Notifications method-level details (interface names, capability negotiation) — landing-page fetch only.
- Linux SNI implementation library/binding capabilities (Qt/GTK vs direct D-Bus).
- macOS LoginItem mechanism; the concrete signing/bundling recipe for the .app (bundle-identifier + codesign identity requirements).
- All BSD tray/notification claims — nothing fetched; the entire BSD GUI lane is deferred rather than designed.
- Cross-platform framework capability claims — D1 rejected Option B on structure, explicitly not on any framework's claimed capabilities.
- Fleet-fact "no BSD GUI bench exists" — operator-context statement, marked for triage confirmation.

## Sources appendix

Operator hand-fetched (2026-11-10, via OPERATOR-SUPPLIED GROUNDING card comment; treated as verified):
- https://learn.microsoft.com/en-us/windows/apps/develop/notifications/app-notifications/ (Windows app notifications, AppNotificationManager)
- https://specifications.freedesktop.org/status-notifier-item/latest-single (StatusNotifierItem spec, single-page)
- https://developer.apple.com/library/archive/documentation/Performance/Conceptual/power_efficiency_guidelines_osx/AppNap.html (App Nap, archived power-efficiency guide)
- https://developer.apple.com/documentation/usernotifications/unusernotificationcenter (UNUserNotificationCenter)

Worker-side pre-fetched snapshots (2026-11-10; quoted text verified against staged copies):
- https://www.launchd.info/ (LaunchAgent directories, login-time loading)
- https://www.manpagez.com/man/5/launchd.plist/ (launchd.plist(5): RunAtLoad, KeepAlive, LimitLoadToSessionType)
- https://learn.microsoft.com/en-us/windows/win32/shell/appids (AppUserModelIDs: purpose, where to assign)
- https://learn.microsoft.com/en-us/windows/win32/setupapi/run-and-runonce-registry-keys (Run/RunOnce keys)
- https://specifications.freedesktop.org/notification-spec/latest/ (Desktop Notifications Specification v1.3 landing page; thin — details UNVERIFIED)
- https://specifications.freedesktop.org/autostart-spec/latest/ (XDG autostart: directories, Hidden key)
- https://wiki.archlinux.org/title/Systemd/User (systemd user units, autostart, linger)
- https://docs.freebsd.org/en/articles/rc-scripting/ (FreeBSD rc.d / rc.conf rcvar)

Fetch-failure record (why marks exist): earlier lane attempts failed 3× on the Windows toast pages, the SNI spec, and the Apple App Nap page from this host (404/bot-block/DNS-dead), and the Apple usernotifications page returned a JavaScript-only shell offering a .md mirror [PF snapshot]. Those four topics were then covered by the operator blocks above; the freedesktop notifications landing page remains thin by design of the fetch.

Tree citations: file:line anchors inline throughout; verified against the working tree at authoring time (2026-11-10), which carries uncommitted Sprint 34 P1-P3 slices — the §5/§6 rails (AssetProvider, CheckUpdate serving, cert-badge constants) reflect that uncommitted state and must be re-anchored after the operator signs those lanes.
