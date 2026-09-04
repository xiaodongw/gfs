## vscode — + writeback cache
mount flags: --writeback-cache   read-through: 2000 files under src/   largest blob: src/vs/base/test/node/uri.perf.data.txt (8874330 bytes)   copy: 10225 files

| step | + writeback cache |
| --- | ---: |
| mount | 0.161 s |
| prewarm (waited) | – |
| read 2000 files, cold | 0.733 s |
| read again, warm | 0.264 s |
| `rg -F TODO`, first (1683 lines) | 0.630 s |
| `rg -F TODO`, second | 0.174 s |
| read largest blob, cold | 0.006 s |
| read largest blob, warm | 0.006 s |
| write 64 MiB, 4 KiB `dd` | 0.854 s |
| `cp -r` 10225 files in | 28.899 s |
| read the 64 MiB back | 0.060 s |
| read the copied files back | 2.813 s |
| `git status` after the writes | 0.715 s |
| `git add -A` + commit | 4.923 s |
| open+close, base blob | 63.3 µs |
| open+read+close, base blob | 65.4 µs |
| stat, cached | 1.9 µs |
| write 4 KiB, overlay file | 52.3 µs |
| open+close, overlay file | 73.4 µs |
| open+read+close, overlay file | 221.9 µs |

    kernel     0 opens served by passthrough
anchors left: 0

## django — + writeback cache
mount flags: --writeback-cache   read-through: 2000 files under django/   largest blob: tests/gis_tests/data/rasters/raster.numpy.txt (709050 bytes)   copy: 3688 files

| step | + writeback cache |
| --- | ---: |
| mount | 0.091 s |
| prewarm (waited) | – |
| read 2000 files, cold | 0.709 s |
| read again, warm | 0.271 s |
| `rg -F TODO`, first (35 lines) | 0.228 s |
| `rg -F TODO`, second | 0.082 s |
| read largest blob, cold | 0.005 s |
| read largest blob, warm | 0.005 s |
| write 64 MiB, 4 KiB `dd` | 0.839 s |
| `cp -r` 3688 files in | 10.982 s |
| read the 64 MiB back | 0.053 s |
| read the copied files back | 1.356 s |
| `git status` after the writes | 0.451 s |
| `git add -A` + commit | 10.305 s |
| open+close, base blob | 64.4 µs |
| open+read+close, base blob | 66.3 µs |
| stat, cached | 1.9 µs |
| write 4 KiB, overlay file | 51.9 µs |
| open+close, overlay file | 73.0 µs |
| open+read+close, overlay file | 223.5 µs |

    kernel     0 opens served by passthrough
anchors left: 0

