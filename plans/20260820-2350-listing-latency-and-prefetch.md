# Cold-walk latency: server-side memoization and client-side prefetch

## Summary

The agent-workflow benchmark records a first `git status` on a vscode mount at
**555 s** — 5 328 uncached directory listings, serialized, at 38–126 ms each.
This feature attacks that number from both ends: make one listing cheap on the
server, and stop asking for them one at a time from the client.

**What the measurement actually found.** Profiling the server showed the cost
is *not* in reading trees:

| | django (29 311 refs) | vscode (73 989 refs) |
| --- | ---: | ---: |
| `list_directory`, server-side median | 8.69 ms | 39 ms (mean 291 ms under index contention) |
| decode **every** directory tree (3 274 / 4 318 of them) | 0.56 ms | 1.8 ms |
| `read_header` for **every** blob (7 074 / 17 925) | 9.8 ms | 47 ms |
| `is_visible` — one call | 5.7–8.8 ms | 24–28 ms (740 ms cold) |

Every snapshot RPC calls `Authorizer::authorize_commit`, which calls
`is_visible`, which **enumerates and peels every ref in the repository**. That
one check is ~100 % of a listing's server time; the tree read it wraps is
~2.5 µs. The comment in `libgit2.rs` already says the answer "is cached per
(commit, ref generation); see M1.5" — it never was.

So the two improvements land as three:

1. **Server: stop re-deciding authorization per request.** Check the mount
   capability *first* when one is presented (the FUSE steady state — it binds
   subject, repository, commit and is signed by the server), and memoize the
   ref-reachability verdict per (repository, commit, ref generation) for
   everything else.
2. **Server: one call for a subtree.** A `ListTree` RPC that walks a subtree
   and returns complete directories, paged at directory boundaries, so a client
   can fill many listings from one round trip.
3. **Client: prefetch on a recognized access pattern.** A walk detector that
   turns a descending sequence of listing misses into one `ListTree` for the
   subtree, and a read detector that turns "the third file opened in this
   directory" into a background fetch of the rest of that directory's blobs —
   both bounded, budget-aware, and counted in `gfs inspect`.

## Plan

**Phase 1 — authorization** (`crates/gfs-service/src/auth/`)
* `auth/visibility.rs`, new: `VisibilityCache`, verdicts keyed by
  `(repository, commit)` and stamped with the repository's ref generation, with
  a TTL backstop (10 s for reachable, 2 s for not).
* `Catalog` grows an in-process ref generation per repository, bumped by every
  observation that changed something (`observe_ref`, `reconcile_refs`) and by
  `Registry::evict`, which is the signal that the repository changed on disk.
  Deliberately *not* `MAX(ref_version)`: deleting the highest-versioned ref
  removes its row, so that value can return to a number it already had.
* `authorize_commit` tries the capability first and holds any refusal until
  reachability has also said no — which is what keeps an expired capability
  reported to its owner as `Expired` rather than masked.

**Phase 2 — the recursive listing**
* `gfs-git`: `TreePage` and `GitRepository::list_tree`, a depth-first walk with
  `after` as a resumable cursor; `directory_tree` factored out so
  `list_directory` and `list_tree` cannot disagree about a gitlink.
* `gfs-proto`: `ListTree` RPC and its two messages, pinned in `golden.rs`.
* `gfs-service`: the handler, clamping `max_entries` and minting no tickets.
* `gfs-mount`: `SnapshotClient::list_tree`; gRPC message ceilings raised from
  tonic's 4 MiB default to the protocol's own `MAX_RESPONSE_BYTES`.

**Phase 3 — the client** (`crates/gfs-mount/src/prefetch.rs`, new)
* `WalkDetector` (misses inside a 2 s window → their common ancestor) and
  `ReadDetector` (distinct files read from one directory).
* `Prefetcher` lives inside `Pinned`, so a repin ends its evidence and its
  in-flight fetches by construction, as the listing cache already did.
* A miss inside an in-flight subtree waits on it, re-checking the cache per
  page, instead of issuing its own call.
* `ListingCache` gains an entry bound (the one that describes memory) and a
  batched trim; defaults raised to 32 768 directories / 150 000 entries,
  because 4 096 was below vscode's 4 318 and the walk was evicting itself.
* New `FsStats` counters, reported on a `prefetch` line in `gfs inspect`.

**Phase 4 — verification**
* `crates/gfs-service/tests/authorization.rs`: a verdict is decided once per ref
  generation; a capability authorizes with no reachability scan at all.
* `crates/gfs-git/tests/repository.rs`: a recursive listing agrees with the
  per-directory listing entry for entry, and paging never splits a directory.
* `crates/gfs-mount/tests/prefetch.rs`: a recognized walk sees exactly what the
  same walk sees with prefetching off, and reading a directory through fetches
  the rest of it without touching the oversized files.
* Full workspace suite, clippy, and a re-run of
  `spikes/corpus/benchmark-workflow.sh`.

Built as planned.

## Decisions

* **Fix authorization, not the listing.** The request was a server-side listing
  cache. Measurement said the tree read is 2.5 µs and the authorization check
  around it is 8.7–28 ms, so a listing cache would have optimized 0.03 % of the
  request. The decoded-tree cache that already exists is the listing cache;
  what was missing was the memo the code comment claimed to have.
* **Capability first, and equally authoritative.** A mount capability is signed
  by this server and binds subject, repository, and commit — the same fact the
  ref scan establishes, for one HMAC. The visible change is in audit:
  `via_capability` now means "the caller presented one and it was used", not "no
  visible ref reached this commit".
* **Generation *and* TTL.** The generation covers every ref change the catalog
  records and invalidates immediately; the TTL covers a ref changed underneath
  the server, which the catalog cannot see. Reachable verdicts are held 10 s and
  unreachable ones 2 s, because refusing a commit that just became reachable is
  a visible failure on a fresh push while serving one a moment past its last ref
  is not.
* **Pages break between directories.** A cached listing's value is that it can
  answer the *absence* of a name; half a directory cannot. `max_entries` is
  therefore soft, and the response names the directories it completed so an
  empty one is distinguishable from an unfetched one.
* **A resumable cursor by replay.** `after` names the next directory to visit
  and a resumed walk replays the traversal to reach it. Replay costs tree
  decodes, which the tree cache mostly answers and which measured at 1.8 ms for
  all 4 318 of vscode's directories — cheaper than carrying traversal state
  across a request boundary.
* **Detectors need evidence.** Nothing prefetches on a first miss or a first
  read. A job that opens one file must pay for one file, or this becomes the
  whole-tree materialization ADR 0009 refused.
* **Waiting beats racing.** A miss inside an in-flight subtree waits for it
  rather than fetching alongside. Locally that trades ~1 ms for a wait; over a
  network it is the difference between one round trip and thousands. The wait
  re-checks per page, so a directory that arrives early does not wait for the
  rest of the tree.
* **Prefetched bytes are charged, and stop at a reserve.** They cross the
  network exactly as a real read's would, so a budget that ignored them would
  stop describing what the job cost. Prefetching stops with 25 % of the budget
  unspent, so a wrong guess can never be the reason a real read gets `EDQUOT`.
* **No blob tickets on `ListTree`.** A ticket is short-lived authorization; one
  per entry would mint thousands for reads that may never happen. Content
  prefetch mints them through `BatchGetEntry` for the blobs it is about to read.

## Details

* **Numbers, on this machine and corpus.** A cold `find` over every directory:
  django **33.0 s → 1.27 s** (3 274 `ListDirectory` calls → 4), vscode
  **1 131 s → 1.71 s** (4 318 → 4, plus 3 `ListTree` pages costing 0.15 s
  server-side). Both are now within 0.3 s of the same walk warm, which means the
  remaining time is FUSE and the walker, not the network. Per-listing server
  time on django fell from 8.69 ms to 0.18 ms.
* **The old listing cache was too small to hold a monorepo.** vscode has 4 318
  directories against a 4 096-directory bound, so the baseline walk re-fetched
  directories it had already listed — 7 283 listings for 4 318 directories. The
  new bounds are 32 768 directories and 150 000 entries, the second being the
  one that describes memory (~25 MB of paths and object IDs at the bound).
* **`Gfs::stats` and the budget are now `Arc`-shared**, because a prefetch
  outlives the call that triggered it and what it fetched still belongs in this
  mount's counters.
* **`gfs inspect` gained a `prefetch` line**: walks, pages, listings filled,
  directories read ahead, blobs and bytes. That line is how you tell a prefetch
  that paid for itself from one that guessed wrong.
* **Off-switches on the daemon**: `gfs-fuse --walk-prefetch-threshold` and
  `--read-prefetch-threshold` (`GFS_WALK_PREFETCH_THRESHOLD`,
  `GFS_READ_PREFETCH_THRESHOLD`), both zero to disable. Only these two are
  exposed as flags — the rest are `FsConfig` fields, on the principle the
  hydration budget already set: expose what an operator must be able to turn
  off in the field, not every constant.
* **Knobs**, all on `FsConfig`: `walk_prefetch_threshold` (4, zero disables),
  `tree_page_entries`, `tree_prefetch_max_entries` (200 000),
  `read_prefetch_threshold` (3, zero disables), `read_prefetch_max_bytes`
  (32 MiB), `read_prefetch_max_file_bytes` (8 MiB),
  `read_prefetch_concurrency` (4), `prefetch_budget_reserve_percent` (25).
* **The benchmark harness had a measurement bug of its own**, found by this
  work: it timed the *warm* search immediately after the cold one, so on vscode
  it was measuring the tail of the index build (2.385 s) rather than a query
  against a ready index (0.245 s). It now waits for the index to answer before
  timing, treating ADR 0004's exit code 2 as "still building" — and not `&&
  break`, because the probe's `-m 1` truncates and truncation is exit 3.
* **Not done here.** The visibility memo is per process; a multi-process
  deployment relies on each process's own generation plus the TTL. If ref
  changes ever need to invalidate across processes, the catalog's `ref_events`
  table is the place to hang it.
