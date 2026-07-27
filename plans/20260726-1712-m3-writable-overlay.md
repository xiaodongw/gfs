# M3 — Writable overlay and export

## Summary

M2 delivered a read-only lazy mount of one pinned commit: a FUSE filesystem, a
verified blob cache, the synthesized `.git` surface, and the mount lifecycle
around them. Every mutation answers `EROFS`, `xvfs status` does not exist, and
the `git` shim reports a clean tree because there is nothing yet that could make
it dirty.

M3 makes the workspace writable. It adds a crash-safe copy-on-write overlay —
journal, content store, path state machine — wires every FUSE mutation to it,
derives status/diff/export from the journal without scanning the base, and proves
under fault injection that an acknowledged mutation is never lost.

PLAN.md section 6 states three exit criteria:

1. random mutation sequences match a reference in-memory filesystem model;
2. an export applied to the pinned Git commit produces the same tree as the
   mounted workspace;
3. fault injection meets the no-lost-acknowledged-mutation goal.

## Plan

Four phases, one commit each, then a completion report.

### M3.1 — `xvfs-overlay`: the data model

The crate is a declared placeholder today. It becomes a synchronous,
network-free library that owns the overlay's durable state, in the same shape as
`xvfs-server`'s `Catalog`: SQLite behind a mutex, called from async code through
`spawn_blocking`.

- `journal.rs` — `overlay.sqlite`: schema, versioning, migration, open/recovery.
- `store.rs` — `files/`: content allocation, copy-up materialization, orphan
  collection, the fsync/rename ordering.
- `state.rs` — the path state machine types.
- `merge.rs` — pure resolution and readdir-merge functions over overlay state
  plus caller-supplied base facts.
- `model.rs` — the reference in-memory model, plus a seeded random sequence
  driver used by the property tests.

### M3.2 — Mutation operations in the filesystem

Rewire every `EROFS` in `fs.rs` to the overlay, and make `lookup`, `readdir`,
and `readdirplus` merge. Includes the `O_TRUNC` copy-up elision, open-file
semantics across rename/unlink, quota enforcement, and `statfs` reporting real
overlay usage.

### M3.3 — Status, diff, and export

Status and diff derived from the journal. Deterministic JSON and Git-patch
export with an atomic bundle and checksums. `xvfs status|diff|export` on the
CLI, the same three over the control socket, and the `git` shim's `status`,
`diff`, and `ls-files` rewired to the journal. A verifier that applies an export
to a clean checkout of the pinned commit and compares trees.

### M3.4 — Crash and concurrency testing

A fault-injection harness that kills a real process at each journal/file
transaction boundary and asserts recovery is idempotent and loses nothing
acknowledged. Disk full, permission failure, server loss, concurrent writers,
rename cycles, unmount with open files. The compatibility suite re-run in
writable mode.

## Decisions

_Filled in as the work proceeds; see the completion report for the final set._

### The journal is durable state; memory is the index

The whole overlay entry set is held in memory and mirrored to SQLite. Reads
never touch the database. A mutation writes the SQLite transaction, commits, and
only then updates memory and replies — so a crash mid-transaction leaves neither.
The entry count is bounded by what one job edits, not by the size of the
repository, which is what makes holding all of it affordable.

### Content is written before the journal row that names it

Ordering per content mutation: write the temporary file, `fsync` it, `rename` it
into place, `fsync` the directory, then commit the journal transaction. The
invariant is one-directional — **a committed row's content always exists; an
unreferenced content file is garbage** — so recovery is a sweep for orphans
rather than a repair of half-written state.

### Copy-up is lazy in two stages, not one

An overlay entry's content is either `Local(id)` or `Base(oid)`. A mode change,
a rename, or a directory materialization produces a `Base(oid)` entry: overlay
metadata, base bytes, no download. Only a write turns it into `Local`. This is
what lets `mv` of a large file cost nothing and `chmod +x` cost nothing.

### A base directory rename materializes metadata eagerly

Renaming a base directory walks its subtree and writes one overlay row per path,
all still `Base(oid)`. The alternative — an overlayfs-style redirect — is a
second resolution mechanism to get wrong. The cost is metadata proportional to
the subtree and no blob content, and it is bounded by a configurable entry limit.

### Overlay inode numbers come from a persisted high range

Created paths take numbers from `1 << 48` upward, allocated and persisted by the
journal. A copied-up base path **keeps the number it already had**, because an
editor that stats a file before and after writing it must see one identity. On
daemon restart the inode table is seeded from the journal so no number is reused.

### Status hashes local content rather than trusting the journal

An entry whose bytes were modified and then restored is reported as unchanged,
because status computes the Git OID of the local content and compares it with
the base OID. Only changed paths are hashed, so the cost is proportional to the
edit set rather than to the tree.

## Details

_Filled in as the work proceeds._
