# The agent edit workflow, raw Git against GFS

Date: 2026-07-27
Reproduce: `./spikes/corpus/benchmark-workflow.sh django`

[`baseline.md`](baseline.md) measures the **clone**, which is the first step of a
task and the one GFS wins by the widest margin. This measures the **whole
task**, because the ranking changes once search is in it:

```
acquire a workspace -> git log -10 -> start a branch ->
find by name -> grep by content -> edit files -> status -> commit
```

The clone is where GFS wins. Search is where it can give all of it back.

## Machine and corpus

| | |
| --- | --- |
| Host | WSL2, Linux 6.18.33.2-microsoft-standard-WSL2, 32 cores, 46 GiB |
| Git | 2.53.0 |
| ripgrep | 15.2.0 (a real binary; see `baseline.md`'s note on `rg` as a shell function) |
| Repository | `django`, 788 MiB mirror, 34 814 commits, **29 298 refs**, 7 077 files at tip |
| Base commit | `c2517faff335f683e1cbe55d9844910b3fb40670` |

Clones run over `file://` against a local bare copy, so no step carries internet
variance. Both flows apply the same five-change edit set — two modifies, one
add, one delete, one rename — to the **same four paths**, chosen once from
`git ls-tree` and passed to both.

## Results

Every step's *result* is recorded next to its time. A faster search that returns
a different answer is not a faster search.

| step | raw git full | raw git `--depth 10` | GFS | raw result | GFS result |
| --- | ---: | ---: | ---: | --- | --- |
| acquire | 12.367 s | 1.836 s | **0.211 s** | clone | mount |
| `log -10` | 0.006 s | 0.006 s | 0.554 s | 10 commits | 10 commits |
| find (`*test*`) | 0.007 s | 0.007 s | 0.192 s | 2 621 files | 2 621 files |
| grep (`TODO`) | 0.017 s | 0.017 s | 1.042 s | 35 lines | 35 lines |
| edit 5 files | 0.005 s | 0.005 s | 0.559 s | | |
| `status` | 0.116 s | 0.116 s | **0.009 s** | | |
| commit | 0.054 s | 0.054 s | 0.078 s | | export + apply |
| **total** | **12.572 s** | **2.041 s** | **2.644 s** | | |

| | raw git full | raw git `--depth 10` | GFS |
| --- | ---: | ---: | ---: |
| local disk | 338 MiB | 58 MiB | **266 KiB** state + 9 KiB cache |
| bytes fetched | ~294 MiB | ~14 MiB | **8 828 bytes, 4 blobs** |

**Correctness: the two flows produce the identical tree**,
`d9416486b601990add98ac423b64c3f5dc21c926`, including the rename. Each search
step returns the same count as the tool it replaces.

### Warm numbers

The table above is one cold run: a server that has just imported the repository,
an unbuilt search index, and a first gRPC connection. That is the honest cost of
the *first* job. A second job on the same server pays none of it:

| | cold (table above) | warm, median of 3 |
| --- | ---: | ---: |
| mount | 0.211 s | 0.211 s |
| `gfs log -10` | 0.554 s | **0.069 s** |
| `gfs find '*test*'` | 0.192 s | **0.029 s** |
| `gfs rg -F TODO` | 1.042 s | **0.043 s** |

Warm hydration is **0 blobs, 0 bytes**: none of the three tools reads the mount.

## What the numbers say

**GFS wins the task, not just the clone.** 2.6 s cold and well under 1 s warm,
against 12.6 s for a full clone and 2.0 s for the cheapest raw-git option that
can still answer `git log -10`. Disk is the larger margin: 266 KiB against
338 MiB, because nothing is materialized that the task did not read.

**`--depth 1` is not a competitor for this workflow.** It clones in 1.8 s but
`git log -10` returns **one** commit. `--depth 10` is the honest comparison and
costs 2.0 s.

**`status` is the one step GFS wins outright on a warm clone** — 0.009 s against
0.116 s — because it is derived from the overlay journal and touches no base
metadata, where Git stats every index entry.

**The searches are slower per call and that is the correct trade.** `rg` over a
materialized tree is 0.017 s; `gfs rg` is 0.043 s warm. The comparison is not
0.017 against 0.043, it is 0.017 **plus 12.4 s of clone and 338 MiB** against
0.043 plus 0.2 s and 266 KiB.

**The server's first import is a real cost and is not in the table.** Importing
django took ~50 s, almost all of it reconciling 29 298 refs. It is one-time and
amortized across every job on that repository — a restart against warm state is
0.23 s — but it scales with ref count, and django is a high-ref repository for
its size.

## History

An earlier revision of this workflow, before `gfs find` and `gfs log` existed
and while `gfs rg` could not find its own workspace, measured **~53 s** for the
GFS column: the filename step went through the `git` shim's `ls-files`, which
issued one snapshot-API round trip per directory (28.9–53.7 s for 7 077 files),
and the content step returned nothing at all. The three tools were introduced to
delegate those questions to the server; see
[`docs/agent-search.md`](../docs/agent-search.md).

## Caveats

- One run per configuration except where "median of 3" is stated. Clone times in
  `baseline.md` varied under 1 %; the sub-second steps here vary more.
- Clone times exclude network transfer. Over a real network the 294 MiB against
  8 828 bytes gap widens well past the wall-clock ratio.
- The edit step is slower on GFS (0.559 s cold against 0.005 s) because five
  writes go through FUSE. That is a per-file cost this edit set is too small to
  characterize.
- **The corpus is still the public stand-in set.** Every number moves when
  `spikes/corpus/corpus.conf` points at the real monorepos. Open since M0.1.
- The harness selects text files for the edit set. An earlier run picked `.mo`
  locale catalogs and the export failed: `git apply` cannot apply a binary patch
  without a full index line, which the M3 completion report already records as a
  known limitation of the bundle's patch half. The bundle's `content/` files
  carry those bytes exactly; only the patch representation is affected.
