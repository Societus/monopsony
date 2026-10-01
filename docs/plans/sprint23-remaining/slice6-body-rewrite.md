# Slice 6 — Phase B+: artifact-body rewrite module (pip links + cargo dl template)

**Pack first:** `docs/plans/sprint23-remaining/context-pack-common.md`
**Safe-list:** `crates/monopsony-proxy/src/body_rewrite.rs` (NEW file — placeholder +
`mod body_rewrite;` already landed in lib.rs by the parent; replace the placeholder
comment, never edit lib.rs).

## Problem (from the Phase B handoff, §4 note)

pip's simple pages embed absolute `https://files.pythonhosted.org/...` links and
cargo's sparse-index `config.json` has a `dl` template pointing at upstream https —
artifact downloads follow those links and BYPASS the shims (CONNECT = opaque,
uncached). Fix: rewrite those bodies on SERVE for the shim routes only.

## Design (follow exactly — byte-faithful cache stays intact)

Store upstream bytes UNCHANGED (sidecar checksums stay origin-true; verify-on-serve
keeps comparing origin bytes). Rewrite is a serve-time transform, applied ONLY to
`lang_metadata` responses served via the shim routes (buffered — these bodies are
KB-scale; after rewrite, recompute Content-Length / serve chunked).

Pinned API (parent wiring compiles against this):

```rust
/// pip simple-index page: rewrite absolute files.pythonhosted.org artifact links
/// to the proxy's /pypi/packages shim route. proxy_base like "http://10.0.0.5:3128".
/// Links are HTML anchors: href="https://files.pythonhosted.org/packages/..."
/// Preserve the #sha256= fragment (pip verifies it client-side).
pub fn rewrite_pypi_simple(body: &str, proxy_base: &str) -> String;

/// cargo sparse-index config.json: {"dl":"https://crates.io/api/v1/crates","api":"https://crates.io"}
/// → dl = "<proxy_base>/crates"  (cargo appends /<name>/<ver>/download;
/// the /crates shim re-adds the static.crates.io path segment — verified 200 shape).
/// api key: leave unchanged (publish path, never proxied).
pub fn rewrite_cargo_config(body: &str, proxy_base: &str) -> String;

/// Dispatch: Some(rewritten) when the (shim_route, content_type) pair warrants a
/// rewrite, None otherwise. Routes: "/pypi/simple" → pypi_simple,
/// "/cargo-index" + config.json body → cargo_config. Everything else None.
pub fn rewrite_if_needed(shim_route: &str, body: &str, proxy_base: &str) -> Option<String>;
```

Rewrite mechanics: plain string replacement on the host prefix
(`https://files.pythonhosted.org/` → `<proxy_base>/pypi/packages/`), NOT HTML
parsing (no new deps). Be conservative: only rewrite when the match is exact-host;
already-relative or proxied links pass through untouched (idempotent).

## Tests (minimum — fixtures realistic, not invented-brief)

- pip: fixture with 3 anchor forms — absolute files.pythonhosted.org href with
  `#sha256=` fragment (rewritten, fragment PRESERVED), already-proxied href
  (untouched), relative href (untouched). Hash segment NOT fixed-length (real
  shapes: `/packages/ab/cd/<~59-60 hex>/name-1.0-py3-none-any.whl`).
- cargo config.json: dl rewritten to `<proxy_base>/crates`, api untouched, other
  keys preserved; non-config JSON → None.
- Idempotency: rewrite(rewrite(x)) == rewrite(x).
- rewrite_if_needed routing table incl. negative cases (distro route → None).
- No validation needed on proxy_base beyond being a prefix string.

## Verify (once, from repo root)

`cargo test -p monopsony-proxy --lib body_rewrite` — all pass, no new warnings
naming body_rewrite.rs.
