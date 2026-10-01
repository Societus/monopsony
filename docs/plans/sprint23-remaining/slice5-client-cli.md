# Slice 5 — Phase E: client CLI `proxy on|off|status --pm npm|pip|cargo`

**Pack first:** `docs/plans/sprint23-remaining/context-pack-common.md`
**Safe-list:** `crates/monopsony-client/src/main.rs` — EXACTLY these touch points:
the `--pm` help text of `ProxyCommand::On`/`Off`/`Status` (clap doc comments, lines
~368-400), `run_proxy_command` (line ~2535) and any helper fns you add. NO other
regions of main.rs (it is 2,600+ lines and shared — keep the diff anchored).
Second file allowed: a NEW `crates/monopsony-client/src/lang_pm_config.rs` if you
prefer the logic there with unit tests; main.rs holds only thin dispatch.

## Current state (verified from tree)

`ProxyCommand::On { pm: Option<String>, docker, shadow, all, server }` already
exists; `run_proxy_command` at line 2535 dispatches Status/On/Off. The `--pm` value
currently accepts distro PM names (pacman/apt/...). Your job: make `--pm npm|pip|cargo`
route to the Phase B backends' config actions instead of erroring/being ignored.

## Design

- The three backends ALREADY implement `proxy_config(proxy_addr)` /
  `remove_proxy_config()` (staged Phase B files in
  `crates/monopsony-common/src/package_manager/{npm,pip,cargo}.rs` — read the one
  you're dispatching for, it's in your read whitelist). The client should call
  through the backend trait: `monopsony_common::package_manager::get_backend(kind)`
  → `get_backend(PackageManagerKind::Npm)` etc. (registered in mod.rs, landed).
  Execute the returned `ProxyConfigAction` via the SAME executor the existing
  Configure path uses — find it (`rg 'ProxyConfigAction' crates/monopsony-client/`)
  and reuse; do NOT write a second executor.
- `status --pm npm`: check whether the config the action would write exists and
  contains the proxy line (read-only file check), print ON/OFF like the distro
  status path does.
- Update the clap doc comment: "Enable specific package manager (pacman, apt, dnf,
  apk, winget, choco, scoop, npm, pip, cargo)".
- Unknown `--pm` value → clear error listing valid values (existing behavior for
  distro PMs — match it).

## Tests

Unit tests for any parsing/dispatch helper you add (in-file `#[cfg(test)]` or in the
new module). Execution of real config writes is fleet-verification territory — do
NOT write to real HOME files in tests; test the action-selection logic
(backend→ProxyConfigAction mapping) with asserts on the returned action shapes
(WriteFile path/content) exactly like the backend tests do.

## Verify (once, from repo root)

`cargo test -p monopsony-client --lib 2>/dev/null; cargo test -p monopsony-client lang_pm 2>/dev/null; cargo check -p monopsony-client` — compiles clean, new tests pass.
(The client crate is bin-heavy; if `--lib` finds no tests, `cargo check` + any
in-file test module via `cargo test -p monopsony-client --bin monopsony-client
<filter>` is the verdict.)
