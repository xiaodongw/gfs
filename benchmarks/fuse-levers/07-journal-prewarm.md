## vscode — journal + prewarm
mount flags: --prewarm   read-through: 2000 files under src/   largest blob: src/vs/base/test/node/uri.perf.data.txt (8874330 bytes)   copy: 10225 files

| step | journal + prewarm |
| --- | ---: |
| mount | 0.179 s |
| prewarm (waited) | 0.442 s |
| read 2000 files, cold | 0.523 s |
| read again, warm | 0.280 s |
| `rg -F TODO`, first (1683 lines) | 0.450 s |
| `rg -F TODO`, second | 0.177 s |
| read largest blob, cold | 0.008 s |
| read largest blob, warm | 0.007 s |
| write 64 MiB, 4 KiB `dd` | 2.661 s |
| `cp -r` 10225 files in | 7.886 s |
| read the 64 MiB back | 0.066 s |
| read the copied files back | 3.046 s |
| `git status` after the writes | 0.763 s |
| `git add -A` + commit | 5.113 s |
| open+close, base blob | 65.0 µs |
| open+read+close, base blob | 67.5 µs |
| stat, cached | 1.8 µs |
| write 4 KiB, overlay file | 163.0 µs |
| open+close, overlay file | 77.5 µs |
| open+read+close, overlay file | 236.4 µs |

    kernel     0 opens served by passthrough
    prewarm    done, 17336 blobs, 232323636 bytes in 424 ms
anchors left: 0

## django — journal + prewarm
mount flags: --prewarm   read-through: 2000 files under django/   largest blob: tests/gis_tests/data/rasters/raster.numpy.txt (709050 bytes)   copy: 3688 files

| step | journal + prewarm |
| --- | ---: |
| mount | 0.099 s |
| prewarm (waited) | 0.135 s |
| read 2000 files, cold | 0.509 s |
| read again, warm | 0.274 s |
| `rg -F TODO`, first (35 lines) | 0.192 s |
| `rg -F TODO`, second | 0.091 s |
| read largest blob, cold | 0.006 s |
| read largest blob, warm | 0.006 s |
| write 64 MiB, 4 KiB `dd` | 2.617 s |
| `cp -r` 3688 files in | 3.277 s |
| read the 64 MiB back | 0.068 s |
| read the copied files back | 1.412 s |
| `git status` after the writes | 0.486 s |
| `git add -A` + commit | 2.493 s |
| open+close, base blob | 68.5 µs |
| open+read+close, base blob | 67.3 µs |
| stat, cached | 1.9 µs |
| write 4 KiB, overlay file | 160.4 µs |
| open+close, overlay file | 77.0 µs |
| open+read+close, overlay file | 238.9 µs |

    kernel     0 opens served by passthrough
    prewarm    done, 6289 blobs, 46688486 bytes in 138 ms
anchors left: 0

