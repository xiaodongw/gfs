# M0.4 — Search representation spike

Milestone exit gate (PLAN.md M0.4):

> demonstrate correct results and acceptable projected storage/query cost, with
> retained-snapshot storage projected over the pilot's expected commit churn.

**Met.** Results match ripgrep exactly on the supported corpus, and the number
the gate actually turns on — manifest bytes per retained snapshot — is small
enough that on-demand search for arbitrary commits is clearly affordable.

Decision: [ADR 0004](../../docs/adr/0004-search-representation.md).

## How to reproduce

```sh
cd spikes
cargo build -p search-probe --release
S=./target/release/search-probe
M="$GFS_CORPUS_DIR/mirrors"
$S build     --repo $M/linux.git
$S manifests --repo $M/linux.git --commits 25 --retained 200
$S verify    --repo $M/vscode.git --patterns TODO RequestContext '```' 日本語
$S tantivy   --repo $M/vscode.git
$S query     --repo $M/vscode.git registerAction2 --limit 10
```

Machine: WSL2, 32 cores, 46 GiB RAM. ripgrep 14.1.1 built from crates.io.

## Index and manifest cost

| Repository | Tip files | Indexed content | Trigram postings | Postings / content | Manifest per snapshot |
| --- | ---: | ---: | ---: | ---: | ---: |
| vscode | 16 862 | 183.1 MiB | 45.9 MiB | 0.25 | **0.52 MiB** |
| linux (worst case) | 94 751 | 1385.5 MiB | 201.2 MiB | 0.15 | **1.99 MiB** |

Manifest composition for linux: path table 1.44 MiB, reverse table 0.54 MiB,
membership bitmap 16 KiB. The Roaring bitmap is negligible — the sorted path
table dominates, which is worth knowing because it is also the part that
compresses best if it ever needs to.

## The exit-gate projection

25 successive first-parent commits per repository, then projected across
concurrently retained snapshots:

| Repository | Mean manifest | 200 retained | New blobs per commit (median) |
| --- | ---: | ---: | ---: |
| vscode | 0.52 MiB | **0.10 GiB** | ~4 |
| linux | 1.99 MiB | **0.39 GiB** | ~39 |

Under half a gigabyte to keep 200 distinct commits of the Linux kernel
independently searchable. Snapshot manifests are not a reason to restrict search
to configured branch tips.

The shared blob registry does not grow with retained snapshots at all: it stayed
at 94 590 unique blobs for linux and 16 454 for vscode across all 25 commits.
That is the property the whole representation is built around — successive
commits add tens of blobs, not tens of thousands.

## Versus a per-snapshot Tantivy index

Same corpus policy on both sides (binary and oversized excluded identically), on
vscode:

| | Build time | On-disk | Cost for N retained snapshots |
| --- | ---: | ---: | --- |
| Per-snapshot Tantivy | 1 710 ms | 52.1 MiB | N × 52.1 MiB |
| Trigram + manifests | 7 131 ms | 45.9 MiB | 45.9 MiB + N × 0.52 MiB |

At 200 retained snapshots that is **10.2 GiB versus 150 MiB**, a factor of ~69.
The two are level at roughly N = 1, so the trigram representation is not paying
an up-front premium that only amortizes later — it wins almost immediately.

**Tantivy builds four times faster**, and that is a real result, not noise: it is
a mature, optimized indexer and the probe's trigram builder is a naive
`HashMap<u32, RoaringBitmap>` with no batching or compaction. Build time is the
one axis where the custom index is behind, and PLAN.md is explicit that build
time is not what the decision turns on. If it ever becomes the constraint, the
naive builder has obvious headroom.

## Correctness against ripgrep

Compared over a raw materialized tree (`ls-tree` + `cat-file` bytes, not
`git checkout`, so `.gitattributes` conversion cannot make the oracle disagree
with what GFS serves):

| Pattern | GFS | ripgrep | Agree |
| --- | ---: | ---: | :---: |
| `TODO` | 1587 | 1587 | yes |
| `RequestContext` | 284 | 284 | yes |
| `registerAction2` | 2128 | 2128 | yes |
| `createDecorator` | 1184 | 1184 | yes |
| ` ``` ` | 7107 | 7107 | yes |
| `日本語` | 11 | 11 | yes |
| `function` | 59112 | 59112 | yes |

> **A bug the oracle caught.** The first implementation emitted one match per
> *line*; `rg --count-matches` counts every *occurrence*. Five of seven patterns
> disagreed, and the gap was largest exactly where it matters — 4095 against
> 7107 for a backtick. A line-oriented count under-reports precisely the
> patterns agents use most: a brace, a quote, an identifier appearing twice in
> one call. Nothing but a real oracle would have found this.

## Corpus coverage

| Repository | Unique blobs | Excluded | Share | Reasons |
| --- | ---: | ---: | ---: | --- |
| linux | 94 118 | 16 | **0.02 %** | 5 binary, 11 oversized |
| vscode | 16 297 | 228 | **1.40 %** | 226 binary, 2 oversized |

Ordinary repositories that contain binaries do not make search meaningfully
incomplete, which is what the two-dimensional contract needs to be true to stay
usable: if exclusions were routinely large, `--require-exhaustive` would be
unusable and the default warning would be noise. At 0.02–1.4 % it is neither.

## The completion contract

Measured end to end on vscode. Execution status and coverage move independently,
and each maps to a distinct exit code:

| Scenario | Execution | Truncation | Eligible | Hits | Coverage gaps | Exit |
| --- | --- | --- | ---: | ---: | --- | ---: |
| complete, unbounded | COMPLETE | – | 16 622 | 2128 | binary 238, oversized 2 | 0 |
| truncated by result budget | TRUNCATED | `result_limit` | 16 622 | 10 | binary 238, oversized 2 | **3** |
| absent symbol | COMPLETE | – | 16 622 | **0** | binary 238, oversized 2 | **1** |
| scoped to `src/vs/editor` | COMPLETE | – | **1 277** | 13 706 | **binary 4** | 0 |
| regex with no 3-byte literal | TRUNCATED | `no_required_literal` | 16 622 | 21 436 | binary 238, oversized 2 | **3** |
| same, `--require-exhaustive` | COMPLETE | – | 16 622 | 2128 | binary 238 | **4** |

The two rows that matter most are "absent symbol" (exit 1) and "truncated by
result budget" (exit 3). Both return few or no results; they are impossible to
confuse. And the scoped row shows coverage is reported *within the requested
scope* — 4 excluded paths under `src/vs/editor`, not the repository's 240 — so a
binary in an unrelated directory never makes a scoped query look incomplete.

A pattern with no usable three-byte literal is reported as `TRUNCATED` with
reason `no_required_literal` rather than silently scanning everything, because
the answer really was bounded by a budget rather than by the index.

## Limitations

- Query latency against a *warm, in-process* index is sub-millisecond, but the
  probe rebuilds the index per invocation, so no end-to-end warm-search latency
  number is claimed here. That belongs with M4.6.
- Incremental manifest construction is measured as a full rebuild per commit
  (445 ms for linux). The first-parent incremental path that M4.2 specifies is
  not implemented; the full-rebuild number is therefore an upper bound.
- Only literal search is verified against ripgrep. Regex correctness beyond
  literal-run extraction, and the `--regex` path's semantics against `rg`, are
  M4.6's scope.
- The retained-snapshot projection assumes manifests for distinct commits are
  stored independently. Manifests for adjacent commits are highly similar, so
  delta-encoding them would reduce this further; the projection does not assume
  that saving.
