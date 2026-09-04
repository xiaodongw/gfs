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
- **passthrough** — the same build after
  `sudo setcap cap_sys_admin+ep target/release/gfs-fuse`.
- **+ writeback cache** — [ADR 0016](../docs/adr/0016-writeback-cache.md),
  mounted with `--writeback-cache`, without the capability (the two levers
  are independent; this column isolates the second).

<!-- merged tables begin -->
### vscode

mount flags: (none)   read-through: 2000 files under src/   largest blob: src/vs/base/test/node/uri.perf.data.txt (8874330 bytes)   copy: 10225 files

| step | baseline | passthrough code, no cap | + writeback cache |
| --- | ---: | ---: | ---: |
| mount | 0.270 s | 0.226 s | 0.161 s |
| prewarm (waited) | – | – | – |
| read 2000 files, cold | 0.746 s | 0.740 s | 0.733 s |
| read again, warm | 0.264 s | 0.265 s | 0.264 s |
| `rg -F TODO`, first (1683 lines) | 0.654 s | 0.634 s | 0.630 s |
| `rg -F TODO`, second | 0.158 s | 0.164 s | 0.174 s |
| read largest blob, cold | 0.006 s | 0.006 s | 0.006 s |
| read largest blob, warm | 0.005 s | 0.006 s | 0.006 s |
| write 64 MiB, 4 KiB `dd` | 3.950 s | 4.260 s | 0.854 s |
| `cp -r` 10225 files in | 29.383 s | 27.599 s | 28.899 s |
| read the 64 MiB back | 0.048 s | 0.075 s | 0.060 s |
| read the copied files back | 2.772 s | 2.799 s | 2.813 s |
| `git status` after the writes | 0.728 s | 0.717 s | 0.715 s |
| `git add -A` + commit | 4.919 s | 5.010 s | 4.923 s |
| open+close, base blob | 63.5 µs | 63.7 µs | 63.3 µs |
| open+read+close, base blob | 65.9 µs | 65.2 µs | 65.4 µs |
| stat, cached | 1.8 µs | 1.9 µs | 1.9 µs |
| write 4 KiB, overlay file | 244.1 µs | 249.1 µs | 52.3 µs |
| open+close, overlay file | 72.1 µs | 72.6 µs | 73.4 µs |
| open+read+close, overlay file | 216.2 µs | 219.7 µs | 221.9 µs |

### django

mount flags: (none)   read-through: 2000 files under django/   largest blob: tests/gis_tests/data/rasters/raster.numpy.txt (709050 bytes)   copy: 3688 files

| step | baseline | passthrough code, no cap | + writeback cache |
| --- | ---: | ---: | ---: |
| mount | 0.087 s | 0.091 s | 0.091 s |
| prewarm (waited) | – | – | – |
| read 2000 files, cold | 0.695 s | 0.726 s | 0.709 s |
| read again, warm | 0.255 s | 0.269 s | 0.271 s |
| `rg -F TODO`, first (35 lines) | 0.218 s | 0.221 s | 0.228 s |
| `rg -F TODO`, second | 0.082 s | 0.083 s | 0.082 s |
| read largest blob, cold | 0.005 s | 0.005 s | 0.005 s |
| read largest blob, warm | 0.005 s | 0.005 s | 0.005 s |
| write 64 MiB, 4 KiB `dd` | 3.943 s | 3.970 s | 0.839 s |
| `cp -r` 3688 files in | 11.345 s | 10.345 s | 10.982 s |
| read the 64 MiB back | 0.049 s | 0.054 s | 0.053 s |
| read the copied files back | 1.335 s | 1.345 s | 1.356 s |
| `git status` after the writes | 0.448 s | 0.458 s | 0.451 s |
| `git add -A` + commit | 8.976 s | 9.002 s | 10.305 s |
| open+close, base blob | 62.9 µs | 64.1 µs | 64.4 µs |
| open+read+close, base blob | 66.3 µs | 65.6 µs | 66.3 µs |
| stat, cached | 2.0 µs | 1.9 µs | 1.9 µs |
| write 4 KiB, overlay file | 243.9 µs | 244.4 µs | 51.9 µs |
| open+close, overlay file | 72.7 µs | 73.1 µs | 73.0 µs |
| open+read+close, overlay file | 218.4 µs | 219.9 µs | 223.5 µs |

<!-- merged tables end -->

## What the numbers say

**Commit 1, passthrough, without the capability: nothing moved.** Every
step is within run-to-run noise of the baseline, and `gfs inspect` reports
zero passthrough opens. The probe is one read of `/proc/self/status` at
`init`; no ioctl is attempted.

**Commit 1, passthrough, with the capability:** pending — needs the
capability set on the binary, which is a privileged one-time action outside
this build.

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
