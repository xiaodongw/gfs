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
* Refs: `visible_ref_targets` skips `refs/prefetch/` (git maintenance's hidden
  namespace, 100k of universe's 131k refs) and reads an object's type from its
  header before paying `find_tag`, so only real tags are peeled.
* Sizes without libgit2's locks: the tree descent emits entries in index order
  with no size, then one pass fills every blob size. For a local clone
  (`Libgit2Repository::with_git_size_lookup`) the pass asks stock Git: up to 16
  `git cat-file --batch-check` processes, each over a contiguous slice, answers
  checked for order, `GIT_NO_REPLACE_OBJECTS=1` and the `GIT_DIR` family
  removed so the answer is the one libgit2 would give. If Git cannot be run the
  pass falls back to in-process header reads across the handle pool (the path
  server mode always uses), and an object Git calls missing is read in-process
  so its error is unchanged.
* A per-commit index cache under the host's cache directory,
  `<cache_dir>/<repo-id>/indices/<commit>.index`, beside the odb projection's
  per-repository state: at most 8 per clone by mtime (counted on disk, so the
  bound survives restarts; a hit touches the file), written to a writer-unique
  temp file, fsynced, renamed; the SHA-1 trailer is verified on every read and
  a bad file is dropped. All file work on blocking threads with no lock held.
* Deferred: reusing sizes from the clone's own `.git/index` (see Decisions),
  hardlinking the cached file into the workspace, the 206 vs 121 MiB index size
  question.

Phases 0, 1, and A were built as planned; Phase A took four passes (see
Decisions for what was built and removed along the way). One addition to
phase 0 was measured and not kept: a larger listing cache (see Decisions).

**Phase B — the first `git status`** (complete)
* Measured split of a fresh universe workspace's first status: the `lstat`
  sweep of 1.32M entries 13.8 s (`status -uno`), the untracked walk 61 s
  (390 354 `opendir`s). Both are state Git builds once and writes back.
* `seed_git_dir` now appends both to the index it seeds, per workspace
  (`gfs_git::index::with_workspace_caches`), when the fsmonitor hook is
  installed: `FSMN` v2 with the token `gfs:<generation>:0` and an all-valid
  dirty bitmap, and `UNTR` with every tree directory valid, no untracked
  entries, each directory's `.gitignore` blob ID, the ident Git computes
  (`Location <worktree realpath>, system <sysname>`), the `dir_flags`
  `status.showUntrackedFiles` implies, and the blob IDs of `info/exclude` and
  `core.excludesFile` as Git hashes them. The shared per-commit cache file stays
  pristine; the extensions and a new trailer are added at seed time.
* Smoke test `crates/gfs-mount/tests/seeded_caches.rs`: stock Git on a seeded
  fixture clone opens no directory and agrees with its own uncached answer,
  pristine and with a reported change.

**Phase C — zero-message open** (built; opt-in, not the local default)
* `open` and `opendir` answer ENOSYS when `FsConfig::zero_message_open` is set
  (`gfs-fuse --zero-message-open`); `init` then does not ask for
  `FUSE_ATOMIC_O_TRUNC`, so `O_TRUNC` arrives as `setattr(size=0)`. Reads,
  writes, and `readdir` without a handle are served by inode
  (`read_unopened`, `write_unopened`, a `DirState` built per `readdir`).
  `GFS_TEST_ZERO_MESSAGE_OPEN=true` forces it on for every mount the test
  harness creates.
* Not built, and why it stays opt-in: see Decisions.

**Not done**: profiling warm `git status` (~2 s) and commit (~6 s), and the
206 vs 121 MiB index size question.

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
* **Skip `refs/prefetch/*`.** Git hides the namespace from decorations; it
  only feeds `git maintenance`'s background fetch. The workspace's
  `packed-refs` no longer carries it either.
* **Blob sizes from stock Git, not from more libgit2 threads.** libgit2 1.9.6
  keeps packfiles in one process-wide cache shared by every repository handle
  (`git_mwindow__pack_cache`, `mwindow.c`), and every header read takes that
  pack's `p->lock` and `p->mwf.lock` (`pack.c`); universe's blobs sit in one
  26 GiB pack, so threads take turns — 1.45x at best, measured. Separate
  processes share nothing: `git cat-file --batch-check` does all 1.32M in
  0.66 s over 8 processes, 0.44 s over 16. Considered and rejected: gitoxide
  (a second Git implementation in the daemon for one pass), reading pack
  headers ourselves (a pack/midx reader to maintain). Local mode only: Git is
  already a prerequisite there, and server mode keeps its in-process path.
* **Not the clone's `.git/index` for sizes.** A pass that parsed it was built
  and removed: an index records the working-tree file's size after smudge,
  eol conversion and `ident` expansion, which is not the blob's size wherever
  those apply; it read skip-worktree/intent-to-add from the wrong flag word
  and desynchronized on the first extended-flags entry; and it bought ~0.2 s
  on top of the subprocess pass. Revisit only with an attribute-aware filter.
* **The in-process size pass cannot deadlock.** Each worker checks out one
  handle and waits holding nothing, and the descent returns its handle before
  the pass starts; the first parallel version held one while its threads
  waited for the rest of the pool.
* **The index cache lives with host state, bounded, verified.** A first
  version wrote into the clone's `.git/gfs/` (breaking the rule that local
  mode writes only its anchors into a clone, unbounded, and wrong for a bare
  repository or a linked worktree) with a non-atomic write; a second moved it
  to `$HOME/.cache` with an in-memory LRU that forgot files across restarts,
  a shared temp name, and 206 MiB of I/O under a mutex on a runtime worker.
  The shipped one is the shape described under Plan. The hit path reads the
  file and seeding writes it again rather than hardlinking it, because the
  next phase (seeding `FSMN`/`UNTR`, whose ident names the worktree path)
  needs per-workspace bytes anyway.

* **Seed the caches rather than make the walk cheaper.** The walk is ~4 FUSE
  round trips per directory; zero-message `opendir` would halve it and still
  leave ~30 s on universe. Only the untracked cache removes it.
* **Why the seeded caches cannot make `git status` lie** (read against Git
  2.54's `dir.c` and `fsmonitor.c`): Git trusts a valid untracked-cache
  directory without `lstat` only after a successful, non-trivial fsmonitor
  answer, and applies that answer first, invalidating the directory of every
  path it names. The daemon's answer is every overlay change relative to the
  pin — not a delta — or `/` for a token of another generation, after which
  Git compares each directory's recorded stat data (zero here, so never a
  match) and re-reads it. Every other seeded value — the exclude-file IDs, each
  directory's `.gitignore` ID, the ident, the flags — is compared with what is
  there now, and a mismatch invalidates. So a wrong value costs a walk, never
  an answer; the matrix below checks the answers.
* **Hash exclude files the way Git does.** Git appends a newline before hashing
  a file it reads from disk (`add_patterns`); the first seeded version hashed
  the bytes as they are, the `core.excludesFile` ID never matched, and Git
  invalidated the whole cache — correct output, full 63 s walk.
* **The directory tree comes from the index, not from gitignore evaluation.**
  Git records no node for an ignored directory holding tracked files (403 on
  universe: `.claude/`, `.vscode/`, `logs/`, …) because its walk never enters
  one. The seeded cache carries them anyway; Git never looks them up and drops
  them when it writes the index back. Evaluating ignore rules to match exactly
  would add a second gitignore implementation for no gain.
* **Discarded from the first pass at this phase**: an FSMN helper with an empty
  EWAH buffer and the claim that FSMN alone bought nothing (never measured —
  it takes the sweep from 13.8 s to 0.4 s), and a placeholder UNTR function.

* **Zero-message open stays opt-in.** Built as briefed except for the parts
  that replace what `release` and open descriptors did, and those decide it:
  once `open` answers ENOSYS the kernel sends no `release` for *any* file on
  the connection (`fuse_file_put` checks the connection's `no_open`), created
  files included. So (a) a created file's daemon handle and descriptor are
  never dropped — a `cp -r` of enough files runs the daemon out of
  descriptors; (b) a written overlay row's size and mtime, committed once at
  `release`, are never committed, leaving durability to the unsettled-row
  recovery whose test already fails on `main`; (c) an unlinked-but-open
  overlay file has no descriptor keeping its content: measured, a read after
  unlink returns EIO once its pages leave the page cache; (d) a `.git` file
  renamed over while open (Git's lockfile protocol; gitstatusd mmaps the
  index) is read by path, so the old inode would serve the new file's bytes.
  What the default needs: settle on write or on the journal's consumers,
  drop created handles on `forget`, and park a descriptor for an unlinked or
  replaced file until the kernel forgets its inode.

## Details

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
* **Phase A results, universe** (1 326 149 entries, private daemon):

  | | before Phase A | after |
  |---|---|---|
  | `index_for_commit`, cold (fresh process) | 7.5–15 s | 4.6 s (in-process fallback path: 9.5 s) |
  | create, cache miss | 7.5–11 s | 5.6 s fresh daemon, 3.6 s with the tree cache warm |
  | create, cache hit | — | 1.0 s |

  Correctness: the index built through Git and through libgit2 are
  byte-identical (`cmp`), and every one of the 1 326 149 sizes in it matches
  `git ls-tree -r -l HEAD`. `git status` in each mounted workspace reports no
  changes; `git log -1` names the pinned commit.

* **Phase B results, universe** (fresh workspace, private daemon; each first
  status compared with `git -c core.fsmonitor=false -c core.untrackedCache=false
  status --porcelain=v2` run right after):

  | case | first `git status` before | after | `opendir` | same answer |
  |---|---|---|---|---|
  | pristine | 64.6 s | 3.9–4.4 s | 587 (was 390 354) | yes |
  | base file modified | 65.4 s | 4.2 s | 588 | yes |
  | base file deleted | 66.2 s | 4.3 s | 588 | yes |
  | new top-level file | 67.2 s | 4.2–4.3 s | 588 | yes |
  | new file in new nested dirs | 66.7 s | 4.1–4.3 s | 592 | yes |
  | new file under an ignore rule | 65.4 s | 4.3 s | 590 | yes |
  | `.gitignore` edited to un-ignore | 67.6 s | 5.6 s | 7 081 | yes |

  The remaining 587 `opendir`s are the directories under the 16 whose
  `.gitignore` Git re-validates (`gitignore-invalidation:16`); not chased.
  The `lstat` sweep alone went from 13.8 s to 0.4 s (FSMN).
* Cost: create on a cache hit 1.0 s → 1.4–1.6 s, which misses the 1.3 s target.
  Seeding one universe workspace is 0.52 s (0.14 s of it the new SHA-1
  trailer over 225 MiB; the tree pass over 1.32M entries the rest, after
  replacing a per-component map lookup that took it to 0.72 s). Taken: it
  buys ~60 s on the first status. A cache miss is 6.9 s.

* **Phase C probe, universe, `--zero-message-open`** (`/tmp/gfsinv/probe.py`):
  `O_TRUNC` over a base file and over an overlay file, append, write-then-read
  before close, in-place overwrite, read of a base file after unlink, and
  `git status`/`git diff` (identical to the uncached answer) all pass; read of
  an overlay file after unlink fails with EIO once its pages are dropped
  (`posix_fadvise(DONTNEED)` before the unlink). With the mode forced on, the
  `gfs-mount` suite has 3 failures beyond the known ones, all remote-mode
  open-time semantics: `a_spent_hydration_budget_refuses_the_open_with_edquot`,
  `a_second_read_of_the_same_blob_costs_nothing`,
  `losing_the_server_fails_a_copy_up_without_damaging_the_overlay`.
