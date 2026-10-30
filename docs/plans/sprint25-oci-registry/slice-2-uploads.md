# Sprint 25 — OCI Registry: Slice 2 (blob upload path) — implementation plan

**Date:** 2026-10-29 · **Status:** approved-by-queue (handoff 2026-10-29 item 2) — implementation in progress
**Parent:** `docs/plans/2026-10-16-sprint25-oci-registry-push.md` (wire facts, slice table, exit criteria)
**Prior slice:** `slice-1-pullback.md` (S1 landed: end-1/2/3/8a pull-back on :3131)

## Corrections applied at slice start (tree re-verification)

1. **The "fetch_upstream body fix" prerequisite was already done.** The backlog line
   "docker.rs fetch_upstream still collects bodies" is stale: Sprint 19 Wave B landed
   SpoolHandle streaming in `docker.rs::cache_and_respond` (O(chunk) memory, atomic
   commit, abort-on-fetch-error). `fetch_upstream` returns the response unbuffered.
   Roadmap backlog line corrected 2026-10-29. Consequently the handoff's S2
   prerequisite reduces to the docker.rs mount-or-cut *decision*, which stays at S5
   per parent-plan open-question #4 (the 2026-10-19 dedicated-port decision explicitly
   keeps both surfaces alive). S2 is unaffected either way: disjoint listener, disjoint
   `registry/` key namespace.

## Wire facts — OCI distribution spec (fetched 2026-10-29, `spec.md@main`, 940 lines;
full text cached at `~/.hermes/cache/scratch/oci-dist-spec-2026-10-29.md`)

- end-4a `POST /v2/<name>/blobs/uploads/` (`Content-Length: 0`) → `202 Accepted` +
  `Location: <blob-push-location>`; unique session per POST (spec: concurrent uploads
  MUST be supported).
- end-4b monolithic `POST /v2/<name>/blobs/uploads/?digest=<digest>` with the full body
  → `201 Created` + `Location: <blob-location>` (pullable URL). Same handler as finalize.
- end-5 `PATCH <blob-push-location>` with `Content-Range: <range>` where range matches
  `^[0-9]+-[0-9]+$`, inclusive both ends, first chunk MUST begin at 0 → `202 Accepted` +
  `Location` + `Range: 0-<end-of-range>` (end = position of last uploaded byte, matching
  the request's Content-Range end). Chunks MUST be in order (first byte = previous end +
  1); out-of-order → `416 Requested Range Not Satisfiable`.
- Final chunk MAY ride the last PATCH or the closing PUT; the session MUST be closed with
  end-6 `PUT <blob-push-location>?digest=<digest>` (digest of the WHOLE blob) →
  `201 Created` + `Location`. Out-of-order final chunk → 416.
- end-13 `GET <blob-push-location>` → `204 No Content` + `Location` + `Range: 0-<end>`.
- end-14 `DELETE <blob-push-location>` → `204 No Content`; server SHOULD eventually
  timeout unfinished uploads (our TTL sweep is that SHOULD).
- Digest mismatch/unsupported algorithm → `400 DIGEST_INVALID` (parent plan, from the
  2026-10-16 spec fetch; unchanged in the 2026-10-29 text).

## Decisions locked for S2

| Decision | Choice | Rationale |
|---|---|---|
| Session store | In-proc `HashMap<uuid, UploadSession>` behind `parking_lot::Mutex` on a registry-owned `RegistryState` (not `ProxyState`). Session = `{repo, Option<SpoolHandle>, offset, last_activity}` — the handle is TAKEN out of the map while a PATCH streams (no await under the lock); `offset` is an explicit field because the handle is absent during that window. | Parent plan: lost session on restart = `404 BLOB_UPLOAD_UNKNOWN`, spec-legal, no persistence. `commit()`/`abort()` consume the handle and run AFTER removal from the map (no await under lock — sibling-deadlock class; also required because a held std/parking-lot guard makes the future `!Send`). |
| Unknown-digest-at-open | Session spool opens on staging key `registry/<repo>/.uploads/<sid>`; finalize commits at the staging key and **stream-copies** to `registry/<repo>/blobs/<digest>` in O(chunk) memory, then deletes the staging object. | Original plan (rekey-before-commit) is IMPOSSIBLE: the object-store writer is bound to its location at open — a metadata re-key cannot move the data (caught by the end-to-end tests, 2026-10-29; finalize returned 201 while the blob sat at the staging key). The copy costs one extra write at finalize; a `rename`/`copy` on CacheStorage would drop it — deferred as an S4 optimization. Zero-byte blobs additionally take a buffered-write fast path (object_store rejects zero-part multipart finish). |
| Empty-object reads | `read_stream` on a 0-byte object returned an error (`range.start >= total` is trivially true), making any empty blob unreadable through every consumer — fixed in `object_store_backend.rs` with an empty-stream fast path (found by the empty-blob push test). | Latent backend quirk surfaced by S2; benefits the whole proxy chain. |
| Digest check | At finalize: `StorageMeta.checksum_sha256` (bare 64-hex, from the spool hasher) compared against `?digest=` (must be `sha256:<64hex>`; other algorithms → `400 DIGEST_INVALID` — we compute sha256 only). Mismatch → `storage.delete(final_key)` + `400 DIGEST_INVALID` (nothing published). | Spec + plan. get_checksum read-back not needed — commit returns the hasher result. |
| TTL sweep | `UploadSessions::sweep(ttl)` collects expired sessions and aborts their spools outside the lock; `run_registry` spawns an interval task (10 min sweep, 24 h TTL). | The registry listener has no janitor loop of its own; a self-contained sweeper matches the listener's self-contained construction-site shape. Spec end-14: "server SHOULD eventually timeout unfinished uploads". |
| `mount`/`from` params (end-11) | Fall through to a normal upload session (202) with a comment — the spec's sanctioned can't-mount behavior. Real mounting stays S5. | Parent slice table: S5 owns cross-repo mount. |
| Docker-client auth (S1's deferred design item) | `Authorization: Basic base64(user:pass)` → rewrite to `X-Monopsony-Key: <pass>` (username ignored) when no `X-Monopsony-Key` header is present, in a registry-layer middleware BEFORE the shared `auth_middleware`; validation stays 100% in the existing middleware (sha256 → DB key lookup). | `docker login <host> -u <any> -p <monopsony-key>` is the standard token-as-password pattern; zero new trust code. Grounded in docker CLI login behavior (credentials stored as base64 auth, sent as Basic) — NOT a spec token flow. |
| No Bearer/token-flow auth v1 | A 401 carries `WWW-Authenticate: X-Monopsony-Key` (existing middleware response). Docker does not auto-retry Basic without a prior `docker login`; operator flow = `docker login host:3131 -u monopsony -p <key>`. Documented limitation, noted for hardening. | Minimal auth surface per the kickoff decision "all routes keyed"; token flow is a separate mechanism spike (UNVERIFIED until doc-grounded). |

## Module shape (extends `crates/monopsony-proxy/src/oci_registry.rs`)

- `RegistryState { proxy: Arc<ProxyState>, uploads: Arc<UploadSessions> }` as the router
  state; `FromRef<RegistryState> for Arc<ProxyState>` keeps every S1 handler signature
  unchanged.
- `handle_v2` parser: after popping one trailing empty segment, check the upload tails
  BEFORE the generic repo/kind split — `[..., "blobs", "uploads"]` → start,
  `[..., "blobs", "uploads", <sid>]` → session ops; repo segments decoded/validated by
  the same grammar helper as S1 (extracted from the inline loop).
- Bodies are streamed frame-by-frame into `spool.write()` (`futures::StreamExt`) — never
  buffered whole (monolithic POST and PUT-with-final-chunk included).
- `record_spool_meta` written after successful commit (scrub-sidecar convention,
  mirroring docker.rs).
- Location forms: push location `/v2/<repo>/blobs/uploads/<uuid-simple>`; pullable blob
  location `/v2/<repo>/blobs/<digest>` (relative, byte-exact echo per spec).

## Test plan (in-crate, `tower::ServiceExt::oneshot`, seeded like S1)

1. POST (no digest) → 202 + `Location` matching `/v2/<repo>/blobs/uploads/<32-hex>`
2. PATCH in-order → 202 + `Location` + `Range: 0-<end>`; GET status → 204 + `Range` echo
3. PATCH out-of-order → 416; malformed Content-Range → 400
4. Session bound to repo: PATCH under a different repo path → 400
5. PUT finalize → 201 + `Location` = blob URL + `Docker-Content-Digest`; GET blob (S1
   handler) returns the pushed bytes end-to-end
6. PUT with final chunk (after PATCHes) → 201, bytes complete
7. PUT digest mismatch → 400 DIGEST_INVALID and blob ABSENT (delete-on-mismatch)
8. PUT non-sha256 digest → 400; PUT without digest → 400
9. Monolithic POST?digest → 201 + blob served
10. DELETE session → 204; subsequent PUT → 404 BLOB_UPLOAD_UNKNOWN
11. Unknown session id → 404; new-router "restart" with an old id → 404
12. TTL sweep aborts an aged session → next PATCH 404
13. Basic-auth mapping unit tests: Basic header + no X-Monopsony-Key → key header set;
    existing X-Monopsony-Key preserved; non-Basic Authorization untouched
14. Token mode without DB + Basic credentials → 401 (fail-closed path unchanged);
    positive keyed coverage remains the proxy router's existing tests

## What S2 does NOT do

No cross-repo mount (S5), no manifest push / referenced-blob validation / tagless
auto-version (S3), no refcount GC or `[registry]` budget (S4), no tags pagination, no
TLS (plain-HTTP tailnet posture per S1), no Bearer token flow, no docker.rs pull-through
decision (S5).
