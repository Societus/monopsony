# Sprint 25 — Personal OCI Registry (push path)

**Date:** 2026-10-16 · **Status:** PREP — staged for kickoff (grounding complete, slices sized)
**Origin:** backlog item (user decisions 2026-09-26) + `docs/plans/2026-10-01-kickoff-backlog-and-macos.md` Part A.
**Motivation:** hometest swarm has no registry — paperclip POC images ship as hand-carried tars
(scp'd node-to-node). First-party push/pull storage so locally built images stop doing that.

## Problem / proven failure

Today `docker push` has no target: the swarm nodes exchange images via scp. The existing
`docker.rs` pull-through cache covers docker.io pulls but (a) is **not mounted anywhere**
(construction-site check 2026-10-16: `docker_routes()`/`DockerState` have zero call sites
outside `crates/monopsony-proxy/src/docker.rs`) and (b) is GET-only — no write path exists.

## Decisions log (settled — do not re-litigate)

| Decision | Choice | Source |
|---|---|---|
| Positioning | NOT a pull-through mirror. An image exists only because it was pushed. No upstream resolution. | user, 2026-09-26 |
| Untagged pushes | Auto-versioned by build date so every push is addressable/rollback-able without tag discipline. | user, 2026-09-26 |
| GC policy | Blobs refcounted across manifests; auto-versioned tags **never** auto-deleted. | user, 2026-09-26 |
| Sequencing | Independent of Sprints 23/24; macOS sprint led, OCI fills the next lane. | kickoff plan, 2026-10-01 |
| Deferral-gate coupling | Docker/OCI deferral track lands AFTER this registry so both share tag/version plumbing. | deferral spec §11.4 |

## Verified wire facts — OCI Distribution Spec (fetched 2026-10-16, `spec.md@main`, 940 lines)

Full endpoint table (end-1…end-14) + error codes saved at
`~/.hermes/cache/scratch/oci-dist-spec.md`. The facts below are normative language from the spec:

- **Version check:** `GET /v2/` → `200` (end-1). Nonexistent repo pull → `404`.
- **Blob pull:** `GET|HEAD /v2/<name>/blobs/<digest>` → `200`/`404` (end-2). HEAD is the
  existence probe docker/containers-image uses before upload — implement it.
- **Manifest pull:** `GET|HEAD /v2/<name>/manifests/<tag-or-digest>` → `200`/`404` (end-3).
- **Upload session:** `POST /v2/<name>/blobs/uploads/` → **`202 Accepted` + `Location:
  <blob-push-location>`** (end-4a). Location may be relative; must be echoed back byte-exact
  by the client. Registries MUST support concurrent uploads → unique session id per POST.
- **Monolithic single-POST:** `POST /v2/<name>/blobs/uploads/?digest=<digest>` with the full
  body → `201`/`202` (end-4b). Cheap to support (same handler as finalize).
- **Chunked:** `PATCH <blob-push-location>` with `Content-Range` → `202 + Location +
  Range: 0-<end>` (end-5). Chunks MUST be in order; out-of-order → `416`. `GET` on the upload
  location returns current offset → `204` (end-13). `DELETE` on it cancels → `204` (end-14).
  Final chunk may ride the PATCH or the closing PUT.
- **Finalize:** `PUT <blob-push-location>?digest=<digest>` → **`201 Created` + `Location`
  (pullable blob URL)** (end-6). Digest mismatch/unsupported algorithm → `400` (DIGEST_INVALID).
- **Cross-repo mount:** `POST /v2/<name>/blobs/uploads/?mount=<digest>&from=<other_name>` →
  `201` on mount, **`202` fall-through to a normal upload session** when it can't (end-11).
  `from` MAY be treated as optional (mount by digest alone is legal).
- **Manifest push:** `PUT /v2/<name>/manifests/<tag-or-digest>` → `201` + `Location` +
  `Docker-Content-Digest` (end-7a). Manifest stored **byte-exact**. `Content-Type` must equal
  the manifest's `mediaType` when present. Extra tags via `?tag=a&tag=b` (end-7b) → `OCI-Tag`
  response header; ≥10 tags per request SHOULD be supported. Registry SHOULD enforce a max
  manifest size (oversize → `413`).
- **Tags list:** `GET /v2/<name>/tags/list` (+ `n=`/`last=` pagination) → `200` (end-8a/b).
- **DELETE** manifest/blob → `202` (`405` acceptable if unsupported) (end-9/10).
- **Referrers** (end-12a/b) optional — spec has an explicit "Unavailable Referrers API"
  compat section; clients tolerate absence. V1: omit.
- **Errors:** 4XX bodies `{"errors":[{"code","message","detail"}]}`; codes used here:
  `BLOB_UNKNOWN`, `BLOB_UPLOAD_INVALID`, `BLOB_UPLOAD_UNKNOWN`, `DIGEST_INVALID`,
  `MANIFEST_BLOB_UNKNOWN`, `MANIFEST_INVALID`, `NAME_UNKNOWN`, `TAG_INVALID`.

## Verified tree facts (2026-10-16)

| Fact | Where |
|---|---|
| `CacheStorage` trait: `spool(key)→SpoolHandle{write,commit→StorageMeta,abort}`, `write`, `read`, `read_stream(range)`, `exists`, `delete`, `get_checksum`, `set_retention`, `list_keys`, meta-JSON sidecars | `crates/monopsony-proxy/src/cache_storage.rs:144` |
| Spool commit is atomic + abort discards (test-proven) — blob upload sessions map onto it 1:1 | `cache_storage.rs` tests |
| Docker pull-through (GET-only, `resolve_registry` host sniffing, `docker/v2/...` keys, auth passthrough) exists but is **unmounted** | `docker.rs` (no call sites) |
| Shim mounting pattern: `axum::routing::any(handler)` fallbacks on the proxy router | `proxy.rs:249-277` |
| ghcr OCI precedent: manifest + token dance + blob-307 bespoke reqwest handler + sha256-before-spool-commit | `brew_shim.rs` (Sprint 26 Ph1) |
| Janitor maintenance-loop pattern for periodic sweeps | `proxy.rs::run_language_maintenance` |
| Policy/JSONB storage + admin route pattern | `db.rs:1812/1828` (`get_setting`/`set_setting`), `monopsony-admin/src/language_pms.rs` |

## Trust / architecture model

- Registry serves under the **mirror router :3129** (default; see open questions) — mirror
  auth semantics already live there: pulls follow the mirror policy (private = keyed, public =
  rate-limited); **push routes are ALWAYS authenticated** (API key / admin session) regardless
  of mirror policy. A registry that accepts anonymous pushes is a malware dropbox.
- Blobs are content-addressed → store via `spool()` streaming (never buffer a whole layer);
  digest verified at commit (`get_checksum`), `400 DIGEST_INVALID` on mismatch.
- Manifests are small JSON → `write()` + tag-index sidecar (meta JSON), byte-exact.
- Keys: `registry/{name}/blobs/{sha256…}`, `registry/{name}/manifests/{digest}`,
  `registry/{name}/tags.json`. Keep them disjoint from cache/`docker/v2/` namespaces so
  retention/scrub can treat the registry as its own class.

## Phased slices (each ships + fleet-verifies independently)

| # | Slice | Detail | Est. |
|---|---|---|---|
| S1 | Registry router + pull-back | `oci_registry.rs` in monopsony-proxy: end-1/2/3/8a serving pushed content from CacheStorage + tag sidecar. **Mounted in this slice** (construction site, not library-complete). Empty-registry 404s are correct. | 1–1.5 d |
| S2 | Blob upload path | Session store (in-proc `Arc<Mutex<HashMap<uuid, SpoolHandle>>>` + TTL sweep in janitor): end-4a/4b/5/6/13/14. In-order enforcement, Range echo, 416, digest-verified commit. Lost session (restart) = `404 BLOB_UPLOAD_UNKNOWN` — spec-legal, no persistence needed. | 2 d |
| S3 | Manifests + tags + auto-version | end-7a/7b: byte-exact store, referenced-blob validation (reject → `MANIFEST_BLOB_UNKNOWN`; config knob), tag index, `Docker-Content-Digest`, auto-version on tagless push: **default `YYYYMMDD-HHMMSS`** (final scheme = user call), OCI-Tag header. | 1.5 d |
| S4 | Refcount GC + retention interplay | Blob refcount across manifests (sidecar or `registry_blobs` table), manifest DELETE decrements (physical reclaim deferred/janitor), auto-versioned tags exempt; registry namespace excluded from cache eviction, own `[registry]` budget knob. | 1–1.5 d |
| S5 | Mount + docker pull-through companion | end-11 cross-repo mount (201/202 fall-through). PLUS the pre-existing `docker.rs` pull-through: **mount it or cut it, explicitly** — it has sat unmounted since it landed; if mounted, fold the `fetch_upstream` body-collection fix (backlog) into the same slice. | 0.5–1 d |
| S6 | Fleet verification | hometest swarm: `docker push`/`pull` e2e through the registry; tagless push addressable + rollback-able; refcount survives manifest re-push; checksums verify; one second client pulls the same image (LAN serve). | 0.5 d |

**Exit criteria** (from kickoff plan Part A): docker push/pull through the registry on the
hometest swarm; tagless push addressable + rollback-able; refcount survives manifest re-push.

## What this does NOT defend

- No upstream resolution or replication — single-server storage, backup = server's own.
- No HA / multi-node registry.
- No per-repo ACLs — authz is the server's existing auth modes only.
- No signature verification (cosign/sigstore) — content trust stays client-side.
- No quota enforcement beyond the retention budget knob.

## Open questions (assumed defaults; user flips cheaply at kickoff)

1. **Auto-version scheme** — `YYYYMMDD-HHMMSS` (default) vs calendar `YYYY.MM.DD-<shorthash>`.
2. **Listen point** — mirror router :3129 (default) vs dedicated port. Swarm nodes need
   `insecure-registries` or the server cert if TLS — plain-HTTP on the tailnet/LAN is the
   expected v1 posture.
3. **Push auth default** — always-authenticated (recommended default); tie to existing API keys.
4. **docker.rs pull-through** — mount in S5 (default) or declare cut.
5. **Referenced-blob validation** — reject missing blobs (default, spec-idiomatic) vs allow
   lazy pushes (some CI flows push manifests first; docker CLI doesn't, buildkit might).
