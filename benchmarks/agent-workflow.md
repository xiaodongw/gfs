# The agent edit workflow, raw Git against GFS

Date: 2026-08-17
Reproduce: `./spikes/corpus/benchmark-workflow.sh vscode django`

[`baseline.md`](baseline.md) measures the **clone**, which is the first step of a
task and the one GFS wins by the widest margin. This measures the **whole
task**, because the ranking changes once everything Git does afterwards is in
it:

```
acquire a workspace -> git log -10 -> find by name ->
grep by content -> edit files -> status -> commit
```

Since [ADR 0009](../docs/adr/0009-raw-git-over-a-projected-object-store.md) the
mount carries a real object database, so **both flows run stock Git in their own
working tree** — `log`, `ls-files`, `status` and `commit` are the same commands
on both sides. Only content search differs, because search is the one question
GFS deliberately answers somewhere else. An earlier revision of this report
measured `gfs log` and `gfs find`, which ADR 0009 deleted, and an
export-and-apply commit path that `git commit` replaced.

## Machine and corpus

| | |
| --- | --- |
| Host | WSL2, Linux 6.18.33.2-microsoft-standard-WSL2, 32 cores, 46 GiB |
| Git | 2.53.0 |
| ripgrep | 15.2.0 (a real binary; see `baseline.md`'s note on `rg` as a shell function) |
| Primary repository | `vscode`, 2 485 MiB mirror, **17 926 files**, **73 989 refs**, base `fc62850c022bbba2c93b0202dd24f42d1e0b3882` |
| Secondary repository | `django`, 803 MiB mirror, 7 078 files, 29 310 refs, base `274a1d494d11d87a1b767340d1f398f197810f93` |

Clones run over `file://` against a local bare copy, so no step carries internet
variance. Both flows apply the same five-change edit set — two modifies, one
add, one delete, one rename — to the **same four paths**, chosen once from
`git ls-tree` and passed to both.

## Results: vscode

| step | raw git full | raw git shallow+blobless | GFS | raw result | GFS result |
| --- | ---: | ---: | ---: | --- | --- |
| acquire | 55.803 s | 9.960 s | **0.300 s** | clone | mount |
| `log -10` | 0.006 s | 0.007 s | 1.503 s | 10 commits | 10 commits |
| `ls-files '*test*'` | 0.013 s | 0.013 s | 0.021 s | 6 146 files | 6 245 files |
| grep `TODO`, cold index | 0.039 s | 0.040 s | 6.426 s | 1 683 lines | error, see below |
| grep `TODO`, warm | 0.039 s | 0.040 s | 0.283 s | 1 683 lines | 1 574 lines |
| edit 5 files | 0.005 s | 0.005 s | 2.908 s | | |
| `git status`, cold | 0.032 s | 0.032 s | **555 s** | | |
| `git status`, warm | 0.033 s | 0.034 s | 1.753 s | | |
| `gfs status` | – | – | **0.0097 s** | | journal |
| commit | 0.937 s | 0.934 s | 1 448 s | | |
| **local disk** | 1 648 MiB | 363 MiB | **21 MB** + 145 MB host cache | | |

## Results: django

| step | raw git full | raw git shallow+blobless | GFS |
| --- | ---: | ---: | ---: |
| acquire | 10.987 s | 2.560 s | **0.180 s** |
| `log -10` | 0.006 s | 0.006 s | 0.241 s |
| `ls-files '*test*'` | 0.007 s | 0.008 s | 0.019 s (2 621 files, both) |
| grep `TODO`, cold index | 0.019 s | 0.018 s | 1.461 s (35 lines, both) |
| grep `TODO`, warm | 0.019 s | 0.018 s | 0.217 s |
| edit 5 files | 0.006 s | 0.005 s | 0.482 s |
| `git status`, cold | 0.128 s | 0.127 s | 7.229 s |
| `git status`, warm | 0.035 s | 0.124 s | 0.116 s |
| `gfs status` | – | – | **0.011 s** |
| commit | 0.053 s | 0.055 s | 9.811 s |
| **local disk** | 338 MiB | 68 MiB | **15.6 MB** + 46.7 MB host cache |

Commit correctness: **PASS**, both flows produced tree
`c297292656bb794d6e231778d3f9272d22a52c03`.

## What the numbers say

**The workspace is effectively free; the first full-tree walk is not.** 0.300 s
against 55.8 s to acquire, and 21 MB against 1 648 MiB on disk. But the first
`git status` in a fresh vscode workspace costs 555 s, and the first `commit`
1 448 s, because both walk every directory once to populate the untracked cache.
That is **5 328 uncached listings, serialized**. One listing measures 38–126 ms,
inside DESIGN.md section 11's 250 ms target — the target is per call, and a
monorepo's first walk multiplies it by several thousand. Warm, the same command
is 1.75 s. Prefetching that walk is the open item this benchmark argues for.

**A repository-wide search moves no file bytes.** The hydration counters are
byte-identical either side of searching all 17 926 files:

| after | working tree | object store |
| --- | ---: | ---: |
| mount | 0 B | 0 B |
| `log -10` + `ls-files` | 122 B | 7.96 MB |
| grep `TODO` over the whole repository | 122 B | 7.96 MB |

**Search says what it did not read.** `gfs rg` returned 1 574 lines against
ripgrep's 1 683 and reported the difference itself, on stderr:
`412 of 17925 paths in scope were not searched: 312 binary, 98 lfs, 2 oversized`.

**An unready index is an error, not an empty result.** The first search of the
vscode run returned nothing, and the audit log records why:
`outcome="error" error_code="SNAPSHOT_BUILDING"`. This is the property section
7.5 exists to guarantee, and it survives only because the harness now keeps
stderr and the exit code — an earlier version discarded both and the run looked
like a silent empty answer.

**The clone is not automatically the reference answer.** On vscode the two flows
disagreed, and the GFS side was right: diffed against the base tree, the GFS
commit contains exactly the five intended changes; the clone's contains **105**
— the five, one renormalized YAML file, and **100 deleted**
`extensions/copilot/test/simulation/cache/*.sqlite` paths. Those are LFS
pointers; git-lfs is installed on the host, its smudge filter could not reach an
LFS endpoint through the `file://` mirror, and it left the files missing, which
`git add -A` recorded as deletions. The mount never runs a smudge filter, so it
had nothing to lose (ADR 0012).

**The server's first import used to dominate and no longer does.** Reconciling
vscode's 73 989 refs took **311.8 s** before the listeners bound, all of it
catalog bookkeeping — quadratic, because each observation scanned for its
repository's `MAX(ref_version)` against a table that did not index it. With
schema v2's `repository_refs_by_version` and a batched reconcile it is
**1.78 s**; django's is 0.679 s, from ~50 s.

## Caveats

- One run per configuration. Clone times in `baseline.md` varied under 1 %; the
  sub-second steps here vary more.
- The vscode step timings were taken with the pre-fix binary. Only the server
  import changed; it was re-measured against the same mirror afterwards.
- `ls-files` reports 6 146 for the clone against 6 245 in the mount **after**
  each side committed: the clone had already lost 99 matching LFS paths to the
  smudge failure above.
- Clone times exclude network transfer. Over a real network the gap widens well
  past the wall-clock ratio.
- The edit step is slower on GFS (2.9 s against 0.005 s on vscode) because five
  writes go through FUSE with copy-up.
- **The corpus is still the public stand-in set.** Every number moves when
  `spikes/corpus/corpus.conf` points at the real monorepos. Open since M0.1.
- The harness selects text files for the edit set. An earlier run picked `.mo`
  locale catalogs and `git apply` could not apply a binary patch without a full
  index line.
