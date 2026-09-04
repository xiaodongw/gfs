## vscode — passthrough + prewarm + journal
mount flags: --prewarm   read-through: 2000 files under src/   largest blob: src/vs/base/test/node/uri.perf.data.txt (8874330 bytes)   copy: 10225 files

| step | passthrough + prewarm + journal |
| --- | ---: |
| mount | 0.166 s |
| prewarm (waited) | 0.452 s |
| read 2000 files, cold | 0.491 s |
| read again, warm | 0.280 s |
| `rg -F TODO`, first (1683 lines) | 0.404 s |
| `rg -F TODO`, second | 0.162 s |
| read largest blob, cold | 0.007 s |
| read largest blob, warm | 0.006 s |
| write 64 MiB, 4 KiB `dd` | 0.871 s |
| `cp -r` 10225 files in | 6.411 s |
| read the 64 MiB back | 0.010 s |
| read the copied files back | 1.761 s |
| `git status` after the writes | 0.744 s |
| `git add -A` + commit | 3.621 s |
| open+close, base blob | 64.8 µs |
| open+read+close, base blob | 67.1 µs |
| stat, cached | 1.9 µs |
| write 4 KiB, overlay file | 50.6 µs |
| open+close, overlay file | 79.3 µs |
| open+read+close, overlay file | 87.0 µs |

    kernel     77516 opens served by passthrough
    prewarm    done, 17336 blobs, 232323636 bytes in 435 ms
anchors left: 0

## django — passthrough + prewarm + journal
mount flags: --prewarm   read-through: 2000 files under django/   largest blob: tests/gis_tests/data/rasters/raster.numpy.txt (709050 bytes)   copy: 3688 files

| step | passthrough + prewarm + journal |
| --- | ---: |
| mount | 0.085 s |
| prewarm (waited) | 0.132 s |
| read 2000 files, cold | 0.424 s |
| read again, warm | 0.267 s |
| `rg -F TODO`, first (35 lines) | 0.164 s |
| `rg -F TODO`, second | 0.084 s |
| read largest blob, cold | 0.006 s |
| read largest blob, warm | 0.005 s |
| write 64 MiB, 4 KiB `dd` | 0.839 s |
| `cp -r` 3688 files in | 2.723 s |
| read the 64 MiB back | 0.009 s |
| read the copied files back | 1.003 s |
| `git status` after the writes | 0.457 s |
| `git add -A` + commit | 1.941 s |
| open+close, base blob | 65.6 µs |
| open+read+close, base blob | 67.8 µs |
| stat, cached | 1.9 µs |
| write 4 KiB, overlay file | 51.5 µs |
| open+close, overlay file | 79.3 µs |
| open+read+close, overlay file | 84.4 µs |

    kernel     37128 opens served by passthrough
    prewarm    done, 6289 blobs, 46688486 bytes in 125 ms
anchors left: 0

