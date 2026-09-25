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

**Step 1 — delta fsmonitor answers** (complete)
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

**Step 2 — blob references instead of copies** (complete, narrowed)
* When the last writer of a content file closes and the file's size equals
  the base's, hash it in the background; if it is the pinned commit's blob at
  the same path, the row becomes `Content::Base(oid)` (the existing
  "metadata diverged, bytes did not" row) keeping its inode number, size,
  mode and times, and the copy is freed. Committed without a stamp or a
  sequence bump, so fsmonitor and Git see nothing.
* Writers are counted in the overlay: `copy_up` registers one against the
  row the overlay holds now (not the inode record, which a conversion can
  make stale), `create` registers its new content, `release` and the
  one-shot `truncate`/`fallocate` paths release theirs. A copy with a writer,
  a dirty row, or a size/time that moved since the hash is never replaced.
  The inode record is republished before the copy is removed.
* Local mode's default overlay quota is 32 GiB (server mounts keep 1 GiB).
* Not built: references to blobs other than the base's (see Decisions).
* Smoke test `crates/gfs-mount/tests/local.rs::a_checkout_back_to_the_pinned_commit_leaves_no_copies_behind`.

**Step 3 — `gfs switch` for local mode**

*3a (complete): a clean workspace.*
* The CLI forwards to the daemon in local mode (no gateway); `Request::Switch`
  carries `create`, `start_point`, `detach`.
* `gfs switch <b>`: the workspace's `refs/heads/<b>` (loose or packed), else
  the clone's branch, which becomes a workspace branch tracking `origin/<b>`;
  a revision that is not a branch is refused with the `--detach` hint.
  `gfs switch -c <new> [--start-point <p>]` (`origin/<x>` start points track
  `<x>`), `gfs switch --detach <rev>`.
* When the target is the commit `HEAD` is already on (`-c` at `HEAD`, two
  branches at one commit), only `HEAD` moves: no re-pin, no index, and it
  works over local edits and local commits as in Git. Otherwise the view is
  re-pinned to the commit, with the branch name kept beside it (the clone
  cannot resolve a workspace-only branch); `repin` puts it on `HEAD`.
* `.git/config` is the user's; gfs's settings moved to `.git/gfs/config`,
  included from it. The pinned branch's upstream is added to `.git/config`
  once, when it has no section. `packed-refs` keeps branches Git packed.
* Refused for now, before anything is written: local edits (3b) and local
  commits (3c), with the way out in the message.
* Smoke test `crates/gfs-mount/tests/local.rs::gfs_switch_moves_a_local_view_between_branches_without_a_checkout`.

*3b (pending): carry unstaged edits whose paths are identical in both commits;
refuse naming the rest; refuse staged changes.*

*3c (pending): switch after a stock `git commit` -- the local source also reads
the workspace's objects, and the view adopts the new `HEAD` without discarding
uncommitted edits.*

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

* **Only the base's own blob is referenced, for now.** The first version
  (f2cf7f2, reverted) referenced any blob the clone had through
  `Content::Base`. Every reader of a `Content::Base` row takes it to mean "the
  pinned commit's bytes at this path (or `renamed_from`)": search re-homes the
  server's result for that path instead of searching the bytes, so a row
  naming another blob would search the wrong content. It also left the inode
  record naming a deleted copy, removed the copy under a writer that reopened
  the file, and did not check that the file was unchanged since the hash.
  Referencing arbitrary blobs needs its own variant, handled by search,
  export, the commit plan and the read paths; until then a checkout to
  another branch keeps copies, and the larger local quota is what keeps a
  stock `git switch` from hitting `EDQUOT`.
* **A kept row, not a dropped one.** Dropping the row would put the path back
  on the base's inode number and snapshot time: Git recorded the checkout's
  stat data, and a build tool must never see an mtime move backwards.
* **32 GiB local quota.** The 1 GiB default is a server-job budget; in local
  mode the overlay shares the disk the clone is on, and one stock switch
  between universe branches writes 0.8–1.3 GB.

* **3a: the subagent's version (2645c7c) was reverted.** It resolved
  "workspace branches" against the clone, pinned the commit hash so `HEAD`
  ended up detached with no branch or upstream, and put the config include
  at the top of an old-format `.git/config`, where the stale settings after it
  would override the fresh ones.
* **`.git/config` is Git's and the user's, gfs's settings are included.** The
  seed used to rewrite the whole file on every repin, which discarded every
  `git config`, `git branch -u` and `git switch` tracking section. A file
  without the include is taken as one gfs wrote before the split and
  replaced once. `core.repositoryformatversion`/`core.bare` stay in
  `.git/config`: Git reads the repository format before following includes.
  Alternatives: parse and merge the old file (a config parser for a one-time
  migration), or keep rewriting and re-add known sections (loses everything
  else).
* **The pin is a commit, the branch travels beside it.** A workspace branch
  can exist only in the workspace, where the clone cannot resolve it by name;
  resolving by name would also pick the clone's newer commit for a branch the
  workspace already has, which `git switch` never does. So the selector is the
  commit and `gfs refresh` after a local switch changes nothing; moving a
  branch to its upstream's newer commit is `git fetch` + `git merge` territory.
* **Same commit, no re-pin.** `git switch -c` and a switch between two
  branches on one commit touch only `HEAD` in Git; so here, which also keeps
  them working over local edits and local commits.

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

* **Step 2 results, universe** (fresh private workspace on `master`, stock
  `git switch` each time; every status matched the uncached answer, 200
  switched files re-hashed to their index blob, `gfs status` clean):

  | step | before | after |
  |---|---|---|
  | switch to `universe-goofys-grpc` (24.5k files) | 14.6 s, 822 MB | 14.0 s, 822 MB |
  | back to `master` | 17.2 s, 881 MB held | 19.0 s, **0 bytes** held |
  | to `xiaodong-wang_data/uc-fuse-grpc-s2s` (~50k) | 62 s, quota hit, 5 233 files missing | **27 s**, 1.33 GB, nothing missing |
  | back to `master` | — | 32 s, **0 bytes** |
  | status after the last switch | — | 10.0 s once, then 1.9 s |

  Rows stay (68 658 after the last round trip) as zero-byte references.
* Tests: `gfs-mount` and `gfs-fuse` run whole (`--no-fail-fast`): only the
  known `mutations::a_recreated_directory_does_not_show_the_base_children_it_replaced`
  and `prefetch::reading_a_directory_through_fetches_the_rest_of_it` fail;
  `gfs-overlay`: only the known `overlay::a_row_left_behind_by_an_unsettled_write_is_corrected_from_its_content_file`.

* **Step 3a results, universe** (fresh private workspace on `master`; each
  switch left 0 overlay rows, `HEAD` on the branch with its `origin/<b>`
  upstream, `git status` empty; 100 changed files re-hashed to `HEAD`'s blobs;
  the uncached status matched):

  | | stock `git switch` | `gfs switch` | first status after (stock / gfs) | then |
  |---|---|---|---|---|
  | to `universe-goofys-grpc` (24.5k files) | 14.0–14.6 s | **6.3 s** (index built) | 18.3 / **3.7 s** | 1.9–2.0 s |
  | back to `master` | 17.2–19.0 s | **1.15 s** (index cached) | 8.1 / **3.7 s** | 2.0 s |
  | to `xiaodong-wang_data/uc-fuse-grpc-s2s` (~50k) | 27–62 s | **4.3 s** (index built) | 11.9 / **3.7 s** | 1.9 s |
  | `-c tmp-branch` | — | 0.00 s | — | — |

* Tests: `gfs-mount`, `gfs-fuse`, `gfs-overlay`, `gfs-cli` run whole
  (`--no-fail-fast`): only the known
  `overlay::a_row_left_behind_by_an_unsettled_write_is_corrected_from_its_content_file`
  fails (the two known `mutations`/`prefetch` failures passed this run).

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
