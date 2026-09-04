## vscode — passthrough + journal
mount flags: (none)   read-through: 2000 files under src/   largest blob: src/vs/base/test/node/uri.perf.data.txt (8874330 bytes)   copy: 10225 files

| step | passthrough + journal |
| --- | ---: |
| mount | 0.160 s |
| prewarm (waited) | – |
| read 2000 files, cold | 0.652 s |
| read again, warm | 0.267 s |
| `rg -F TODO`, first (1683 lines) | 0.602 s |
| `rg -F TODO`, second | 0.161 s |
| read largest blob, cold | 0.006 s |
| read largest blob, warm | 0.005 s |
| write 64 MiB, 4 KiB `dd` | 0.837 s |
| `cp -r` 10225 files in | 6.213 s |
| read the 64 MiB back | 0.009 s |
| read the copied files back | 1.747 s |
| `git status` after the writes | 0.743 s |
| `git add -A` + commit | 3.630 s |
| open+close, base blob | 65.1 µs |
| open+read+close, base blob | 67.2 µs |
| stat, cached | 1.9 µs |
| write 4 KiB, overlay file | 52.4 µs |
| open+close, overlay file | 87.7 µs |
| open+read+close, overlay file | 88.7 µs |

    kernel     77516 opens served by passthrough
anchors left: 0

## django — passthrough + journal
mount flags: (none)   read-through: 2000 files under django/   largest blob: tests/gis_tests/data/rasters/raster.numpy.txt (709050 bytes)   copy: 3688 files

| step | passthrough + journal |
| --- | ---: |
| mount | 0.088 s |
| prewarm (waited) | – |
| read 2000 files, cold | 0.640 s |
| read again, warm | 0.268 s |
| `rg -F TODO`, first (35 lines) | 0.209 s |
| `rg -F TODO`, second | 0.084 s |
| read largest blob, cold | 0.005 s |
| read largest blob, warm | 0.005 s |
| write 64 MiB, 4 KiB `dd` | 0.843 s |
| `cp -r` 3688 files in | 2.747 s |
| read the 64 MiB back | 0.009 s |
| read the copied files back | 1.021 s |
| `git status` after the writes | 0.480 s |
| `git add -A` + commit | 2.140 s |
| open+close, base blob | 67.2 µs |
| open+read+close, base blob | 70.0 µs |
| stat, cached | 1.9 µs |
| write 4 KiB, overlay file | 52.9 µs |
| open+close, overlay file | 82.8 µs |
| open+read+close, overlay file | 87.2 µs |

    kernel     37128 opens served by passthrough
anchors left: 0

