## vscode — passthrough
mount flags: (none)   read-through: 2000 files under src/   largest blob: src/vs/base/test/node/uri.perf.data.txt (8874330 bytes)   copy: 10225 files

| step | passthrough |
| --- | ---: |
| mount | 0.194 s |
| prewarm (waited) | – |
| read 2000 files, cold | 0.679 s |
| read again, warm | 0.274 s |
| `rg -F TODO`, first (1683 lines) | 0.617 s |
| `rg -F TODO`, second | 0.180 s |
| read largest blob, cold | 0.006 s |
| read largest blob, warm | 0.006 s |
| write 64 MiB, 4 KiB `dd` | 0.862 s |
| `cp -r` 10225 files in | 26.288 s |
| read the 64 MiB back | 0.009 s |
| read the copied files back | 1.796 s |
| `git status` after the writes | 0.789 s |
| `git add -A` + commit | 3.937 s |
| open+close, base blob | 64.9 µs |
| open+read+close, base blob | 67.7 µs |
| stat, cached | 1.9 µs |
| write 4 KiB, overlay file | 52.2 µs |
| open+close, overlay file | 82.3 µs |
| open+read+close, overlay file | 87.3 µs |

    kernel     77516 opens served by passthrough
anchors left: 0

## django — passthrough
mount flags: (none)   read-through: 2000 files under django/   largest blob: tests/gis_tests/data/rasters/raster.numpy.txt (709050 bytes)   copy: 3688 files

| step | passthrough |
| --- | ---: |
| mount | 0.095 s |
| prewarm (waited) | – |
| read 2000 files, cold | 0.651 s |
| read again, warm | 0.272 s |
| `rg -F TODO`, first (35 lines) | 0.223 s |
| `rg -F TODO`, second | 0.090 s |
| read largest blob, cold | 0.006 s |
| read largest blob, warm | 0.006 s |
| write 64 MiB, 4 KiB `dd` | 0.861 s |
| `cp -r` 3688 files in | 10.095 s |
| read the 64 MiB back | 0.011 s |
| read the copied files back | 1.032 s |
| `git status` after the writes | 0.474 s |
| `git add -A` + commit | 8.813 s |
| open+close, base blob | 65.9 µs |
| open+read+close, base blob | 70.8 µs |
| stat, cached | 1.9 µs |
| write 4 KiB, overlay file | 58.6 µs |
| open+close, overlay file | 86.0 µs |
| open+read+close, overlay file | 87.5 µs |

    kernel     37128 opens served by passthrough
anchors left: 0

