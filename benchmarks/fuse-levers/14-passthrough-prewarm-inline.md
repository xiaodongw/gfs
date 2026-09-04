## vscode — passthrough + prewarm + inline mutations
mount flags: --prewarm   read-through: 2000 files under src/   largest blob: src/vs/base/test/node/uri.perf.data.txt (8874330 bytes)   copy: 10225 files

| step | passthrough + prewarm + inline mutations |
| --- | ---: |
| mount | 0.205 s |
| prewarm (waited) | 0.449 s |
| read 2000 files, cold | 0.460 s |
| read again, warm | 0.279 s |
| `rg -F TODO`, first (1683 lines) | 0.392 s |
| `rg -F TODO`, second | 0.163 s |
| read largest blob, cold | 0.006 s |
| read largest blob, warm | 0.006 s |
| write 64 MiB, 4 KiB `dd` | 0.874 s |
| `cp -r` 10225 files in | 5.789 s |
| read the 64 MiB back | 0.010 s |
| read the copied files back | 1.821 s |
| `git status` after the writes | 0.839 s |
| `git add -A` + commit | 3.854 s |
| open+close, base blob | 66.5 µs |
| open+read+close, base blob | 69.5 µs |
| stat, cached | 1.9 µs |
| write 4 KiB, overlay file | 55.8 µs |
| open+close, overlay file | 83.0 µs |
| open+read+close, overlay file | 88.9 µs |

    kernel     77516 opens served by passthrough
    prewarm    done, 17336 blobs, 232323636 bytes in 447 ms
anchors left: 0

## django — passthrough + prewarm + inline mutations
mount flags: --prewarm   read-through: 2000 files under django/   largest blob: tests/gis_tests/data/rasters/raster.numpy.txt (709050 bytes)   copy: 3688 files

| step | passthrough + prewarm + inline mutations |
| --- | ---: |
| mount | 0.093 s |
| prewarm (waited) | 0.136 s |
| read 2000 files, cold | 0.445 s |
| read again, warm | 0.281 s |
| `rg -F TODO`, first (35 lines) | 0.178 s |
| `rg -F TODO`, second | 0.091 s |
| read largest blob, cold | 0.006 s |
| read largest blob, warm | 0.007 s |
| write 64 MiB, 4 KiB `dd` | 0.906 s |
| `cp -r` 3688 files in | 2.364 s |
| read the 64 MiB back | 0.012 s |
| read the copied files back | 1.045 s |
| `git status` after the writes | 0.484 s |
| `git add -A` + commit | 2.031 s |
| open+close, base blob | 66.0 µs |
| open+read+close, base blob | 68.9 µs |
| stat, cached | 2.0 µs |
| write 4 KiB, overlay file | 56.3 µs |
| open+close, overlay file | 82.8 µs |
| open+read+close, overlay file | 89.7 µs |

    kernel     37128 opens served by passthrough
    prewarm    done, 6289 blobs, 46688486 bytes in 141 ms
anchors left: 0

