# B4 §5 — CC-toggle sudo-osascript prototype (bench run 2026-10-31)

**Bench:** m2mini (Mac mini M2 Pro, macOS 26.3.1 25D2128), over read-mostly ssh as
`martin@100.91.238.111` (MagicDNS `m2mini` does not resolve from thequietbox).
**Status:** mechanism VERIFIED — final in-person approval at the console still pending
(nobody at the bench during the ~7.5-minute dialog window; agent torn down cleanly,
no state changed).

## Bench state found (pre-existing, not changed by this run)

- **Content Caching is ALREADY active and correctly scoped** — so `setup mac --enable`
  is idempotent here; the prototype validates the elevated *chain*, not a state change:
  - `AssetCacheManagerUtil -j isActivated` → `{"name":"isActivated","result":true}`
  - `settings` → `AllowPersonalCaching: false` / `AllowSharedCaching: true`
  - `~/.local/bin/monopsony-client setup mac --status` →
    `cache ON ✓ active — shared content only (personal/iCloud caching off)`
- **monopsony-client daemon is DOWN**: LaunchAgent `com.societus.monopsony-client` is
  registered (gui/501, `RunAtLoad`+`KeepAlive`) but crash-loops with
  `Error: No client certificates found in /Users/martin/.config/monopsony`. **Root cause
  refined in the completion pass (§11.2): the certs EXIST — the whole dir is root-owned
  (client.key is 600 root) from the Oct 15 elevated pairing run, so martin's daemon can't
  read them.** `~/.local/state/monopsony/` is root-owned the same way, which silently
  freezes the HIT-ratio counters. One-line console fix:
  `sudo chown -R martin:staff ~/.config/monopsony ~/.local/state/monopsony`.
  The completion pass paired fresh via `--cert-dir` (dashboard pair/init code,
  dedup-replace, same client_id d1e8834e-4585-444c-8662-9d89d753cae5) and proved the
  daemon + proxy path work that way.
- Unprivileged probes (`isActivated`, `canActivate`, `settings`) all work over ssh.
  `AssetCacheManagerUtil statistics` is **root-gated** on 26.3.1
  (unprivileged → `must run as root`) — consistent with content_cache.rs, which only
  promises status/capability probes unprivileged.

## Elevation matrix — what sudo+osascript can and cannot do without a GUI prompt

| Path | Result | Evidence (real output) |
|---|---|---|
| `sudo -n` (headless, no ticket) | ✗ | `sudo: a password is required` rc=1 — no NOPASSWD, no cached ticket over ssh |
| osascript `with administrator privileges` **from an ssh session** | ✗ instant | `0:162: execution error: The administrator user name or password was incorrect. (-60007)` — no prompt is ever presented into an ssh session; osascript cannot elevate headless |
| osascript **non-admin** shell-out over ssh | ✓ | `do shell script "echo NONADMIN-OK whoami=$(whoami)"` → `NONADMIN-OK whoami=martin`, rc=0 |
| osascript admin from a **GUI-domain process** (transient LaunchAgent, bootstrapped gui/501) | ◐ presents | Dialog presented and waited (agent `state = running`, no error, 7.5 min unattended) → **presentation mechanism verified**; completion needs one human approval at the console |
| osascript admin with **embedded credentials** (`user … password …`) | ✓ headless but FORBIDDEN | Works by design, but the password rides in process arguments — plaintext exposure; never ship this |
| **sudoers NOPASSWD whitelist** | ✓ headless | The only clean no-prompt path; needs ONE root step at console/MDM to install (see below) |

## Structural facts that shape the prototype (all verified)

- **TILDE PITFALL:** inside `do shell script … with administrator privileges` the `~`
  expands in **root's** context → `/var/root` (verified: `dscl . -read /Users/root
  NFSHomeDirectory` → `/var/root`). The bench-pack §5 sample command
  (`~/.local/bin/monopsony-client …`) would fail file-not-found once elevated.
  **Always use the absolute `/Users/martin/.local/bin/monopsony-client`.**
- **Single-prompt chain:** osascript elevates to root → `setup mac --enable` runs → its
  `sudo sh -c '/usr/bin/AssetCacheManagerUtil activate && /usr/bin/defaults write
  /Library/Preferences/com.apple.AssetCache AllowPersonalCaching -bool false &&
  /usr/bin/AssetCacheManagerUtil reloadSettings'` executes under root (sudo as root =
  no second prompt, standard sudo behavior) → **one GUI password prompt total**.
- **No SIP interaction:** the CC toggle needs no softwareupdated restart —
  `reloadSettings` handles the cache daemon. The SIP killall/kickstart limitation
  belongs to the SU catalog lane (B3 verdict), not here; do not conflate.
- **Adjacent finding:** `networksetup` system-proxy toggling (`proxy on/off --system`)
  works over ssh from the admin user with **no sudo** — Tier-1 is headless-safe.
  Cosmetic bug spotted: the success row renders empty args (`ran /usr/sbin/networksetup -> ` ×8).

## The prototype (validated shapes)

Console / menu-app target — one dialog, Cancel-safe:

```sh
# enable (activate + scope shared-only + reload)
osascript -e 'do shell script "/Users/martin/.local/bin/monopsony-client setup mac --enable" with administrator privileges with prompt "Monopsony: enable Content Caching (shared content only)"'
# disable
osascript -e 'do shell script "/Users/martin/.local/bin/monopsony-client setup mac --disable" with administrator privileges with prompt "Monopsony: disable Content Caching"'
```

Presentation path proven this run: the enable line was delivered via a transient
LaunchAgent (`com.monopsony.b4-dialog-test`, ProgramArguments = `/usr/bin/osascript -e
<enable line>`), `plutil -lint` OK, `launchctl bootstrap gui/501` rc=0, dialog presented
(agent stayed `running` with empty stdout/stderr until torn down). Teardown:
`launchctl bootout` rc=0, plist removed, CC state unchanged (still activated + shared-only).

Headless production alternative (after a one-time root step at console/MDM): a root-owned
`/usr/local/libexec/monopsony-cc-toggle` wrapper (fixed content mirroring
`enable_action()`/`disable_action()` — no user input, no quoting hazards) whitelisted as:

```
martin ALL=(root) NOPASSWD: /usr/local/libexec/monopsony-cc-toggle enable, /usr/local/libexec/monopsony-cc-toggle disable
```

Then both CLI and menu app toggle with **no prompt at all**, and the GUI osascript path
is only needed on machines without the sudoers step.

## §5 verdict (wire the button vs status-only)

**WIRE THE BUTTON** — the mechanism reliably presents the auth dialog from a GUI-domain
process; one final approval at the m2mini console closes it. Implement as a Swift
osascript wrapper with: absolute binary path (no `~`), explicit `with prompt` text, and
both enable/disable verbs. Fallback stays as documented: if the dialog misbehaves inside
the menu app itself, slice 1 reverts to status-only + "run in Terminal" hint.

## Leftovers on the bench (intentional)

`/tmp/b4-dialog.out`/`.err` (empty), `/tmp/b4-sus.log`, `/tmp/b4-sus2.log`,
`/tmp/b4-proxyhits.log`, `/tmp/b4-su-list*.log` — evidence for the companion note
(`2026-10-31-b4-scan-traffic-verdict.md`). Daemon still crash-looping as found
(root-owned cert dir — fix above); completion pass also left `~/.monopsony-b4-certs`
(2 KB martin-owned paired certs) + `/tmp/b4-home` (HOME skeleton for the counter proof)
+ `/tmp/b4-daemon*.log`, `/tmp/b4-su-list4.log`.
