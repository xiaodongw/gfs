# Local mode against `git worktree add` and the server mount

Reproduce: `./spikes/corpus/benchmark-local.sh vscode django` (release build,
real ripgrep). Run 2026-09-02 on the M4.6 machine profile (WSL2, 32 cores,
46 GiB, Git 2.53.0, Rust 1.97.1). One run per configuration, page cache warm
from the clone step.

The question: a developer has a full clone of a monorepo and wants one working
tree per change. `git worktree add` is the incumbent. Local mode (ADR 0013)
mounts the same commit lazily from the clone's own object database, with no
server. The server mount is included to show what the remote deployment costs
on the same machine, so the comparison is three working trees of one clone
running one task: acquire, history, `ls-files`, content search, read one
directory through twice, edit five paths, `git status` twice, commit.

## vscode (17 926 files, 2.8 GiB clone)

| step | git worktree | gfs local | gfs server | result |
| --- | ---: | ---: | ---: | --- |
| acquire one workspace | 2.330 s | **0.209 s** | 0.217 s | |
| acquire 4 more | 9.759 s | **0.563 s** | 2.287 s | |
| `log -10` | 0.006 s | 0.017 s | 0.056 s | 10 commits, all |
| `ls-files '*test*'` | 0.010 s | 0.020 s | 0.023 s | 6 245 files, all |
| `rg -F TODO` over the tree | 0.037 s | 1.143 s | 4.175 s | 1 683 lines, all |
| `gfs rg -F TODO` | – | 0.282 s | 0.241 s | 1 574 lines, both |
| read 2 000 files under `src/`, cold | 0.036 s | 0.643 s | 0.862 s | |
| read again, warm | 0.034 s | 0.635 s | 0.853 s | |
| edit | 0.006 s | 0.019 s | 0.027 s | |
| `git status`, cold | 0.308 s | 1.446 s | 1.488 s | |
| `git status`, warm | 0.052 s | 0.059 s | 0.071 s | |
| `gfs status` | – | 0.010 s | 0.009 s | |
| commit | 0.166 s | 1.605 s | 1.889 s | |

| disk, allocated | git worktree | gfs local | gfs server |
| --- | ---: | ---: | ---: |
| one workspace | 297.9 MiB | **2.8 MiB** | 3.2 MiB + 61.9 MiB host cache |
| five workspaces | 1 489.5 MiB | **12.8 MiB** | |

Commit correctness: all three flows produced tree `cd55379271c5…`. Anchors
left in the clone after unmount: 0. `gfs rg` coverage: 314 of 17 928 paths not
searched — 312 binary, 2 oversized — reported on stderr.

## django (7 078 files, 871 MiB clone)

| step | git worktree | gfs local | gfs server | result |
| --- | ---: | ---: | ---: | --- |
| acquire one workspace | 1.203 s | **0.096 s** | 0.111 s | |
| acquire 4 more | 4.793 s | **0.204 s** | 0.801 s | |
| `log -10` | 0.010 s | 0.015 s | 0.048 s | 10 commits, all |
| `ls-files '*test*'` | 0.007 s | 0.014 s | 0.015 s | 2 621 files, all |
| `rg -F TODO` over the tree | 0.018 s | 0.484 s | 2.331 s | 35 lines, all |
| `gfs rg -F TODO` | – | 0.098 s | 0.037 s | 35 lines, both |
| read 2 000 files under `django/`, cold | 0.035 s | 0.616 s | 0.802 s | |
| read again, warm | 0.036 s | 0.615 s | 0.793 s | |
| edit | 0.005 s | 0.019 s | 0.016 s | |
| `git status`, cold | 0.116 s | 1.064 s | 1.079 s | |
| `git status`, warm | 0.113 s | 0.035 s | 0.039 s | |
| `gfs status` | – | 0.009 s | 0.010 s | |
| commit | 0.054 s | 1.228 s | 1.350 s | |

| disk, allocated | git worktree | gfs local | gfs server |
| --- | ---: | ---: | ---: |
| one workspace | 73.4 MiB | **1.1 MiB** | 1.2 MiB + 23.6 MiB host cache |
| five workspaces | 367.0 MiB | **5.1 MiB** | |

Commit correctness: all three flows produced tree `c297292656bb…`. Anchors
left after unmount: 0.

## What the numbers say

**Acquire is the win, and it compounds.** One vscode workspace is 11× faster
to create and 100× smaller; five of them are 17× faster and 116× smaller,
because a worktree's cost is the tree's size and a local mount's cost is an
index and a `packed-refs`. The mount's 0.2 s is `index_for_commit` in-process
(the same walk the server does, ~70 ms on vscode) plus the FUSE session. The
server's "4 more" is slower than local's because each mount is a `CreateMount`
round trip, an index download, and an odb manifest fetch.

**Local mode holds no copy of anything.** The host cache is 0 MiB after the
run because blobs are served from memory and never written to disk; the
workspace's 2.8 MiB is the seeded index, `packed-refs`, the overlay journal,
and the loose objects of one commit. The server leg's 62 MiB host cache is the
blob copies and odb blocks the same task fetched.

**The recurring cost is FUSE, and it is per file.** Reading 2 000 files costs
0.32 ms each through the mount against 0.018 ms native, cold and warm alike
— the warm number does not move, which says the inflate (which the in-memory
LRU makes free on the second pass) is not where the time goes; the kernel
round trips for `open`, `read`, and `release` are. Raw `rg` over the tree is
the same story at 17 926 files: 1.1 s against 0.04 s. `gfs rg` reads the pack
directly and is 4× faster than raw `rg` over the mount, at 0.28 s, without a
trigram index. Against the server, local mode's raw `rg` is 3.7× faster
because there is no cache write and no network.

**Cold `git status` is the untracked-cache fill**: 4 318 directories read
through FUSE once, then a warm `status` is faster than a worktree's on django
(35 ms against 113 ms) because fsmonitor answers from the journal and Git
walks nothing. **Commit is 1.2–1.6 s on both gfs legs against 0.05–0.17 s
native**; the difference is the same on the server leg, so it is not local
mode's — it is `git add -A` walking the tree through FUSE, and it is the next
thing to measure.

**Where a worktree still wins**: any tool that reads a large fraction of the
tree file by file — a full build, a linter over everything — pays the FUSE
per-file cost, which on vscode is roughly a second per 3 000 files. Local
mode is for the workflow where most of the tree is never opened, which is what
"one workspace per change" on a monorepo is.

**Search coverage** is the same on both gfs legs: the scan and the index apply
one corpus policy, and both say what they skipped. Raw `rg` finds 1 683 lines
where `gfs rg` finds 1 574 — the 109 lines `rg` finds in the 312 files the
policy classifies as binary.
