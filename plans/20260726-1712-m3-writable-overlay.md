# M3 — Writable overlay and export

## Summary

M2 delivered a read-only lazy mount of one pinned commit: a FUSE filesystem, a
verified blob cache, the synthesized `.git` surface, and the mount lifecycle
around them. Every mutation answers `EROFS`, `gfs status` does not exist, and
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

### M3.1 — `gfs-overlay`: the data model

The crate is a declared placeholder today. It becomes a synchronous,
network-free library that owns the overlay's durable state, in the same shape as
`gfs-server`'s `Catalog`: SQLite behind a mutex, called from async code through
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
export with an atomic bundle and checksums. `gfs status|diff|export` on the
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

The full set, with the reasoning that survived contact with the implementation,
is in [`docs/reports/m3-completion.md`](../docs/reports/m3-completion.md). The
ones below are the load-bearing ones, plus the four the implementation forced
that were not in the original plan.

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

### Forced during implementation: whiteouts are never dropped as redundant

The original plan let `rmdir` delete the per-file whiteouts under a directory,
since the directory whiteout already hides them. That would have made status and
export walk the base subtree to find out what to delete. `rm -rf` unlinks each
child first, so the per-file record already exists; keeping it is what lets
status be journal-only for every case that occurs in practice.

### Forced during implementation: `readdir` extras are keyed on base names

The plan assumed a row that records base facts is one the base listing will also
produce. After a rename that is false — the row remembers the base of the path it
came *from* — so extras are decided by the names the base listing actually
produced.

### Forced during implementation: the inode table follows a rename

The kernel does not look a path up again after `rename(2)`; it relinks the dentry
it has. The destination therefore arrives carrying the source's inode number, and
a table that did not follow answered the next request out of the source's record.

### Forced during implementation: teardown is bounded

`umount` of a busy filesystem fails with `EBUSY` and `umount_and_join` then waits
forever. Job cleanup falls back to a lazy unmount after a timeout.

## Details

### The guarantee, stated precisely

- **the daemon dies**: every committed transaction survives, so every
  acknowledged mutation is recoverable. This is what `synchronous = NORMAL` buys
  without an fsync per metadata operation.
- **the host dies**: recent commits may be lost unless something forced them out,
  which is what `fsync(2)` on a mounted file does.

### What the mount reports differently from M2

- base files are `0644`/`0755` rather than `0444`/`0555`, because with a
  copy-on-write overlay behind them they are writable;
- `MountOption::RO` is gone, and the synthesized `.git` surface is what stays
  read-only;
- `statfs` reports `f_namemax` as 255 rather than the path limit, and creating a
  longer component is `ENAMETOOLONG`;
- `FUSE_ATOMIC_O_TRUNC` is requested, so replacing a file does not first
  download it.

### Where things live

```text
<state-dir>/
  mount.json            M2
  generations/<n>       M2: the FUSE mount point
  overlay/<n>/
    overlay.sqlite      the journal
    files/<shard>/<id>  content
```

One overlay per mount generation, bound to that generation's commit. `gfs
refresh` refuses a non-empty overlay, and a retired generation's (empty) overlay
directory is removed with it.
