## vscode — passthrough code, no cap
mount flags: (none)   read-through: 2000 files under src/   largest blob: src/vs/base/test/node/uri.perf.data.txt (8874330 bytes)   copy: 10225 files

| step | passthrough code, no cap |
| --- | ---: |
| mount | 0.226 s |
| prewarm (waited) | – |
| read 2000 files, cold | 0.740 s |
| read again, warm | 0.265 s |
| `rg -F TODO`, first (1683 lines) | 0.634 s |
| `rg -F TODO`, second | 0.164 s |
| read largest blob, cold | 0.006 s |
| read largest blob, warm | 0.006 s |
| write 64 MiB, 4 KiB `dd` | 4.260 s |
| `cp -r` 10225 files in | 27.599 s |
| read the 64 MiB back | 0.075 s |
| read the copied files back | 2.799 s |
| `git status` after the writes | 0.717 s |
| `git add -A` + commit | 5.010 s |
| open+close, base blob | 63.7 µs |
| open+read+close, base blob | 65.2 µs |
| stat, cached | 1.9 µs |
| write 4 KiB, overlay file | 249.1 µs |
| open+close, overlay file | 72.6 µs |
| open+read+close, overlay file | 219.7 µs |

anchors left: 0

## django — passthrough code, no cap
mount flags: (none)   read-through: 2000 files under django/   largest blob: tests/gis_tests/data/rasters/raster.numpy.txt (709050 bytes)   copy: 3688 files

| step | passthrough code, no cap |
| --- | ---: |
| mount | 0.091 s |
| prewarm (waited) | – |
| read 2000 files, cold | 0.726 s |
| read again, warm | 0.269 s |
| `rg -F TODO`, first (35 lines) | 0.221 s |
| `rg -F TODO`, second | 0.083 s |
| read largest blob, cold | 0.005 s |
| read largest blob, warm | 0.005 s |
| write 64 MiB, 4 KiB `dd` | 3.970 s |
| `cp -r` 3688 files in | 10.345 s |
| read the 64 MiB back | 0.054 s |
| read the copied files back | 1.345 s |
| `git status` after the writes | 0.458 s |
| `git add -A` + commit | 9.002 s |
| open+close, base blob | 64.1 µs |
| open+read+close, base blob | 65.6 µs |
| stat, cached | 1.9 µs |
| write 4 KiB, overlay file | 244.4 µs |
| open+close, overlay file | 73.1 µs |
| open+read+close, overlay file | 219.9 µs |

anchors left: 0

