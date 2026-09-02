# ADR 0014: Answer on the FUSE thread, and let the kernel keep what cannot change

- Status: Accepted (implemented 2026-09-02)
- Date: 2026-09-02
- Amends: [ADR 0003](0003-fuse-deployment-model.md) — the dispatch rule keeps
  its first half (a callback never blocks) and refines its second (the work
  runs on the runtime): a callback may *complete* on the FUSE thread when
  nothing in it waits.
- Evidence: [`benchmarks/local-mode.md`](../../benchmarks/local-mode.md),
  [`plans/20260902-1130-fuse-round-trips.md`](../../plans/20260902-1130-fuse-round-trips.md)

## Context

Local mode (ADR 0013) put a workspace over a clone on the same disk, and the
benchmark said the recurring costs had not moved the way "everything is
local" implies: reading 2 000 files took 0.64 s against 0.04 s native, warm or
cold alike; `git add -A` took 1.5 s against 0.06 s; `rg` over the tree 1.1 s
against 0.04 s. Nothing in local mode was slow — the blob LRU made inflate
free on the second pass and the second pass was no faster.

The investigation measured per operation, with the daemon's CPU attributed
per thread and the kernel's request stream logged, and found two things:

1. **Every request took a detour.** ADR 0003's rule was implemented as "spawn
   every handler onto the tokio runtime and reply from there." A warm `open`
   — a lookup in the inode table, an LRU hit, a map insert — therefore woke a
   runtime worker, waited for it, and replied from it: 146 µs wall and 98 µs
   of daemon CPU for `open`+`close`, where `fuser`'s own hello filesystem
   does the same in 48 µs. Worker count made no difference (32 or 1); the
   hop did.
2. **The kernel asked for things it could have kept.** Three synchronous
   round trips per `open`+`close` (`open`, a `flush` we answered with nothing,
   `release`), a `read` on every open because the page cache was dropped at
   each open, and four round trips per directory on every walk because the
   kernel was never told a listing could be cached.

## Decision

**Poll each handler's future once on the FUSE thread, inside the runtime's
context, and hand it to the runtime only if it returns pending.** This is one
change, in `GfsFilesystem::spawn`, and every handler gets it. A request the
caches can answer completes without leaving the thread that read it; a
request that has to wait — a server fetch, a `spawn_blocking`, a lock someone
holds — is spawned from its first pending point and polled again the moment a
worker picks it up. The first poll uses a waker that does nothing, which is
sound because a spawned task is always polled once more and every primitive
re-registers whichever waker the current poll carries.

**Tell the kernel what a pinned commit cannot change.**

- `FOPEN_NOFLUSH` on every file. Nothing is buffered above the host
  filesystem — a write reaches its content file inside the callback that
  carried it — so the `FUSE_FLUSH` the kernel sent on every `close` was a
  round trip that answered nothing.
- `FOPEN_KEEP_CACHE` on base blobs (`Memory` and `Blob` states, in both
  modes). The bytes behind a pinned commit never change under one inode, so
  the page cache may outlive the descriptor. A write goes through the kernel,
  which keeps its own cache coherent; a path re-created after a delete
  announces a new size, which the kernel treats as a reason to drop what it
  held; a re-pin now invalidates the inode as well as the dentry.
- `FOPEN_CACHE_DIR | FOPEN_KEEP_CACHE` on merged-view directories. The base
  never changes under one pin, every overlay mutation goes through the
  kernel (which bumps the directory's version itself), and a re-pin
  invalidates. The two passthrough trees are excluded because the daemon
  writes into them behind the kernel's back. Both flags are required:
  `fuse_dir_open` drops a directory's pages on every `opendir` unless
  `KEEP_CACHE` is set, and the listing cache lives in those pages.
- `FUSE_PARALLEL_DIROPS` at init. Every handler was already concurrent
  across directories; the kernel no longer serializes inside one.

## What was measured

| operation, warm | before | after | fuser hello |
| --- | ---: | ---: | ---: |
| `open`+`close` of a base blob | 146 µs | 63 µs | 48 µs |
| `open`+`read`+`close` of a base blob | 242 µs | 66 µs | 97 µs |
| negative lookup | 100 µs | 56 µs | 51 µs |
| `listdir`, ~120 entries | 377 µs | 82 µs | – |

On the vscode corpus: reading 2 000 files warm 0.64 s → 0.16 s; `rg` over
the tree 1.14 s → 0.78 s; cold `git status` 1.45 s → 0.56 s; commit 1.6 s →
0.42 s. The server mount improved by the same mechanism, since none of this
is local mode's.

## Consequences

- **A callback's synchronous prefix runs on a FUSE thread.** Std mutexes are
  taken there, the inode table is touched there, an overlay resolve happens
  there. None of it blocks on I/O; ADR 0003's measurement — one blocked
  callback serializes the mount — still governs what may go in that prefix,
  and `spawn_blocking` remains the way to a syscall.
- **The kernel holds state the daemon must invalidate.** A re-pin sends
  `inval_inode` for every record it moves, not only `inval_entry`. Anything
  new that changes content or a listing without going through the kernel —
  there is nothing today — has to do the same.
- **`readdirplus` stays implemented and unadvertised.** Advertising it made
  the kernel's first read of every directory a `readdirplus`, a dentry and an
  inode per entry, for walks that never stat. Plain `readdir` plus the
  kernel's listing cache is what `git status` and `git add -A` want.
- **What is left is the kernel's own floor**: one round trip for `open`, one
  for `release`, and for a directory `opendir` and `releasedir`. On this
  machine (WSL2, 6.18) a round trip is 16–25 µs; the daemon now sits within
  ~15 µs of the hello filesystem for a warm open.

## Alternatives considered

- **Fewer runtime workers.** Tried; 32 or 1 changed nothing. The cost was the
  hop, not the pool.
- **More FUSE threads, blocking callbacks.** ADR 0003 measured what that
  does to a parallel reader against a remote source; local mode would
  survive it and remote mode would not, and the trait is one filesystem.
- **`FUSE_CACHE_SYMLINKS`.** A path keeps its inode number across delete and
  re-create, and the kernel does not drop a cached link target for an inode
  it still holds. Correct only with more invalidation than it is worth.
- **`FUSE_NO_OPENDIR_SUPPORT`.** Removes the `opendir`/`releasedir` pair, but
  the kernel's listing cache is keyed off the flags an `opendir` reply
  carries; without an open there is no cache, and every walk asks again.
- **FUSE passthrough** (kernel 6.9+, `fuser` 0.18): a backing memfd per open
  blob and no `read` request at all, even cold. The backing-file ioctl is
  refused without `CAP_SYS_ADMIN`, so it is a deployment decision — a file
  capability on `gfs-fuse` — and is recorded in the benchmark as the next
  lever rather than taken here.
