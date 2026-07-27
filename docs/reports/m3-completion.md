# M3 — Writable overlay and export: completion report

Date: 2026-07-26  
Milestone: M3 (PLAN.md section 6)  
Status: **Complete**, with two recorded gaps carried into M6.1.

M2 delivered a lazy read-only mount of one pinned commit. Every mutation
answered `EROFS`, `xvfs status` did not exist, and the `git` shim reported a
clean tree because there was nothing that could make it dirty. M3 makes the
workspace writable: a crash-safe copy-on-write overlay, every FUSE mutation
wired to it, status and diff derived from the journal without scanning the base,
an export that a downstream applier can actually apply, and fault injection that
kills the process at each transaction boundary to prove nothing acknowledged is
lost.

## The exit gate

PLAN.md section 6 states three criteria.

| # | Criterion | Verified by | Result |
| --- | --- | --- | --- |
| 1 | Random mutation sequences match the reference in-memory filesystem model | `xvfs-overlay/tests/state_machine.rs` | **Met** — 64 seeds × 200 operations, compared on outcome *and* full merged tree after every step |
| 2 | An export applied to the pinned Git commit produces the same tree as the mounted workspace | `xvfs-fuse/tests/export.rs::an_export_applied_to_the_pinned_commit_reproduces_the_workspace_tree` | **Met** — stock `git apply` onto a stock-Git checkout, trees compared with the M2 raw-tree materializer |
| 3 | Fault injection meets the no-lost-acknowledged-mutation goal | `xvfs-test/tests/overlay_crash.rs` | **Met** — 5 boundaries × 3 mutation shapes, plus rename |

### What criterion 3 actually guarantees

The goal needs stating precisely or it cannot be tested, so the journal fixes
two failure models and gives them different guarantees:

- **the daemon dies** (`kill -9`, panic, OOM): every committed transaction
  survives. Every mutation the filesystem acknowledged is recoverable.
- **the host dies** (power loss): recent commits may be lost unless something
  forced them out, which is what `fsync(2)` on a mounted file does.

`synchronous = NORMAL` is what buys the first without paying an fsync per
metadata operation. The catalog uses `FULL` because a lease that survives the
process but not the machine lets `gc` prune a live mount's objects; the
overlay's exposure is one job's un-fsynced edits, and POSIX already says those
need an `fsync` to be durable.

The crash harness aborts rather than panicking. A panic unwinds, and unwinding
runs `Drop`: the SQLite connection closes cleanly, the staged file removes
itself, and the recovery path under test never sees the state a real crash
leaves.

### Measured

Machine: the M0.1 profile (WSL2, Linux 6.18.33.2, 32 logical CPUs, 46 GiB RAM),
debug build, server and client in one process over loopback.

| Measurement | Result |
| --- | ---: |
| `status` over a 5002-entry directory with 2 changes | **0** metadata requests, **0** directory pages, **0** blob bytes |
| `mv src source` of a base directory (3 descendants) | **0** blob bytes transferred |
| `chmod +x` on a 12 MiB base blob | **0** blob bytes, **0** local bytes |
| `O_TRUNC` over a 12 MiB base blob | **0** blob bytes |
| `diff` of one edited 20-byte file in a 16 MiB snapshot | **< 1 KiB** fetched |
| Random-sequence agreement with the model | 64 seeds × 200 ops, **0** divergences |

The zeros in rows one to four are the milestone's point. A workspace is only
cheaper than a clone if the operations an agent performs constantly — `status`,
`mv`, `chmod`, replacing a file — do not quietly hydrate the tree.

`status`'s zero is the one that generalizes: ADR 0005 measured stock `git status`
against a partial clone of the Linux kernel stat'ing all 94 850 index entries,
which inside a mount is a full metadata sweep of the monorepo per invocation.

## Findings that changed something

Twelve, grouped by what they were about.

### The three that would have shipped a broken feature

**1. The mount was still mounted `MountOption::RO`.** M2 set it because the base
is immutable and there was no overlay, so the kernel refused every mutation
before it became an upcall. With it left on, the entire M3.2 write path could
have been written, reviewed, and merged without one syscall ever reaching it.
Found because a hard-link test expected `EPERM` and got `EROFS`.

**2. The kernel does not look a path up again after `rename(2)`.** It relinks the
dentry it already has, so the destination arrives carrying the *source's* inode
number and the record behind it still describes the source. A renamed directory
therefore listed empty: `opendir` answered out of the old path's record. The
inode table now re-paths the moved subtree and refreshes what each record is
backed by.

**3. `shutdown()` could block forever.** A plain unmount of a filesystem with an
open descriptor fails with `EBUSY`, and `fuser`'s `umount_and_join` then waits
for a session thread that will not exit — so job cleanup hung for as long as one
process held one file. Teardown is now bounded and falls back to a lazy unmount,
which is the semantics a teardown wants anyway: unreachable now, released when
the last reader finishes.

### The four about the merge being subtly wrong

**4. `readdir` cannot decide what to append by asking whether a row records base
facts.** After a rename a row still remembers the base of the path it came
*from*, so that test made a moved file invisible — or, with the opposite
polarity, listed it twice. Extras are now keyed on the names the base listing
actually produced.

**5. A directory arriving by rename must become opaque** even when the row being
moved was an *adopted* base directory, which was not opaque where it came from.
Otherwise the destination's base children show through the moved subtree.

**6. `materialize` and `adopt` have to resolve rather than look up.** A whiteout
is a row, so looking one up found it and fell through to the base facts, and a
copy-up resurrected a file the workspace had deleted.

**7. A moved base file must be fetched from the path the base still has.**
Asking the server for the new name returns an honest `ENOENT` for a file the
workspace can plainly see.

### The three about export being merely plausible

**8. A rename is a whiteout *and* a present row**, and reporting both told
`git apply` to rename a file and then delete what it had renamed. It refuses the
pair outright: `path ... has been renamed/deleted`.

**9. A deletion's new side is nothing**, not an echo of the old side. Echoing it
produced a `deleted file mode` header with no hunk, which `git apply` rejects as
"removal patch leaves file contents".

**10. Myers' backtrack has to special-case `d == 0`.** The general step computes
`previous_y == -1`, which as a `usize` is enormous and silently stops the
diagonal walk — so every hunk lost its leading context line. The patches still
*looked* right and `git apply` still accepted many of them.

### The two about the oracle and the boundary

**11. The verifier has to run stock Git hermetically.** A developer with
`core.autocrlf = true` in their global config makes `git apply` rewrite every
line ending on the way in, and the verifier then reports the export as wrong when
the only thing wrong is the oracle's environment. This is the same class of
finding as M2's: an oracle that is not isolated blames the thing under test.

**12. `statfs` reported the *path* limit as `f_namemax`**, so the mount claimed
4096-byte file names were fine. The kernel does not enforce `NAME_MAX` for FUSE
(its own cap is 1024) and `f_namemax` is advisory, so a job could create a
300-byte name and produce a tree no ordinary filesystem could check out — with
the failure landing on someone else's machine rather than at the write that
caused it. Creating one is now `ENAMETOOLONG`; reading a base path that already
has one still works, because refusing to read what the commit contains would be
worse.

## Decisions worth carrying forward

**The state machine is three facts, not six states.** DESIGN.md section 6.4 names
six — base, copied-up, created, deleted, renamed, type-changed — and modelling
them as six flags is how an overlay ends up unable to express "renamed *and* then
modified". A row is `present?`, `content` (`Base(oid)` / `Local(id)` / none), and
`base` (what the pinned commit has here); the six named states fall out.

**`Content::Base` is what keeps metadata changes free.** A `chmod +x` or an `mv`
of a 100 MiB file changes no bytes. A row whose content still points at the
pinned commit's blob is metadata diverged, content untouched — and it is why
rows 2 and 3 of the measurement table are zero.

**Content is published before the journal row that names it.** Write the
temporary, fsync it, rename it in, fsync the directory, then commit. The
invariant is one-directional — *a committed row's content always exists; an
unreferenced content file is garbage* — so recovery is a sweep rather than a
repair.

**Whiteouts are never dropped as redundant.** `rm -rf src/` unlinks each child
before removing the directory, so the per-file record already exists; deleting it
on `rmdir` as "covered by the directory whiteout" would have forced status and
export to walk the base subtree to find out what to delete. Keeping it is what
lets status be journal-only for the cases that actually occur.

**Status hashes local content rather than trusting the journal.** A file written
and written back is reported as unchanged. Only changed paths are hashed, so the
cost is the edit set, not the tree — and without it every `git status` after an
aborted edit carries a phantom entry.

**A directory rename materializes metadata eagerly.** One row per base
descendant, all still `Content::Base`. The alternative — an overlayfs-style
redirect — is a second resolution mechanism to get wrong. The cost is metadata
proportional to the subtree and no blob content, bounded by
`max_rename_entries`.

**An open descriptor is the authority for a write, not a path.** Writes go
through the content id, which is what makes an open file survive `rename` and
keep working after `unlink`. The unlinked case relies on the kernel keeping the
inode alive behind the descriptor, so no reference counting was needed on top.

**The `git` shim now talks to the daemon, and still carries no credential.** The
control socket is local and mode 0600; nothing that crosses it is a repository
token. A shim that called the *server* would need the mount capability, and
putting one in a `PATH`-installed wrapper any process can invoke is a worse
trade.

## Recorded gaps

### Directory-level deletions can still need expansion

`Status::directory_deletions` reports a whiteout on a directory whose per-file
whiteouts are not in the journal. Every mutation path that occurs in practice
leaves the per-file record — the kernel unlinks children before `rmdir`, and a
directory rename materializes its descendants — so the list is empty for an
ordinary edit set, and the exit-criterion test asserts that. It is *reported*
rather than silently omitted, and the export manifest carries it under
`unexpanded_directory_deletions`, so a consumer knows the patch is not the whole
story. Expanding it needs base access, which the overlay deliberately does not
have; the caller can. Nothing in M3 requires it.

### `pjdfstest` and xfstests are still not run

M2 recorded this and it has not changed: neither suite is installed here nor
packaged as a Rust dependency. `tests/compat.rs` now covers a hand-written
writable subset — `EEXIST`, `ENOTDIR`, `EISDIR`, `ENOTEMPTY`, `ENOENT`,
`ENAMETOOLONG`, `EPERM`, and the five conditions POSIX names for `rename` — on
top of M2's read-only subset. It should close before M6.1.

### `RENAME_EXCHANGE` and `RENAME_WHITEOUT` are `EINVAL`

The journal could express an atomic exchange; nothing in the pilot's tooling
uses one. `EINVAL` is what a filesystem returns for a `renameat2` flag it does
not implement, and callers already handle it.

## What M3 deliberately did not build

Commit and push. The export is the handoff, and M8 owns the direct-write path.
Three-way refresh: `xvfs refresh` refuses a non-empty overlay, because an overlay
is bound to the commit it diverged from and carrying edits across would make
every subsequent `status` be about a base that is no longer mounted.

Permission bits other than `+x`. Git records exactly one, and an export could not
reproduce the rest, so storing them would report a mode nothing downstream could
honour.

`GIT binary patch` encoding. A binary change is recorded in the patch as a
`Binary files ... differ` line and carried byte-exactly in the bundle's
`content/` files. Producing a delta or zlib literal that `git apply` accepts is
separate work with no consumer yet.

## Test inventory

| Suite | Cases | Covers |
| --- | ---: | --- |
| `xvfs-overlay` unit tests | 28 | row encoding, resolution and masking, the content store's ordering and sweep, Git blob hashing, Myers hunks and quoting, the fault-point table |
| `xvfs-overlay/tests/overlay.rs` | 14 | restart, copy-up laziness, quota short-writes, orphan collection, binding and schema refusal, the overlay clock, rename bounds |
| `xvfs-overlay/tests/state_machine.rs` | 2 | 64 seeds × 200 operations against the reference model, plus the same across reopens |
| `xvfs-test/tests/overlay_crash.rs` | 5 | 5 transaction boundaries × 3 mutation shapes, rename atomicity, orphan collection, recovery idempotence |
| `xvfs-fuse/tests/mutations.rs` | 20 | the whole mutation surface through real syscalls: copy-up, `O_TRUNC`, whiteouts, opacity, rename identity, open-file semantics, quota, `statfs`, paged readdir merge |
| `xvfs-fuse/tests/export.rs` | 9 | status shapes, undone edits, diff cost, bundle atomicity and checksums, the apply-and-compare verifier, the shim against the journal |
| `xvfs-fuse/tests/faults.rs` | 10 | quota exhaustion, server loss, 16 and 8 concurrent writers, rename cycles, repeated directory replacement, unwritable overlay, cache churn, unmount with open handles, daemon restart |
| `xvfs-fuse/tests/compat.rs` | 22 | M2's read-only subset plus the writable POSIX errno matrix and `rename`'s refusals |
| `xvfs-fuse` others | 51 unit + 66 integration | M2's suites, still green |

`scripts/dev-stack.sh` now demonstrates the writable path end to end: edit,
delete, rename, `xvfs status`, `xvfs diff`, the shim's porcelain output, an
export bundle, a refresh refused for a dirty workspace, a daemon restart that
resumes the job's edits, and a clean refresh afterwards.
