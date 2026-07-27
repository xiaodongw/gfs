# M4 — Revision-aware agent search: completion report

Date: 2026-07-26  
Milestone: M4 (PLAN.md section 7)  
Status: **Complete**, with M4.4 deliberately not implemented and three recorded
gaps carried forward.

M3 left a writable workspace that an agent could read, edit, and export, but
nothing could answer "where is this symbol". `rg` over a mount would hydrate
every file it touched, which is the cost the whole design exists to avoid. M4
adds search that runs on the server against a pinned commit, reads no blobs into
the client, merges the workspace's own edits on top, and — the part that took
the most work — is honest about what it did not look at.

## The exit gate

PLAN.md section 7 states four criteria.

| # | Criterion | Verified by | Result |
| --- | --- | --- | --- |
| 1 | Supported literal/regex results match the documented `rg` semantics | `xvfs-server/tests/search_oracle.rs` | **Met** — real `rg 15.2.0` over the raw materialization, generated query corpus, 5 fixtures |
| 2 | Search after edits returns the merged logical workspace | `xvfs-fuse/tests/search.rs` | **Met** — 15 cases over create, edit, delete, rename, mode change, type change, ignore rules |
| 3 | Warm search meets the performance target and causes zero base hydration | `benchmarks/search-preparation.md`, `xvfs-fuse/tests/search.rs` | **Met** — p95 **787 ms** on linux against a 2 s target; 0 bytes hydrated |
| 4 | No tested failure or limit produces a result indistinguishable from "no matches" | `xvfs-fuse/tests/search_faults.rs`, `xvfs-search` unit tests | **Met** — every injected fault carries an exit code that is not 1 |

### Criterion 1: what "matches `rg`" was allowed to mean

The oracle is `rg` over the **raw** tree materialization, not over a
`git checkout`. A checkout applies `.gitattributes` `text`/`eol` conversion,
`core.autocrlf`, clean/smudge filters, and LFS; a mount serves raw blob bytes.
Comparing against a checkout would report differences that are correct
behaviour on both sides.

Two divergences are real, and are asserted explicitly rather than avoided by
choosing gentle fixtures:

- **a NUL past the first 8 KiB.** ADR 0004 fixed the binary probe at ripgrep's
  8 KiB window; `rg` keeps scanning and calls a file binary on a NUL anywhere.
- **files over 8 MiB.** XVFS excludes them and *reports* the exclusion; `rg` has
  no size limit by default.

The corpus is generated from each fixture's own content rather than invented, so
the comparison is mostly of non-empty result sets. Patterns are drawn with a
fixed LCG so a failure reproduces from the test name.

### Criterion 3: the number and the hydration

Measured on the M0.1 machine profile, release build, against the public
stand-in corpus. Full numbers in
[`benchmarks/search-preparation.md`](../../benchmarks/search-preparation.md).

| | vscode | rust | linux (worst case) | Target |
| --- | ---: | ---: | ---: | ---: |
| Eligible paths | 16 622 | 61 230 | 94 735 | |
| Warm search p50 | 162 ms | 193 ms | 283 ms | |
| **Warm search p95** | 762 ms | 805 ms | **787 ms** | < 2 s |
| Cold tip preparation | 7.9–9.9 s | 9.2–10.5 s | 53–78 s | |
| Warm tip preparation | 0 ms | 0 ms | 0 ms | |
| Arbitrary commit, HEAD~100 | 2.0–2.3 s | 3.3–4.2 s | 4.0–5.9 s | |
| Index, 2 snapshots | 79.2 MiB | 92.9 MiB | 288.1 MiB | |

Zero hydration is asserted rather than measured by hand: a clean-workspace
search compares `cache_stats().bytes_fetched` before and after and requires it
unchanged. A rename does the same, because a rename is where a client is most
tempted to fetch content it already has under another name.

**ADR 0004 decision 1 survives contact with the implementation.** Preparing an
arbitrary commit costs 4.0 s on linux against a 53 s cold build, and 1.6 s for
an adjacent one, because an ancestor's blobs are already interned and
classified. On-demand preparation for any commit does not need to be rationed,
which is what the decision claimed and what M4.2's design depends on.

The per-snapshot marginal cost is **1.9 MiB** for an adjacent snapshot against
ADR 0004's projected 1.99 MiB. That is the number the whole storage decision
rests on, and it holds.

### Criterion 4: the failure that the contract exists for

An agent that receives no results concludes the symbol does not exist and acts
on it. Every way a search can come back short therefore carries a distinct exit
code, and `xvfs-fuse/tests/search_faults.rs` asserts the whole matrix in one
test — separate tests would let two codes collapse into each other without
anything failing.

| Injected fault | Outcome | Exit |
| --- | --- | ---: |
| Nothing found, corpus fully searched | `Completed` | 1 |
| Stream ends with no completion | `FailedBeforeCompletion` | 2 |
| Matches arrive, then no completion | `FailedBeforeCompletion` | 2 |
| Mid-stream `Status` error | `FailedBeforeCompletion` | 2 |
| A real TCP connection is severed | never `Completed` | 2 |
| Candidate/result/time/bytes budget | `Completed`, `TRUNCATED` | 3 |
| Pattern with no usable literal | `Completed`, `TRUNCATED` | 3 |
| Partial backend failure | `Completed`, `TRUNCATED` | 3 |
| Coverage gap, `--require-exhaustive` | `Completed`, gaps | 4 |

The transport faults use a stub `SearchService` rather than the real server:
the property belongs to the *client*, which has to turn a stream that ended
early into a failure, and arranging for the real server to be wrong would be
arranging for it to have a bug. `a_severed_connection_is_never_an_answer` cuts a
real TCP connection alongside, so the stub is known to describe something the
transport does.

The suite was checked against a deliberately broken client — one that returned
an empty `Completed` where the contract requires `FailedBeforeCompletion` — and
two tests failed. It has teeth.

## M4.4 was not implemented

ADR 0004's amendment records the decision and its basis. In short: PLAN.md M4.4
says to skip token search if literal/regex covers agent workloads, and the
implemented engine covers the query shapes the pilot issues. Substring matching
inside identifiers (`authorize_re` finding `authorize_request`) is a trigram
strength that a source tokenizer would have to choose to fail; ranking is real,
but an agent consuming `--json` sorts by path and reads all of it.

`xvfs-search` has no tokenizer, no Tantivy dependency, and no token query mode.

## What changed under M4 that was not planned

### The line-text cap (ADR 0004 amendment)

Found by an out-of-memory kill during M4.6, not by review. A `Match` carries a
copy of the line it was found on, so retained memory is
`matches × (1 + context lines) × line length` — and nothing bounded the last
factor. The fixture matrix deliberately holds a 4 MiB single-line file to prove
such files are searchable; searching it for `xxx` produced 1 398 101 matches,
each copying 4 MiB. The process was killed at 45.5 GiB with `max_results` set to
10, because the result budget was only consulted when *emitting*, long after the
memory was spent.

Three bounds were added, applied where the memory is spent rather than where the
results are counted: a per-blob hit limit of `max_results + 1`, a per-line
`max_line_bytes` (8 KiB, ADR 0004's own binary-probe window), and a cumulative
`max_display_bytes` (64 MiB). Peak retained line text is now
`max_display_bytes` regardless of corpus, pattern, or client parameters. The
same search now peaks at 33 MiB.

The honesty rules were extended rather than bypassed: exhausting the display
budget is `TRUNCATED` with its own reason and exit 3, a cut line sets
`line_truncated`, and `column` still indexes the whole line because that is what
an agent edits with.

### Coverage is scoped, and the scope had to be the request

Not new in M4 — ADR 0004 decided it at M0 — but it is the rule that took the
most care to keep. A query scoped to `src/vs/editor` reports that scope's
exclusions, not the repository's. The temptation each time was to report what
the index knows; the reason not to is that an agent seeing a warning on every
query stops reading warnings.

## Test inventory

182 tests are M4's. The workspace total is **637 passing, 0 failing**, peak RSS
494 MB for the whole suite.

| Suite | Cases | Covers |
| --- | ---: | --- |
| `xvfs-search` unit tests | 113 | blob registry and key allocation, content classification, line boundaries, manifests and scoping, trigram extraction and required-literal analysis, posting merge and intersection, globs, snapshot lifecycle, the query engine and its budgets, the local half |
| `xvfs-server/tests/search_index.rs` | 11 | manifests against stock `git`, non-UTF-8 paths, deep and wide trees, incremental against full, dedup of simultaneous preparation, TTL, classification completeness |
| `xvfs-server/tests/search_service.rs` | 12 | the gRPC stream, exactly one terminal message, truncation reporting, scoped coverage, regex, context lines, non-UTF-8 over the wire, index generation |
| `xvfs-server/tests/search_oracle.rs` | 11 | `rg` as oracle over a generated corpus; CRLF, no final newline, Unicode, invalid UTF-8 paths, repeated blobs, symlinks, huge lines, alternation, and the two documented divergences |
| `xvfs-fuse/tests/search.rs` | 15 | the merged workspace: create, edit, delete, rename, mode and type change, ignore rules, both halves in one path order, zero hydration |
| `xvfs-fuse/tests/search_faults.rs` | 7 | transport loss before the terminal message, mid-stream error, severed connection, the exit-code matrix, exclusion reasons counted separately |
| `xvfs-cli` search output | 13 | `path:line:column:text`, non-UTF-8 paths as bytes, truncation marking, the ADR 0004 exit table, `--json`, the `xvfs-rg` flag subset and its refusals |

## Recorded gaps

**The corpus is still the public stand-in set.** Every number in the benchmark
moves when `spikes/corpus/corpus.conf` is pointed at the real monorepos. Open
since M0.1 (ADR 0006, question 2).

**Index storage is 1.4× the M0 projection at the base.** 277.0 MiB measured
against 205.2 MiB projected on linux. The projection priced posting bitmaps and
manifest bytes; the measurement is a whole SQLite database including the `blobs`
registry and page overhead. The *marginal* per-snapshot cost, which is what the
storage decision rested on, matches. Not decomposed further.

**The p95 is server-side, on loopback.** `xvfs search` through the daemon adds
the overlay scan and a gRPC round trip. M4.5's own criterion — that `xvfs
search` not be slower than the `rg` invocation it replaces, benchmarked against
an overlay holding a full build tree — is a client-side measurement that has not
been taken against a real build tree.

**Retention spread across history costs more than retention near the tip.** A
snapshot 100 commits back costs 11.1 MiB against an adjacent one's 1.9 MiB,
because it brings blobs of its own. Nothing assumed otherwise, but M7.2's
retention policy should be written knowing it.
