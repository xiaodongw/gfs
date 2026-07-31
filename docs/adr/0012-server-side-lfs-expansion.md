# ADR 0012: Server-side LFS expansion

- Status: Proposed
- Date: 2026-07-31
- Extends: [ADR 0009](0009-raw-git-over-a-projected-object-store.md) — the
  projected object store is untouched; expansion happens in entry metadata and
  the working-tree view. [ADR 0002](0002-git-object-authorization-boundary.md)
  — the authorize-at-tree-walk, fetch-by-ticket model gains one key form.
  Removes the "LFS files appear as pointer files" divergence from DESIGN.md
  section 12.
- Evidence: [`spikes/reports/m05d-lfs-expansion.md`](../../spikes/reports/m05d-lfs-expansion.md)

## Context

The MVP serves LFS files as their pointers (DESIGN.md section 12): an agent
that opens a model weight, a test fixture, or a design asset gets three lines
of text where the tool expects content. "Before production" already lists LFS
pointer awareness; this ADR decides what shape it takes.

The obvious dodge — ship git-lfs in the agent image and run `git lfs pull`
after `gfs clone` — fails on this architecture three separate ways. The seeded
`origin` URL points at the gateway, so git-lfs derives a batch endpoint the
gateway does not serve. The smudged content lands in the copy-on-write
overlay: per-workspace duplication of exactly the bytes ADR 0008 shares, and
an immediate `EDQUOT` against the 1 GiB default overlay quota. Worst, the
overlay journal cannot tell a smudge from an edit, so `gfs commit` — which
commits overlay changes to the gateway's mirror — would replace pointers with
gigabytes of expanded content on the branch. A mechanism that makes an
innocent `gfs commit` a corruption vector is disqualified, not mitigated.

The correct observation underneath that dodge is worth keeping: the state
`git lfs pull` leaves behind — expanded working tree, pointer blobs in the
object store and index — is a state every LFS-aware tool already understands.
The m05d spike measured exactly when stock Git stays truthful in that state:
always, if a `filter.lfs` clean/smudge configuration exists; and with the
filter absent, `git status` misreports every LFS file and `git add` writes
the expanded blob into the object store. The filter is a correctness
requirement, not a nicety.

## Decision

The server resolves LFS; the workspace presents the post-`git lfs pull`
state, filter configuration included, so no component downstream of entry
metadata knows LFS exists — and stock Git, which must know, is told through
the one channel it already understands.

- **Detection at snapshot preparation.** An entry is an LFS entry when its
  path carries `filter=lfs` in `.gitattributes` at that revision and its blob
  parses as a spec v1 pointer. Pointers are ~130 bytes, so reading candidates
  costs one small-blob read each, once per prepared snapshot.
- **Substitution in entry metadata.** For LFS entries, `GetEntry`,
  `ListDirectory`, and `BatchGetEntry` report the expanded size and the
  content key `lfs-sha256:{oid}` instead of the pointer's git object ID. The
  qualified `{algorithm}:{hex}` spelling and the per-entry blob ticket already
  exist; the immutable blob endpoint serves the new key form from the LFS
  store. Every metadata consumer — the mount's file attributes, `gfs cat`,
  WebDAV — expands without knowing it did.
- **Client verification extends, not weakens.** The LFS oid *is* the sha256
  of the raw content, so the blob cache verifies `lfs-sha256:` downloads by
  hashing raw bytes — one new arm beside the `blob <size>\0` arm, same
  no-unverified-bytes rule, and the same shared cache and hydration
  accounting as every other blob (ADR 0008).
- **The gateway holds an LFS store.** Content-addressed by sha256, populated
  from the upstream's `/info/lfs/objects/batch` API at import and fetch time
  with the caller's upstream credential — the same trust shape as `gfs clone`
  and `gfs push`, which already hold that credential for the same upstream.
  A miss may fill lazily; an unfetchable object degrades that one entry to
  its pointer (the MVP behavior) rather than failing the snapshot. LFS
  objects retained by a snapshot fall under its retention lease.
- **The projected object store is untouched.** Trees reference the pointer
  blobs and object hashes must verify, so the projection keeps serving them.
  This is what ADR 0009's real-`.git` guarantee requires, and it is also what
  makes the git-layer state below coherent.
- **The workspace is born reconciled.** The mount seeds `filter.lfs.clean`,
  `smudge`, and `required` in the workspace git config, pointing at
  daemon-backed shims: clean answers the pointer from entry metadata for
  base-identical content and hashes only genuinely edited bytes; smudge
  hydrates by oid through the normal blob path, which is what makes stock
  `git checkout` across LFS revisions work (m05d arm D). The index is seeded
  with expanded sizes and snapshot-time stat data, and base mtimes are the
  sanitized snapshot time — in the past by construction, so no entry is ever
  racily clean and steady-state `git status` costs zero filter invocations
  deterministically (m05d: 6 ms, versus ~250 ms per status forever when
  raciness is left to luck).
- **The write path re-cleans.** `gfs commit`'s gateway side stores edited
  LFS-path content into the LFS store and commits a fresh pointer — m05d
  confirmed the filter contract produces exactly that pointer for stock
  `git commit`, and the gateway does the equivalent for overlay commits.
  `gfs push` uploads the branch's new LFS objects via the batch API, with the
  caller's credential, before pushing the ref.
- **Search skips LFS entries** with their own coverage reason (`lfs`), the
  way `binary` and `oversized` already work: expanded content is unsearchable
  by nature and pointer text is noise that would match `oid sha256:`.
- **The smart-HTTP gateway is unchanged.** Stock `git clone` receives
  pointers, which is correct LFS behavior for a clone; a client that wants
  content brings its own git-lfs, exactly as against any Git host.

## What the spike settled

m05d staged the synthesized state against stock Git 2.53.0 with no git-lfs
installed, fabricated pointers, and stub filters standing in for the
daemon-backed shims:

1. Filter absent: `status` misreports, `git add` stores the 64 MiB expanded
   blob. Filter present: every command truthful in every run, including
   commit of a genuinely edited LFS file as a fresh, correct pointer.
2. The metadata-answering clean reconciles 4× faster than the hashing clean
   (65 ms vs 273 ms over 72 MiB) and never reads file content for
   base-identical paths.
3. Snapshot-time mtimes plus a refreshed index make the zero-cost steady
   state deterministic; current mtimes leave every future `status` re-cleaning
   at full cost, silently, forever.
4. Stock checkout across LFS revisions hydrates through the smudge and
   verifies; its freshly written files are racily clean until the next index
   write, a bounded drain-cost seam.

## Alternatives considered

- **Client-side `git lfs pull` after `gfs clone`**: rejected above — endpoint
  mismatch, overlay duplication and quota, and the `gfs commit` corruption
  vector. Configuring `lfs.url` and teaching the overlay journal to recognize
  smudge-equivalent writes would patch the first and third, but the result
  still stores every expanded object once per workspace instead of once per
  host, which is the problem ADR 0008 exists to avoid.
- **Transparency without filters**, relying on seeded stat data alone:
  rejected by m05d arm A. The stat cache is a performance layer; any path
  that forces a rehash — `git add` above all — then lies or corrupts.
- **Requiring real git-lfs in the agent image**: produces the same state and
  is safe, but pays content hashing on every reconcile, adds a tool to ADR
  0007's surface, and still needs endpoint and credential plumbing per
  workspace. The seeded config deliberately uses the standard `filter.lfs`
  name so an image that ships git-lfs anyway remains coherent; coexistence
  beyond that is untested (m05d limitation) and pinned as an open item.
- **Proxying `/info/lfs` at the gateway** for client-side git-lfs: less
  server work than a store, but keeps the per-workspace duplication and the
  overlay-journal ambiguity; it solves only the endpoint problem, which is
  the cheapest of the three.

## Consequences

- First `open()` of a large LFS file blocks for a whole-object fetch, verify,
  and cache write — the existing whole-blob behavior at LFS scale. Range
  fetch for `lfs-sha256:` objects is the known relief valve and stays out of
  this ADR's scope; DESIGN.md section 12's "documented large-file behavior"
  item now has a concrete forcing function.
- Snapshot preparation reads every candidate pointer blob once and the
  gateway learns the LFS batch protocol; both are bounded, but import of an
  LFS-heavy repository now has a fetch phase whose size is the LFS working
  set, and a policy question (prefetch all reachable objects vs. tip-only vs.
  lazy) that the implementation must surface rather than bury.
- An entry can degrade to its pointer when upstream LFS is unreachable, so
  "expanded" is per-entry state, not per-repository — `gfs status` should say
  which entries degraded, for the same reason search reports scoped coverage.
- The clean shim may run more than once per git command on stat-dirty paths
  (m05d arm D measured 4 invocations across 2 files in one `status`); the
  long-running `filter.<driver>.process` protocol is the escalation if spawn
  overhead ever shows up in a real workload.
