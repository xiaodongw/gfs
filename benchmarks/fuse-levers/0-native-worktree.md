## vscode — native worktree
mount flags: native worktree   read-through: 2000 files under src/   largest blob: src/vs/base/test/node/uri.perf.data.txt (8874330 bytes)   copy: 10225 files

| step | native worktree |
| --- | ---: |
| mount | 2.488 s |
| prewarm (waited) | – |
| read 2000 files, cold | 0.041 s |
| read again, warm | 0.040 s |
| `rg -F TODO`, first (1683 lines) | 0.041 s |
| `rg -F TODO`, second | 0.034 s |
| read largest blob, cold | 0.006 s |
| read largest blob, warm | 0.007 s |
| write 64 MiB, 4 KiB `dd` | 0.070 s |
| `cp -r` 10225 files in | 1.100 s |
| read the 64 MiB back | 0.009 s |
| read the copied files back | 0.202 s |
| `git status` after the writes | 0.549 s |
| `git add -A` + commit | 1.140 s |
| open+close, base blob | 2.7 µs |
| open+read+close, base blob | 3.6 µs |
| stat, cached | 1.8 µs |
| write 4 KiB, overlay file | 3.0 µs |
| open+close, overlay file | 2.7 µs |
| open+read+close, overlay file | 5.0 µs |

anchors left: 0

## django — native worktree
mount flags: native worktree   read-through: 2000 files under django/   largest blob: tests/gis_tests/data/rasters/raster.numpy.txt (709050 bytes)   copy: 3688 files

| step | native worktree |
| --- | ---: |
| mount | 1.300 s |
| prewarm (waited) | – |
| read 2000 files, cold | 0.041 s |
| read again, warm | 0.040 s |
| `rg -F TODO`, first (35 lines) | 0.021 s |
| `rg -F TODO`, second | 0.018 s |
| read largest blob, cold | 0.006 s |
| read largest blob, warm | 0.005 s |
| write 64 MiB, 4 KiB `dd` | 0.072 s |
| `cp -r` 3688 files in | 0.788 s |
| read the 64 MiB back | 0.010 s |
| read the copied files back | 0.097 s |
| `git status` after the writes | 0.118 s |
| `git add -A` + commit | 1.063 s |
| open+close, base blob | 2.7 µs |
| open+read+close, base blob | 3.5 µs |
| stat, cached | 1.8 µs |
| write 4 KiB, overlay file | 2.9 µs |
| open+close, overlay file | 2.7 µs |
| open+read+close, overlay file | 5.1 µs |

anchors left: 0

