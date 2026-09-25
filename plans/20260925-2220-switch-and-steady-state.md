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

**Step 1 — delta fsmonitor answers** (complete: commit df5d53b)
* Token `gfs:<generation>:<instance>:<sequence>`. The instance is this opening
  of the overlay (a counter in the journal's `meta`, bumped by every
  `Overlay::open`); the sequence counts mutations in the instance.
* Every path a mutation touches is stamped in memory with the sequence it
  produced: `commit()`'s puts and deletes (both names of a rename), paths
  entering `vanished`, and in-place writes (`note_written`), so a write through
  a descriptor still open is reported before the close. Stamps are cleared on
  rebind.
* Same generation and instance: the answer is exactly the paths stamped after
  the caller's sequence, read under one lock with the sequence it is current
  to; with nothing new the token comes back byte-identical. Otherwise the
  full list (overlay changes, directory deletions, vanished paths), with a full
  rescan only for another generation or a vanished overflow, as before.
* Smoke test `gfs-fuse/tests/fsmonitor.rs::the_answer_is_a_delta_and_a_quiet_status_rewrites_nothing`.

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

* **Delta stamps live in memory, with an instance in the token.** Tried
  first and replaced: a persisted per-row `last_changed_sequence` (schema v3)
  answered from SQL. It broke three ways -- a caller at the newest stamp got
  the full list, so a quiet status still rewrote the index; `note_written`
  advanced the sequence without persisting, so a pre-restart token could be
  ahead of post-restart stamps and hide edits; and the delta was filtered
  through `status`, dropping stamped paths it does not list. An instance in the
  token makes a restart a full answer (today's behaviour, always correct) and
  lets the stamps be a plain in-memory map, with no SQL on the hook's path.
  The cumulative answer is what this replaces.

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

* **Content::Base reused for arbitrary blobs in local mode.** When written
  content matches an existing blob in the object store, the row's Content::Local
  is converted to Content::Base(oid) with the blob's oid, even if the blob is not
  from the current path in the commit. This works because LocalSource.serves_blobs_in_memory()
  is true, so the mount's open_blob path calls read_blob_shared(oid, "") directly
  without resolving through the tree. In server mode, has_blob() returns false,
  so conversion only happens for blobs equal to the base (same path, same oid, same mode),
  which are served through the normal tree resolution path.

* **Blob conversion is background, no stamping.** After settle_content or settle_all,
  a separate async task hashes the file, checks if the blob exists, and calls
  convert_to_blob_reference. The conversion commits to the journal without stamping
  the path or advancing the fsmonitor sequence, since the bytes and stat data are
  unchanged. This is safe because the content_id is checked against the current row
  to ensure the file hasn't been deleted or replaced since hashing.

* **Metadata is preserved on conversion.** When a row is converted from Content::Local
  to Content::Base, the ino, size, mtime, ctime, and mode are kept unchanged. This
  matches Git's expectations for stat data: build tools must never see an mtime go
  backwards, and Git records the stat data at checkout.

## Details

* **Step 1 results, universe** (fresh private workspace, 3 edited or new
  files; every answer compared with `git -c core.fsmonitor=false -c
  core.untrackedCache=false status --porcelain=v2`, all identical):

  | | before | after |
  |---|---|---|
  | status with nothing new since the last | index rewritten every time (+0.9 s fresh, +5.8 s in the long-lived workspace: 8.5 s) | **2.2 s**, no index write, 10-byte hook answer |
  | first status after edits | — | 3.1 s (writes the index once) |
  | statuses after a stock `git switch` (24.5k files) | 9.2, 6.0, 7.9 s … forever | 7.6 s (one 2.6 MB answer), 3.3 s, then **1.9 s** |

* Tests: `gfs-fuse --test fsmonitor` 5/5; `gfs-overlay`, and `gfs-mount`
  `seeded_caches`, `workspace_git`, `local`, `mutations` pass except the known
  `overlay::a_row_left_behind_by_an_unsettled_write_is_corrected_from_its_content_file`.
  A daemon restart is not covered by a test (the harness has no restart);
  by construction it yields a full answer.

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
