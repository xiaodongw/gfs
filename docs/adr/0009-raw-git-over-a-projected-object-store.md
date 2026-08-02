# ADR 0009: Raw Git over a projected object store

- Status: Accepted
- Date: 2026-07-29
- Milestone: M6.2 (budget) then M9 (projection)
- Supersedes: [ADR 0005](0005-git-command-surface.md) — the synthesized `.git`
  surface and the frozen `git` shim grammar
- Amends: DESIGN.md sections 8.3, 8.4, 8.6; ADR 0006's closed revision grammar
- Evidence: `spikes/git-projection`,
  [`spikes/reports/m05b-git-projection.md`](../../spikes/reports/m05b-git-projection.md)

## Context

ADR 0005 chose a synthesized, object-free `.git` plus a `git` shim, and every
history question then became a server-side tool: `gfs log`, `gfs find`,
`gfs show`, `gfs diff`, `gfs blame`. The ADR was amended twice in four days, each
amendment moving one more question to the server. That is the shape of a
treadmill rather than a boundary: the shim's grammar has to grow for every tool
that exists or will exist, and an agent late in a long session reverts to its
strongest prior, which is `git`.

ADR 0005's deciding measurement was that a real `.git` turns `git status` into a
full metadata sweep — 94 850 first-time FUSE lookups on the Linux kernel. It was
taken with Git's default settings, and `spikes/reports/m05-git-surface.md` records
the gap:

> `core.untrackedCache` and `core.fsmonitor` can reduce the sweep substantially
> and were not evaluated; **neither is available through a FUSE mount without
> further work, which is why they do not change the decision.**

`core.fsmonitor` is a hook program. GFS's overlay journal is exactly the
modified-path set it asks for, and a better source than watchman because the FUSE
server sees every mutation by construction. The second clause was untested and is
wrong.

## Decision

**Project the gateway's real object database into the mount and let stock Git
answer for itself, bounded by a hydration budget enforced at the filesystem.**

No forked Git. Every mechanism below is a documented Git feature.

### The workspace shape

```text
<workspace>/          FUSE mount: working tree projected from the pinned commit
  .git                a FILE: "gitdir: <state>/git"
  src/...
<state>/              plain local disk, per mount
  git/
    HEAD  config  refs/  index          writable, local, fast
    objects/
      info/alternates  -> <mount>/objects
      <loose objects the agent creates>
  overlay.sqlite  files/
```

This is `git worktree`'s own split — per-worktree state (`HEAD`, `index`, `logs/`)
small and writable, the object store shared and read-only — reached through
`objects/info/alternates`, which is what `git clone --shared` uses. Because git's
writes land on local disk rather than in the overlay, the FUSE layer never has to
be correct for lockfiles, `O_EXCL`, or rename-over-existing inside `.git`.

**`refs/` and `index` are per mount, not shared.** A real linked worktree shares
`refs/heads/` through `commondir`; GFS must not, or every agent sees every other
agent's branches and two agents on one branch collide. Each mount gets a local
`refs/` seeded from a **pinned ref view** captured at mount time. Live-projecting
`refs/heads/main` would let it move under a workspace whose index still describes
the old commit, and Git would report the entire repository as modified.

That view is the *whole* filtered ref set, not just the pinned branch. The
daemon calls `ListRefs` once per pin — repository-scoped, the same
reserved-namespace filter the gateway advertisement uses — and writes
`packed-refs`: tags verbatim with their peel lines, branches mapped to
`refs/remotes/origin/*`, everything else left out because the seeded fetch
refspec cannot refresh it. Upstream branches never land in `refs/heads/`, which
belongs to the agent. Seeding only the pinned branch, as the first
implementation did, left `git describe` answering "No names found",
`origin/main` an unknown revision, and `git status -sb` with no upstream to
count against — three messages that read as a corrupt repository rather than as
a surface nobody had materialized.

### What the gateway must ship, per commit

| artifact | why |
| --- | --- |
| index with `snapshot_time` stat data | `core.checkStat=minimal` compares mode, size and whole-second mtime. DESIGN.md section 8.2's deterministic `snapshot_time` makes one index valid on every host that mounts the commit |
| `commit-graph` with `--changed-paths` | answers graph traversal and `log -- <path>` without reading commit or tree objects |

### The required configuration

Non-negotiable, and set by the daemon in the projected config:

```text
core.checkStat = minimal        core.fsmonitor = <journal-backed hook>
core.trustctime = false         core.untrackedCache = true
gc.auto = 0                     maintenance.auto = false
```

The first two stop Git re-hashing the tree; the next two stop it sweeping the
tree; the last two stop routine housekeeping from downloading the object
database. **None of them is enforceable** — `git -c core.checkStat=default status`
overrides them and costs 1 615 MiB. That is why the budget below is a
prerequisite rather than a companion.

### The hydration budget is mandatory

DESIGN.md section 8.4 makes hard budgets opt-in. **They become mandatory and
default-on**, because they are the only enforcement the projection has:

- **`EDQUOT`**, chosen for its `strerror`. Every tool prints "Disk quota
  exceeded" with the path; `EIO` would read as a corrupt filesystem and send an
  agent looking for the wrong problem.
- **Refuse at `open` and `opendir`**, not at `read`. Refusing reads makes
  `grep -r` emit one error per file and keep walking — thousands of identical
  lines into an agent's context.
- **Count unique blocks, not total fetches**, so a re-read of an evicted block is
  free. Monotonic counting plus eviction makes a well-behaved job die on
  `EDQUOT`.
- **Separate counters for the working tree and the object store.** Object-store
  traffic is bounded by the command; tree traffic is bounded only by repository
  size. One counter lets a legitimate `blame` exhaust the budget and then fail
  ordinary file reads.

Byte counting is what discriminates, and it does so without a deny list: measured,
`git status` with fsmonitor never touches the budget (zero tree reads) while
`grep -r` trips it inside 2 MB.

### Two budgets, one at a time

| mode | behaviour | default |
| --- | --- | --- |
| hydration budget | bytes fetched per job; `EDQUOT` at the limit | **yes** |
| cache residency budget | bytes held; evict and re-fetch, never refuse | opt-in |

The second exists so a small disk can work a monorepo slowly rather than not at
all. They are one mechanism with two policies over the same block cache and are
mutually exclusive: a monotonic limit over an evicting cache is a job that dies
for behaving.

Cache mode's risk is not a stall — every fetch serves its read — but an unbounded
re-fetch multiplier, worst when a cyclic scan exceeds the cache under LRU. That
presents as a slow job rather than an error, and to an agent with a timeout a slow
job is indistinguishable from a hang. So cache mode reports **re-fetched over
unique bytes**, and the cache is **scan-resistant**: a `blame` streaming 590 MiB
of pack must not flush the source blocks a build is using.

### 64 KiB blocks, fetched lazily

Pinning 632 MiB of pack index and commit-graph per node before the first command
would trade away the 0.211 s mount. Fetch lazily instead, in **64 KiB blocks** —
measured, not chosen:

| chunk size | `log --oneline -20` (0.23 MiB read) | `blame` (196 MiB read) |
| --- | ---: | ---: |
| 64 KiB | 0.3 MiB | 274 MiB |
| 1 MiB | 2 MiB | 1 004 MiB |
| **8 MiB** | **16 MiB** | **2 320 MiB** |
| 32 MiB | 64 MiB | 3 648 MiB |

Locating an object is a binary search and reconstructing one walks a delta chain,
so access is sparse and random — exactly what large chunks punish, and worst on
the commands that are otherwise cheapest. 64 KiB costs 1.0–1.4×, and that residual
is the kernel's own readahead. It also matches FUSE's 128 KiB maximum read, so one
block per request with nothing to tune, and "lazy chunked fetch" becomes "serve
each read and cache the block".

**Key blocks by `(pack name, offset)`.** A pack's filename is its own checksum, so
those bytes are fixed forever and a cached block can never be stale. Keying by
"the repository's current pack" would make staleness a discipline instead of an
impossibility.

### What survives, what goes

| | |
| --- | --- |
| **Deleted** | the `git` shim's frozen grammar; `gfs find` + `FindPaths`; `gfs log` + `Log`; ADR 0006's closed revision grammar; the overlay export/apply commit path |
| **Kept** | `gfs rg` — search is not a Git command and `rg` reads every file by nature |
| **Kept** | `gfs show`, `gfs diff <a> <b>`, `gfs blame` — measured 91.5 MiB and 196 MiB of pack data on linux |
| **Kept** | `gfs clone`, `gfs mount`, `gfs switch` — re-pinning is free where `git checkout` to a distant commit materializes the whole diff |
| **New** | `git push` through the M5 gateway replaces export/apply, and `git rebase` resolves the two-agents-on-one-branch conflict the overlay could not |

`ls-files` costs zero projection traffic against a real local index, which is why
`gfs find` goes. `gfs show` and `gfs blame` stay because a tree comparison on a
94 851-file repository moves a lot of bytes wherever it runs, and only the gateway
already has them.

### The shim becomes a hint layer, not a grammar

With a real `.git`, `ls-files` and `diff` no longer lie, so the shim is no longer
load-bearing for correctness — it only routes cost. It is therefore a **fixed
short list with a default of pass-through**, and any pressure to grow it into a
grammar is a signal that this ADR is repeating ADR 0005's mistake.

| | |
| --- | --- |
| route to `gfs` | `blame`, `show --stat`, `log -- <path>`, `checkout`/`switch` to a distant commit |
| fail | `gc`, `repack`, `fsck`, `maintenance`, `clone` from the mount, `log --all --graph` |
| pass | everything else |

`grep` and `find` **degrade, they do not fail**: a bounded pathspec passes, an
unbounded recursive sweep is redirected. Failing them outright breaks
`./configure`, Makefiles and linters, and the failure looks like a GFS bug. The
budget is the boundary, so these wrappers are tuned permissively.

### Pack retention is a policy, not a subsystem

A local worktree needs no retention because POSIX provides it: unlinking a pack
another process has open leaves the inode alive. A mount holds no descriptor on
the gateway's file, so that guarantee must be restated — but a pack name is a
content hash, so a stale block can only be **missing, never wrong**. This is an
availability concern, not an integrity one, and it needs three things:

- the gateway does not prune packs while a lease predating the repack is live —
  `gc.pruneExpire` derived from the lease TTL rather than Git's two weeks;
- `ESTALE` plus "the repository was repacked; remount" when it happens anyway;
- blocks keyed by `(pack name, offset)`, per above.

## Alternatives considered

**Keep ADR 0005's synthesized `.git` and shim.** The architectural fit is better —
Git is peer-symmetric and assumes a local object database, GFS is client/server
with a thin client, and the measurements land exactly on that seam: questions
about *local state* are cheap through a projection, questions needing a *local
object database* are not. But the cost of the custom surface is unbounded, while
the projection's cost is enumerable: six config settings, two expensive commands,
one retention policy. Compatibility with an ecosystem is worth a known price.

**VFS for Git.** Microsoft built this and abandoned it for Scalar. The
difference that matters is that GVFS required a forked Git; nothing here does,
which is the one configuration in which the compatibility bet can pay.

**EdenFS/Sapling.** Meta won by owning the client end to end and giving up Git
compatibility. Not available to a project whose premise is that existing agents
and IDEs keep working.

**Client-side partial clone.** Still rejected, and now for a measured reason
rather than a derived one: under Git's defaults the first `git status` re-hashes
the entire working tree — 1 615 MiB, 18 811 ms on linux — because a shipped
index's `dev`/`ino` cannot match another filesystem.

**PID-based blocking of `grep`/`find`/`rg`.** Rejected. It is a name check rather
than a boundary — renaming the binary or reaching for `python3` defeats it —
`/proc` lookups are racy, readahead does not carry the caller's PID, and denying
by name breaks build systems. Process identity is used for **attribution**, and
may select *which* budget applies, so that an unrecognized process gets the
stricter limit and evasion fails safe.

## Consequences

- The hydration budget ships **first**, against the current design, and is
  valuable whether or not the projection follows.
- `git status` on 94 851 files costs 170 lookups and 49 ms, against 108 445 and
  18 811 ms under Git's defaults.
- The compatibility boundary changes character: unsupported commands now *work
  slowly* instead of failing or lying. That is a better failure mode, and it moves
  the risk from "the tool is broken" to "the budget tripped".
- `gfs status`'s journal answer becomes an fsmonitor answer. The journal is still
  the source of truth, but a protocol now sits between it and the result, so a
  wrong token is a wrong `status` — silently. The hook's token semantics are
  correctness-critical and `spikes/git-projection` only exercised a static
  stand-in.
- The index is a second view of "what changed", which DESIGN.md section 8.6 raised
  as an objection to a real `.git`. It is now real and accepted, mitigated by the
  journal feeding it.
- ADR 0002 is unaffected: repository read access already implies object-database
  read access, so projecting `objects/` discloses nothing new. Ref *listing* still
  must be filtered per mount.
- ADR 0007's question — whether these tools occupy the standard names on `PATH` —
  is narrowed rather than answered: `git` needs only the short routing list above,
  and `grep`/`find` need a degrade rule rather than a refusal.

## Amendment, 2026-07-29: two deltas from the build

M9.1–M9.6 were implemented the same day, and two decisions changed shape on
contact with the code.

**The `Log` RPC survives, as `gfs show`'s internal fetch.** The retirement
table above says `gfs log` + `Log` both go. The CLI command is gone, but
`gfs show` — which stays, on the measured 91.5 MiB cost of a monorepo tree
diff — fetches its one-commit header through the same `Log` walk, so the RPC
remains as internal support. Deleting it would have meant rebuilding `show` on
a second header path for no caller-visible change.

**`blame` advises instead of routing.** Routing `git blame` to `gfs blame`
would substitute output in a different format, and a tool parsing
`git blame --porcelain` would break — the `ls-files`-lies failure mode this ADR
exists to end, reintroduced by its own shim. So the shim runs real `git blame`
after a stderr note naming `gfs blame` and the measured cost. stderr reaches
the agent; stdout stays byte-exact for tools. The routing table's other rows
are unchanged: five maintenance commands fail with the reason, everything else
passes through, and outside a GFS workspace the shim is fully transparent —
which closes ADR 0005's recorded objection to a `PATH`-wide install.

One test consequence worth recording: the old shim's tests asserted overlay
semantics (`A  added.txt` for any new file). Stock Git reports `?? added.txt`
until `git add`, and `git ls-files` keeps listing a deleted-but-unstaged file.
The rewritten tests assert Git's behaviour, because Git's behaviour is the
compatibility this ADR buys.

## Amendment, 2026-07-30: the recorded gaps, closed

The 2026-07-29 build left four gaps, recorded in its plan file. All four are
now built, and two of them changed shape on contact with the code.

**The residency budget is implemented, as SLRU rather than plain LRU.** The
two-budgets table above stands: opt-in (`--odb-residency-budget`, default off),
bytes held, evict-and-refetch, never refuse. The scan-resistance requirement —
a streaming `blame` must not flush a build's working set — is what forced the
segmented policy: single-touch blocks never leave probation, so protected
blocks survive a one-pass scan of any size. Eviction clears the presence bit
and punches a hole in the sparse file, in that order, under the same lock that
marks presence; blocks of an in-flight read are pinned. The refetch/unique
ratio this ADR names as the thrash signal is counted and surfaced in
`gfs status`.

**`grep`/`find` got their degrade rule.** A second shim (`gfs-scan-shim`,
installed as `grep`, `find`, and `rg`) advises on stderr — naming `gfs rg` and
`git ls-files` and the budget that will otherwise price the sweep — then execs
the real tool. Output is never substituted; outside a GFS workspace it is
silent. `grep` is only nagged when a recursive flag is present.

**`git push` to the gateway exists, confined to the pusher's own namespace.**
Receive-pack runs as a sandboxed child like upload-pack, with one policy fetch
does not have: `receive.hideRefs=refs/` followed by `!refs/gfs/work/<subject>`
(Git checks the list back to front), plus a pre-spawn command check for a
legible refusal. The mirror's `refs/heads/*` stays upstream's — written by
fetch, never by push. `receive.autogc` is off, because a post-receive gc could
repack away files live projections reference. The workspace's seeded `.git`
carries an `origin` push refspec mapping `refs/heads/*` into the caller's work
subtree (the root travels in `CreateMountResponse`), and a credential helper
reading `GFS_TOKEN` at push time — the token is not written to disk.
`CommitChanges` stays for API callers.

**Per-job odb attribution moved up a layer instead of into the store.** The
projection is shared, so the store cannot know which job read; a pid lookup at
read time is racy and was already rejected. Each workspace now mounts its own
*view* — a second small FUSE mount over the same shared block store — and its
`objects/info/alternates` names the view, so which-mountpoint-was-read is the
job identity. Blocks and residency stay shared; only the counting is per view.
`gfs status` reports both the repository's traffic and this job's share.

## Amendment, 2026-07-31: the gateway is a fork, and push lands on real branches

The 2026-07-30 confinement — pushes only into `refs/gfs/work/<subject>/`, the
branch namespace written by fetch alone — produced a repository no other Git
host behaves like: commit, push, delete the clone, clone again, and the work
is invisible, because the push went to a namespace clones do not resolve. The
diagnosis was that the *sync* was wrong, not the push: `refs/heads/*` was only
unsafe to accept because the fetch force-mirrored and pruned it.

So the sync now has fork semantics, the way a GitHub fork relates to its
upstream. Upstream state is fetched — forced, pruned — into
`refs/remotes/upstream/*`, a namespace nothing else writes, and folded into
`refs/heads/*` by a fast-forward-only pass: a branch that can follow upstream
does, a branch that has diverged (or that upstream deleted) is left exactly
where it is and reported, and resolving the divergence is the pusher's job —
merge, rebase, or force-push, against the advertised
`refs/remotes/upstream/<branch>`. With that in place receive-pack un-hides
`refs/heads/` (tags stay fetch-owned, `refs/gfs/` stays reserved), the seeded
push refspec became `refs/heads/*:refs/heads/*`, and `gfs push` falls back to
`refs/heads/<branch>` when the caller has no work branch of that name.

Everyone sharing a gateway repository shares its branches, exactly as a team
sharing a fork does; per-branch protection (a deny list in receive-pack, where
the namespace check already lives) is the eventual refinement, deliberately
not built yet. The work namespace and the RPC commit flow stay as they were,
for callers that do not speak Git.

## Amendment, 2026-07-31: the scan shims delegate instead of advising

The 2026-07-30 degrade rule made `grep`/`find`/`rg` advise on stderr and run
the real tool. The note reached nobody a sweep was about to hurt — an agent
mid-plan does not switch tools because stderr suggested one — so the shims now
take the cheap route themselves: `rg` becomes `gfs rg` (flag-compatible),
`find` becomes the new `gfs find` (find's grammar, answered from the git index
plus the overlay journal — no readdir sweep, no hydration), and a recursive
`grep` is translated to `gfs rg` when the translation is exact, including
declining default-BRE patterns that lean on backslash escapes.

The degrade property survives in the fallback: `gfs rg`/`gfs find` refuse an
unimplemented flag by name at parse time, before any output exists, and the
shim then runs the real tool over the mount — unsupported invocations still
work slowly rather than failing or lying, with the hydration budget pricing
the sweep as before. `GFS_SHIM_BYPASS` forces pass-through, which is how
`--hydrate` runs the real tool without the shim delegating straight back.

## Amendment, 2026-08-02: the hook has to name what the journal forgot

The Consequences above flagged the fsmonitor hook's token semantics as
correctness-critical and only exercised against a static stand-in. The
correctness gap turned out to be one layer down, in what the hook has to
*name*.

The answer is derived from `Status`, which is derived from the journal's rows.
A file created and then deleted leaves no row — `Overlay::remove` drops it
outright when the base has nothing at that path to whiteout — so the change set
forgets it ever existed. Git's response to a path fsmonitor does not mention is
to trust what it already believed, and it believes two things: its untracked
cache extent (once fsmonitor is configured, `valid_cached_dir` stops `lstat`ing
directories entirely and trusts the cached `valid` flag until fsmonitor
invalidates it) and its index entries' `CE_FSMONITOR_VALID` flag (which skips
the `lstat` that would notice a deletion). Measured against a live mount:
`git status` printed `?? f.txt` indefinitely for a deleted file, and a
`git add`ed file that was then removed reported as `A  f.txt` where stock Git
reports `AD f.txt`.

So the journal now keeps the names of paths whose rows are gone, for the life of
the generation, capped — past the cap the hook answers "rescan everything",
which is slow and correct rather than fast and wrong. The token gained a
sequence (`gfs:<generation>:<count>`) so it advances when the workspace changes,
as the v2 protocol asks; only the generation still decides a rescan, because the
answer is deliberately cumulative and a superset is always safe.

The same investigation found the mount root reporting the snapshot time forever:
`touch_parent` skipped the empty path, since giving the root an overlay row
would make it resolvable two ways. Its timestamps now live in the journal's meta
table, which is the same durability without the second spelling. That half
matters for anything keyed on directory mtime — builds, watchers, and Git's own
untracked cache when fsmonitor is *not* configured.
