# Slice 4 — Phase D: dashboard language-PM toggles (SvelteKit)

**Pack first:** `docs/plans/sprint23-remaining/context-pack-common.md`
**Safe-list:** `dashboard/src/routes/settings/+page.svelte` (extend — do NOT rewrite)
plus ONE new component `dashboard/src/lib/components/LanguagePmPanel.svelte` (if the
components dir convention differs, check `dashboard/src/lib/components/` and follow it).

## Deliverable

A "Language package managers" panel on the existing settings page: three switches
(npm / pip / cargo), each showing its `source` badge (`default` | `auto` | `manual`),
backed by the API pinned in slice 3's spec (`GET/PATCH /api/v1/admin/language-pms` —
contract is settled; code against it even though slice 3 runs in parallel).

Follow the existing toggle precedent: read `dashboard/src/routes/monetization/+page.svelte`
first and mirror its fetch/api-client usage and styling (use the shared api client in
`dashboard/src/lib/api/` if that's what monetization uses — check, don't assume).

## Rules

- Svelte 5 runes if the codebase uses runes (`$state`, `$derived`) — match
  monetization/+page.svelte's style exactly; SvelteKit 2.
- a11y: switches need proper label association (`for`/`id` or implicit), keyboard
  operable. svelte-check a11y warnings are fixed proactively, not left.
- Optimistic UI optional; on PATCH failure, revert the switch and surface the error
  message. Do not swallow fetch errors silently.
- **NO `npm install`. NO building the dashboard.** Verify via `npx svelte-check` only.

## Verify (once, from dashboard/)

`npx svelte-check 2>&1 | tail -5` — zero NEW errors/warnings vs baseline. Baseline:
the hub has NOT measured svelte-check on this tree; if there are pre-existing errors,
they must not name your files, and your report states the pre-existing count.
