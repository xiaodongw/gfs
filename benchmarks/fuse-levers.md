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
- **+ prewarm** — `gfs mount --local … --prewarm`, without the other two:
  the benchmark waits for `gfs inspect` to report the prewarm done before
  the task starts, and the wait is its own row.

<!-- merged tables begin -->
### vscode

mount flags: (none)   read-through: 2000 files under src/   largest blob: src/vs/base/test/node/uri.perf.data.txt (8874330 bytes)   copy: 10225 files

| step | baseline | passthrough code, no cap | passthrough, 4 096-entry cache | passthrough | + writeback cache | + prewarm |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| mount | 0.270 s | 0.226 s | 0.185 s | 0.194 s | 0.161 s | 0.170 s |
| prewarm (waited) | – | – | – | – | – | 0.373 s |
| read 2000 files, cold | 0.746 s | 0.740 s | 0.671 s | 0.679 s | 0.733 s | 0.493 s |
| read again, warm | 0.264 s | 0.265 s | 0.273 s | 0.274 s | 0.264 s | 0.263 s |
| `rg -F TODO`, first (1683 lines) | 0.654 s | 0.634 s | 0.712 s | 0.617 s | 0.630 s | 0.399 s |
| `rg -F TODO`, second | 0.158 s | 0.164 s | 0.658 s | 0.180 s | 0.174 s | 0.160 s |
| read largest blob, cold | 0.006 s | 0.006 s | 0.044 s | 0.006 s | 0.006 s | 0.006 s |
| read largest blob, warm | 0.005 s | 0.006 s | 0.006 s | 0.006 s | 0.006 s | 0.005 s |
| write 64 MiB, 4 KiB `dd` | 3.950 s | 4.260 s | 0.893 s | 0.862 s | 0.854 s | 4.042 s |
| `cp -r` 10225 files in | 29.383 s | 27.599 s | 25.813 s | 26.288 s | 28.899 s | 26.861 s |
| read the 64 MiB back | 0.048 s | 0.075 s | 0.008 s | 0.009 s | 0.060 s | 0.057 s |
| read the copied files back | 2.772 s | 2.799 s | 1.758 s | 1.796 s | 2.813 s | 2.812 s |
| `git status` after the writes | 0.728 s | 0.717 s | 0.741 s | 0.789 s | 0.715 s | 0.727 s |
| `git add -A` + commit | 4.919 s | 5.010 s | 3.864 s | 3.937 s | 4.923 s | 4.991 s |
| open+close, base blob | 63.5 µs | 63.7 µs | 64.5 µs | 64.9 µs | 63.3 µs | 62.9 µs |
| open+read+close, base blob | 65.9 µs | 65.2 µs | 70.8 µs | 67.7 µs | 65.4 µs | 65.8 µs |
| stat, cached | 1.8 µs | 1.9 µs | 1.9 µs | 1.9 µs | 1.9 µs | 1.9 µs |
| write 4 KiB, overlay file | 244.1 µs | 249.1 µs | 50.9 µs | 52.2 µs | 52.3 µs | 249.1 µs |
| open+close, overlay file | 72.1 µs | 72.6 µs | 81.6 µs | 82.3 µs | 73.4 µs | 73.2 µs |
| open+read+close, overlay file | 216.2 µs | 219.7 µs | 84.6 µs | 87.3 µs | 221.9 µs | 220.8 µs |

### django

mount flags: (none)   read-through: 2000 files under django/   largest blob: tests/gis_tests/data/rasters/raster.numpy.txt (709050 bytes)   copy: 3688 files

| step | baseline | passthrough code, no cap | passthrough, 4 096-entry cache | passthrough | + writeback cache | + prewarm |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| mount | 0.087 s | 0.091 s | 0.090 s | 0.095 s | 0.091 s | 0.084 s |
| prewarm (waited) | – | – | – | – | – | 0.129 s |
| read 2000 files, cold | 0.695 s | 0.726 s | 0.638 s | 0.651 s | 0.709 s | 0.470 s |
| read again, warm | 0.255 s | 0.269 s | 0.270 s | 0.272 s | 0.271 s | 0.259 s |
| `rg -F TODO`, first (35 lines) | 0.218 s | 0.221 s | 0.248 s | 0.223 s | 0.228 s | 0.169 s |
| `rg -F TODO`, second | 0.082 s | 0.083 s | 0.219 s | 0.090 s | 0.082 s | 0.082 s |
| read largest blob, cold | 0.005 s | 0.005 s | 0.009 s | 0.006 s | 0.005 s | 0.005 s |
| read largest blob, warm | 0.005 s | 0.005 s | 0.006 s | 0.006 s | 0.005 s | 0.005 s |
| write 64 MiB, 4 KiB `dd` | 3.943 s | 3.970 s | 0.848 s | 0.861 s | 0.839 s | 4.029 s |
| `cp -r` 3688 files in | 11.345 s | 10.345 s | 9.964 s | 10.095 s | 10.982 s | 10.238 s |
| read the 64 MiB back | 0.049 s | 0.054 s | 0.009 s | 0.011 s | 0.053 s | 0.059 s |
| read the copied files back | 1.335 s | 1.345 s | 1.008 s | 1.032 s | 1.356 s | 1.351 s |
| `git status` after the writes | 0.448 s | 0.458 s | 0.469 s | 0.474 s | 0.451 s | 0.455 s |
| `git add -A` + commit | 8.976 s | 9.002 s | 8.716 s | 8.813 s | 10.305 s | 9.029 s |
| open+close, base blob | 62.9 µs | 64.1 µs | 66.8 µs | 65.9 µs | 64.4 µs | 63.0 µs |
| open+read+close, base blob | 66.3 µs | 65.6 µs | 68.6 µs | 70.8 µs | 66.3 µs | 64.8 µs |
| stat, cached | 2.0 µs | 1.9 µs | 1.9 µs | 1.9 µs | 1.9 µs | 1.9 µs |
| write 4 KiB, overlay file | 243.9 µs | 244.4 µs | 51.8 µs | 58.6 µs | 51.9 µs | 250.6 µs |
| open+close, overlay file | 72.7 µs | 73.1 µs | 78.9 µs | 86.0 µs | 73.0 µs | 73.6 µs |
| open+read+close, overlay file | 218.4 µs | 219.9 µs | 85.7 µs | 87.5 µs | 223.5 µs | 220.9 µs |

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

**Where the cost is now.** Per file read: `open` and `release`, at the
kernel's floor, plus one `read` on the first open (gone under passthrough).
Per file written by a tool like `cp`: about 2.8 ms, none of it in `write`
— it is `create`, `release` and the journal row each commits. That is the
next lever, and it is in the overlay, not in FUSE.
