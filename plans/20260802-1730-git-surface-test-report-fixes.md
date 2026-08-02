# Git surface: the test report's three findings

## Summary

A live agent session against `~/.gfs-lab/flask` reported three problems with the
`git` surface inside a workspace. Reproduced and diagnosed here:

1. **Phantom untracked files.** `echo x > f.txt; git status; rm f.txt; git status`
   still prints `?? f.txt`, forever. Two independent causes, both real:
   * the **fsmonitor hook forgets**. A file created and then deleted leaves *no*
     journal row (`Overlay::remove` deletes the row outright when the base has
     nothing at the path), so `status()` no longer mentions it and the hook's
     cumulative answer drops it. Git's untracked cache, once `use_fsmonitor` is
     set, ignores directory stat data entirely and trusts its cached extent
     until fsmonitor names a path inside it — so the extent is never
     invalidated. Worse than the phantom: `git add f.txt && rm f.txt` reports
     `A  f.txt` (clean) where stock Git reports `AD f.txt`, because the index
     entry keeps `CE_FSMONITOR_VALID` and Git skips the `lstat`.
   * the **mount root's mtime is frozen**. `Gfs::touch_parent` skips the root
     (`parent.path.is_empty()`), so a create or delete at the workspace root
     never advances the root directory's timestamps. With `core.fsmonitor=`
     unset the untracked cache falls back to directory stat, and at the root it
     never invalidates. Confirmed by `stat`: `src` advances on create *and*
     delete, `.` never does.
2. **No tags, no remote-tracking refs.** The seed writes `HEAD` and one branch
   ref and nothing else, so `git describe`, `git rev-parse origin/main` and
   `git status -sb`'s ahead/behind have nothing to work with, even though the
   gateway advertises the full set.
3. **fsmonitor "always answers `/`".** Not reproduced: the reporter invoked the
   hook by hand with a token (`0`) the daemon never issued, which is exactly the
   case the protocol requires a full-rescan answer for. `GIT_TRACE2_EVENT` shows
   Git passing `gfs:1` and getting a 6-byte non-trivial answer in the steady
   state. The one true observation is that the token never advances, which the
   v2 protocol asks it to.

## Plan

**Phase 1 — the overlay remembers what it forgot** (`crates/gfs-overlay`)
* Journal schema v2: a `vanished` table plus a `vanished_overflow` meta flag.
  Every `Change::Delete` that is not re-`Put` in the same transaction records
  the path; a `Put` clears it. Capped (`VANISHED_LIMIT`); past the cap the flag
  is set, the table cleared, and the answer degrades to "rescan everything".
* Root directory timestamps as `root_mtime`/`root_ctime` meta keys, moved by a
  new `Overlay::touch_root`, read by `Overlay::root_times`. The root cannot have
  a journal row (the empty path is every ancestor walk's terminator), which is
  why this is meta rather than an entry.
* Both are cleared by `rebind`: a new generation forces one full rescan anyway.

**Phase 2 — the mount reports it** (`crates/gfs-mount`)
* `fsmonitor_changes` appends the vanished paths and ORs the overflow flag into
  `full_rescan`.
* The token becomes `gfs:<generation>:<sequence>`, where the sequence is the
  overlay's committed-change counter. Full rescan is still decided by the
  *generation* alone — the answer stays cumulative per generation, which is a
  safe superset of "changes since any token in it" — but the token now advances
  when the filesystem changes, as the v2 protocol says it should.
* `touch_parent` stops skipping the root; `Gfs::attr` overlays the root's
  recorded times onto the root inode's attributes.

**Phase 3 — the refs exist** (`gfs-proto`, `gfs-git`, `gfs-service`, `gfs-mount`)
* `SnapshotService.ListRefs`: repository-scoped like `ResolveRevision` (refs are
  exactly what `git ls-remote` shows the same token), returning name, target,
  and the peeled commit for annotated tags. Backed by a new
  `GitRepository::visible_ref_targets`, with `visible_refs` reduced to a default
  method over it so the reserved-namespace filter has one home.
* The daemon calls it once at pin time, best-effort like `get_commit`, and the
  seed writes `packed-refs`: `refs/tags/*` verbatim (with `^peeled` lines and
  the `fully-peeled` trait), `refs/heads/*` mapped to `refs/remotes/origin/*`.
  Local branches are never packed — they are the agent's, and the loose file is
  the only copy.
* The seed also writes `branch.<name>.remote`/`.merge` for the pinned branch, so
  `git status -sb` prints `## main...origin/main` with ahead/behind.

**Phase 4 — docs and verification.** README, ADR 0009, `docs/manual-test.md`;
build and `cargo test --workspace --all-features`.

Built as planned. Verification is four new tests, each of which was checked to
fail against the unfixed code:

| test | file | what it pins |
| --- | --- | --- |
| `a_created_then_deleted_file_does_not_haunt_status` | `gfs-fuse/tests/fsmonitor.rs` | the reported repro, at the root and one level down |
| `a_staged_file_that_is_then_deleted_is_reported_as_deleted` | same | `AD`, not a bare `A ` — the severe half |
| `the_token_advances_when_the_workspace_changes` | same | the v2 token contract, and that an alien token still rescans |
| `the_mount_root_is_a_directory_like_any_other_for_timestamps` | `gfs-fuse/tests/compat.rs` | the root's mtime, the non-fsmonitor half of the same bug |
| `tags_and_remote_tracking_refs_are_materialized_at_mount` | `crates/gfs-mount/tests/workspace_git.rs` | tags (lightweight, annotated, tree-peeled), `origin/*`, `-sb` upstream, and that `refs/gfs/` never appears |

880 tests pass. Live re-verification against `~/.gfs-lab/flask` needs the dev
stack restarted onto the new binaries (the `gfs-fuse` host serves from the
image it started with), which is the user's call — the workspace holds a local
test commit.

## Decisions

* **Cumulative fsmonitor answer, kept.** A per-path change sequence would let the
  hook answer "since token" exactly, but the journal has no per-row sequence and
  adding one touches every mutation path. The cumulative answer is a superset,
  and a superset is always safe — Git re-checks what it is told and trusts only
  the unlisted. The token now advances so the protocol contract holds; the
  answer's scope is documented as per-generation.
* **Vanished paths are capped, not unbounded.** A build that creates and deletes
  a million temporaries would otherwise grow the set without limit. Past the cap
  the hook says "rescan", which is slow and correct rather than fast and wrong.
* **The root's times live in meta, not in a row.** Giving the empty path a
  journal row would create a second spelling of the root for every resolver that
  walks ancestors — the reason `touch_parent` skipped it in the first place. Meta
  keys are the same durability with none of that.
* **`refs/heads/*` is not packed.** Writing upstream branches into `refs/heads/`
  would collide with the agent's own branches and resurrect ones Git deleted.
  Remote-tracking is what a clone would have produced, and it is what `git
  describe`, `git log origin/main`, and `@{upstream}` actually want.
* **No new authorization surface for `ListRefs`.** ADR 0002 already concludes
  that repository read access implies what the Git gateway advertises; this RPC
  serves the same filtered set through gRPC, reusing `visible_refs`'s
  reserved-namespace exclusion rather than inventing a second policy.

## Details

* Reproductions live in the report; the diagnostic that settled the untracked
  cache question is `valid_cached_dir` in Git's `dir.c` — with `use_fsmonitor`
  set it never `lstat`s the directory, so no amount of correct directory mtime
  can fix a hook that does not name the path.
* `git -c core.untrackedCache=false status` remains the reporter's escape hatch
  and now should never be needed.
* The `packed-refs` file is rewritten on every seed, so a ref deleted locally
  comes back on the next repin. That is the same "pinned ref view" contract the
  branch ref already has.
