## vscode — passthrough, 4 096-entry cache
mount flags: (none)   read-through: 2000 files under src/   largest blob: src/vs/base/test/node/uri.perf.data.txt (8874330 bytes)   copy: 10225 files

| step | passthrough |
| --- | ---: |
| mount | 0.185 s |
| prewarm (waited) | – |
| read 2000 files, cold | 0.671 s |
| read again, warm | 0.273 s |
| `rg -F TODO`, first (1683 lines) | 0.712 s |
| `rg -F TODO`, second | 0.658 s |
| read largest blob, cold | 0.044 s |
| read largest blob, warm | 0.006 s |
| write 64 MiB, 4 KiB `dd` | 0.893 s |
| `cp -r` 10225 files in | 25.813 s |
| read the 64 MiB back | 0.008 s |
| read the copied files back | 1.758 s |
| `git status` after the writes | 0.741 s |
| `git add -A` + commit | 3.864 s |
| open+close, base blob | 64.5 µs |
| open+read+close, base blob | 70.8 µs |
| stat, cached | 1.9 µs |
| write 4 KiB, overlay file | 50.9 µs |
| open+close, overlay file | 81.6 µs |
| open+read+close, overlay file | 84.6 µs |

    kernel     77516 opens served by passthrough
anchors left: 0

## django — passthrough, 4 096-entry cache
mount flags: (none)   read-through: 2000 files under django/   largest blob: tests/gis_tests/data/rasters/raster.numpy.txt (709050 bytes)   copy: 3688 files

| step | passthrough |
| --- | ---: |
| mount | 0.090 s |
| prewarm (waited) | – |
| read 2000 files, cold | 0.638 s |
| read again, warm | 0.270 s |
| `rg -F TODO`, first (35 lines) | 0.248 s |
| `rg -F TODO`, second | 0.219 s |
| read largest blob, cold | 0.009 s |
| read largest blob, warm | 0.006 s |
| write 64 MiB, 4 KiB `dd` | 0.848 s |
| `cp -r` 3688 files in | 9.964 s |
| read the 64 MiB back | 0.009 s |
| read the copied files back | 1.008 s |
| `git status` after the writes | 0.469 s |
| `git add -A` + commit | 8.716 s |
| open+close, base blob | 66.8 µs |
| open+read+close, base blob | 68.6 µs |
| stat, cached | 1.9 µs |
| write 4 KiB, overlay file | 51.8 µs |
| open+close, overlay file | 78.9 µs |
| open+read+close, overlay file | 85.7 µs |

    kernel     37128 opens served by passthrough
anchors left: 0

