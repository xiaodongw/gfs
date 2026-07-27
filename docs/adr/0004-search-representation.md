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

**M4.4 is not implemented.** `xvfs-search` has no tokenizer, no Tantivy
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
