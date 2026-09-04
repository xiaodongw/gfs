## vscode — journal (ADR 0017)
mount flags: (none)   read-through: 2000 files under src/   largest blob: src/vs/base/test/node/uri.perf.data.txt (8874330 bytes)   copy: 10225 files

| step | journal (ADR 0017) |
| --- | ---: |
| mount | 0.292 s |
| prewarm (waited) | – |
| read 2000 files, cold | 0.758 s |
| read again, warm | 0.275 s |
| `rg -F TODO`, first (1683 lines) | 0.693 s |
| `rg -F TODO`, second | 0.162 s |
| read largest blob, cold | 0.008 s |
| read largest blob, warm | 0.006 s |
| write 64 MiB, 4 KiB `dd` | 2.585 s |
| `cp -r` 10225 files in | 8.861 s |
| read the 64 MiB back | 0.062 s |
| read the copied files back | 2.865 s |
| `git status` after the writes | 0.782 s |
| `git add -A` + commit | 4.896 s |
| open+close, base blob | 64.3 µs |
| open+read+close, base blob | 66.0 µs |
| stat, cached | 1.9 µs |
| write 4 KiB, overlay file | 154.8 µs |
| open+close, overlay file | 74.5 µs |
| open+read+close, overlay file | 220.9 µs |

    kernel     0 opens served by passthrough
anchors left: 0

## django — journal (ADR 0017)
mount flags: (none)   read-through: 2000 files under django/   largest blob: tests/gis_tests/data/rasters/raster.numpy.txt (709050 bytes)   copy: 3688 files

| step | journal (ADR 0017) |
| --- | ---: |
| mount | 0.094 s |
| prewarm (waited) | – |
| read 2000 files, cold | 0.748 s |
| read again, warm | 0.264 s |
| `rg -F TODO`, first (35 lines) | 0.256 s |
| `rg -F TODO`, second | 0.086 s |
| read largest blob, cold | 0.006 s |
| read largest blob, warm | 0.006 s |
| write 64 MiB, 4 KiB `dd` | 2.522 s |
| `cp -r` 3688 files in | 3.390 s |
| read the 64 MiB back | 0.059 s |
| read the copied files back | 1.378 s |
| `git status` after the writes | 0.467 s |
| `git add -A` + commit | 2.557 s |
| open+close, base blob | 64.9 µs |
| open+read+close, base blob | 66.5 µs |
| stat, cached | 1.9 µs |
| write 4 KiB, overlay file | 154.5 µs |
| open+close, overlay file | 76.4 µs |
| open+read+close, overlay file | 244.6 µs |

    kernel     0 opens served by passthrough
anchors left: 0

