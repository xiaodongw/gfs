## vscode — mutations inline, 4 threads
mount flags: --dispatch mutations-inline   read-through: 2000 files under src/   largest blob: src/vs/base/test/node/uri.perf.data.txt (8874330 bytes)   copy: 10225 files

| step | mutations inline, 4 threads |
| --- | ---: |
| mount | 0.221 s |
| prewarm (waited) | – |
| read 2000 files, cold | 0.865 s |
| read again, warm | 0.284 s |
| `rg -F TODO`, first (1683 lines) | 0.691 s |
| `rg -F TODO`, second | 0.173 s |
| read largest blob, cold | 0.007 s |
| read largest blob, warm | 0.007 s |
| write 64 MiB, 4 KiB `dd` | 1.912 s |
| `cp -r` 10225 files in | 6.190 s |
| read the 64 MiB back | 0.096 s |
| read the copied files back | 2.993 s |
| `git status` after the writes | 0.779 s |
| `git add -A` + commit | 4.891 s |
| open+close, base blob | 64.1 µs |
| open+read+close, base blob | 65.9 µs |
| stat, cached | 1.9 µs |
| write 4 KiB, overlay file | 107.2 µs |
| open+close, overlay file | 74.8 µs |
| open+read+close, overlay file | 225.8 µs |

    kernel     0 opens served by passthrough
anchors left: 0

## django — mutations inline, 4 threads
mount flags: --dispatch mutations-inline   read-through: 2000 files under django/   largest blob: tests/gis_tests/data/rasters/raster.numpy.txt (709050 bytes)   copy: 3688 files

| step | mutations inline, 4 threads |
| --- | ---: |
| mount | 0.091 s |
| prewarm (waited) | – |
| read 2000 files, cold | 0.721 s |
| read again, warm | 0.268 s |
| `rg -F TODO`, first (35 lines) | 0.224 s |
| `rg -F TODO`, second | 0.084 s |
| read largest blob, cold | 0.005 s |
| read largest blob, warm | 0.005 s |
| write 64 MiB, 4 KiB `dd` | 1.785 s |
| `cp -r` 3688 files in | 2.379 s |
| read the 64 MiB back | 0.056 s |
| read the copied files back | 1.374 s |
| `git status` after the writes | 0.464 s |
| `git add -A` + commit | 2.359 s |
| open+close, base blob | 64.7 µs |
| open+read+close, base blob | 67.1 µs |
| stat, cached | 1.8 µs |
| write 4 KiB, overlay file | 105.5 µs |
| open+close, overlay file | 73.3 µs |
| open+read+close, overlay file | 229.2 µs |

    kernel     0 opens served by passthrough
anchors left: 0

