## vscode — baseline
mount flags: (none)   read-through: 2000 files under src/   largest blob: src/vs/base/test/node/uri.perf.data.txt (8874330 bytes)   copy: 10225 files

| step | baseline |
| --- | ---: |
| mount | 0.270 s |
| prewarm (waited) | – |
| read 2000 files, cold | 0.746 s |
| read again, warm | 0.264 s |
| `rg -F TODO`, first (1683 lines) | 0.654 s |
| `rg -F TODO`, second | 0.158 s |
| read largest blob, cold | 0.006 s |
| read largest blob, warm | 0.005 s |
| write 64 MiB, 4 KiB `dd` | 3.950 s |
| `cp -r` 10225 files in | 29.383 s |
| read the 64 MiB back | 0.048 s |
| read the copied files back | 2.772 s |
| `git status` after the writes | 0.728 s |
| `git add -A` + commit | 4.919 s |
| open+close, base blob | 63.5 µs |
| open+read+close, base blob | 65.9 µs |
| stat, cached | 1.8 µs |
| write 4 KiB, overlay file | 244.1 µs |
| open+close, overlay file | 72.1 µs |
| open+read+close, overlay file | 216.2 µs |

anchors left: 0

## django — baseline
mount flags: (none)   read-through: 2000 files under django/   largest blob: tests/gis_tests/data/rasters/raster.numpy.txt (709050 bytes)   copy: 3688 files

| step | baseline |
| --- | ---: |
| mount | 0.087 s |
| prewarm (waited) | – |
| read 2000 files, cold | 0.695 s |
| read again, warm | 0.255 s |
| `rg -F TODO`, first (35 lines) | 0.218 s |
| `rg -F TODO`, second | 0.082 s |
| read largest blob, cold | 0.005 s |
| read largest blob, warm | 0.005 s |
| write 64 MiB, 4 KiB `dd` | 3.943 s |
| `cp -r` 3688 files in | 11.345 s |
| read the 64 MiB back | 0.049 s |
| read the copied files back | 1.335 s |
| `git status` after the writes | 0.448 s |
| `git add -A` + commit | 8.976 s |
| open+close, base blob | 62.9 µs |
| open+read+close, base blob | 66.3 µs |
| stat, cached | 2.0 µs |
| write 4 KiB, overlay file | 243.9 µs |
| open+close, overlay file | 72.7 µs |
| open+read+close, overlay file | 218.4 µs |

anchors left: 0

