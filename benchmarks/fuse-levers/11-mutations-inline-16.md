## vscode — mutations inline, 16 threads
mount flags: --fuse-threads 16 --dispatch mutations-inline   read-through: 2000 files under src/   largest blob: src/vs/base/test/node/uri.perf.data.txt (8874330 bytes)   copy: 10225 files

| step | mutations inline, 16 threads |
| --- | ---: |
| mount | 0.178 s |
| prewarm (waited) | – |
| read 2000 files, cold | 0.777 s |
| read again, warm | 0.286 s |
| `rg -F TODO`, first (1683 lines) | 1.645 s |
| `rg -F TODO`, second | 0.745 s |
| read largest blob, cold | 0.006 s |
| read largest blob, warm | 0.005 s |
| write 64 MiB, 4 KiB `dd` | 1.817 s |
| `cp -r` 10225 files in | 6.020 s |
| read the 64 MiB back | 0.067 s |
| read the copied files back | 2.890 s |
| `git status` after the writes | 0.804 s |
| `git add -A` + commit | 5.228 s |
| open+close, base blob | 69.0 µs |
| open+read+close, base blob | 72.4 µs |
| stat, cached | 1.9 µs |
| write 4 KiB, overlay file | 116.9 µs |
| open+close, overlay file | 77.9 µs |
| open+read+close, overlay file | 242.2 µs |

    kernel     0 opens served by passthrough
anchors left: 0

## django — mutations inline, 16 threads
mount flags: --fuse-threads 16 --dispatch mutations-inline   read-through: 2000 files under django/   largest blob: tests/gis_tests/data/rasters/raster.numpy.txt (709050 bytes)   copy: 3688 files

| step | mutations inline, 16 threads |
| --- | ---: |
| mount | 0.087 s |
| prewarm (waited) | – |
| read 2000 files, cold | 0.741 s |
| read again, warm | 0.274 s |
| `rg -F TODO`, first (35 lines) | 0.694 s |
| `rg -F TODO`, second | 0.379 s |
| read largest blob, cold | 0.005 s |
| read largest blob, warm | 0.005 s |
| write 64 MiB, 4 KiB `dd` | 1.819 s |
| `cp -r` 3688 files in | 2.511 s |
| read the 64 MiB back | 0.056 s |
| read the copied files back | 1.441 s |
| `git status` after the writes | 0.533 s |
| `git add -A` + commit | 2.455 s |
| open+close, base blob | 65.1 µs |
| open+read+close, base blob | 66.7 µs |
| stat, cached | 1.9 µs |
| write 4 KiB, overlay file | 115.4 µs |
| open+close, overlay file | 75.2 µs |
| open+read+close, overlay file | 241.4 µs |

    kernel     0 opens served by passthrough
anchors left: 0

