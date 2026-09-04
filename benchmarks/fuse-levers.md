# Three FUSE levers on the local-mode mount, one commit each

Reproduce: `./spikes/corpus/benchmark-levers.sh <label> vscode django` on a
release build, then `./spikes/corpus/merge-levers.py benchmarks/fuse-levers/*.md`
to rebuild the tables below from the raw runs kept in
[`fuse-levers/`](fuse-levers/). Run 2026-09-03 on the same machine as
[`local-mode.md`](local-mode.md) (WSL2, kernel 6.18, 32 cores, 46 GiB). One
run per column; a fresh mount and a fresh host per run.

The starting point is the mount after ADR 0014: a warm base blob costs
`open` and `release`, a cold one adds a `read` and libgit2's inflate, an
overlay file pays a `read` on every open, and every `write` is a round trip
plus a journal row. The task is deliberately heavier on writes than
`local-mode.md`'s, because that is where the remaining cost was: read 2 000
files through twice, `rg` the tree twice, read the largest blob twice, write
64 MiB in 4 KiB pieces, copy a source directory in from the clone, read both
back, `git status`, commit; then a per-operation loop.

The columns, in the order the commits landed:

- **native worktree** — the same task in a `git worktree add` checkout of
  the same clone (`GFS_BENCH_NATIVE=1`): ext4, no daemon, the number every
  other column is measured against.
- **baseline** — the ADR 0014 build (`f779597`).
- **passthrough code, no cap** — [ADR 0015](../docs/adr/0015-kernel-passthrough.md)
  built and running *without* `CAP_SYS_ADMIN`: the daemon sees the kernel
  offers passthrough, sees it lacks the capability, and takes the old path.
  This column exists to show the change costs nothing when it cannot help.
- **passthrough, 4 096-entry cache** — the same build after
  `sudo setcap cap_sys_admin+ep target/release/gfs-fuse`, as first committed:
  the backing-file cache kept 4 096 registrations.
- **passthrough** — the same, with the cache raised to 65 536 registrations
  (the follow-up commit), which is the shipped setting.
- **+ writeback cache** — [ADR 0016](../docs/adr/0016-writeback-cache.md),
  mounted with `--writeback-cache`, without the capability (the two levers
  are independent; this column isolates the second).
- **passthrough + prewarm** — the best configuration: the capability set
  and `--prewarm`. Not `--writeback-cache` as well: the kernel refuses
  passthrough and writeback together and grants neither (a run with all
  three flags served zero passthrough opens and looked like the baseline
  with prewarm), so the daemon now drops the writeback request when it has
  passthrough.
- **+ prewarm** — `gfs mount --local … --prewarm`, without the other two:
  the benchmark waits for `gfs inspect` to report the prewarm done before
  the task starts, and the wait is its own row.
- **journal (ADR 0017)** — [ADR 0017](../docs/adr/0017-fewer-journal-commits.md):
  no fsync per content file, no journal row per `write`, the parent's
  timestamp bump in the child's transaction, cached statements. Mounted
  with no flags and without the capability, so it reads against
  **baseline**.
- **journal + prewarm** — the same build with `--prewarm`, which reads
  against **+ prewarm**.
- **passthrough + prewarm + journal** — the ADR 0017 build with the
  capability set again and `--prewarm`: the best configuration today.
- **passthrough + journal** — the same without `--prewarm`, so the two
  levers can be read apart from the third.

<!-- merged tables begin -->
### vscode

mount flags: native worktree   read-through: 2000 files under src/   largest blob: src/vs/base/test/node/uri.perf.data.txt (8874330 bytes)   copy: 10225 files

| step | native worktree | baseline | passthrough code, no cap | passthrough, 4 096-entry cache | passthrough | + writeback cache | + prewarm | passthrough + prewarm | journal (ADR 0017) | journal + prewarm | passthrough + prewarm + journal | passthrough + journal |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| mount | 2.488 s | 0.270 s | 0.226 s | 0.185 s | 0.194 s | 0.161 s | 0.170 s | 0.196 s | 0.292 s | 0.179 s | 0.166 s | 0.160 s |
| prewarm (waited) | – | – | – | – | – | – | 0.373 s | 0.450 s | – | 0.442 s | 0.452 s | – |
| read 2000 files, cold | 0.041 s | 0.746 s | 0.740 s | 0.671 s | 0.679 s | 0.733 s | 0.493 s | 0.452 s | 0.758 s | 0.523 s | 0.491 s | 0.652 s |
| read again, warm | 0.040 s | 0.264 s | 0.265 s | 0.273 s | 0.274 s | 0.264 s | 0.263 s | 0.273 s | 0.275 s | 0.280 s | 0.280 s | 0.267 s |
| `rg -F TODO`, first (1683 lines) | 0.041 s | 0.654 s | 0.634 s | 0.712 s | 0.617 s | 0.630 s | 0.399 s | 0.413 s | 0.693 s | 0.450 s | 0.404 s | 0.602 s |
| `rg -F TODO`, second | 0.034 s | 0.158 s | 0.164 s | 0.658 s | 0.180 s | 0.174 s | 0.160 s | 0.182 s | 0.162 s | 0.177 s | 0.162 s | 0.161 s |
| read largest blob, cold | 0.006 s | 0.006 s | 0.006 s | 0.044 s | 0.006 s | 0.006 s | 0.006 s | 0.007 s | 0.008 s | 0.008 s | 0.007 s | 0.006 s |
| read largest blob, warm | 0.007 s | 0.005 s | 0.006 s | 0.006 s | 0.006 s | 0.006 s | 0.005 s | 0.006 s | 0.006 s | 0.007 s | 0.006 s | 0.005 s |
| write 64 MiB, 4 KiB `dd` | 0.070 s | 3.950 s | 4.260 s | 0.893 s | 0.862 s | 0.854 s | 4.042 s | 0.894 s | 2.585 s | 2.661 s | 0.871 s | 0.837 s |
| `cp -r` 10225 files in | 1.100 s | 29.383 s | 27.599 s | 25.813 s | 26.288 s | 28.899 s | 26.861 s | 26.060 s | 8.861 s | 7.886 s | 6.411 s | 6.213 s |
| read the 64 MiB back | 0.009 s | 0.048 s | 0.075 s | 0.008 s | 0.009 s | 0.060 s | 0.057 s | 0.008 s | 0.062 s | 0.066 s | 0.010 s | 0.009 s |
| read the copied files back | 0.202 s | 2.772 s | 2.799 s | 1.758 s | 1.796 s | 2.813 s | 2.812 s | 1.796 s | 2.865 s | 3.046 s | 1.761 s | 1.747 s |
| `git status` after the writes | 0.549 s | 0.728 s | 0.717 s | 0.741 s | 0.789 s | 0.715 s | 0.727 s | 0.762 s | 0.782 s | 0.763 s | 0.744 s | 0.743 s |
| `git add -A` + commit | 1.140 s | 4.919 s | 5.010 s | 3.864 s | 3.937 s | 4.923 s | 4.991 s | 3.782 s | 4.896 s | 5.113 s | 3.621 s | 3.630 s |
| open+close, base blob | 2.7 µs | 63.5 µs | 63.7 µs | 64.5 µs | 64.9 µs | 63.3 µs | 62.9 µs | 65.3 µs | 64.3 µs | 65.0 µs | 64.8 µs | 65.1 µs |
| open+read+close, base blob | 3.6 µs | 65.9 µs | 65.2 µs | 70.8 µs | 67.7 µs | 65.4 µs | 65.8 µs | 68.3 µs | 66.0 µs | 67.5 µs | 67.1 µs | 67.2 µs |
| stat, cached | 1.8 µs | 1.8 µs | 1.9 µs | 1.9 µs | 1.9 µs | 1.9 µs | 1.9 µs | 2.0 µs | 1.9 µs | 1.8 µs | 1.9 µs | 1.9 µs |
| write 4 KiB, overlay file | 3.0 µs | 244.1 µs | 249.1 µs | 50.9 µs | 52.2 µs | 52.3 µs | 249.1 µs | 51.6 µs | 154.8 µs | 163.0 µs | 50.6 µs | 52.4 µs |
| open+close, overlay file | 2.7 µs | 72.1 µs | 72.6 µs | 81.6 µs | 82.3 µs | 73.4 µs | 73.2 µs | 79.7 µs | 74.5 µs | 77.5 µs | 79.3 µs | 87.7 µs |
| open+read+close, overlay file | 5.0 µs | 216.2 µs | 219.7 µs | 84.6 µs | 87.3 µs | 221.9 µs | 220.8 µs | 86.1 µs | 220.9 µs | 236.4 µs | 87.0 µs | 88.7 µs |

### django

mount flags: native worktree   read-through: 2000 files under django/   largest blob: tests/gis_tests/data/rasters/raster.numpy.txt (709050 bytes)   copy: 3688 files

| step | native worktree | baseline | passthrough code, no cap | passthrough, 4 096-entry cache | passthrough | + writeback cache | + prewarm | passthrough + prewarm | journal (ADR 0017) | journal + prewarm | passthrough + prewarm + journal | passthrough + journal |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| mount | 1.300 s | 0.087 s | 0.091 s | 0.090 s | 0.095 s | 0.091 s | 0.084 s | 0.089 s | 0.094 s | 0.099 s | 0.085 s | 0.088 s |
| prewarm (waited) | – | – | – | – | – | – | 0.129 s | 0.132 s | – | 0.135 s | 0.132 s | – |
| read 2000 files, cold | 0.041 s | 0.695 s | 0.726 s | 0.638 s | 0.651 s | 0.709 s | 0.470 s | 0.432 s | 0.748 s | 0.509 s | 0.424 s | 0.640 s |
| read again, warm | 0.040 s | 0.255 s | 0.269 s | 0.270 s | 0.272 s | 0.271 s | 0.259 s | 0.269 s | 0.264 s | 0.274 s | 0.267 s | 0.268 s |
| `rg -F TODO`, first (35 lines) | 0.021 s | 0.218 s | 0.221 s | 0.248 s | 0.223 s | 0.228 s | 0.169 s | 0.168 s | 0.256 s | 0.192 s | 0.164 s | 0.209 s |
| `rg -F TODO`, second | 0.018 s | 0.082 s | 0.083 s | 0.219 s | 0.090 s | 0.082 s | 0.082 s | 0.086 s | 0.086 s | 0.091 s | 0.084 s | 0.084 s |
| read largest blob, cold | 0.006 s | 0.005 s | 0.005 s | 0.009 s | 0.006 s | 0.005 s | 0.005 s | 0.006 s | 0.006 s | 0.006 s | 0.006 s | 0.005 s |
| read largest blob, warm | 0.005 s | 0.005 s | 0.005 s | 0.006 s | 0.006 s | 0.005 s | 0.005 s | 0.007 s | 0.006 s | 0.006 s | 0.005 s | 0.005 s |
| write 64 MiB, 4 KiB `dd` | 0.072 s | 3.943 s | 3.970 s | 0.848 s | 0.861 s | 0.839 s | 4.029 s | 0.854 s | 2.522 s | 2.617 s | 0.839 s | 0.843 s |
| `cp -r` 3688 files in | 0.788 s | 11.345 s | 10.345 s | 9.964 s | 10.095 s | 10.982 s | 10.238 s | 10.070 s | 3.390 s | 3.277 s | 2.723 s | 2.747 s |
| read the 64 MiB back | 0.010 s | 0.049 s | 0.054 s | 0.009 s | 0.011 s | 0.053 s | 0.059 s | 0.009 s | 0.059 s | 0.068 s | 0.009 s | 0.009 s |
| read the copied files back | 0.097 s | 1.335 s | 1.345 s | 1.008 s | 1.032 s | 1.356 s | 1.351 s | 1.012 s | 1.378 s | 1.412 s | 1.003 s | 1.021 s |
| `git status` after the writes | 0.118 s | 0.448 s | 0.458 s | 0.469 s | 0.474 s | 0.451 s | 0.455 s | 0.470 s | 0.467 s | 0.486 s | 0.457 s | 0.480 s |
| `git add -A` + commit | 1.063 s | 8.976 s | 9.002 s | 8.716 s | 8.813 s | 10.305 s | 9.029 s | 1.959 s | 2.557 s | 2.493 s | 1.941 s | 2.140 s |
| open+close, base blob | 2.7 µs | 62.9 µs | 64.1 µs | 66.8 µs | 65.9 µs | 64.4 µs | 63.0 µs | 65.3 µs | 64.9 µs | 68.5 µs | 65.6 µs | 67.2 µs |
| open+read+close, base blob | 3.5 µs | 66.3 µs | 65.6 µs | 68.6 µs | 70.8 µs | 66.3 µs | 64.8 µs | 67.5 µs | 66.5 µs | 67.3 µs | 67.8 µs | 70.0 µs |
| stat, cached | 1.8 µs | 2.0 µs | 1.9 µs | 1.9 µs | 1.9 µs | 1.9 µs | 1.9 µs | 1.9 µs | 1.9 µs | 1.9 µs | 1.9 µs | 1.9 µs |
| write 4 KiB, overlay file | 2.9 µs | 243.9 µs | 244.4 µs | 51.8 µs | 58.6 µs | 51.9 µs | 250.6 µs | 51.3 µs | 154.5 µs | 160.4 µs | 51.5 µs | 52.9 µs |
| open+close, overlay file | 2.7 µs | 72.7 µs | 73.1 µs | 78.9 µs | 86.0 µs | 73.0 µs | 73.6 µs | 81.9 µs | 76.4 µs | 77.0 µs | 79.3 µs | 82.8 µs |
| open+read+close, overlay file | 5.1 µs | 218.4 µs | 219.9 µs | 85.7 µs | 87.5 µs | 223.5 µs | 220.9 µs | 88.2 µs | 244.6 µs | 238.9 µs | 84.4 µs | 87.2 µs |
<!-- merged tables end -->

## What the numbers say

**Commit 1, passthrough, without the capability: nothing moved.** Every
step is within run-to-run noise of the baseline, and `gfs inspect` reports
zero passthrough opens. The probe is one read of `/proc/self/status` at
`init`; no ioctl is attempted.

**Commit 1, passthrough, with the capability: the write path and the
overlay read path.** `gfs inspect` reports 77 516 opens served by
passthrough on vscode and zero reads or writes through the daemon for them.
A 4 KiB `write(2)` goes from 244 µs to 52 µs with no writeback cache at
all, because the kernel writes the content file directly; reopening and
reading an overlay file from 216 µs to 87 µs; reading the 64 MiB back from
48 ms to 9 ms; the copied tree back from 2.8 s to 1.8 s; `cp -r` and the
commit gain a tenth to a fifth. Base blobs gain almost nothing warm — the
page cache already served them with no request — and a few microseconds
are added per open (the kernel opens the backing file behind the
descriptor), which is why the base-blob per-op rows are 2–5 µs higher.

The first privileged run had a regression the second column fixes: the
second `rg` over vscode took 0.66 s against 0.16 s. The backing-file cache
held 4 096 registrations, vscode has 17 926 files, so a full-tree pass
evicted everything and every open on the next pass re-inflated its blob
(the daemon had dropped its own copy in favour of the memfd). At 65 536
registrations — kernel-side file references, not daemon descriptors, and
the 256 MiB bytes cap still bounds memory — the second pass is 0.18 s.

**Commit 2, writeback cache: the small-write path, and only that.** A 4 KiB
`write(2)` goes from 244 µs to 52 µs and the 64 MiB `dd` from 3.95 s to
0.85 s on both corpora, because 16 384 write requests — each a journal
commit — become a few hundred. Nothing else moves: `cp -r` of ten thousand
files is still 29 s, because its cost is one `create`, one `release` and
their journal rows per file, not the write in between; the read side is
untouched. The first run of this column regressed every `open`+`close` by
40 µs: with the writeback cache on, the kernel ignores `FOPEN_NOFLUSH` and
sends a `flush` per close again. Answering `flush` with `ENOSYS` once makes
the kernel stop sending it for the life of the mount, which is what the
column shows. The flag stays off by default because it moves a refused
write's error from `write(2)` to `close` (see the ADR).

**Commit 3, prewarm: the cold read path, at the price of a third of a
second.** The walk inflates vscode's whole tree — 17 336 blobs, 232 MB —
in 0.38 s on eight blocking workers (django: 6 289 blobs in 0.12 s), and the
first read of 2 000 files drops from 0.75 s to 0.49 s, the first `rg` from
0.65 s to 0.40 s. What remains above the warm figure (0.26 s) is the
`read` request itself: prewarm fills the daemon's memory, not the kernel's
page cache, and only passthrough removes that request. Everything else is
unchanged. It is a flag because a job that opens a hundred files gains
nothing from inflating seventeen thousand.

**Against a native worktree.** With everything on, the mount is created
13× faster than the worktree (0.20 s against 2.49 s on vscode) and holds
no copy of the tree; from there every operation costs more than ext4, by
an amount that is now almost entirely round trips per file, not bytes:

| vscode | native | passthrough + prewarm, pre-0017 | passthrough + prewarm + journal | ratio now |
| --- | ---: | ---: | ---: | ---: |
| read 2 000 files, first time | 0.041 s | 0.452 s | 0.491 s | 12× |
| read again | 0.040 s | 0.273 s | 0.280 s | 7× |
| `rg` over the tree, first | 0.041 s | 0.413 s | 0.404 s | 10× |
| `rg` again | 0.034 s | 0.182 s | 0.162 s | 5× |
| 64 MiB in 4 KiB writes | 0.070 s | 0.894 s | 0.871 s | 12× |
| `cp -r` 10 225 files in | 1.10 s | 26.1 s | 6.41 s | 5.8× |
| read those files back | 0.20 s | 1.80 s | 1.76 s | 9× |
| `git status` | 0.55 s | 0.76 s | 0.74 s | 1.4× |
| `git add -A` + commit | 1.14 s | 3.78 s | 3.62 s | 3.2× |
| open+close | 2.7 µs | 65 µs | 65 µs | 24× |
| 4 KiB write | 3.0 µs | 52 µs | 51 µs | 17× |

Per file, ext4 answers an `open`+`close` in 2.7 µs and the mount in 65 µs:
two kernel round trips at this machine's 16–25 µs floor plus the daemon's
work, which is the ADR 0014 residue and the reason a whole-tree read is
about 10× native regardless of how warm the blobs are. Per 4 KiB write the
gap is 3 µs against 52 µs: passthrough removed the request, and what is left
is the page-cache write through a backing file plus the kernel's attribute
bookkeeping on the FUSE inode after each write. The one thing that was
*not* round trips was the `cp -r` gap, 24× before ADR 0017; with the
journal work behind `create` and `release` gone it is 5.8×, below the read
side, because passthrough also removed the `write` requests in between. `git status` is the closest to native because fsmonitor
answers from the journal and Git walks nothing.

**Commit 4, the journal (ADR 0017): the per-file write path, without any
capability.** The per-file cost was never the `write`: it was two fsyncs
per created content file (about 1.5 ms of waiting), one journal
transaction for the row, one or two more for the parent directory's
timestamp, and one per `write(2)` for the row's size. Removing the fsyncs
(the journal was already at `synchronous = NORMAL`, so the bytes were
stricter than the row naming them), folding the parent touch into the
child's transaction, and committing a written row once at `release`
instead of per write takes `cp -r` of 10 225 files from 29.4 s to 8.9 s on
vscode and 3 688 files from 11.3 s to 3.4 s on django, and a 4 KiB write
from 244 µs to 155 µs (the request itself, now with no commit behind it),
so the 64 MiB `dd` goes from 3.95 s to 2.6 s. Reads, `git status` and the
per-open rows are unchanged — nothing in that path was touched. Measured
by hand on a django mount, old and new binary back to back: `create`+`close`
2 350 → 416 µs wall, `mkdir` 456 → 321 µs, `unlink` 483 → 320 µs, `rename`
708 → 465 µs. The rest of a `create` — about 410 µs of daemon CPU for one
30 µs transaction and two round trips — is the daemon's own code around
the commit and wants a CPU profile next, not another lever.

**Where the cost is now.** Per file read: `open` and `release`, at the
kernel's floor, plus one `read` on the first open (gone under passthrough).
Per file written by a tool like `cp`: about 0.6 ms on vscode with
passthrough (0.85 ms without), of which the journal transaction is 30 µs
and the two round trips about 65 µs; the remainder is daemon CPU in the
create and release paths. The two levers stack as expected: ADR 0017 alone
takes the copy from 29.4 s to 8.9 s, passthrough on top to 6.2 s, and the
64 MiB `dd` is passthrough's alone (2.6 s → 0.84 s).
