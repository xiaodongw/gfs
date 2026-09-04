## vscode — + prewarm
mount flags: --prewarm   read-through: 2000 files under src/   largest blob: src/vs/base/test/node/uri.perf.data.txt (8874330 bytes)   copy: 10225 files

| step | + prewarm |
| --- | ---: |
| mount | 0.170 s |
| prewarm (waited) | 0.373 s |
| read 2000 files, cold | 0.493 s |
| read again, warm | 0.263 s |
| `rg -F TODO`, first (1683 lines) | 0.399 s |
| `rg -F TODO`, second | 0.160 s |
| read largest blob, cold | 0.006 s |
| read largest blob, warm | 0.005 s |
| write 64 MiB, 4 KiB `dd` | 4.042 s |
| `cp -r` 10225 files in | 26.861 s |
| read the 64 MiB back | 0.057 s |
| read the copied files back | 2.812 s |
| `git status` after the writes | 0.727 s |
| `git add -A` + commit | 4.991 s |
| open+close, base blob | 62.9 µs |
| open+read+close, base blob | 65.8 µs |
| stat, cached | 1.9 µs |
| write 4 KiB, overlay file | 249.1 µs |
| open+close, overlay file | 73.2 µs |
| open+read+close, overlay file | 220.8 µs |

    kernel     0 opens served by passthrough
    prewarm    done, 17336 blobs, 232323636 bytes in 380 ms
anchors left: 0

## django — + prewarm
mount flags: --prewarm   read-through: 2000 files under django/   largest blob: tests/gis_tests/data/rasters/raster.numpy.txt (709050 bytes)   copy: 3688 files

| step | + prewarm |
| --- | ---: |
| mount | 0.084 s |
| prewarm (waited) | 0.129 s |
| read 2000 files, cold | 0.470 s |
| read again, warm | 0.259 s |
| `rg -F TODO`, first (35 lines) | 0.169 s |
| `rg -F TODO`, second | 0.082 s |
| read largest blob, cold | 0.005 s |
| read largest blob, warm | 0.005 s |
| write 64 MiB, 4 KiB `dd` | 4.029 s |
| `cp -r` 3688 files in | 10.238 s |
| read the 64 MiB back | 0.059 s |
| read the copied files back | 1.351 s |
| `git status` after the writes | 0.455 s |
| `git add -A` + commit | 9.029 s |
| open+close, base blob | 63.0 µs |
| open+read+close, base blob | 64.8 µs |
| stat, cached | 1.9 µs |
| write 4 KiB, overlay file | 250.6 µs |
| open+close, overlay file | 73.6 µs |
| open+read+close, overlay file | 220.9 µs |

    kernel     0 opens served by passthrough
    prewarm    done, 6289 blobs, 46688486 bytes in 120 ms
anchors left: 0

