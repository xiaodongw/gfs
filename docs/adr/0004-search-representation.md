# ADR 0004: Search representation and the completion contract

- Status: Accepted
- Date: 2026-07-26
- Milestone: M0.4
- Evidence: `spikes/search-probe`, `spikes/reports/m04-search-representation.md`

## Context

DESIGN.md section 6.5 proposes indexing each unique blob once under a stable
`blob_key`, and filtering by a per-snapshot Roaring bitmap, rather than indexing
path/content occurrences per commit. Tantivy is offered as an optional
tokenized/ranked mode. PLAN.md M0.4 required this be priced — specifically
*steady-state manifest storage per retained snapshot*, not index build time.

## Decision

### 1. Adopt the blob-key + trigram + snapshot-bitmap representation

Measured on the worst-case repository (Linux, 94 751 tip files, 1385 MiB of
indexed content):

- trigram postings 201.2 MiB, or 0.15 bytes per byte of indexed content;
- **manifest 1.99 MiB per snapshot**;
- **200 concurrently retained snapshots: 0.39 GiB**.

Against a per-snapshot Tantivy index on vscode, with the same corpus policy:
N × 52.1 MiB versus 45.9 MiB + N × 0.52 MiB. At 200 retained snapshots that is
10.2 GiB against 150 MiB. The two are level at about N = 1, so there is no
up-front premium to amortize.

**Consequence for the design: on-demand search for arbitrary commits is
affordable, and does not need to be rationed.** Manifest storage was the stated
reason it might not be. It is not a reason.

### 2. Keep tokenized search out of the MVP

PLAN.md M4.4 says to skip token search if literal/regex covers agent workloads.
Literal search matches ripgrep exactly on the supported corpus, which is the
semantics agents already expect, and the per-snapshot cost of the ranked index
is two orders of magnitude worse at realistic retention. Tantivy stays an
explicit later mode, not MVP scope.

This is not a claim that Tantivy is worse at its job. It builds four times
faster than the probe's naive trigram builder, and ranking is something trigrams
cannot do at all. It is a claim about *this* workload: exact code search over
many retained snapshots.

### 3. The completion contract is two independent dimensions, with distinct exit codes

Adopted and demonstrated end to end:

| Outcome | Exit |
| --- | ---: |
| Complete, matches found | 0 |
| Complete, no matches | 1 |
| Missing terminal message / transport failure | 2 |
| Execution truncated (any budget, or no usable literal) | 3 |
| Coverage gap under `--require-exhaustive` | 4 |

The pair that justifies the whole mechanism is exit 1 against exit 3: both
return few or no results, and an agent that cannot tell them apart will conclude
a symbol does not exist when the query was merely cut short. They are separate
codes derived from separate fields.

Two further rules are adopted from the measurements:

- **Coverage is scoped to the request.** A query scoped to `src/vs/editor`
  reported 4 excluded paths, not the repository's 240. Reporting repository-wide
  exclusions would make every scoped query look incomplete.
- **A pattern with no usable three-byte literal is `TRUNCATED`, not `COMPLETE`.**
  Such a query is bounded by a scan budget rather than by the index, and saying
  so is the difference between an honest answer and a plausible one.

`SearchOutcome` is modelled so that "the stream ended without a terminal
message" is a representable, testable state rather than an empty result — the
design says a missing completion is an error, and a type that cannot express it
would let that rule quietly lapse.

### 4. Binary/oversized classification follows ripgrep

A NUL byte in the first 8 KiB means binary; the size cap is 8 MiB, as DESIGN.md
section 7.5 suggests. Excluded share measured at **0.02 %** of unique blobs for
linux and **1.40 %** for vscode.

That number matters more than it looks. If exclusions were routinely large,
`--require-exhaustive` would be unusable and the default coverage warning would
be noise an agent learns to ignore. At this scale the contract stays meaningful.

## Alternatives considered

**Per-snapshot Tantivy index.** Priced above; rejected on retained-snapshot
storage.

**Index path/content occurrences per commit.** The approach DESIGN.md rejects up
front. The measurements support that: successive commits add ~4 (vscode) to ~39
(linux) new blobs, so per-commit content indexing would re-index ~94 000
unchanged blobs per commit to no benefit.

**Restrict search to configured branch tips.** Was the fallback if manifest
storage proved expensive. Not needed.

**Delta-encode manifests between adjacent commits.** Adjacent manifests are
highly similar and this would reduce the projection further. Not adopted,
because the undeflated number is already comfortably small and the complexity
would buy nothing the pilot needs. The projection deliberately does not assume
this saving.

## Consequences

- M4.2's on-demand arbitrary-commit preparation is affordable; the manifest TTL
  and garbage collection it specifies remain worth having, but as hygiene rather
  than as a load-bearing cost control.
- M4.4 (token search) is deferred out of the MVP, with a recorded basis.
- M4.3's terminal-message contract and M4.5's CLI exit codes inherit the table
  above verbatim.
- The trigram builder is the one component measured as slower than the
  alternative. If index build time becomes a constraint, that is where to look
  first, and it has obvious headroom (batching, compaction, parallel merge).

## Amendment, 2026-07-26: M4.4 token search is skipped, confirmed against the implementation

Decided after M4.3 completed. This confirms decision 2 rather than changing it,
and records the confirmation where M4.4's absence would otherwise look like an
oversight.

### What changed since the original decision

At M0 the reasoning was a projection: literal/regex search *would* match ripgrep
on the supported corpus, and a per-snapshot Tantivy index *would* cost two
orders of magnitude more at realistic retention. M4.3 makes the first half
observable rather than projected.

The implemented literal/regex engine covers the query shapes the pilot's agents
issue: exact identifiers, escaped literals, alternation, anchored regexes,
case-insensitive lookups, path-scoped and glob-filtered searches, and context
lines. Two properties that a tokenized index would have been needed for turn out
not to be needed:

- **Substring matching inside identifiers.** `authorize_re` matching
  `authorize_request` is what agents actually search for, and it is a trigram
  strength, not a tokenizer strength — a source tokenizer would have to split
  `authorize_request` and then fail this query, or keep it whole and fail
  `authorize`.
- **Ranking.** Trigrams cannot rank and Tantivy can, but an agent consuming
  `--json` sorts by path and reads all of it. Ranking matters when a human scans
  the first screen; it does not change which files an agent opens.

### Decision

**M4.4 is not implemented.** `gfs-search` has no tokenizer, no Tantivy
dependency, and no token query mode.

### Consequences

- The dependency set stays smaller by one large crate and its index format.
- `PLAN.md` M4.4's own instruction — "skip this task if M0 shows literal/regex
  covers agent workloads" — is satisfied, with M0's projection now backed by a
  working engine.
- If ranking is ever required, the natural home is a second explicit query mode
  rather than a change to the default: the exit-code contract in decision 3
  assumes a corpus whose boundaries are declared, and a ranked mode that
  silently returned the top *N* would be a truncation the contract would have to
  learn to describe.

## Amendment, 2026-07-26: a result's line text is capped, and the cap is a budget

Decided after an out-of-memory kill during M4.6. This adds to decision 3 rather
than changing it: the new bound is a truncation, reported through the mechanism
already there.

### What the measurements missed

Every cost in the original decision is a cost of the *index*: postings per byte
of content, manifest per snapshot, blobs read per query. None of them is a cost
of the *answer*, and the answer turns out to be the one that is unbounded.

A `Match` carries a copy of the line it was found on. So the memory a query
retains is not the size of the corpus but

```
matches x (1 + context_before + context_after) x line length
```

and nothing in the design bounded the last factor. The fixture matrix's
`content` tree has a file that is 4 MiB of `x` on a single line — deliberately,
to prove such a file is searchable. Searching it for `xxx` yields 1 398 101
matches, each copying 4 MiB: about 5.6 TiB requested. The process was killed at
45.5 GiB, having exhausted 47.6 GiB of RAM and 12 GiB of swap. `max_results` was
set to 10 at the time. It was consulted only when emitting results, which is
after the memory was already spent.

The shape is not exotic. A minified bundle or a generated JSON file is one long
line, and searching for a common token in one is an ordinary thing to do.

### Decision

**Three bounds, applied where the memory is spent rather than where the results
are counted.**

1. **A per-blob hit limit.** The matcher stops at `max_results + 1` hits in any
   one blob. No path can emit more than `max_results`, so a further hit is
   unreportable by construction; the `+ 1` is what keeps an over-full page
   distinguishable from an exactly-full one, which decision 3's
   complete-versus-truncated rule depends on.
2. **`max_line_bytes`, default 8 KiB.** A cap on any one line returned for
   display, matched or context, and on the matched span itself — a regex may
   match a whole line, so the span is as unbounded as the line. 8 KiB is this
   ADR's own binary-probe window, reused because it is already the distance the
   design treats as far enough into a file to know what it is, and it is one to
   two orders of magnitude past any line that is read rather than generated.
3. **`max_display_bytes`, default 64 MiB.** A cap on the total line text one
   query retains. The per-line cap alone still multiplies by results and context
   lines, both of which a client may legitimately set high (the server clamps
   them at 10 000 and 64 each way, whose product with 8 KiB is over 10 GiB).

Both budgets clamp downward only at every boundary — gRPC request, daemon
request, CLI flag. A caller may ask for narrower lines than the default; it may
not ask a server to retain wider ones, because the memory is the server's.

### The honesty rules this inherits

Decision 3 says an answer must never be quietly smaller than the question.
Applied here:

- Exhausting `max_display_bytes` is `TRUNCATED` with reason `display_budget`,
  and therefore **exit 3**. It is deliberately not `result_limit`: the page was
  not full, the answer was too wide to hold, and a caller that narrows the
  pattern gets further than one that pages forward.
- A cut line sets `line_truncated` on the match. `--json` carries the flag;
  text output appends `[... line truncated]` after the text. Not inside
  `line_text`: an agent grepping that field must not find a marker this code
  invented, and a byte field that is sometimes bytes-as-stored and sometimes
  bytes-plus-commentary cannot be consumed at all.
- **`column` is unchanged, and still an offset into the whole line.** On a cut
  line it may point past the end of `line_text`. That is the existing rule in
  `gfs-search`'s line module — the column is what an agent edits with, the text
  is what it displays — and `blob_oid` still leads to the untruncated bytes.
- A cut never splits a UTF-8 sequence, so a terminal is not shown a replacement
  character the file does not contain. On content that is not UTF-8 this gives up
  at most three bytes.

### Consequences

- Peak retained line text per query is `max_display_bytes`, independent of the
  corpus, the pattern, and the client's parameters. Under the defaults that is
  64 MiB.
- `gfs search --max-columns` and `gfs rg -M/--max-columns` expose the per-line
  cap. `rg` suppresses a line that wide and prints a note in its place; GFS
  keeps the first bytes and marks them. The flag is spelled the same because the
  intent is the same.
- The oracle comparison in M4.6 is unaffected: it compares `(path, line,
  column)`, none of which this changes.
- `SearchMatch.line_truncated` (field 9) and `SearchRequest.max_line_bytes` /
  `max_display_bytes` (fields 17, 18) are additive, so an older client decodes a
  newer server's messages unchanged — it simply does not learn that a line was
  cut. That is the reason `line_truncated` defaults to `false` rather than being
  signalled by a sentinel inside `line_text`.
