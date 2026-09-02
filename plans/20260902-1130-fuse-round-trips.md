# Local mode is slow for the same reason every mode is: round trips

## Summary

The local-mode benchmark put a workspace over a clone on the same disk next
to `git worktree add`, and the recurring costs did not close the gap the way
"everything is local" suggests they should: reading 2 000 files took 0.64 s
against 0.04 s, `git add -A` 1.5 s against 0.06 s, `rg` over the tree 1.1 s
against 0.04 s. This plan is the investigation into why, and the changes it
led to. The short version: nothing in local mode was slow; the daemon paid
three to four kernel round trips per file and per directory, and each round
trip cost two to four times what the kernel itself charges, because every
request took a detour through the tokio runtime before it was answered.

## Plan

1. **Measure, per operation, where a request's time goes.** A microbenchmark
   over the mount (`lstat`, `open`+`close`, `open`+`read`+`close`, `listdir`,
   negative lookup) against the native clone and against `fuser`'s own hello
   filesystem, with the daemon's CPU attributed per thread from `schedstat`.
   `strace -c` on `git add -A` in the mount and in a worktree for the syscall
   shape. `fuser`'s request log to count what the kernel actually sends.
2. **Stop hopping.** Poll every handler's future once on the FUSE thread
   inside the runtime's context, and hand it to the runtime only if it has to
   wait. One change in `GfsFilesystem::spawn`; every handler benefits.
3. **Stop asking.** Let the kernel keep what a pinned commit cannot change:
   `FOPEN_KEEP_CACHE` on base blobs, `FOPEN_CACHE_DIR | FOPEN_KEEP_CACHE` on
   merged-view directories, `FOPEN_NOFLUSH` everywhere (our `flush` was a
   no-op round trip), `FUSE_PARALLEL_DIROPS`. Invalidate inodes as well as
   entries on a re-pin.
4. **Re-run the benchmark** on both corpora, and record what is left.

## Decisions

* **The FUSE thread runs the synchronous prefix of every handler.** ADR 0003
  says a callback must never block and the work runs on the runtime; this
  keeps the first half and refines the second: a callback may *complete* on
  the FUSE thread when nothing in it waits. Measured before: a warm
  `open`+`close` cost 146 µs wall and 98 µs of daemon CPU, of which the
  runtime hand-off was a third; a negative lookup 100 µs. After: 63 µs and
  56 µs. The hand-off's cost was never the spawn itself (32 workers or one
  made no difference); it was that every request woke a second thread and
  waited for it.
* **A noop waker for the first poll is sound**, because a spawned task is
  always polled once more and every primitive re-registers the waker it is
  polled with. Written down in the function's doc comment, since it is the
  kind of thing a reader will otherwise re-derive with alarm.
* **`KEEP_CACHE` is per state, not per mode.** Base blobs are immutable under
  one inode in remote mode too; the flag goes on `Memory` and `Blob` states.
  Passthrough `.git` files and overlay content files do not get it, because
  the daemon and Git write those behind the kernel's back.
* **`CACHE_DIR` needs `KEEP_CACHE`.** `fuse_dir_open` drops a directory's
  pages on every `opendir` unless `KEEP_CACHE` is set, and the listing cache
  lives in those pages. With `CACHE_DIR` alone the kernel refilled the cache
  on every listing and never once read from it — confirmed from the request
  log, then from the kernel source, before the second flag went in.
* **No `readdirplus`.** Advertising it made the kernel's first read of every
  directory a `readdirplus`, which creates a dentry and inode per entry and
  gave nothing to a walk that never stats (`git add -A`, `git status`'s
  untracked fill). Plain `readdir` plus the kernel's own listing cache is what
  those walks want. The implementation stays; it is simply not advertised.
* **No `FUSE_CACHE_SYMLINKS`.** A path keeps its inode number across delete
  and re-create, and the kernel does not drop a cached link target for an
  inode it still holds. Not worth the correctness argument for a benefit the
  benchmark cannot see.
* **No `FUSE_NO_OPENDIR_SUPPORT`.** It would remove the `opendir`/`releasedir`
  pair per directory, but the kernel's listing cache is keyed off the open
  flags an `opendir` reply carries; without an open there is no cache, and a
  second walk would ask again.
* **FUSE passthrough (kernel 6.9+, `fuser` 0.18) is the next lever, and it is
  a deployment decision.** It would let the kernel read a blob straight from a
  memfd with no `read` round trip at all. Tested here: the backing-file ioctl
  is refused without `CAP_SYS_ADMIN`. Left out; recorded in the report.

## Details

* Numbers, before and after, in `benchmarks/local-mode.md`.
* The kernel floor on this machine (WSL2 6.18): `fuser`'s hello filesystem
  answers `open`+`flush`+`release` in 48 µs wall with one thread and 60 µs
  with four. That is the price of a round trip here; the daemon now sits
  within ~15 µs of it for a warm open.
* What is left per file, warm: `open` and `release`, each a round trip. What
  is left per directory on a second walk: `opendir` and `releasedir`, plus one
  `getattr` after an uncached read because the kernel invalidates `atime` on
  every uncached `readdir` and glibc's `opendir` calls `fstat`.
* A cold `open` still pays `spawn_blocking` plus libgit2's inflate, ~130 µs
  over the warm cost. Git's own `cat-file --batch` does 2 000 blobs in
  0.12 s, so most of that is the inflate, not the hand-off.
