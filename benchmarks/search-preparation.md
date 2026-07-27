# M4.6 — Snapshot preparation and warm search

The deliverable PLAN.md M4.6 asks for: cold/warm branch and arbitrary-commit
preparation, measured. ADR 0004 decided the index representation on a
*projection* of those costs from the M0 spike; this is the same question asked
of the implementation.

The number the milestone turns on is **arbitrary-commit preparation**. ADR 0004
decision 1 concluded that "on-demand search for arbitrary commits is affordable,
and does not need to be rationed". If preparing an ancestor cost what preparing
a tip costs, that conclusion would not survive and M4.2's on-demand path would
have to be replaced by a policy that rations it.

## Machine profile

| | |
| --- | --- |
| Host | WSL2, Linux 6.18.33.2-microsoft-standard-WSL2 |
| CPU | 32 logical cores |
| Memory | 46 GiB |
| Git | 2.53.0 |
| Rust | 1.97.1 |
| Build | `--release` |

## Reproducing

```sh
./spikes/corpus/fetch-corpus.sh    # once; ~12.5 GiB of bare mirrors
cargo build --release -p xvfs-server --example prepare-bench

./target/release/examples/prepare-bench ~/xvfs-corpus/mirrors/vscode.git main 100
./target/release/examples/prepare-bench ~/xvfs-corpus/mirrors/rust.git   main 100
./target/release/examples/prepare-bench ~/xvfs-corpus/mirrors/linux.git  master 100
```

The third argument is how far back the arbitrary commit is. `0` resolves to the
tip itself, which prepares nothing new and so reports the index size for a
single retained snapshot — that is how the marginal cost below was isolated.

The index is on disk (`Server::with_search_index`), as the real binary keeps it,
and is **closed before its size is read**. SQLite folds the write-ahead log into
the database on the last connection close; measuring while open reported 79 MiB
of database beside 71 MiB of uncheckpointed `-wal` on vscode, which would have
been close to double the steady-state figure this table is about.

## Corpus

The public stand-ins from `spikes/corpus/corpus.conf`. Replacing them with the
real target monorepos remains open (ADR 0006, question 2).

| | vscode | rust | linux (worst case) |
| --- | ---: | ---: | ---: |
| Files at tip | 16 863 | 61 313 | 94 850 |
| Eligible paths after corpus policy | 16 622 | 61 230 | 94 735 |
| Excluded by policy | 241 | 83 | 115 |

## Preparation

Each cell is every run taken, not a best-of. Cold preparation on linux is the
one measurement with real spread; it is I/O-bound on a mirror larger than the
page cache holds, and the 72.56 s run followed an unrelated read of the same
mirror.

| | vscode | rust | linux |
| --- | ---: | ---: | ---: |
| **Cold branch tip** | 7.90, 7.92, 8.33, 9.90 s | 9.16, 9.30, 10.52 s | 53.21, 53.30, 55.94, 59.40, 72.56, 77.95 s |
| **Warm branch tip** | 0 ms | 0 ms | 0 ms |
| **Arbitrary commit, HEAD~1** | — | — | 1.62 s |
| **Arbitrary commit, HEAD~100** | 1.96, 2.01, 2.05, 2.32 s | 3.33, 3.41, 4.16 s | 4.00, 4.67, 5.56, 5.87 s |
| **Peak RSS** | 355 MB | 543 MB | 1.40 GB |

**Warm preparation is 0 ms on every repository.** It is a claim lookup against
the `snapshots` row and returns `Ready` without touching the object database —
M4.2's "repeated preparation of the same commit does not rebuild", timed rather
than asserted.

**Arbitrary-commit preparation on linux is 4.00 s against a 53.21 s cold
build**, a factor of 13, and 1.62 s for an adjacent commit. The reason is the
blob registry: an ancestor's blobs are almost all interned and classified
already, so preparing it walks a tree and writes a manifest without re-reading
or re-indexing content. **ADR 0004 decision 1 holds against the
implementation.** On-demand preparation for an arbitrary commit costs seconds,
and does not need to be rationed.

## Warm search

DESIGN.md section 13 states the target as **p95 under 2 seconds** for a warm
literal search on an indexed branch. A p95 needs a distribution, so this is 20
queries against the prepared tip, not one: common identifiers, rare ones, a
three-byte literal (the shortest the trigram index can bound at all), two
case-insensitive lookups, five regexes, and one pattern that matches nothing —
the shape an agent issues most and notices least. The list is in
`prepare-bench.rs` and is fixed, so two machines compare.

| | vscode | rust | linux | Target |
| --- | ---: | ---: | ---: | ---: |
| Eligible paths | 16 622 | 61 230 | 94 735 | |
| **p50** | 162 ms | 193 ms | 283 ms | |
| **p95** | 762 ms | 805 ms | **787 ms** | < 2 s |
| max | 928 ms | 935 ms | 937 ms | |
| Matches, 20 queries | 10 929 | 14 316 | 18 893 | |
| Candidate blobs read | 27 052 | 17 143 | 30 246 | |

**The target is met on every repository, with the worst case at 787 ms against
2 s.** p95 barely moves between a 16 622-path repository and a 94 735-path one —
283 ms against 162 ms at the median, and 787 against 762 at p95 — because the
trigram index makes cost a function of *matches*, not of corpus size. linux
reads 30 246 candidate blobs across 20 queries, about 1 500 per query against
94 735 eligible paths.

The maxima cluster at ~930 ms on all three, which is not a corpus property: it
is the default `max_results` of 1000 being filled by the broadest patterns
(`get`, `return`, `const`). Those queries stop on the result budget and report
`TRUNCATED`, so the number is the cost of filling a page, and it is the same
page everywhere.

## Index storage, against the projection

Measured on linux, the repository ADR 0004 gates on.

| | Measured | ADR 0004 projected |
| --- | ---: | ---: |
| One retained snapshot | 277.0 MiB | 201.2 MiB postings + 1.99 MiB manifest |
| Plus an adjacent snapshot (HEAD~1) | 278.9 MiB (**+1.9 MiB**) | +1.99 MiB |
| Plus a snapshot 100 commits back | 288.1 MiB (**+11.1 MiB**) | — |

Two findings, one confirming and one not.

**The per-snapshot marginal cost is confirmed.** An adjacent retained snapshot
costs **1.9 MiB**, against ADR 0004's projected 1.99 MiB. That is the number the
storage decision rests on — the case against a per-snapshot Tantivy index was
`N × 52.1 MiB` versus `base + N × small` — and it survives contact with the
implementation. At 200 adjacent retained snapshots the index is about 660 MiB,
against 10.2 GiB for the rejected alternative.

**The base is 1.4× the projection**: 277.0 MiB measured against 205.2 MiB. The
projection priced the posting bitmaps and the manifest bytes. The measurement is
a whole SQLite database, and also contains the `blobs` registry — one row per
unique blob, carrying its object ID as text, size, and classification, for
94 735 blobs — together with SQLite's page and index overhead. Neither was in
the M0 estimate, and neither is waste; they are what makes an index gap
reportable rather than invisible. The gap is not decomposed further here.

A snapshot 100 commits back costs **11.1 MiB**, not 1.9. The difference is not
manifest: it is the blobs unique to that commit, which have to be interned,
classified, and indexed. ADR 0004 measured ~39 new blobs per linux commit, so
100 commits of drift is a few thousand new blobs and their postings. Retention
spread far across history is therefore materially more expensive than retention
clustered near the tip. Nothing in the design assumed otherwise, but the
retention policy in M7.2 should be written knowing it.

## Open

- The corpus is still the public stand-in set. Every number here moves when
  `spikes/corpus/corpus.conf` is pointed at the real monorepos.
- Cold preparation on linux spans 53–78 s across six runs. The spread is I/O,
  not compute; it has not been measured on a warm page cache or on the hosted
  runner's disk, and M6.1 will need the latter.
- The p95 is measured server-side, on loopback, with no client. `xvfs search`
  through the daemon adds the overlay scan and the gRPC round trip; M4.5's own
  criterion is the comparison against `rg`, which is a separate measurement.
- The 1.4× base-storage gap against the projection is stated, not decomposed. If
  index size becomes a constraint, the `blobs` table's text object IDs are the
  obvious first thing to measure.
