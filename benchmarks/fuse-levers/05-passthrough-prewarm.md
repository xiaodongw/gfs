## vscode — passthrough + prewarm
mount flags: --prewarm   read-through: 2000 files under src/   largest blob: src/vs/base/test/node/uri.perf.data.txt (8874330 bytes)   copy: 10225 files

| step | passthrough + prewarm |
| --- | ---: |
| mount | 0.196 s |
| prewarm (waited) | 0.450 s |
| read 2000 files, cold | 0.452 s |
| read again, warm | 0.273 s |
| `rg -F TODO`, first (1683 lines) | 0.413 s |
| `rg -F TODO`, second | 0.182 s |
| read largest blob, cold | 0.007 s |
| read largest blob, warm | 0.006 s |
| write 64 MiB, 4 KiB `dd` | 0.894 s |
| `cp -r` 10225 files in | 26.060 s |
| read the 64 MiB back | 0.008 s |
| read the copied files back | 1.796 s |
| `git status` after the writes | 0.762 s |
| `git add -A` + commit | 3.782 s |
| open+close, base blob | 65.3 µs |
| open+read+close, base blob | 68.3 µs |
| stat, cached | 2.0 µs |
| write 4 KiB, overlay file | 51.6 µs |
| open+close, overlay file | 79.7 µs |
| open+read+close, overlay file | 86.1 µs |

    kernel     77516 opens served by passthrough
    prewarm    done, 17336 blobs, 232323636 bytes in 454 ms
anchors left: 0

## django — passthrough + prewarm
mount flags: --prewarm   read-through: 2000 files under django/   largest blob: tests/gis_tests/data/rasters/raster.numpy.txt (709050 bytes)   copy: 3688 files

| step | passthrough + prewarm |
| --- | ---: |
| mount | 0.089 s |
| prewarm (waited) | 0.132 s |
| read 2000 files, cold | 0.432 s |
| read again, warm | 0.269 s |
| `rg -F TODO`, first (35 lines) | 0.168 s |
| `rg -F TODO`, second | 0.086 s |
| read largest blob, cold | 0.006 s |
| read largest blob, warm | 0.007 s |
| write 64 MiB, 4 KiB `dd` | 0.854 s |
| `cp -r` 3688 files in | 10.070 s |
| read the 64 MiB back | 0.009 s |
| read the copied files back | 1.012 s |
| `git status` after the writes | 0.470 s |
| `git add -A` + commit | 1.959 s |
| open+close, base blob | 65.3 µs |
| open+read+close, base blob | 67.5 µs |
| stat, cached | 1.9 µs |
| write 4 KiB, overlay file | 51.3 µs |
| open+close, overlay file | 81.9 µs |
| open+read+close, overlay file | 88.2 µs |

    kernel     37128 opens served by passthrough
    prewarm    done, 6289 blobs, 46688486 bytes in 124 ms
anchors left: 0

