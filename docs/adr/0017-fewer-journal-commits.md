# ADR 0017: One transaction per name change, none per write, no fsync per file

- Status: Accepted (implemented 2026-09-03)
- Date: 2026-09-03
- Amends: [ADR 0014](0014-answer-on-the-fuse-thread-and-let-the-kernel-keep-it.md)
  — the per-file cost it left in the overlay — and the durability model in
  `crates/gfs-overlay/src/journal.rs` and `store.rs`.
- Evidence: [`benchmarks/fuse-levers.md`](../../benchmarks/fuse-levers.md),
  [`docs/performance.md`](../performance.md)

## Context

After ADR 0015 and 0016 the read side of a local mount sits at about 10×
a native checkout, bounded by two kernel round trips per file. The write
side did not follow: `cp -r` of ten thousand files stayed at 26 s against
1.1 s native, and a `create` measured 2.5 ms of wall time and 1 ms of daemon
CPU with nothing of it in FUSE. Per created file the overlay did:

- an `fsync` of the new content file and an `fsync` of its shard directory
  (about 1.5 ms of waiting), to make the bytes durable before the row that
  named them was committed;
- one journal transaction for the file's row;
- one or two more for the parent directory's timestamp bump — adopting a
  base-only directory into the overlay and then setting its times were
  separate commits, and the mount root's times were two autocommitted
  meta writes;
- and then one transaction per `write(2)`, to keep the row's size and mtime
  current.

SQLite itself was not the cost. The exact transaction shape measured 31 µs
at `synchronous = NORMAL` on the same disk; the fsyncs and the number of
transactions were. Swapping the engine (Turso was asked about) would have
moved a 2.5 ms create by 30 µs.

The fsyncs were also stricter than what they protected. The journal has
always run at `NORMAL`: every acknowledged mutation survives the daemon
dying, and only `fsync(2)` on the mount promises survival of the host
dying. Fsyncing each content file bought power-loss safety for bytes whose
row had no such safety.

## Decision

1. **No fsync per published content file.** `ContentStore::publish` is
   write + rename. The overlay remembers every id published since the last
   sync, and `Overlay::sync` — what `fsync(2)` and `fsyncdir` on the mount
   reach — fsyncs those files, then their shard directories, then
   checkpoints the journal, in that order so a name never outlives its
   bytes. A `create` opens the empty content file in place and returns it,
   so the mount does not open it a second time.

2. **A `write` commits nothing.** The row's size and mtime move in memory
   and the content id is marked dirty; the fsmonitor sequence still
   advances. The row is committed once by `Overlay::settle_content` when the
   descriptor is released (the passthrough release already worked this way
   through `refresh_content`), by `fsync`, or by `sync`. Recovery stats
   every content file at open and corrects a row whose size or mtime lags
   the file in one transaction, which also covers a passthrough writer that
   died before it settled — a gap that existed before this change.

3. **The parent's timestamp bump travels in the child's transaction.**
   `create_file`, `mkdir`, `symlink`, `remove` and `rename` take the parent
   as `Parent { ino, base }` — the inode number the caller's table already
   uses for it and what the pinned commit has there — and put the parent's
   row (adopting a base-only directory if needed) or the root's meta times
   in the same `apply`. `touch_directory` is gone; the mount republishes
   the parent's row from memory.

4. **Cheaper commits.** Statements are prepared once per connection; the
   allocator counters are rewritten only when they moved; root times ride
   in `apply` rather than as separate autocommits.

## Consequences

- Per operation on a django local mount, without passthrough (wall time,
  2 000 iterations): `create`+`close` 2 508 → 428 µs; `create`+4 KiB
  `write`+`close` 2 354 → 598 µs; `mkdir` 448 → 353 µs; `unlink` 425 →
  309 µs. Whole-suite numbers are in `benchmarks/fuse-levers.md` under the
  "journal" columns.
- A power loss after a create that was never fsynced can lose the file's
  bytes or the file itself. The open-time sweep reports a row whose content
  is missing rather than inventing an empty file; a short file is reported
  by size, exactly as ext4 reports any file that was not fsynced. Daemon
  death loses nothing that was acknowledged, as before, because the rename
  and the WAL write are both in the kernel's hands.
- `truncate` still commits immediately: it can arrive without a descriptor
  (`truncate(2)`) and would otherwise leave a dirty row with no release to
  settle it.
- The crash matrix (`gfs-test/tests/overlay_crash.rs`) is unchanged in
  shape: the `content-synced` boundary now means "staging complete, not yet
  renamed". Its `create` operation settles explicitly, as a release would.
- The overlay's public API changed: `create_file` returns the entry with
  its open content file, and the five name-changing calls take a `Parent`.
  There is no compatibility shim; every caller is in this repository.
