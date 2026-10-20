# Sprint 25 — OCI Registry: Slice 1 (pull-back) — implementation plan

**Date:** 2026-10-19 (kickoff) · **Status:** approved — implementation in progress
**Parent:** `docs/plans/2026-10-16-sprint25-oci-registry-push.md` (wire facts, slice table, exit criteria)
**Corrections record:** this file replaces the 2026-10-19 session-local draft
(`~/.hermes/plans/`, deleted). Three corrections applied at kickoff, see
"Corrections applied" below.

## Corrections applied at kickoff (20/20-hindsight review)

1. **Retracted wire claim.** The draft claimed docker sends blob digests as
   `sha256:%20hash`. False — the OCI digest grammar is `algorithm ":" hex`, and hex
   cannot contain a space, so `%20` can never appear in a well-formed digest. The
   per-segment percent-DECODE step is retained on its real justification: handlers
   read `request.uri().path()` raw (undecoded), and decode-after-split is the
   correct RFC 3986 order; raw-path keying is a fleet-bitten bug class in this repo
   (origin-form passthrough, dl-template trailing slash).
2. **Mount point resolved.** The draft contradicted the parent plan (draft said
   `create_proxy_router` :3128; parent default was mirror router :3129). Resolved by
   user decision 2026-10-19: **dedicated registry listener** behind a new
   `--registry-bind` CLI arg (default `0.0.0.0:3131`). Rationale, verified in-tree:
   `docker.rs` pull-through registers `/v2/`, `/v2/:name/manifests/:reference`,
   `/v2/:name/blobs/:digest` — the same `/v2` namespace the registry needs, and
   docker clients cannot be prefix-routed. Same-listener mounting would hard-close
   the S5 mount-or-cut option; a dedicated port keeps both alive. Threat-landscape
   note: the extra socket is negligible (same binary, tailnet-bindable); the risk
   shape it avoids is always-auth push routes threaded through the mirror router's
   public/private policy exemptions.
3. **Plan location.** Slice plans live in-repo (`docs/plans/` is tracked), not in
   session-local `.hermes/plans/`.

## Decisions locked at kickoff (user, 2026-10-19)

| Decision | Choice |
|---|---|
| Listen point | Dedicated port. `--registry-bind`, default `0.0.0.0:3131` (keeps the 312x family). Follows the `--proxy-bind`/`--mirror-addr` arg pattern. |
| Auth (v1) | ALL registry routes keyed (pulls and pushes), via the existing `auth_middleware` + `AuthState { auth_mode, db }` reuse — same as the proxy router. No rate limiter v1 (noted for hardening). |
| Auto-version tag scheme | `YYYYMMDD-HHMMSS`, no shorthash suffix. **Scope: OCI registry tagless pushes ONLY** — non-OCI naming (cache keys, PM version identities, deferral-gate version identity) is untouched. Rollback identity = content digest (`name@sha256:…`), which OCI provides by construction; the tag is a convenience pointer. Same-second double-push is backstopped by pull-by-digest. A signature-key surface for hash-less PMs belongs to the deferral-gate track (spec §11.4), not here. |

**Deferred to S2 (design item, do not lose):** docker-client credential mapping.
`docker login` sends Basic auth / the auth middleware expects `X-Monopsony-Key`;
the registry needs an explicit story (map Basic creds → monopsony API key, or a
minimal token flow) before S6's `docker push/pull` e2e through a keyed registry.
S1 unit tests exercise handlers with open-mode AuthState + one keyed-reject test;
the positive keyed path is the proxy router's already-tested territory.

## What S1 builds (read-only pull-back)

Serving pushed content over OCI distribution-spec end-points 1/2/3/8a:

- **end-1** `GET /v2` or `/v2/` → `200 {}` + `Docker-Distribution-API-Version: registry/2.0` (repo-independent — empty registry still answers)
- **end-2** `GET|HEAD /v2/<repo>/blobs/<digest>` → `200`/`206`/`404`/`416`, opaque bytes
- **end-3** `GET|HEAD /v2/<repo>/manifests/<reference>` → `200`/`404` (reference = tag via tag index, or digest direct)
- **end-8a** `GET /v2/<repo>/tags/list` → `200` from `registry/<repo>/tags.json`; unknown repo → `404 NAME_UNKNOWN`

Blobs served byte-exact via `CacheStorage::read_stream(key, range)` — never
gunzipped/re-parsed server-side (two-site-class lesson). HEAD sizes come from the
same call's total (drop the stream). `Docker-Content-Digest` echoed on blob and
manifest responses.

## Module shape (`crates/monopsony-proxy/src/oci_registry.rs`)

- `run_registry(addr, storage, db, auth_mode)` — self-contained construction site
  (own `ProxyState::new`, own AuthState, plain `axum::serve` like the mirror
  listener; no CONNECT bridge needed — registry traffic is plain GET/HEAD/PUT).
- `create_registry_router(state, auth_state)` — routes `/v2`, `/v2/`,
  `/v2/*path` (single wildcard; parse segments manually — repo names contain
  slashes, and axum params can't do wildcard+trailing-param), auth middleware
  layered with its own state, then `.with_state(proxy_state)`.
- Path parsing: strip `/v2`, split remainder on `/` RAW, decode each segment
  AFTER the split (`percent-encoding` crate, already in the lock graph via `url`),
  classify by last two segments: `blobs/<digest>`, `manifests/<ref>`, `tags/list`.
- **Input validation before any key construction (path-traversal guard):**
  repo segments must match the OCI repo grammar `^[a-z0-9]+([._-][a-z0-9]+)*$`;
  digests `^[a-z0-9]+:[a-f0-9]{32,128}$` → else `400 DIGEST_INVALID`; tag
  references `^[A-Za-z0-9_][A-Za-z0-9._-]{0,127}$` → else `400 TAG_INVALID`.
  `/` is split away and `..`/`:` cannot match the grammars, so storage keys
  `registry/<repo>/blobs/<digest>`, `registry/<repo>/manifests/<ref>` are
  traversal-proof by construction.
- Error bodies: `{"errors":[{"code","message"}]}` — `BLOB_UNKNOWN`,
  `MANIFEST_UNKNOWN`, `NAME_UNKNOWN`, `DIGEST_INVALID`, `TAG_INVALID`,
  `UNSUPPORTED`.
- Range handling: single range only (`bytes=a-b`, `a-`, `-n`); malformed or
  multi-range → ignore, serve 200 full (RFC-permissible); `start >= total` →
  `416` + `Content-Range: bytes */total`.

## Test plan (in-crate, seeded through CacheStorage)

`ObjectStoreBackend::local(tempdir)` + `ProxyState::new(storage, None)` + rustls
provider install (every ProxyState-building test); seed blobs/manifests/tags.json
via `storage.write()`; drive the router with `tower::ServiceExt::oneshot`.

1. version check `200` (repo-independent)
2. blob HEAD + GET `200`, `Docker-Content-Digest` + `Content-Length` correct
3. missing blob → `404 BLOB_UNKNOWN`
4. range → `206` + clamped `Content-Range`; `start >= size` → `416`
5. manifest by digest + by tag (via tags.json sidecar) `200` byte-exact
6. unknown reference → `404 MANIFEST_UNKNOWN`; unknown repo tags/list → `404 NAME_UNKNOWN`
7. malformed digest (`../../etc`, bad charset) → `400 DIGEST_INVALID`; uppercase repo → `400`
8. percent-encoded segment (`%3A` in a digest position) decodes and resolves
9. auth: keyed mode without key → `401` (middleware reused, one reject test)

## What S1 does NOT do

No push write path (S2), no manifest referenced-blob validation (S3), no tagless
auto-versioning (S3), no refcount GC (S4), no cross-repo mount, NOT the `docker.rs`
pull-through decision (S5 mount-or-cut, now unblocked by the dedicated port), no
rate limiting, no pagination on tags/list (SHOULD-level; fine for personal-scale
repos), no TLS (plain HTTP tailnet/LAN posture; `insecure-registries` on swarm nodes).
