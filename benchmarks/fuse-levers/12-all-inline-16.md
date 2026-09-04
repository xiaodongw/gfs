## vscode — all inline, 16 threads
mount flags: --fuse-threads 16 --dispatch all-inline   read-through: 2000 files under src/   largest blob: src/vs/base/test/node/uri.perf.data.txt (8874330 bytes)   copy: 10225 files

| step | all inline, 16 threads |
| --- | ---: |
| mount | 0.174 s |
| prewarm (waited) | – |
| read 2000 files, cold | 0.767 s |
| read again, warm | 0.278 s |
| `rg -F TODO`, first (1683 lines) | 1.634 s |
| `rg -F TODO`, second | 0.745 s |
| read largest blob, cold | 0.006 s |
| read largest blob, warm | 0.005 s |
| write 64 MiB, 4 KiB `dd` | 1.840 s |
| `cp -r` 10225 files in | 5.881 s |
| read the 64 MiB back | 0.058 s |
| read the copied files back | 3.139 s |
| `git status` after the writes | 1.089 s |
| `git add -A` + commit | 5.545 s |
| open+close, base blob | 69.6 µs |
| open+read+close, base blob | 68.2 µs |
| stat, cached | 1.9 µs |
| write 4 KiB, overlay file | 119.0 µs |
| open+close, overlay file | 75.9 µs |
| open+read+close, overlay file | 245.1 µs |

    kernel     0 opens served by passthrough
anchors left: 0

## django — all inline, 16 threads
mount flags: --fuse-threads 16 --dispatch all-inline   read-through: 2000 files under django/   largest blob: tests/gis_tests/data/rasters/raster.numpy.txt (709050 bytes)   copy: 3688 files

| step | all inline, 16 threads |
| --- | ---: |
| mount | 0.102 s |
| prewarm (waited) | – |
| read 2000 files, cold | 0.761 s |
| read again, warm | 0.272 s |
| `rg -F TODO`, first (35 lines) | 0.693 s |
| `rg -F TODO`, second | 0.375 s |
| read largest blob, cold | 0.007 s |
| read largest blob, warm | 0.006 s |
| write 64 MiB, 4 KiB `dd` | 1.927 s |
| `cp -r` 3688 files in | 2.477 s |
| read the 64 MiB back | 0.057 s |
| read the copied files back | 1.372 s |
| `git status` after the writes | 0.508 s |
| `git add -A` + commit | 2.375 s |
| open+close, base blob | 64.1 µs |
| open+read+close, base blob | 65.6 µs |
| stat, cached | 1.9 µs |
| write 4 KiB, overlay file | 108.4 µs |
| open+close, overlay file | 73.6 µs |
| open+read+close, overlay file | 223.8 µs |

    kernel     0 opens served by passthrough
anchors left: 0

