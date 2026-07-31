# ADR 0010: A read-only WebDAV surface

- Status: Accepted
- Date: 2026-07-30
- Extends: DESIGN.md section 6.3's rule that new surfaces are gateways onto the
  same internal services; ADR 0006's HTTP policies

## Context

FUSE on macOS is a poor foundation: macFUSE is a kernel extension users must
approve, and DESIGN.md section 3 excludes macOS filesystems from the first
production version outright. A Mac still wants to browse a snapshot. Finder
speaks WebDAV natively (Go → Connect to Server), as does almost everything else
with an HTTP stack, so a WebDAV surface gives read access with no client
software at all.

## Decision

Serve **read-only WebDAV class 1** on the existing HTTP listener under
`/dav/{repository-id}/{branch}/{path}`:

- **Methods**: OPTIONS, PROPFIND (Depth 0 and 1), GET, HEAD. Every write method
  answers 405. OPTIONS advertises `DAV: 1` and no LOCK support, which is what
  makes Finder mount the volume read-only; it needs no credential because it
  discloses nothing and Finder probes it before prompting.
- **The namespace is hierarchical refs**: `/dav/` lists the repositories the
  subject may read (others are simply absent); `/dav/{repo}/` lists
  `refs/heads/*`, with a slash-bearing branch such as `topic/deep` browsing as
  folder `topic/` containing `deep/`. Git forbids one ref being a prefix of
  another, so matching URL segments against branch names longest-first is
  unambiguous. Below the branch, segments are the tree path, decoded to raw
  bytes and never round-tripped through `String` (ADR 0006).
- **Auth is the gateway's**: Basic with the token as the password, or Bearer,
  and a `WWW-Authenticate: Basic` challenge on 401 -- the same function stock
  Git already goes through, so repository permission stays literally one check.
- **Everything is `Cache-Control: no-store`**: the whole namespace is
  branch-tip addressed, so the same URL names different bytes after a push. The
  ETags (blob and tree object IDs) still give exact 304 revalidation; immutable
  serving stays at `/v1/repos/.../blobs/`.
- **Timestamps are the ADR 0006 sanitized snapshot time**, so Finder and a FUSE
  mount of the same commit agree on what they report.
- **Hand-rolled multistatus XML and RFC 1123 dates**: the workspace has no XML
  or time crate and read-only DAV needs only string building -- the same
  no-dependency-at-the-trust-boundary reasoning as the gateway's base64. The
  PROPFIND body is ignored and answered as `allprop`.
- A **missing Depth header is infinity** (RFC 4918 section 9.1) and infinity is
  refused with `propfind-finite-depth`, because Depth infinity over a monorepo
  is a tree walk nobody meant to request. Finder and cadaver always send 0 or 1.
- **Symlinks are served as small files** holding the target bytes -- that is
  the blob's content, and WebDAV has no symlink concept. Gitlinks and
  unsupported modes are omitted from listings: a node that can never be opened
  breaks a Finder copy of its enclosing folder, which is worse than absence.

## Alternatives considered

- **A `dav-server` crate**: brings an XML parser and a filesystem trait shaped
  for real filesystems, at a trust boundary, to serve four read-only methods.
  Rejected for the same reason the gateway hand-rolls twenty lines of base64.
- **Branch as one URL segment with `%2F`**: proxies are entitled to rewrite
  encoded slashes (the reason ADR 0006 moved paths to `path_b64url`), and Finder
  could not browse the branch list. The hierarchy costs nothing and browses
  naturally.
- **Root-level `/{repo}/...` URLs**: collide with `/healthz`, `/metrics`, and
  `/v1`, and make every future route addition a hazard.
- **Read-write DAV**: every PUT would have to synthesize a commit, and Finder's
  temp-file-and-rename save dance would synthesize several per save. Writes
  belong to the overlay and the commit path.

## Consequences

- A Mac browses and reads any snapshot with zero client software; so does
  `curl -X PROPFIND`.
- `visible_refs()` is enumerated per repository-scoped request. Fine for a
  browse surface; a ref-list cache keyed on the catalog's ref state is the
  known fix if ten-thousand-ref repositories become a target.
- Commit-hex URLs are deferred: a 40-hex first segment is lexically
  indistinguishable from a branch named like one, and no WebDAV client emits
  such URLs.
- GET buffers the whole blob, like `/file`; streaming is a shared later
  improvement.
