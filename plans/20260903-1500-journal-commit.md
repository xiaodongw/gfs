# Cheaper overlay mutations: fewer fsyncs, fewer commits, cheaper commits

## Summary
A file created by `cp -r` on a local mount cost about 2.5 ms of wall time
and 1 ms of daemon CPU, none of it in FUSE: the content store fsynced the
new file and its directory (about 1.5 ms of waiting), the create committed
one journal transaction and the parent-directory touch committed one or two
more, and every `write` committed a fresh row for size and mtime. SQLite
itself was 31 µs of that. Four changes, one commit, measured against
`docs/performance.md`:

1. **No fsync per published content file.** The journal already runs at
   `synchronous = NORMAL`, which promises survival of daemon death and not of
   host death; the content store was stricter than the thing it protected.
   `publish` is now write + rename; the ids published since the last sync
   are remembered and fsynced (files, then shard directories) by
   `Overlay::sync`, which is what `fsync(2)` on the mount reaches.
2. **Size and mtime are not journaled per write.** A write updates the
   in-memory row and marks its content id dirty; the row is committed once
   when the descriptor is released, on `fsync`, or on `sync`. Recovery stats
   every content file and corrects a row whose size or mtime lags the file,
   which also covers a passthrough writer that died before it settled.
3. **The parent touch travels in the child's transaction.** `create_file`,
   `mkdir`, `symlink`, `remove` and `rename` take the parent as
   `Parent { ino, base }` and put the parent's row (adopting a base-only
   directory if needed) or the root's meta times in the same transaction.
   The mount republishes the parent from memory afterwards.
4. **Cheaper commits.** Cached prepared statements; the allocator counters
   are written only when they moved; root times ride in `apply`.

## Plan
- Overlay: `store.rs` publish without fsync, `create_empty` creates in place,
  shard directories created once; `journal.rs` `prepare_cached`, counter
  memo, `root_times` in `apply`; `lib.rs` `Parent`, in-memory writes with a
  dirty set, `settle_content`, recovery correction, `sync` that settles and
  fsyncs; drop `touch_directory`.
- Mount: pass `Parent` through create/mkdir/symlink/unlink/rmdir/rename,
  replace `touch_parent` with a memory-only republish, settle a local writer
  at `release`.
- Tests: overlay unit tests, state machine, crash matrix binary, mount
  mutation tests updated to the new signatures; one new overlay test that a
  reopen corrects a row whose content grew without a settle.
- Measure: the per-operation probe on a live mount, then the levers suite
  columns that need no capability, against the tables in `docs/performance.md`.

## Decisions
- Recovery corrects a lagging row's size from the file and its mtime from
  `max(row mtime, file mtime clamped to the overlay floor)`; ctime becomes
  the recovery time. A row whose content is missing is still reported, not
  repaired.
- `truncate` still commits immediately: it can arrive without a descriptor
  (`truncate(2)`) and would otherwise leave a dirty row with no release to
  settle it.
- Power loss after a create that was never fsynced can lose the file's
  bytes or the file itself. The journal already had that exposure for its
  rows; POSIX says an unsynced file has it too.

## Status
- [x] overlay changes build and the overlay tests pass (two new tests)
- [x] mount changes build; all 17 gfs-mount test binaries and the crash
      matrix pass
- [x] per-op probe, old and new binary back to back on a django local mount
      (wall / daemon CPU per op): create+close 2350/596 → 416/412 µs;
      create+write 4 KiB+close 2624/803 → 668/659; open+write+close 361/292
      → 284/333; mkdir 456/376 → 321/248; rmdir 510/458 → 334/291; unlink
      483/427 → 320/274; rename 708/585 → 465/377; overlay open+read+close
      unchanged at 185
- [x] levers suite columns "journal" and "journal + prewarm" recorded in
      `benchmarks/fuse-levers/` and merged: vscode `cp -r` 10 225 files
      29.4 → 8.9 s, `dd` 3.95 → 2.6 s, 4 KiB write 244 → 155 µs; reads and
      `git status` unchanged
- [x] passthrough + prewarm on this build (setcap re-run): vscode `cp -r`
      6.4 s, `dd` 0.87 s, 4 KiB write 51 µs, commit 3.6 s; the levers stack

## Where the rest of a create goes
A create+close is still ~410 µs of daemon CPU for one 30 µs transaction
and two round trips (~65 µs together). The remainder is our own code around
the commit — resolve, blocking hop, republish, the row clone into the
change list — and needs a CPU profile (`perf` is not installed here) rather
than guesses.

## Profile of the create path (2026-09-03, perf 7.0 on the WSL kernel)
Recipe: build with `RUSTFLAGS="-C force-frame-pointers=yes"
CARGO_PROFILE_RELEASE_DEBUG=1 CARGO_PROFILE_RELEASE_STRIP=none
CARGO_TARGET_DIR=~/gfs-corpus/target-prof`, mount django locally from that
binary, `perf record -F 4999 -g -p <daemon pid>` around a 20 000-iteration
`open(O_CREAT)`+`close` loop, then `perf script -F comm,ip,sym,dso` folded
by syscall and by owning frame (a flat report is useless here: the top
symbol is 1.8 %). `/usr/bin/perf` from `linux-tools-generic` runs against
the WSL kernel; `perf_event_paranoid=1` is enough for a process you own.

Where a create+close's daemon cycles go (180 K samples, 542 µs/op under
perf, 416 µs without):

| bucket | share |
| --- | ---: |
| `Overlay::create_file` in total | 15 % |
| — of which the `openat` of the content file (ext4 inode alloc) | 5 % |
| — of which SQLite (`apply`, WAL `pwrite`) | 8 % |
| tokio worker park/unpark, futex, `__schedule` | ~31 % |
| fuser threads: kernel `fuse_dev_read` | 15 % |
| fuser threads: parse, dispatch, `spawn`, `spawn_blocking` | ~13 % |
| reply `writev` into `/dev/fuse` | ~5 % |
| republish + inode table | 1.5 % |
| `resolve_path`, `parent_touch`, `next_time`, `commit` bookkeeping | <0.5 % |

So the daemon's own work is about 60 µs of the 416, and two thirds of the
rest is thread hand-offs: each request goes fuser thread → `spawn` (wake
a tokio worker) → `spawn_blocking` (wake a pool thread) → back to a worker
→ reply, and every hop is a futex wake, a context switch and a park on a
machine where the workers are otherwise idle. The remaining third is the
kernel's `/dev/fuse` read and write cost, which is the round-trip floor
ADR 0014 measured, attributed to the daemon's threads.

What would move it: answer `create` and `release` for the overlay on the
thread that has the request (the ADR 0014 pattern), or at least reply
from the blocking thread instead of hopping back through a worker —
each hop removed is on the order of 50–80 µs here. Not done: it changes
the "nothing that waits on I/O on a FUSE thread" rule, since the WAL write
can block on ext4, and wants its own decision.
