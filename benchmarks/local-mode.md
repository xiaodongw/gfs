# Local mode against `git worktree add` and the server mount

Reproduce: `./spikes/corpus/benchmark-local.sh vscode django` (release build,
real ripgrep). Run 2026-09-02 on the M4.6 machine profile (WSL2, kernel 6.18,
32 cores, 46 GiB, Git 2.53.0, Rust 1.97.1). One run per configuration, page
cache warm from the clone step.

The question: a developer has a full clone of a monorepo and wants one working
tree per change. `git worktree add` is the incumbent. Local mode (ADR 0013)
mounts the same commit lazily from the clone's own object database, with no
server. The server mount is included to show what the remote deployment costs
on the same machine, so the comparison is three working trees of one clone
running one task: acquire, history, `ls-files`, content search, read one
directory through twice, edit five paths, `git status` twice, commit.

Two runs are shown: the mount as it shipped with ADR 0013 (**before**), and
the mount after ADR 0014 (**after**), which changed nothing in local mode
itself and everything in how the daemon answers the kernel. The investigation
behind the second run is in `plans/20260902-1130-fuse-round-trips.md`.

## vscode (17 926 files, 2.8 GiB clone)

| step | git worktree | gfs local, before | gfs local, after | gfs server, after | result |
| --- | ---: | ---: | ---: | ---: | --- |
| acquire one workspace | 2.314 s | 0.209 s | **0.172 s** | 0.233 s | |
| acquire 4 more | 9.689 s | 0.563 s | **0.521 s** | 2.679 s | |
| `log -10` | 0.006 s | 0.017 s | 0.014 s | 0.051 s | 10 commits, all |
| `ls-files '*test*'` | 0.011 s | 0.020 s | 0.019 s | 0.019 s | 6 245 files, all |
| `rg -F TODO` over the tree | 0.038 s | 1.143 s | 0.777 s | 3.854 s | 1 683 lines, all |
| `gfs rg -F TODO` | – | 0.282 s | 0.298 s | 0.246 s | 1 574 lines, both |
| read 2 000 files under `src/`, cold | 0.038 s | 0.643 s | 0.264 s | 0.316 s | |
| read again, warm | 0.034 s | 0.635 s | 0.164 s | 0.206 s | |
| edit | 0.005 s | 0.019 s | 0.019 s | 0.025 s | |
| `git status`, cold | 0.100 s | 1.446 s | 0.558 s | 0.570 s | |
| `git status`, warm | 0.099 s | 0.059 s | **0.040 s** | 0.047 s | |
| `gfs status` | – | 0.010 s | 0.009 s | 0.009 s | |
| commit | 0.235 s | 1.605 s | 0.421 s | 0.673 s | |

| disk, allocated | git worktree | gfs local | gfs server |
| --- | ---: | ---: | ---: |
| one workspace | 297.9 MiB | **2.8 MiB** | 3.2 MiB + 62.0 MiB host cache |
| five workspaces | 1 489.5 MiB | **12.8 MiB** | |

Commit correctness: all three flows produced the same tree. Anchors left in
the clone after unmount: 0. `gfs rg` coverage: 314 of 17 928 paths not
searched — 312 binary, 2 oversized — reported on stderr.

## django (7 078 files, 871 MiB clone)

| step | git worktree | gfs local, before | gfs local, after | gfs server, after | result |
| --- | ---: | ---: | ---: | ---: | --- |
| acquire one workspace | 1.235 s | 0.096 s | **0.103 s** | 0.110 s | |
| acquire 4 more | 4.821 s | 0.204 s | **0.207 s** | 0.784 s | |
| `log -10` | 0.011 s | 0.015 s | 0.013 s | 0.043 s | 10 commits, all |
| `ls-files '*test*'` | 0.007 s | 0.014 s | 0.012 s | 0.012 s | 2 621 files, all |
| `rg -F TODO` over the tree | 0.018 s | 0.484 s | 0.292 s | 1.873 s | 35 lines, all |
| `gfs rg -F TODO` | – | 0.098 s | 0.095 s | 0.037 s | 35 lines, both |
| read 2 000 files under `django/`, cold | 0.035 s | 0.616 s | 0.266 s | 0.308 s | |
| read again, warm | 0.035 s | 0.615 s | 0.165 s | 0.198 s | |
| edit | 0.005 s | 0.019 s | 0.017 s | 0.019 s | |
| `git status`, cold | 0.128 s | 1.064 s | 0.403 s | 0.407 s | |
| `git status`, warm | 0.035 s | 0.035 s | **0.027 s** | 0.028 s | |
| `gfs status` | – | 0.009 s | 0.009 s | 0.011 s | |
| commit | 0.050 s | 1.228 s | 0.294 s | 0.396 s | |

| disk, allocated | git worktree | gfs local | gfs server |
| --- | ---: | ---: | ---: |
| one workspace | 73.4 MiB | **1.1 MiB** | 1.2 MiB + 23.6 MiB host cache |
| five workspaces | 367.0 MiB | **5.1 MiB** | |

Commit correctness: all three flows produced the same tree. Anchors left after
unmount: 0.

## What the numbers say

**Acquire is the win, and it compounds.** One vscode workspace is 13× faster
to create and 100× smaller; five of them are 19× faster and 116× smaller,
because a worktree's cost is the tree's size and a local mount's cost is an
index and a `packed-refs`. The mount's 0.17 s is `index_for_commit` in-process
(the same walk the server does, ~70 ms on vscode) plus the FUSE session. The
server's "4 more" is slower than local's because each mount is a `CreateMount`
round trip, an index download, and an odb manifest fetch.

**Local mode holds no copy of anything.** The host cache is 0 MiB after the
run because blobs are served from memory and never written to disk; the
workspace's 2.8 MiB is the seeded index, `packed-refs`, the overlay journal,
and the loose objects of one commit. The server leg's 62 MiB host cache is the
blob copies and odb blocks the same task fetched.

**The recurring cost was never local mode's; it was round trips, and most of
them were avoidable.** The "before" column paid three synchronous kernel round
trips per `open`+`close` (`open`, `flush`, `release`) and four per directory
(`opendir`, `readdir`, an empty `readdir`, `releasedir`), and answered each one
after a detour through the tokio runtime that cost more than the kernel's own
round trip. Measured per operation on the warm mount, with the daemon's CPU
attributed from `schedstat`:

| operation, warm | before | after | fuser's hello filesystem |
| --- | ---: | ---: | ---: |
| `open`+`close` of a base blob | 146 µs | 63 µs | 48 µs |
| `open`+`read`+`close` of a base blob | 242 µs | 66 µs | 97 µs |
| negative lookup | 100 µs | 56 µs | 51 µs |
| `listdir`, 8 entries | 300 µs | 75 µs | – |
| `listdir`, ~120 entries | 377 µs | 82 µs | – |
| `lstat`, cached | 2.4 µs | 2.5 µs | 1.7 µs |

ADR 0014 records the two changes: the FUSE thread polls each handler once and
hands off only what has to wait, and the kernel is told what it may keep
(`FOPEN_KEEP_CACHE` on base blobs, `FOPEN_CACHE_DIR | FOPEN_KEEP_CACHE` on
directories, `FOPEN_NOFLUSH` everywhere). A warm re-read of a file is now a
page-cache hit with no `read` request at all, which is the 0.64 s → 0.16 s on
"read again". A second walk of the tree is served from the kernel's listing
cache, which is the 1.6 s → 0.42 s on commit (`git add -A` walks every
directory, every time, without the untracked cache).

**What is left per file** is `open` and `release`, one round trip each, at
about the kernel's floor on this machine; and on a cold open, libgit2's
inflate on a blocking worker, ~130 µs over the warm cost. `rg` over 17 926
files at 0.78 s is that: it opens every file once. `gfs rg` reads the pack
directly and is 2.6× faster than raw `rg` over the mount, without a trigram
index. **What is left per directory** on a warm walk is `opendir` and
`releasedir`, plus one `getattr` after an uncached read, because the kernel
invalidates a directory's `atime` on every uncached `readdir` and glibc's
`opendir` calls `fstat`. Cold `git status` at 0.56 s is the untracked-cache
fill: 4 318 directories, four requests each, the first time only. A warm
`status` is faster than a worktree's on both corpora, because fsmonitor
answers from the journal and Git walks nothing.

**The next lever is FUSE passthrough**, which the kernel here supports
(6.9+) and `fuser` 0.18 exposes: the daemon registers a backing file at `open`
and the kernel reads from it directly, no `read` request even cold. A memfd
holding the inflated blob would do; nothing would touch disk. Tested: the
backing-file ioctl is refused without `CAP_SYS_ADMIN`, so it is a deployment
decision (a file capability on `gfs-fuse`), not a default.

**Where a worktree still wins**: any tool that reads a large fraction of the
tree file by file — a full build, a linter over everything — pays two round
trips per file, which on vscode is roughly a second per 10 000 files warm.
Local mode is for the workflow where most of the tree is never opened, which
is what "one workspace per change" on a monorepo is.

**Search coverage** is the same on both gfs legs: the scan and the index apply
one corpus policy, and both say what they skipped. Raw `rg` finds 1 683 lines
where `gfs rg` finds 1 574 — the 109 lines `rg` finds in the 312 files the
policy classifies as binary.
