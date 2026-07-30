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
