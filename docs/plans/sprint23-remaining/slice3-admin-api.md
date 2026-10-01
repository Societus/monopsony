# Slice 3 — Phase D: admin API for language-PM enablement

**Pack first:** `docs/plans/sprint23-remaining/context-pack-common.md`
**Safe-list:** `crates/monopsony-admin/src/language_pms.rs` (NEW file — placeholder +
`pub mod language_pms;` already landed in lib.rs by the parent; replace the
placeholder comment, never edit lib.rs — route registration in lib.rs is parent glue
and happens after your lane lands).

## Deliverable

Axum handlers for the runtime enablement surface, following the EXACT pattern of
`crates/monopsony-admin/src/features.rs` (read it first — it is your template:
`State(AdminState)` extractors, `Json` bodies, same error/Ok shapes).

## API contract (pinned — dashboard lane codes against this verbatim)

```
GET  /api/v1/admin/language-pms
  → 200 {"npm":   {"enabled": bool, "source": "default"|"auto"|"manual"},
         "pip":   {...}, "cargo": {...}}
PATCH /api/v1/admin/language-pms        body: {"pm": "npm"|"pip"|"cargo", "enabled": bool}
  → 200 updated full map (same shape as GET) · 400 unknown pm
```

Behavior:
- GET: read via `Database::get_language_pms()` (db.rs ~1843). Missing/unset setting →
  all three `{"enabled": false, "source": "default"}`.
- PATCH: read-modify-write via `get_language_pms` → `set_language_pms` (~1871);
  set `source: "manual"`; then `record_pm_enablement_audit(...)` with the transition.
- Settings are runtime (server_settings table) — no restart, per the established
  server_settings pattern.

Write handler unit tests in the new file following features.rs's test pattern (if
features.rs has none, test the pure parts: request parsing, unknown-pm rejection,
default-map construction — do NOT spin up a live DB; note DB-touching paths as
covered-by-fleet-verification in your report).

## Verify (once, from repo root)

`cargo test -p monopsony-admin --lib language_pms 2>/dev/null || cargo test -p
monopsony-admin language_pms` — all pass; if the admin crate has no lib target
separate from its bin, the second form is authoritative. No new warnings naming
language_pms.rs.
