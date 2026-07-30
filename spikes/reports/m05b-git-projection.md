# Raw Git over a projected object store

Date: 2026-07-29
Reproduce: `(cd spikes && cargo build --release -p git-projection-probe) && ./spikes/git-projection/measure.sh linux`
Revisits: [ADR 0005](../../docs/adr/0005-git-command-surface.md), [`m05-git-surface.md`](m05-git-surface.md)

ADR 0005 chose a synthesized, object-free `.git` plus a `git` shim, and the
measurement that decided it was that a real `.git` turns `git status` into a full
metadata sweep of the monorepo: 94 850 first-time FUSE lookups on the Linux
kernel. Every history question was then re-implemented as a server-side tool —
`gfs log`, `gfs find`, `gfs show`, `gfs diff`, `gfs blame` — and the ADR has been
amended twice in four days, each amendment moving one more question to the
server.

That measurement was taken with Git's default settings, and
[`m05-git-surface.md`](m05-git-surface.md) says so:

> `git status` was measured with Git's default settings. `core.untrackedCache`
> and `core.fsmonitor` can reduce the sweep substantially and were not
> evaluated; **neither is available through a FUSE mount without further work,
> which is why they do not change the decision.**

The second clause is load-bearing and this spike tests it. `core.fsmonitor` is a
hook program, and GFS's overlay journal is exactly the modified-path set it asks
for — a *better* source than watchman, because the FUSE server sees every
mutation by construction and there is no watcher race.

## What was measured

Stock Git throughout; no forked Git, no new mechanism.

```text
<mount>/tree      the working tree, projected read-only
<mount>/objects   the gateway's object database, projected read-only
<local>/agent-git the agent's real git dir, on LOCAL disk:
                    objects/info/alternates -> <mount>/objects
                    index                   -> shipped by the gateway
                    HEAD, refs/, config     -> the pinned ref view
```

That split is `git worktree`'s own: a linked worktree's `.git` is a *file*
pointing at a small per-worktree directory (`HEAD`, `index`, `logs/`) while
`commondir` names the shared object store. The `alternates` pointer is what
`git clone --shared` uses. Both are supported, documented Git features.

`spikes/git-projection` is a counting read-only passthrough FUSE filesystem. It
is an instrument, not a prototype: it forwards every operation to a local
directory and reports **counts and bytes** bucketed by path class, because those
are properties of Git rather than of GFS's implementation. One `lookup` here is
one snapshot-API round trip or cache hit in a real mount — the unit ADR 0005's
objection is denominated in — and a byte read under `objects/pack/*.pack` is a
byte a real gateway must ship. 60 s entry and attribute TTLs, matching
DESIGN.md section 8.2 for an immutable base.

### Machine and corpus

| | |
| --- | --- |
| Host | WSL2, Linux 6.18.33.2-microsoft-standard-WSL2, 32 cores, 46 GiB |
| Git | 2.53.0 |

| | django | linux |
| --- | ---: | ---: |
| files at tip | 7 078 | **94 851** |
| working tree | 44.6 MiB | 1 540 MiB |
| shipped index | 0.85 MiB | 9.7 MiB |
| pack data (`.pack`) | 757 MiB | 8 122 MiB |
| pack index (`.idx`) | 23.7 MiB | 460 MiB |
| commit-graph | 7.2 MiB | 121 MiB |

linux's 94 851 files is the same tip ADR 0005's 94 850-entry index was measured
against, so the two numbers are directly comparable.

## Result 1: `git status` — the decision ADR 0005 made is refuted

`git status --porcelain`, worktree traffic only. "steady" is the second
invocation in the same mount.

**linux, 94 851 files:**

| configuration | first run | steady state |
| --- | --- | --- |
| Git defaults | 108 445 lookups, 94 878 opens, **1 615 MiB read**, 18 811 ms | 1 188 lookups, 6 203 readdirs, 1 525 ms |
| `+ checkStat=minimal`, `trustctime=false` | 102 246 lookups, **17 KiB** read, 1 962 ms | 1 188 lookups, 6 203 readdirs, 1 536 ms |
| `+ fsmonitor`, `untrackedCache` | 102 412 lookups, 17 KiB, 1 979 ms | **170 lookups, 0 readdirs, 49 ms** |

**django, 7 078 files:**

| configuration | first run | steady state |
| --- | --- | --- |
| Git defaults | 15 135 lookups, 7 076 opens, **44.6 MiB read**, 2 062 ms | 1 509 lookups, 3 274 readdirs, 757 ms |
| `+ checkStat=minimal` | 11 863 lookups, 373 B read | 1 509 lookups, 3 274 readdirs, 760 ms |
| `+ fsmonitor`, `untrackedCache` | 11 864 lookups, 733 B | **0 lookups, 0 readdirs, 8 ms** |

Three findings, in order of how much they matter.

**Git's defaults are worse than ADR 0005 derived.** It predicted a metadata
sweep. What actually happens is a **full content download**: 1 615 MiB read
against a 1 540 MiB tree, because the index's recorded `dev`/`ino` cannot match a
different filesystem, so every entry is stat-dirty and Git re-hashes the whole
tree. 18.8 s and the entire working tree, on the first `git status` of every job.
ADR 0005 was right to refuse this configuration.

**`core.checkStat=minimal` plus `core.trustctime=false` eliminates the download**
— 1 615 MiB to 17 KiB — and this is where GFS's existing design does the work.
`minimal` compares mode, size, and whole-second mtime, excluding inode and
device. DESIGN.md section 8.2 already gives every base entry a deterministic
`snapshot_time`, so an index built once by the gateway with that value matches on
every host that ever mounts the commit. The sweep of 102 246 lookups remains.

**`core.fsmonitor` plus `core.untrackedCache` eliminates the sweep.** 108 445
lookups to **170**, 18 811 ms to **49 ms** — 638× fewer lookups on the worst-case
repository, and the residual 170 are all negative lookups for `.gitignore`-class
paths that never existed. Both settings are needed: fsmonitor removes the
per-entry `lstat`, untrackedCache removes the 6 203-directory readdir walk.

The m05 caveat is therefore wrong on its second clause. fsmonitor is not
unavailable through a FUSE mount; it is a hook contract that GFS is uniquely well
placed to answer, and the answer it needs is the overlay journal GFS already
maintains.

## Result 2: the dominant cost is shared metadata, not object data

Cold — one fresh mount per command, so nothing is billed to a previous command's
page cache. With a commit-graph written by the gateway
(`commit-graph write --reachable --changed-paths`).

**linux, bytes read through the projection:**

| command | `.pack` | `.idx` | commit-graph | wall |
| --- | ---: | ---: | ---: | ---: |
| `log --oneline -20` | 0.2 MiB | 10.3 MiB | 14.5 MiB | 32 ms |
| `log --format=%H -100` | 0 | 0 | 37.4 MiB | 41 ms |
| `show --stat HEAD` | 91.5 MiB | 287 MiB | 0.9 MiB | 403 ms |
| `log -10 -p` | 1.7 MiB | 19.8 MiB | 7.4 MiB | 35 ms |
| `diff --stat HEAD~5 HEAD` | 6.1 MiB | 53.3 MiB | 3.3 MiB | 81 ms |
| `log -20 -- <path>` | 15.6 MiB | 123 MiB | 116 MiB | 260 ms |
| `blame <path>` | 196 MiB | 376 MiB | 121 MiB | 822 ms |
| `ls-files '*test*'` | 0 | 0 | 0 | 24 ms |

The `.idx` and the commit-graph dominate — and both are **byte-identical for
every mount of the repository and immutable by name**. That is the same
amortization ADR 0008 already applies to the blob cache ("one blob cache per
repository instead of one per mount"). So phase 5 pins them per node and projects
only `*.pack`:

| command | linux `.pack` bytes | wall | django `.pack` bytes | wall |
| --- | ---: | ---: | ---: | ---: |
| `log --oneline -20` | 0.23 MiB | 5 ms | 0.25 MiB | 4 ms |
| `log --format=%H -100` | **0** | 6 ms | **0** | 3 ms |
| `show --stat HEAD` | 91.5 MiB | 131 ms | 0.77 MiB | 5 ms |
| `log -10 -p` | 1.7 MiB | 8 ms | 5.9 MiB | 21 ms |
| `diff --stat HEAD~5 HEAD` | 6.1 MiB | 28 ms | 4.1 MiB | 14 ms |
| `log -20 -- <path>` | 15.6 MiB | 39 ms | 20.6 MiB | 56 ms |
| `blame <path>` | 196 MiB | 306 ms | 12.5 MiB | 35 ms |
| `ls-files '*test*'` | **0** | 21 ms | **0** | 4 ms |
| pinned per node | 632 MiB | | 38 MiB | |

`.idx` and commit-graph traffic goes to **zero** — they never cross the mount —
and wall times drop by 2–3× on the commands that were reading them.

`ls-files` deserves its own line: **zero projection traffic**, because a real
index is local disk. `gfs find` and its `FindPaths` RPC exist to answer a question
Git answers for free.

## Result 3: two commands stay expensive on a monorepo

On linux, with metadata pinned, `show --stat HEAD` still reads 91.5 MiB and
`blame` 196 MiB of pack data. Both are inherent: comparing two 94 851-file trees
means resolving many tree objects through delta chains, and a blame walks a
file's history resolving a blob per candidate commit. Neither is fixed by
configuration.

This **supports** the newest ADR 0005 amendment on cost grounds even while
undercutting the reasoning it used. `gfs show`, `gfs diff` and `gfs blame` run on
the gateway where the objects are local and return one rendered patch; measured
against 91.5 MiB and 196 MiB of client traffic, that is the right call for a
repository this size. The amendment's stated reason — that a client must not
hydrate a piece at a time — was about the *client*; the real reason is simply
that a tree comparison on a 94 851-file repository moves a lot of bytes wherever
it runs, and only the gateway already has them.

## Result 5: chunk granularity — 8 MiB is the wrong size by two orders of magnitude

Pinning 632 MiB per node before the first command trades away the 0.211 s mount
that is GFS's headline win, so the alternative is to fetch lazily in chunks. The
probe records every distinct 64 KiB block each command touches, which gives what
a chunked fetcher would download at any granularity.

**linux, `.pack` traffic, cold, per command:**

| command | actually read | 64 KiB | 1 MiB | 8 MiB | 32 MiB |
| --- | ---: | ---: | ---: | ---: | ---: |
| `log --oneline -20` | 0.23 MiB | 0.3 MiB | 2 MiB | **16 MiB** | 64 MiB |
| `log -10 -p` | 1.7 MiB | 2 MiB | 13 MiB | **96 MiB** | 352 MiB |
| `diff --stat HEAD~5 HEAD` | 6.1 MiB | 8 MiB | 31 MiB | **128 MiB** | 384 MiB |
| `log -20 -- <path>` | 15.6 MiB | 22 MiB | 98 MiB | **408 MiB** | 896 MiB |
| `show --stat HEAD` | 91.5 MiB | 117 MiB | 250 MiB | 368 MiB | 608 MiB |
| `blame <path>` | 196 MiB | 274 MiB | 1 004 MiB | **2 320 MiB** | 3 648 MiB |

At 8 MiB, `log --oneline -20` costs 68× what it reads, and `blame` pulls 2.3 GiB —
over a quarter of the entire 8.1 GiB pack — for 196 MiB of need. The reason is the
access pattern: locating an object is a binary search in a sorted table, and
reconstructing one is a walk down a delta chain whose bases are scattered through
the pack. Sparse random access is exactly what large chunks punish, and the
punishment is worst on the commands that are otherwise cheapest.

64 KiB costs 1.0–1.4× — and that residual is the kernel's own readahead, not the
chunking, because these reads averaged ~92 KiB already. So a 64 KiB block is
effectively free, and it coincides with FUSE's 128 KiB maximum read: one block per
request, nothing to tune.

Two consequences.

**Chunk at 64–128 KiB, and the design simplifies rather than complicates.** At
that granularity "lazy chunked fetch" *is* "serve each read from the gateway and
cache the block", so the node cache becomes a block cache — which is the shape
DESIGN.md section 8.3's quota-based LRU already has, holding smaller entries.
Nothing needs pinning and no population strategy is needed: the cache converges on
the hot subset on its own.

**Laziness makes cheap commands cheap; it does not make expensive ones cheap.**
`blame` still needs ~590 MiB (394 MiB `.idx` + 196 MiB `.pack`) however it is
fetched. Lazy fetching preserves the fast mount and makes `status`, `ls-files`,
`log` and `diff` cost 0–30 MiB; it is not an argument for moving `blame` or
`show --stat` off the gateway.

## Result 6: what a budget refusal actually says to the caller

A FUSE reply carries data or a negative errno and no prose, so the only text a
tool can print is `strerror` of whatever errno is chosen. The probe models
DESIGN.md section 8.4's hard budget with `--worktree-budget`; at 2 MB on django:

| caller | what it printed |
| --- | --- |
| `grep -r` | `grep: <path>: Disk quota exceeded` |
| `rg` | `rg: <path>: Disk quota exceeded (os error 122)` |
| `python3` | `OSError [Errno 122] Disk quota exceeded` |
| `git status`, fsmonitor configured | **never tripped it** — zero working-tree reads |

Three findings.

**`EDQUOT` is the right errno and the reason is its `strerror`.** Every caller
surfaces "Disk quota exceeded" with the offending path, which is unambiguous and
does not read as a corrupt filesystem. `EIO` would send an agent looking for the
wrong problem entirely. DESIGN.md section 8.4 already specifies `EDQUOT`; this
makes it a decision with a reason rather than a default.

**Byte counting discriminates without a deny list.** A configured `git status`
never touched the budget while `grep -r` tripped it inside 2 MB. That distinction
— tool that respects the workspace versus tool that sweeps it — *is* a byte count,
which is why process identity is not needed for enforcement.

**Refusing reads floods the caller's output.** `grep -r` did not abort; it printed
one error per file and kept walking, which for an agent means thousands of
identical lines in its context. Refuse at `open` and `opendir` instead, so the
tool stops early.

What the errno cannot carry is "use `gfs rg`". That has to come from a `PATH`
wrapper, from `gfs status`, or from the orchestrator reading the workspace control
socket after a non-zero exit — the last being the only channel that reliably
reaches an agent's context, and one that lives outside GFS.

## Result 4: the footgun is real and severe

`git repack -a -d` in the agent's git dir, read through the projection:

| | `.pack` | `.idx` |
| --- | ---: | ---: |
| django | 362 MiB | 27.0 MiB |
| linux | **6 578 MiB** | 506 MiB |

Without `-l`, `repack -a` copies every borrowed object out of the alternate into
a local pack. On linux that is a 6.6 GiB download and a second copy on local
disk, triggered by routine housekeeping. `gc.auto=0` and
`maintenance.auto=false` are not tuning; they are required, and they belong in
the projected config rather than in documentation.

## What this means for the design

| question | answer | evidence |
| --- | --- | --- |
| Does raw Git work over a projected object store? | Yes, unmodified, including commit and branch on a read-only store | phase 1 |
| Is `git status` affordable? | Yes — 170 lookups, 49 ms on 94 851 files | result 1 |
| Can `gfs find` be deleted? | Yes — `ls-files` costs zero projection traffic | result 2 |
| Can `gfs log` be deleted? | Yes — 0–16 MiB with metadata pinned | result 2 |
| Can `gfs show`/`gfs blame` be deleted? | **No** — 91.5 and 196 MiB on linux | result 3 |
| What must the gateway ship? | index with `snapshot_time` stat data, commit-graph with `--changed-paths`, and pinned `.idx` per node | results 1–2 |
| What must be disabled? | `gc.auto`, `maintenance.auto` | result 4 |

## Limitations

- **Wall times are a floor, not a prediction.** The lower directory is local
  disk, so every byte counted was served from the host page cache. The
  transferable numbers are the counts and bytes; a real gateway adds per-operation
  latency to each.
- **Byte counts include kernel readahead.** Git `mmap`s packs and the `.idx`, and
  a page fault pulls a readahead window, so reads averaged ~92 KiB. The bytes are
  an upper bound on what Git strictly needed. This does not affect result 1
  (whose reads are whole small files) or the `.idx` conclusion (whose fix is to
  stop crossing the mount at all).
- **The fsmonitor hook is a static stand-in** that answers "nothing changed". It
  proves what Git does with the answer, not that GFS can produce it. Wiring it to
  the overlay journal, and getting the token's advance-on-change semantics right,
  is unmeasured work.
- **Writes were not measured.** Phase 1 confirmed on a scratch repository that
  `checkout -b`, `add` and `commit` write only local loose objects against a
  read-only alternate, but no monorepo-scale write path was exercised, and neither
  was `git push` through the M5 gateway.
- **Pack retention is unmeasured and is the main new engineering cost.** A
  projected `objects/` means the gateway must not delete a pack a live mount
  references. Git's own answer to this race is a grace period
  (`gc.pruneExpire`, cruft packs) rather than locking; GFS's lease already retains
  a commit and would have to be widened to retain the mount's whole pinned ref
  view. Nothing here tests it.
- One run per configuration. The sub-100 ms figures vary more than the large ones.
- The corpus is still the public stand-in set (`spikes/corpus/corpus.conf`).
