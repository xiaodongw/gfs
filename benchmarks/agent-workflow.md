# The agent edit workflow, raw Git against GFS

Date: 2026-08-21
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

| step | raw git full | raw git shallow+blobless | GFS | GFS before the cache tree | raw result | GFS result |
| --- | ---: | ---: | ---: | ---: | --- | --- |
| acquire | 56.983 s | 10.164 s | **0.258 s** | 0.246 s | clone | mount |
| `log -10` | 0.007 s | 0.007 s | 0.066 s | 0.053 s | 10 commits | 10 commits |
| `ls-files '*test*'` | 0.007 s | 0.006 s | 0.026 s | 0.020 s | 0 files, see below | 6 245 files |
| grep `TODO`, cold index | 0.043 s | 0.047 s | 5.881 s | 5.786 s | 1 683 lines | error, see below |
| grep `TODO`, warm | 0.043 s | 0.047 s | 0.259 s | 0.248 s | 1 683 lines | 1 574 lines |
| edit 5 files | 0.007 s | 0.007 s | 0.064 s | 0.070 s | | |
| `git status`, cold | 0.041 s | 0.042 s | 1.701 s | 2.344 s | | |
| `git status`, warm | 0.036 s | 0.030 s | 0.070 s | 0.297 s | | |
| `gfs status` | – | – | **0.011 s** | 0.010 s | | journal |
| commit | 0.957 s | 0.946 s | **1.953 s** | 14.501 s | | |
| **total** | **58.044 s** | **11.219 s** | **4.327 s** | 17.482 s | | |
| **local disk** | 1 648 MiB | 363 MiB | **3.5 MB** + 66.9 MB host cache | 22 MB + 147 MB | | |

## Results: django

| step | raw git full | raw git shallow+blobless | GFS | GFS before the cache tree |
| --- | ---: | ---: | ---: | ---: |
| acquire | 11.298 s | 2.646 s | **0.116 s** | 0.108 s |
| `log -10` | 0.007 s | 0.006 s | 0.046 s | 0.046 s |
| `ls-files '*test*'` | 0.008 s | 0.007 s | 0.015 s (2 621 files, both) | 0.014 s |
| grep `TODO`, cold index | 0.019 s | 0.018 s | 1.576 s (35 lines, both) | 1.546 s |
| grep `TODO`, warm | 0.019 s | 0.018 s | 0.036 s | 0.035 s |
| edit 5 files | 0.006 s | 0.006 s | 0.056 s | 0.055 s |
| `git status`, cold | 0.130 s | 0.136 s | 1.190 s | 1.436 s |
| `git status`, warm | 0.035 s | 0.036 s | 0.041 s | 0.119 s |
| `gfs status` | – | – | **0.011 s** | 0.010 s |
| commit | 0.055 s | 0.055 s | **1.372 s** | 9.705 s |
| **total** | **11.523 s** | **2.873 s** | **2.831 s** | 11.399 s |
| **local disk** | 338 MiB | 68 MiB | **1.4 MB** + 25.0 MB host cache | 15.6 MB + 46.9 MB |

Commit correctness: **PASS**, both flows produced tree
`c297292656bb794d6e231778d3f9272d22a52c03`.

## What the numbers say

**The whole task is now cheaper than the cheapest clone that still works.**
On vscode the full workflow takes **4.327 s** against 11.219 s for a shallow
blobless clone and 58.044 s for a full one, and on django **2.831 s** against
2.873 s and 11.523 s. Two fixes got it there, and neither was where the step's
name pointed.

**The first full-tree walk was the whole cost, and it is gone.** The first
`git status` in a fresh vscode workspace used to take **555 s**, because it walks
every directory once to populate the untracked cache — 5 328 uncached listings,
serialized, at 38–126 ms apiece. That was a *server* cost with nothing to do with
reading trees: every snapshot request re-decided object authorization by
enumerating and peeling all 73 989 refs (24–28 ms), around a directory read that
costs ~2.5 µs. Deciding that once per ref generation, and answering a recognized
walk with one recursive `ListTree` instead of one call per directory, leaves cold
`status` at **1.701 s**. `gfs inspect` shows the mechanism at the end of the
vscode run:

```
metadata   32 server requests, 5 directory pages, 34225 listing hits
prefetch   1 walks in 3 pages filling 4318 listings, 1 directories read ahead
```

Five per-directory listings for a repository with 4 318 directories: the walk
detector fired after four misses and the rest arrived in three pages.

**The first commit was rewriting the repository, not reading it.** With the walk
fixed, `commit` still cost **14.501 s** on vscode and 9.705 s on django, and the
reason was one missing index extension. The shipped index carried no `TREE`
cache tree, so Git could not tell that any directory was unchanged: the *first*
commit in a workspace re-derived every tree in the repository and wrote each one
out — 4 299 loose objects for a five-file change, 4 275 of them duplicates of
objects the projection already served. It compounded with a second defect:
`utime()` on a projected pack returned `EROFS`, and Git reads a failed freshen as
"cannot vouch for this object", so it wrote a copy rather than reusing the packed
one. Shipping the cache tree and accepting the freshen leaves commit at
**1.953 s** on vscode and **1.372 s** on django, writing 25 and 14 objects.

The same change is most of why local disk fell from 22 MB to **3.5 MB** on
vscode, and why the object store fetched 65 MB instead of 146 MB: Git is no
longer reading the whole tree through the projection to recompute what it
already had. The sub-second `status` steps also improved between the two runs;
that is run-to-run variance on this machine, not a claim of this change.

**Cold is now within a rounding error of warm.** Measured on its own — a `find`
over every directory, nothing else running — the cold walk is **1.71 s** against
1.41 s warm on vscode, and **1.27 s** against 1.06 s on django, down from
1 131 s and 33.0 s. What is left is FUSE and the walker, not the network. The
old listing cache was also too small to hold a monorepo: bounded at 4 096
directories against vscode's 4 318, the baseline walk re-fetched what it had
already listed, 7 283 listings for 4 318 directories.

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
- **`ls-files` reports 0 for the raw clone on vscode this run.** The clone's
  checkout failed under the git-lfs smudge filter (the same failure the commit
  disagreement above describes) and left no index behind, so the pathspec
  matched nothing; running `ls-files` in the leftover clone *after* its commit
  rebuilt the index reports 6 146. This is variance in the baseline leg, not in
  GFS — the 2026-08-17 run's clone wrote an index and reported 6 146 at this
  step.
- **The harness now waits for the index before timing the warm search.** Without
  that wait an earlier run of this same build recorded 2.385 s on vscode, which
  is the tail of the index build rather than a query: the cold query fails with
  `SNAPSHOT_BUILDING` and the warm one used to start immediately after. Measured
  against a ready index the query is 0.243–0.248 s across runs, and the table's
  0.248 s is one of them. The cold step keeps its own timing and its error,
  which is the property it exists to measure.
- Clone times exclude network transfer. Over a real network the gap widens well
  past the wall-clock ratio — and widens further for the walk, which used to be
  thousands of serialized round trips and is now a handful.
- Prefetching moved 1.26 MB of file content speculatively on vscode (one
  directory read through during the edit step), which is most of the 1.27 MB
  this workflow hydrated at all. This benchmark barely reads file content, so it
  measures the metadata half of prefetching and does not vindicate the content
  half.
- **The corpus is still the public stand-in set.** Every number moves when
  `spikes/corpus/corpus.conf` points at the real monorepos. Open since M0.1.
- The harness selects text files for the edit set. An earlier run picked `.mo`
  locale catalogs and `git apply` could not apply a binary patch without a full
  index line.
