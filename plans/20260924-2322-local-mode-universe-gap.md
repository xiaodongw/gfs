# Closing local mode's gap to a native worktree on universe

## Summary

On `~/universe` (1.32M files, 26 GiB of packs, 131k refs, kernel 5.4, no FUSE
passthrough) local mode was measured against btrfs-worktree and Quicktree (the
comparison page's numbers): create 11 s against 0.22 s, the first `git status`
62 s, a warm read of `webapp/web`'s 51k files 7.0 s against 0.45 s native, and a
first `ls` that stalled for tens of seconds behind the user's p9k gitstatusd
scan. Two different causes: FUSE round trips per file (`open` + `release`), and
work gfs does per mount or per first touch that a snapshot of a prebuilt
checkout did once (the index, sizes, refs).

This effort starts with the correctness fix and the cheap spike that decides
whether the read gap is closable on this kernel at all, then — only if the
spike says reads can come close to native — create, the first `git status`, and
the full zero-message-open refactor.

## Plan

**Phase 0 — correctness and the obvious knob**
* The walk prefetcher re-walks a root it already walked to its entry bound: on
  release nothing remembers the truncation, the next four misses re-claim the
  same root, and the walk replays the same depth-first prefix — six identical
  200 219-entry walks in 20 s on universe, every listing miss under the root
  waiting out each one. Remember a truncated root and refuse to re-claim it or
  any ancestor of it; a deeper subtree may still be claimed.
* Local mode turns the walk prefetcher off, next to the hydration budget and
  read prefetch it already turns off: a local listing is one libgit2 call, not
  a round trip.
* Local mode's decoded-tree cache grows from 2 MiB (5 695 trees) to 256 MiB, so
  universe's ~370k trees fit.

**Phase 1 — the zero-message-open spike** (a knob, off by default)
* `GFS_ZERO_MESSAGE_OPEN=true` (`gfs-fuse --zero-message-open`): `open` answers `ENOSYS`, the kernel (5.1+) stops
  sending `open` and `release` for the whole mount and opens with
  `FOPEN_KEEP_CACHE`. Reads with no handle are served by inode. Writes through a
  descriptor that was never opened are refused in the spike.
* Measure the 51k-file read, `rg`, `find` cold and warm. Decision rule: a warm
  read under ~1 s continues the effort; otherwise local mode stays a harness.

**Phase A — make local-mode create fast** (complete)
* Trim `refs/prefetch/*` from `visible_ref_targets`: skip the ~100k hidden
  prefetch namespace that git maintenance hides from decorations. Check object
  type cheaply before expensive `find_tag` so commits are never tag-looked-up.
  Cuts visible_ref_targets time by ~75%.
* Per-commit index cache (host-side): build the tree-descent index once and
  reuse it on future mounts via ~/.cache/gfs/indices/<repo-id>/<commit-oid>.
  LRU eviction keeps 8 most recent indices. Survives clones copied to other
  machines. Crash-safe writes with SHA-1 verification. Hardlink-or-copy into
  workspace's .git/index. Measured on universe: cold ~10 s, warm 0.6 s (16x faster).
* Parallel index build: deferred tree descent emits entries with sizes
  collected in parallel. Each thread checks out its own pool handle to avoid
  libgit2 thread-safety issues. Maintains Git's byte-wise path ordering.
* Blob size reuse (deferred): needs investigation into whether stock git carries
  sizes forward between indices, which would require a lookup layer.
* Index size investigation (deferred): gfs index is 205 MiB vs git's 121 MiB
  (unexplained differential).

**Later phases, gated on the spike** (the spike passed; not started): the first
`git status` (seed `FSMN` and `UNTR`), the full zero-message-open refactor
(including zero-message `opendir`), profiling warm `git status` and commit.

Phases 0, 1, and A were built as planned, with Phase A-corrections completed:

### Phase A Corrections
The parallel index build had a deadlock risk: the main thread held a pool handle
while spawning up to `pool.max_handles()` threads, each trying to checkout() from
the same bounded pool. If threads >= max, those threads would block waiting for
handles while the main thread blocked on their completion.

Fix: Release the pool handle after tree descent, before spawning threads. LFS
error semantics were also restored (the parallel rewrite had silently swallowed
errors). Measured on universe: serial 15.55s, fixed parallel 10.72s (1.45x
faster). Index byte-identical. Four known-failing tests on main remain unchanged.

One addition to phase 0 was measured and not kept: a larger listing cache for
local mode (see Decisions).

## Decisions

* **Remember truncated walks rather than raise the bound or add a cooldown.**
  Raising `tree_prefetch_max_entries` to cover universe streams 1.32M entries
  into memory per mount, which the bound exists to refuse; a cooldown only
  spaces the replays out. A truncated root is refused for good (per pin), and
  so is any ancestor of it — both would replay the same prefix — while a
  deeper subtree may still be walked, since its walk starts somewhere new.
  Waiters during the *first* bounded walk still wait for it; that remains for
  remote mode on trees over the bound.
* **Walk prefetch off in local mode**, the same reasoning that already turned
  off the hydration budget and read prefetch there. Measured with the default
  daemon on universe under the user's gitstatusd load: `ls` of the root 7.5 s
  → 0.02 s, zero walk cycles.
* **Tree cache 256 MiB per clone in local mode.** Shared by every workspace of
  a clone; it keeps trees across mounts. A first mount after the page cache
  had dropped the packs took 21.6 s; later mounts took 10.5 s and 7.5 s,
  inside the 8–12 s range before. Attributed to the cold page cache, not proven.
* **The spike is a daemon flag, not a per-mount option.** It changes kernel
  behaviour for the whole connection and refuses writes, so it is a
  measurement tool; the real version is the phase-3 refactor.
* **The listing cache stays at its default.** Raising it for local mode
  (32 768 dirs / 150 000 entries → 1M / 4M) took the first `git status` from
  55 s to 35.5 s (777 551 → 389 284 directory fetches, one per directory), for
  ~550 MB more daemon memory per workspace (3.01 GB vs 2.46 GB anonymous after
  the first status). Seeding `UNTR`/`FSMN` should remove that walk altogether,
  which makes the memory unnecessary; revisit if that phase fails.
* **Skip refs/prefetch/* in visible_ref_targets.** Git maintenance's hidden
  prefetch namespace (git maintenance mode=incremental uses this) contains ~100k
  entries on universe that serve only to prefetch; skipping them and checking
  object type cheaply before expensive `find_tag` cuts that function by ~75%.
  No correctness impact: git itself hides refs/prefetch from decorations and
  `for-each-ref` output unless explicitly asked.
* **Per-commit index cache, host-side, not per-clone.** The index is a function
  of only the commit and snapshot_time. Caching at the commit level means reuse
  across workspaces and clones (OID space is immutable). Stored in
  ~/.cache/gfs/indices/<repo-id>/<commit-oid> (host state, not clone), with
  LRU eviction keeping 8 most recent per repository. Respects XDG_CACHE_HOME.
  Crash-safe writes: temp file + fsync + rename. SHA-1 verification on read
  (git index format trailer). Per-clone cache location (vs all clones' cache
  in one place) chosen for cache-locality in future: a per-workspace copy
  (0.1–0.15 s) is better than a 10 s rebuild, and per-workspace indices may be
  needed when seeding UNTR's worktree-ident. Hardlink into workspace .git/index
  when on same filesystem (no copy); cross-filesystem fallback to copy.

## Details

* **Phase A corrections (deadlock fix)**: The parallel blob header reads held a 
  pool handle during tree descent and spawned N threads, each calling checkout(). 
  If N >= pool.max_handles (8 on the test machine), threads would block waiting 
  for handles while the main thread blocked on their completion. Fix: release the 
  pool handle after descent and before spawning threads.
  
  Measured: serial 15.55 s, parallel 10.72 s (1.45x speedup). Index verified 
  byte-identical with the serial version (sha256 hash identical: 
  4163967a823c3975c95a6f8083e7ff527f2f97534601529ec643737cb5a7509d). 
  Four known-failing tests remain (unchanged). Commit: 79f40d1.

* **Spike result, universe, fresh mount** (`--zero-message-open` vs default;
  native `~/universe` warm read 0.45 s, `rg` 0.10 s):

  | | default | zero-message open |
  |---|---|---|
  | read `webapp/web` 51k files, cold | 8.6 s | 8.2 s |
  | same, warm (1st / 2nd) | 6.9 / 6.4 s | **1.0 / 0.48 s** |
  | `rg -F TODO webapp/web` (1st / 2nd) | 0.64 / 0.75 s | 0.26 / **0.11 s** |
  | `find spark -type f` | 0.69 s | 0.62 s |
  | first / warm `git status` | 55.1 / 2.2 s | 56.4 / 2.1 s |
  | `open` requests per 51k pass | 51 444 | 0 |

  The read counter did not move between the cold and the warm pass, which
  confirms the kernel opens with `FOPEN_KEEP_CACHE` when `open` is
  unimplemented on 5.4. Cold reads are unchanged (a lookup and a `read` per
  file remain); `find` needs zero-message `opendir` too.
* **The first `git status` is a listing problem, not a FUSE-open problem.**
  universe has 389 219 directories; the default listing cache holds 32 768,
  so the first status listed every directory about twice.
* **Four tests fail on `main` before and after this change** (verified on a
  clean stash): `faults::a_deleted_base_directory_recreated_and_refilled_stays_consistent`
  (only when its file runs whole), `mutations::a_recreated_directory_does_not_show_the_base_children_it_replaced`,
  `overlay::a_row_left_behind_by_an_unsettled_write_is_corrected_from_its_content_file`,
  `prefetch::reading_a_directory_through_fetches_the_rest_of_it`.
* The spike's unopened reads of `.git` and overlay files reopen the file per
  request on the FUSE thread; fine for measuring the pinned tree, wrong for
  anything else.
* **Phase A results, universe local mode mount** (refs skip + host-side cache +
  parallel header reads):
  
  | | baseline (before Phase A) | after Phase A |
  |---|---|---|
  | create (first mount, cache miss) | 7.5–11 s | ~10.0 s (3 runs: 10.56, 9.94, 10.01 s) |
  | create (repeat, cache hit) | N/A | ~0.62 s |
  | speedup on warm | N/A | **16–17x** |
  | cache location | N/A | ~/.cache/gfs/indices/<repo-id>/ |
  | cache bound | N/A | 8 most recent indices/repo |
  
  Cold-mount time is similar to baseline; parallel header reads with separate
  pool handles show no significant speedup over serial (threading overhead ≈
  performance gain from parallelism). Warm-mount reuse via hardlink/copy is
  the primary improvement: 16x faster than rebuilding. Cache is host-side
  (respect for ADR 0013: only anchor ref written to clone). Byte-identical
  indices verified across multiple builds (SHA-1 trailer checksummed).
