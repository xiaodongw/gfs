# WebDAV Read Surface

## Summary

FUSE support on macOS is limited, so macOS users cannot use the gfs mount. WebDAV is
natively supported by macOS Finder (Go → Connect to Server) and by plain HTTP
clients. A read-only WebDAV surface on the gfs-server HTTP listener lets a user
browse and read any repository snapshot at
`http://server/dav/{repo}/{branch}/{dir}/{file}` with no client software at all.

Scope decided with the user: read-only (OPTIONS, PROPFIND, GET, HEAD; every write
method answers 405 so Finder mounts read-only), under the `/dav/` prefix — a
root-level `/{repo}` would collide with `/healthz`, `/metrics`, and `/v1`, and ADR
0006 path-versions the HTTP surface. Per DESIGN.md §6.3 this is a gateway onto the
same internal services: `AsyncRepository` + `Authorizer`, no parallel read path.

## Plan

- Phase 1 — skeleton and wiring: `gateway::credential` and `http::parse_range`
  become `pub(crate)`; new `service/webdav.rs` with the router (`/dav`, `/dav/`,
  `/dav/{*rest}`, all via `any` because axum's `MethodFilter` cannot express
  PROPFIND and `Path` would round-trip byte paths through `String`), raw-path
  segment parsing to `Vec<u8>`, method dispatch, unauthenticated OPTIONS (`DAV: 1`,
  class 1 only), 405 for write methods, Basic/Bearer auth with the
  `WWW-Authenticate: Basic realm="GFS"` challenge; merged into
  `Server::http_router()` with its own body-limit and timeout layers.
- Phase 2 — PROPFIND: node resolution (root lists authorized repositories; a repo
  lists `refs/heads/*` hierarchically, slash branches as nested collections;
  longest-prefix branch match, remainder is the tree path), multistatus builder,
  XML escaping, percent codec, hand-rolled RFC 1123 dates from
  `ResolvedRevision.snapshot_time`, Depth 0/1 (infinity or missing → 403 with
  `<D:propfind-finite-depth/>`), directory pagination to completion, audit and
  metrics.
- Phase 3 — GET/HEAD: resolve → entry → `read_blob`; ETag = blob OID,
  If-None-Match → 304, single-range Range support, HEAD from `entry.size` with no
  blob read, symlinks served as files holding the target bytes, gitlinks omitted,
  `Cache-Control: no-store` everywhere under `/dav`.
- Phase 4 — smoke tests in `crates/gfs-service/tests/webdav.rs` following the
  `tests/api.rs` harness.
- Phase 5 — docs: manual-test.md section, ADR 0010, DESIGN.md §6.3 cross-reference.

Build and `cargo test -p gfs-service` after each phase.

## Decisions

_Important decisions made during this feature_

- Hierarchical branch namespace instead of encoding slashes: Git forbids one ref
  being a prefix of another, so matching URL segments against `refs/heads/*` by
  longest prefix is unambiguous, and Finder browses branch folders naturally.
- Hand-rolled multistatus XML and RFC 1123 dates: no XML or time crate exists in
  the workspace and read-only DAV needs only string building — the same
  trust-boundary reasoning as the gateway's hand-rolled base64.
- Missing Depth header is treated as infinity and refused (403), per RFC 4918 §9.1;
  Finder and cadaver always send 0 or 1.
- `getlastmodified` uses the ADR 0006 sanitized snapshot time, so Finder and a FUSE
  mount of the same commit report the same timestamps.
- Commit-hex URLs deferred: lexically ambiguous with branch names and no WebDAV
  client emits them.

## Details

_Any detail that user should be aware_

- Implemented as planned: `crates/gfs-service/src/service/webdav.rs` (the whole
  surface), merged into `Server::http_router()` with its own layers;
  `gateway::credential` and `http::parse_range`/`set`/`request_id` became
  `pub(crate)`. Eleven wire-level smoke tests in
  `crates/gfs-service/tests/webdav.rs`; docs in ADR 0010, manual-test.md, and
  DESIGN.md section 6.3.
- HEAD never reads the blob: the tree entry already carries the size.
- Windows was validated end-to-end on 2026-07-30: curl from `cmd.exe` and a
  `net use Z:` drive mapping against the WSL2-hosted dev server both work
  (details and the two Windows traps are in docs/manual-test.md). Finder still
  needs a one-time validation on a real Mac; the protocol basics are pinned by
  the automated tests.

- `visible_refs()` is enumerated per repo-scoped request; a ref-list cache keyed on
  catalog ref state is the noted fix if 10k-ref repositories become a target.
- Depth-1 multistatus bodies are built in memory, bounded by Git tree width.
- GET buffers the whole blob (existing 512 MiB pattern shared with `/file`);
  streaming is a separate improvement.
