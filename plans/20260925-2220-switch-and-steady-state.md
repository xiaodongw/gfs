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
* Refused for now, before anything is written: local edits (lifted by 3b)
  and local commits (3c), with the way out in the message.
* Smoke test `crates/gfs-mount/tests/local.rs::gfs_switch_moves_a_local_view_between_branches_without_a_checkout`.

*3b (complete): carry unstaged edits, Git's rule.*
* Staged changes are refused: the workspace index is compared entry by entry
  (path, stage, mode, oid, intent-to-add; versions 2-4) with the index gfs
  seeds for the pinned commit (`gfs_git::index::first_staged_difference`).
* Every overlay row is checked against the target commit
  (`Mount::plan_carry`) before anything is written: a row whose bytes and
  mode are still its base's is kept if the path is unchanged, else dropped
  (the target's file replaces it); an adopted directory takes the target's
  tree as its base; a created directory where the target has one becomes it;
  a deletion of a path the target lacks too is dropped; any other row (edit,
  deletion, new file, rename and its source) needs the target to have exactly
  the row's base. A kept path's directories the target lacks are created,
  opaque, under the inode numbers the kernel already has. Anything else is a
  conflict: the refusal lists up to 20 paths with the reason.
* `Overlay::rebind` takes the plan and keeps those rows (content files,
  writers, inode numbers) in the same journal transaction that rebinds, and
  stamps the reported ones at sequence 1, so the re-seeded index's token
  (`:0`) makes Git `lstat` exactly them. A mutation between planning and the
  rebind moves the sequence and the switch is refused ("try again").
* Smoke test `gfs-fuse/tests/fsmonitor.rs::gfs_switch_carries_edits_the_two_commits_agree_on`
  (in `gfs-fuse` because the hook binary is only installed there; without
  the stamps Git hides the carried edit and the test fails).

*3c (complete): switch after a stock `git commit`.*
* Every `gfs switch` first copies the workspace's own objects (loose, and
  packs by name) into the clone (`LocalRepository::import_objects`), so a
  commit made here resolves, pins and is anchored there like any other:
  `gfs switch <branch with local commits>`, `--detach <local sha>`, and
  start points all work.
* The local-commit refusal is gone for `gfs switch`; the commits stay on
  their branch (the seed writes only the target branch's ref). `gfs refresh`
  still refuses.
* Staged changes are judged against `HEAD`: the index's cache-tree root
  equal to `HEAD^{tree}` (always, right after `git commit`) settles it
  without an index for `HEAD`; otherwise the entry comparison of 3b, with
  `HEAD`'s index built if no pin has it.
* `plan_carry` judges each row against `HEAD` (`h`) rather than the pin: a
  row whose bytes and mode are `HEAD`'s (an edit since committed, hashed
  only when `HEAD` changed the path since the pin) is clean -- kept as a blob
  reference (copy freed) where the target agrees, else replaced by the
  target's version; a directory the commit added, holding all its files, is
  treated as a tracked one, so it goes when the target lacks it and nothing
  untracked is left in it. With `HEAD` at the pin the rules are 3b's.
* Refused: a path where `HEAD` differs from the pin but the working tree was
  never written (`git reset --soft`), since carrying it would need the pin's
  bytes copied into the overlay.
* Found on the way, fixed for every re-pin: the kernel keeps a negative
  entry for `negative_ttl` (1 s) and a re-pin has no record of the name to
  invalidate, so a path absent in the old commit stayed invisible for up to
  a second after the switch -- and a `git status` then, trusting fsmonitor,
  said nothing about it. The re-pin now waits out the last negative answer
  (`Gfs::outlast_negative_entries`), at most 1 s and only after a miss.
* Smoke test `gfs-fuse/tests/fsmonitor.rs::gfs_switch_leaves_local_commits_on_their_branch`.

**Step 4 — steer users, and measure the stock path** (complete)
* A `post-checkout` hint, local mode only: `.git/hooks/gfs-post-checkout`,
  registered as a config hook (`hook.gfs-switch-hint.event/command` in
  `.git/gfs/config`). After a branch checkout that moved `HEAD` to another
  commit and wrote 1 000 files or more (counted with `git diff-tree`, 2.7 s
  for 161k, 0.24 s for a small one), one line on stderr: the count and the
  `gfs switch` to use. It cannot know the checkout's time, so it does not
  claim one.
* `checkout.workers = 8` seeded: 93 → 77 s for a 161k-file stock switch; 32
  was no faster (78.7 s).
* Docs: README (local mode: switch with `gfs switch`), `docs/performance.md`
  (a section with the numbers and where a re-pin's time goes), the manual
  test guide (what to check by hand).
* Every re-pin logs one `re-pinned` line with its phases; the host's log now
  goes to stderr, i.e. `host.log` (it went to stdout, `/dev/null` for a
  spawned host, so `host.log` had always been empty).

**Follow-ups (complete)** -- profiled on universe, a stock switch of 161k
files and `gfs switch` back after it:
* Stock checkout 88-93 s → 53 s. `perf` on the daemon: 44% of its CPU was
  `Overlay::remove` scanning every row for each `rmdir` (and the rename
  paths did the same for every rename). Now a range of the sorted row map
  (`rows_under`). `Status` checked each directory whiteout against every
  row (quadratic); now a binary search. The inode table walked, with an
  allocation per entry, every name it had ever numbered on every rename --
  including each lockfile Git renames in `.git` -- and now walks only for a
  directory. What is left is per-operation: ~130 us of daemon CPU per
  unlink of a base file (a whiteout row, the parent's times, the journal
  transaction), spread over allocation, SQLite and lookups with no hotspot.
* `gfs switch` back 19.3 s → 10.5 s. The planner logs `planned the carry`
  with its phases. Hashing the stock checkout's 82k copies runs on up to 16
  threads (5.7 → 0.4 s); tree lookups are split across the repository's 8
  handles and run concurrently with the pin-to-`HEAD` diff (6.8 → 4.2 s);
  changed paths that have a row skip the overlay lookup; kernel
  invalidation runs on 8 threads (4.1 → 1.5 us per name); and the dropped
  copies (2.2 GB) are deleted by a background thread after the rebind
  instead of under the overlay lock (seed and rebind 3.2 → 1.1 s).

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

* **3b done directly, not by a subagent.** Steps 1, 2 and 3a each had their
  subagent version reverted and rewritten, so the user was asked; 3b was
  implemented and verified in the main session.
* **Carry by Git's rule, decided per overlay row, not per changed path.** The
  row already records the base it diverged from, so "same in both commits"
  is one batched lookup of each row's path in the target (plus directories
  and rename sources), not a tree diff of two 1.3M-file commits. Rows that
  only moved times (what a stock checkout back leaves, step 2) are dropped
  where the target differs rather than blocking the switch, which is what
  Git does with an unmodified file. Alternatives: a tree diff between the
  commits intersected with the rows (cost scales with the diff, 122k paths
  between master and universe-goofys-grpc), or refusing any dirty overlay
  (3a's behaviour).
* **Staged changes are refused, not carried.** Carrying them means writing
  the target's index with the staged entries merged in -- Git's three-way
  index merge. Refusing costs 0.37 s on universe and loses nothing.
* **Missing directories are made rather than refused.** Git keeps a
  directory that holds untracked files when the target drops it; the first
  version refused, and the stress run hit it immediately (untracked files
  under directories only master has). Made and orphaned directories are
  opaque, as `mkdir` makes them: a non-opaque directory with no base asks the
  target for a listing it does not have, `readdir` gets `ENOENT`, and glibc
  reports an empty directory.
* **3c: local commits are copied into the clone, not read beside it.** The
  alternative was adding the workspace's object directory to the clone's
  in-process libgit2 odb. It would leave the clone's disk untouched, but the
  lease anchor (`refs/gfs/mounts/<id>`) would then name an object the clone
  lacks, and the clone's own `git gc`/`fsck` fail on such a ref. Copying is
  what `git push` into the clone would do: additive, content-addressed, kept
  alive by the anchor while pinned and pruned by the clone's `gc` after.
  Everything in the workspace's own `objects` is copied, not only what is
  reachable from its commits: they are loose (`gc.auto=0`) and few, and a
  reachability walk would need the clone's tree for every edge.
* **3c: edits are judged against `HEAD`, the pin is left as it is.** Git's
  rule is about `HEAD`, not the commit gfs happens to serve, so a committed
  edit is clean. An explicit "adopt `HEAD`" re-pin after each commit was
  considered (a `post-commit` hook): it would keep the pin at `HEAD`, but cost
  an index build and move every base file's mtime on every commit, which a
  build tool reads as "everything changed". Deferring to the next switch pays
  once, when the view is moving anyway.

* **Local-mode only.** `switch_to` (server mode) still refuses a dirty
  workspace; server mode has no `.git` of its own to re-seed per branch.

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

* **Step 3b results, universe** (2026-09-30; the clone's master had moved to
  `02c5a7e`, 122k files from universe-goofys-grpc; fresh private workspace;
  every status compared with the uncached one, all identical):

  | | time | result |
  |---|---|---|
  | refused: `.bazelrc` edited, differs on target | 0.42 s | names the path, `HEAD` unmoved |
  | refused: `README.md` staged | 0.37 s | names the path |
  | switch carrying an edit, a deletion, 3 untracked files | 2.47 s | 6 rows kept; first status 3.7 s, then 2.2 s; 100 switched files hash to `HEAD` |
  | switch with 60k touched rows (30k same on both, 30k not) + edits + an untracked file 4 levels under directories the target lacks | 5.0 s | 30 006 rows kept, the 30k differing dropped (sampled: 43 hash to the target, 57 gone); status 3.8 s, then 2.2 s |
  | back, same rows, no full walk before it | 2.55 s | status matches |
  | repeated switches, few rows | 1.6-1.8 s | |

* **Step 3c results, universe** (2026-09-30, master at `6e49f8e`; fresh
  private workspace; `gfs switch -c gfs-3c-probe`, a stock `git commit` of a
  `.bazelrc` edit and a new two-level directory, then an uncommitted
  `README.md` edit and `docs/OWNERS` deletion; every status matched the
  uncached one):

  | | time | result |
  |---|---|---|
  | `git commit` (with the user's global Databricks hooks) | 3.2 s | |
  | to universe-goofys-grpc | 4.75 s | 2 rows kept; `.bazelrc` is the target's, the new directory gone; status 3.6 s, then 1.9 s |
  | back to the probe branch (its local commit pinned, index built) | 12.6 s, after an uncached walk | `.bazelrc` and the directory are the commit's; status 4.8 s, then 2.1 s |
  | repeated | 1.3-2.1 s | |
  | a switch straight after another commit | 2.0 s | cache-tree settles "nothing staged" |
  | refused: staged `README.md` | 0.38 s | |

  The clone gained 13 loose objects (the probe commits); its refs were
  unchanged afterwards and nothing names the objects, so its `gc` prunes
  them.

* **A switch right after a full-tree walk is slow**: 9.7-11 s instead of
  1.6-5 s, each time it followed an uncached `git status` (71-74 s, which
  looks up every one of the 1.33M paths). The first guess -- invalidating
  every cached name -- was wrong: the `re-pinned` log shows 1.37 s for 255k
  names (4-5 us each), while *resolving* the pin took 6.3 s instead of
  0.16-0.27 s. After waiting 20 s it took 0.24 s, with no prefetch running.
  The box had 0 GB free (104 of 123 GB used); the walk's ~1.8M cached
  dentries and inodes push the clone's 752 MB pack index and 14 MB
  `packed-refs` out of the page cache, and libgit2 reads them back cold.
  Not a gfs cost to fix; recorded in `docs/performance.md`.
* **The host's memory after that walk**: 9.4 GB RSS, of which 6.9 GB is
  file-backed (the clone's packs, mapped by libgit2, reclaimable) and
  2.5 GB anonymous (per-path state for ~1.8M names the kernel looked up).
  The 9.9 GB seen on the user's long-running daemon is the same shape.
* Tests: `gfs-overlay`, `gfs-git`, `gfs-mount`, `gfs-fuse`, `gfs-cli` whole
  (`--no-fail-fast`): only the known `mutations::a_recreated_directory_does_not_show_the_base_children_it_replaced`
  and `prefetch::reading_a_directory_through_fetches_the_rest_of_it` fail.
  3a had left `history` and `lifecycle` not compiling and
  `workspace_git::a_bare_push_never_fans_out_to_branches_the_caller_is_not_on`
  failing (reading only `.git/config`); fixed in ec13b50 and 990196a. 3a's
  "whole suites pass" above was wrong.
* 3c tests: the same suites whole; failing are the known `mutations`,
  `prefetch` and `overlay` ones and `faults::a_deleted_base_directory_recreated_and_refilled_stays_consistent`,
  which also fails on main (2 of 2 runs) and passed 1 of 2 with 3c.

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
