# macOS Code Signing — Firewall (LuLu) Compatibility for Self-Update

_Date: 2026-10-15_
_Status: PLANNED — not scheduled to a sprint_
_Priority: Medium — blocks frictionless headless self-update on macOS fleet nodes (m2mini)_

## Problem

On macOS fleet machines LuLu (deny-by-default outbound firewall) gates every
binary's network access. For **unsigned** binaries LuLu keys rules on
**path only** (verified: `rules.plist` entries carry `path` + `signingInfo`,
no cdhash/fingerprint enforcement for unsigned code — a rebuild at the same
path inherits the old approval).

Consequences for Monopsony on macOS:

1. `monopsony-client self-update` that installs to a **new path**
   (versioned dir, temp-then-move) loses the approval → LuLu blocks outbound
   silently. On headless nodes (no console user) the GUI alert never
   surfaces and the client just hangs — exactly the m2mini pairing hang
   debugged 2026-10-15: TCP handshake completes, payload blackholed, no
   error, no timeout on either side.
2. Even in-place updates work only by accident of path stability. The
   `allowInstalled=true` LuLu preference exempts binaries that predate LuLu's
   install time, which is why freshly built dev binaries trip prompts while
   system tools don't.

Note: LuLu itself has no CLI (GUI alerts only); there is a third-party
`lulu-cli` (woop/lulu-cli) that edits `rules.plist` with sudo, but relying on
that from `self-update` is fragile and out of scope. The durable fix is on
our side: **stable signing identity**.

## Goal

Monopsony macOS builds (client, and server if ever run on macOS) carry a
code signature whose identity survives `self-update`, so firewall rules and
Gatekeeper assessments match across versions:

- LuLu rules keyed on signing identity → one approval, all future versions.
- Gatekeeper/notarization as a side benefit for first-run on macOS hosts.

## Non-goal

Windows signing; Linux (unaffected — no per-binary outbound prompt).

## Options (decision needed)

### Option A — Developer ID + notarization (full fix)
- Sign with `codesign --options runtime --timestamp` against an Apple
  Developer ID Application cert; notarize via `notarytool`.
- LuLu then keys rules on the stable team ID. Gatekeeper-clean first run.
- Cost: $99/yr Apple Developer Program + key custody in CI/release.
- Fits the eventual public paid-server distribution story.

### Option B — Ad-hoc signing with stable identity (free, partial)
- `codesign -s -` at build/package time. Ad-hoc signatures have a cdhash
  that **changes every build**, so this alone does NOT stabilize identity.
- LuLu treats ad-hoc-signed binaries as unsigned (path-keyed) in practice —
  so Option B only helps Gatekeeper quarantine quirks, not firewall rules.
- Verdict: NOT sufficient for the stated goal. Documented to close the
  "can't we just ad-hoc sign?" question.

### Option C — Self-update contract: stable install path (minimum viable)
- Require `self-update` on macOS to always atomically replace-in-place at
  the canonical path (e.g. `~/.local/bin/monopsony-client` or
  `/usr/local/bin/monopsony-client`), never install to a versioned path.
  Write temp file, `rename(2)` over the target.
- Keeps existing path-keyed LuLu rules valid across updates.
- Zero cost, no certs, works today. Weakness: any path migration or
  reinstall-to-new-location needs one console approval; no Gatekeeper story.
- ALSO: document for users that `allowInstalled` predating LuLu install
  affects dev builds.

## Recommended plan

1. **Now (no cert needed):** implement Option C — audit `self-update` /
   shadow-system install path handling to guarantee in-place replacement on
   macOS; add a note to macOS docs about one-time LuLu approval.
2. **When public macOS distribution becomes real:** Option A (Developer ID +
   notarization) in the release pipeline; then firewall compatibility and
   Gatekeeper are both solved and Option C becomes belt-and-suspenders.

## Verification checklist (when implemented)

- [ ] `self-update` on macOS: after update, LuLu rule count for the binary
      unchanged; client connects without prompt on a headless node.
- [ ] Pairing (50052), gRPC (50051), proxy (3128) all pass post-update.
- [ ] `codesign -dv` output stable across versions (Option A).
- [ ] Documented in README/macOS setup: the LuLu one-approval model and why
      path stability matters.

## References

- m2mini pairing-hang RCA (this repo, session 2026-10-15): LuLu silently
  blocked sshd/rebuilt-binary outbound; server saw zero-byte accepted
  connections on 50052.
- LuLu rules.plist schema inspection on m2mini (2026-10-15): path-keyed,
  no fingerprint for unsigned binaries.
