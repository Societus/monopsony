# B4 §6 — Scan-traffic keep/cut verdict (bench run 2026-10-31)

**Bench:** m2mini (macOS 26.3.1), agent-driven over ssh. Test server 10.10.20.58 was
DOWN (3130/9090/50051 all refused) during the original run, so server-side CONNECT
counters were unavailable then — Mac-side evidence carried the verdict. **Same-day
completion pass (18:05–18:25 MST, server back in service) closed that gap — see
"Server-side confirmation" below and review-spec §11.2.**

## VERDICT: CUT the menu scan button. KEEP the `update-scan` CLI verb as a bench/diagnostic primitive. KEEP system-proxy (Tier 1) + Content Caching (Phase 3.5) as the real macOS surfaces.

## Setup

- As found: system proxy OFF, Content Caching ON (shared-only), daemon down (certs missing).
- Tools: accept-and-log TCP listener on `127.0.0.1:3129` (the client-proxy port) +
  `log stream --info` over `softwareupdate` / `softwareupdated` / `nsurlsessiond` /
  `mobileassetd`.
- Traffic driver: `softwareupdate -l` (foreground). `--background` was throttle-no-oped
  (Finding 1), and `update-scan` wraps exactly that verb.

## Findings (all quoted from real captures, 2026-10-31 10:25–10:35 MST)

1. **`softwareupdate --background` is throttle-gated.** Triggered twice → zero network
   activity, zero softwareupdated log lines, no sockets. Prefs show a successful scan
   ~1¾ h earlier (`LastSuccessfulBackgroundMSUScanDate = 2026-10-31 15:41:19 +0000`).
   The menu button therefore frequently no-ops by Apple's design — it cannot promise a scan.
2. **Catalog + payload topology (foreground scan):** mobileassetd resolves
   `https://mesu.apple.com/assets/macos/` and `https://gdmf.apple.com/v2/assets` for
   `com.apple.MobileAsset.MacSoftwareUpdate`; payload BaseURLs are
   `https://updates.cdn-apple.com/…` and advertise `"__CanUseLocalCacheServer" = 1` —
   Apple's own Content Caching is the sanctioned local-cache path for payloads. The
   catalog fetches themselves carry `canUseCacheServer: 0` — catalogs **never** touch a
   local cache.
3. **Routing through the system proxy is REAL.** With `proxy on --system`
   (127.0.0.1:3129), the SUS stack sent its traffic through the proxy — captured on the
   listener:
   - `CONNECT swscan.apple.com:443` (×3), `CONNECT gdmf.apple.com:443` (×5),
     `CONNECT xp.apple.com:443` (×6+) — nsurlsessiond-family, Proxy-Connection: keep-alive.
   With proxy OFF the same flow ran direct (logs show `proxyHostName:(null)`).
   So the §6 literal rule ("no observable routing → cut as cosmetic") is **not**
   triggered — routing exists. The value analysis below is why CUT still wins.
4. **Blast radius (same window):** every scutil-honoring client transits the proxy —
   Chrome was observed CONNECTing `conductor:8006` / `ingest:8006` (Proxmox noVNC
   websockets; 160+ connections in ~4 minutes). LAN short-names are NOT in the default
   exception list (`*.local`, `169.254/16` only). If system proxy ships broadly,
   Monopsony should add private-range/LAN-host exceptions (separate follow-up) or
   customers' LAN web UIs break when the daemon/server is down.

## Why CUT despite observable routing

- **Caching value: zero, by architecture.** B3 (bench 2026-10-01): client+server
  forwarding is CONNECT-only (no MITM) — tunnelled SU bytes are never cached — and
  custom SU catalogs are EV-gated dead, so Monopsony can never serve SU content as an
  origin (settled; not re-litigated here). Finding 2 confirms Apple's own CC owns the
  local-cache role (`__CanUseLocalCacheServer=1`), and Phase 3.5 already orchestrates it.
- **Observability value: marginal.** All the proxy sees of SUS is encrypted CONNECT
  tunnels — hostnames in the server's CONNECT counters, nothing more.
- **Trigger reliability: poor.** Throttle-gated (Finding 1): the button's primary action
  frequently does nothing.
- **Cost asymmetry.** The button's only Monopsony-relevant effect requires system proxy
  ON, whose blast radius (Finding 4) is a daily-use hazard for zero cache gain.
- **CLI verb stays.** `update-scan` cost nothing, is bench-proven (it drove this
  experiment), and remains the diagnostic primitive.

## What KEEP would mean (if overruled)

With system proxy ON and Apple's throttle permitting, pressing the button yields
swscan/gdmf check-in CONNECTs visible in the server's counters. That is the entire
Monopsony-visible effect. If the team values a one-tap update check as plain UX
independent of Monopsony, keeping the button is defensible on UX grounds — the evidence
does not oppose that, it just doesn't support it.

## Implementation of the cut (if adopted)

`clients/macos/Sources/MonopsonyMenuBar/MonopsonyMenuBarApp.swift` — remove the
`Button("Trigger Update Scan")` block (+ `.keyboardShortcut("u")`). `AppState.runClient`
stays (other callers). CLI `update-scan` untouched. Small enough to ride the next Swift
commit; tracked as a follow-up card.

## Server-side confirmation (completion pass, 2026-10-31 18:05–18:25 MST)

With .58 back up, a fresh pairing (dashboard `pair/init` code → `pair --cert-dir …`,
dedup-replace to the same client_id) ran the daemon via `--cert-dir` and repeated the
chain with server-side observability:

- **The counters are observable and move.** One controlled
  `curl -x 127.0.0.1:3129 https://swscan.apple.com/` through the real chain (local daemon
  → server → upstream; swscan answers 404 to non-SUS clients — irrelevant, the tunnel is
  what counts) moved `monopsony_proxy_connect_bytes_total` from absent (zero) to **5372**.
- **Real third-party traffic transits the same chain:** with system proxy ON, Chrome's
  CONNECTs reached the server and tunnelled ~4 MB (counter 5,372 → 4,016,228) in the same
  window — end-to-end CONNECT forwarding field-proven beyond the controlled test.
- **But the SUS stack bypassed the proxy both times.** Two `softwareupdate -l` scans
  (10 s apart from proxy-on, then ~80 s) produced **0 CONNECTs** at the local daemon and a
  flat counter — §11.1 captured swscan/gdmf/xp CONNECTs under the identical setting, so
  Apple's nsurlsessiond proxy pickup is **session-dependent**, not deterministic. This
  strengthens the CUT verdict: the button's only Monopsony-relevant effect is not even
  reliably reproducible session-to-session.
- **LAN/tailnet blast radius confirmed server-side:** the journal logged per-target dial
  failures while the system proxy was on — `CONNECT upstream dial ingest:8006 failed:
  dial to 100.119.165.62:8006 timed out after 10s`, same for `conductor:8006` and
  `thequietbox.tail0964.ts.net:443`. The .58 server cannot reach tailnet/LAN hosts, so
  scutil clients' LAN browsing breaks whenever system-proxy forwarding is on. Concrete
  evidence for the LAN-exceptions follow-up named in Finding 4.

## Not run / caveats

- Menu-app GUI connect — still console-only (review-spec §11 item 4), and the Oct 14
  bench client needs redeployment first: its gRPC transport dies post-TLS against the
  Oct 30 server (`Verify return code: 0 (ok)` but every reconnect errors in ~11 ms;
  LAN CT clients on newer builds connect fine).
- `AssetCacheManagerUtil statistics` bytes-served delta — root-gated on 26.3.1 over
  ssh, and CC does not serve the host's own downloads anyway; CC-routed SUS traffic
  would come from OTHER LAN devices, not this Mac's scan.

## Raw evidence on the bench (left intentionally)

`/tmp/b4-proxyhits.log` (listener captures incl. all quoted CONNECT lines),
`/tmp/b4-sus.log`, `/tmp/b4-sus2.log` (info-level SUS logs), `/tmp/b4-su-list*.log`
(scan outputs). Bench proxy state restored: `scutil --proxy` → `HTTPEnable : 0`.
