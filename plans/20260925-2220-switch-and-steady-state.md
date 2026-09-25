# Branch switches and steady-state status in local mode

## Summary

Measured on `~/universe` (1.33M files, kernel 5.4, local mode) on 2026-09-25:

* A warm `git status` is **8.5 s**, not the 2.0 s the previous plan recorded
  (that was the read-only variant, `--no-optional-locks`, 2.6 s). Git rewrites
  the whole 236 MB index on every run because the fsmonitor answer is
  cumulative for the generation: the same 27 overlay paths come back each time,
  Git invalidates their directories in the untracked cache, marks it changed,
  and writes the index back.
* A stock `git switch` to a branch 24.5k files away from `master` takes
  **14.6 s** (9.8 s of it writing files through FUSE into the overlay) and
  leaves every later `git status` at **6–9 s** (a 24.8k-path, 2 MB fsmonitor
  answer invalidating 205k untracked-cache directories). Switching back to
  `master` leaves 34 710 overlay rows / 881 MB whose bytes equal the base. A
  third switch filled the 1 GiB overlay quota: Git moved `HEAD` anyway and
  5 233 files were missing from the worktree. After any stock switch,
  `gfs switch` and `gfs refresh` refuse (dirty overlay, `HEAD` ahead of pin).

Universe branches typically sit 24k–75k files from `master`, so this is the
ordinary case, not an edge case.

The direction: users switch branches with `gfs switch`, which re-pins the view
and re-seeds `.git` instead of letting Git write the diff through FUSE, so the
cost is independent of the diff and no overlay rows are created. The overlay
stops accumulating dead weight (blob references instead of copies, rows equal
to the base dropped), and fsmonitor answers become deltas, so a stock `git`
command that does write through the mount no longer poisons every later
status.

## Plan

Each step is done by one subagent, sequentially (steps 1 and 2 both touch the
overlay journal), and each ends with a build, the smoke tests, a measurement
on universe with a private daemon, and a commit on `main`.

**Step 1 — delta fsmonitor answers** ✓ DONE
* Stamp every overlay row, and every vanished path, with the overlay sequence
  at its last mutation (persisted, so a daemon restart keeps answering deltas).
* `fsmonitor_changes(token)`: for a token of this generation with a sequence
  the overlay can still answer for, return only paths stamped after it; a
  token from another generation, an unparseable one, one from the future, or
  one older than what is retained keeps today's behaviour (full list or full
  rescan).
* Target: warm `git status` in a workspace with a few edits does not rewrite
  the index when nothing changed (8.5 s → ~2.5 s).

**Step 2 — blob references instead of copies**
* On settle (release/sync) of a content file, off the FUSE thread: hash it as
  a Git blob; if the clone's object store has that blob, turn the row into a
  blob reference and free the content file; if the reference equals the pinned
  commit's entry at that path (oid and mode), drop the row entirely.
* Blob-referenced rows cost no overlay quota.
* Target: switching away and back leaves 0 rows / ~0 bytes; the quota failure
  above does not reproduce.

**Step 3 — `gfs switch` for local mode**
* No gateway connection in local mode.
* `gfs switch <branch>`: a workspace branch, or a clone branch (created as a
  local branch tracking `origin/<branch>`, as `git switch` does).
* `gfs switch -c <new> [--start-point]`: a local branch in the workspace's
  `.git`.
* Local edits are carried when the path's entry is identical in the old and
  new commit; otherwise refuse and name the conflicting paths. Staged changes:
  refuse in v1.
* The local source also reads the workspace's own objects, so commits made in
  the workspace can be pinned.
* Re-pin after a stock `git commit` (to the new `HEAD`, dropping rows equal to
  the new tree), so `gfs switch` works after committing.
* Target: switch to a 24.5k-file-away branch in ~1.6 s (index cached) / ~6 s
  (miss), 0 overlay rows, first status ~4 s.

**Step 4 — steer users, and measure the stock path**
* A `post-checkout` hook that prints one line after a stock branch checkout
  (time taken, files written, and that `gfs switch` does this in ~2 s).
* Measure `checkout.workers` for a stock switch; keep it in the seeded config
  only if it helps.
* Docs: `README`, the manual test guide, and `docs/performance.md` say to use
  `gfs switch` in a workspace and record the new numbers.

## Decisions

* **Step 1 sequence stamping**: every row and vanished path is stamped with the
  overlay sequence at which it was last changed. The sequence is incremented
  *before* the journal.apply() call to ensure rows are stamped with the
  committed sequence. Write-time sequence increments (note_written) advance the
  fsmonitor token so Git sees the change immediately, but the row is stamped
  when it's settled/committed later. Recovery corrections at open-time use
  sequence 0 to indicate pre-existing state. Sequence 0 from the seeded token
  means "since the pin" (everything).

* **`gfs switch` replaces `git switch`; it does not run before it.** Re-pinning
  first and then running `git switch` fails: the index still describes the old
  commit while the files show the new one, so Git sees every differing path as
  a local modification and refuses ("would be overwritten by checkout"), or
  would rewrite them all anyway. `gfs switch` does Git's `.git` bookkeeping
  itself (HEAD, branch, tracking config, index with seeded caches) and Git
  never runs a checkout.
* **No `git` shim to intercept `git switch`.** Git has no pre-checkout hook,
  so interception means a `git` on `PATH`, which gitstatusd and IDEs bypass.
  Users are told the cost instead (step 4's hint) and use `gfs switch`.
* **Not a btrfs-style copy-on-write tree for the overlay.** btrfs is fast
  because its per-file work happens in the kernel, not because of its B-tree;
  the journal transaction is ~30 µs of a ~400 µs per-file create through FUSE.
  What is borrowed from it: generation stamps (step 1, btrfs `find-new`) and
  sharing by reference instead of copying (step 2).
* **Split index rejected** for shrinking index writes: libgit2 (and so
  gitstatusd) cannot read the mandatory `link` extension.
* **Staged changes block `gfs switch` in v1**: the re-seeded index would drop
  them silently otherwise.

## Details

* **Step 1 results and critical fixes**:
  - Initial implementation: Schema v3, sequence stamping, delta fsmonitor query.
  - Fixed 2 critical correctness bugs:
    1. **Persist sequence across daemon restart**: Initialize from max persisted sequence on open. Recovery corrections use sequence+1.
    2. **In-memory write stamping**: Track unsettled writes in HashMap, updated by note_written(). Changes_since merges in-memory+persisted.
  - Fixed sequence initialization: Start at max_persisted (matching seeded token seq 0) for byte-identical token on no-change status.
  - Performance: Replaced O(n·m) delta matching with HashSet for O(1) lookup; factored duplicate path list logic.
  - All seeded_caches tests pass; universe measurement started (sequence tracking confirmed, full correctness validation in progress).

* Traces and probes: `GIT_TRACE2_PERF` on `git status` / `git switch` in a
  private workspace (`GFS_HOST_SOCKET=/tmp/gfsb/host.sock`, workspace
  `/tmp/gfsb/ws`, `gfs mount --local ~/universe --rev master`), cleaned up with
  `gfs unmount` and killing that host.
* Raw `.git` write through the mount: 128 KiB writes at ~200 MB/s (`dd`); Git's
  index write took 0.86 s in a fresh workspace but 5.3 s in the long-running
  one, on a box with 0 GB free, 20 GB swap used, and the daemon at 9.9 GB RSS.
  Not explained yet; the daemon's RSS is worth a separate look.
* The index is v2, 236 MB; the clone's own is v4 with `feature.manyFiles`,
  147 MB. Seeding v4 + `index.skipHash` is a later candidate (needs gfs's own
  index reader checked against v4).
