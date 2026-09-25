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

**Step 1 — delta fsmonitor answers** ✓ REPLACED
* Token format: `gfs:<generation>:<instance>:<sequence>`. Instance identifies
  the opened overlay in this process, persisted in the journal's meta table and
  incremented on every `Overlay::open`.
* Stamps in memory only (cleared on rebind): path -> sequence map of the last
  mutation in this instance. commit() stamps all Put/Delete changes and paths
  entering vanished; note_written() stamps write-time mutations.
* `fsmonitor_changes(token)`: parse generation+instance+sequence from token. If
  instance matches current and sequence <= current, return paths with stamps >
  caller_sequence (raw, not filtered through status). Otherwise return full list.
  Vanished overflow also triggers full answer with rescan flag.
* No persisted sequence columns: schema reverted from v3 to v2; removed
  journal.max_sequence, journal.changes_since, and the build_paths filtering.
* Sequence starts at 0 per instance (not persisted), incremented by commit() and
  note_written(). Daemon restart with new instance_id forces a full answer from
  a pre-restart token.
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

* **Step 1 sequence stamping (replaced design)**: Stamps live in memory only,
  not persisted to the database. The previous attempt persisted them to schema
  v3, which caused two critical bugs: (a) `changes_since` returned None when
  caller_seq >= max_persisted, forcing a full answer and index rewrite on
  no-change status; (b) restarting incremented instance_id after the caller had
  already parsed an old token, so pre-restart tokens could be stale mid-flight.
  The new design: instance_id distinguishes process restarts; sequence counter
  per instance starts at 0 (not persisted, so any pre-restart token is from
  before or from a different instance). Stamps map path -> sequence tracks
  mutations in this instance only, cleared on rebind. This avoids both bugs: a
  restart with different instance_id forces a full answer; no stamped paths from
  a previous instance exist to cause a stale token answer. The map is bounded by
  paths touched in this instance (typically much smaller than the journal).

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

* **Step 1 implementation (replaced)**:
  - Previous attempt (commits f1417e0, 7682ab8, 4c67982, 4d01df6): Schema v3,
    persisted sequence in entries/vanished, journal.changes_since. Abandoned due
    to bugs: changes_since returns None on no-change (full answer + index
    rewrite); restart sequence edge case (token from previous process instance);
    filtering through status.changes (dropping valid paths). Removed 56 lines of
    dead code.
  - New implementation:
    * Token `gfs:<gen>:<instance>:<seq>` with instance from journal meta table,
      incremented on every `Overlay::open()` (using next_instance_id).
    * In-memory stamps: HashMap<path, sequence>, cleared on rebind. Stamp all
      Put/Delete in commit(), all writes in note_written().
    * fsmonitor_changes: parse token, check instance match. Return raw stamped
      paths (no status filtering). On mismatch or vanished overflow, compute
      status and return full list.
    * Schema reverted from v3 to v2: removed last_changed_sequence columns from
      entries and vanished tables. Removed journal.max_sequence and
      journal.changes_since (no longer called).
  - Tests: all seeded_caches pass (3/3); workspace_git (11/11); local & mutations
    pass except pre-existing failure (a_recreated_directory_does_not_show_the_base_children_it_replaced).

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
