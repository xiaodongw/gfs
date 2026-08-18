# CocoIndex, and what GFS should take from it

Status: prior-art note, written 2026-08-17 against CocoIndex as published that
day. Not a decision record — nothing here is adopted until it has an ADR.  
Companions: [DESIGN.md](DESIGN.md) sections 1 and 6.5,
[ADR 0004](adr/0004-search-representation.md),
[agent-search.md](agent-search.md)

## 1. What CocoIndex is

CocoIndex is an incremental data-indexing engine — Rust core, Python-facing
declarative API — whose pitch is keeping AI-agent context continuously fresh.
You declare `Target = F(Source)` and the engine computes the minimum work to
reach that target when either the source data *or the transform code* changes.
Sources are filesystems, repositories, databases, message queues; targets are
relational, vector, and graph stores.

It ships in two forms, and the distinction matters for us:

- **The framework** (`cocoindex`) — the general pipeline engine. Its flagship
  demo is a codebase RAG index: walk a repo, tree-sitter chunk it, embed the
  chunks, upsert into Postgres/pgvector, query by cosine distance.
- **The product** (`cocoindex-code`, CLI `ccc`) — that demo hardened into a
  zero-config local tool: an embedded index, a CLI, an MCP server, and a Claude
  Code Skill. This is the part aimed at the same user GFS is aimed at, and it is
  what most of this note compares against.

The framework's headline — delta-only reprocessing — is a solved problem inside
GFS already (section 3). The product's *packaging and query surface* is where
the transferable ideas are.

## 2. How CocoIndex makes a codebase discoverable to an agent

Nine concrete mechanisms, separated from the marketing in section 6.

1. **The retrieval unit is an AST chunk, not a file or a line window.**
   Tree-sitter parses the file and `SplitRecursively` cuts on syntactic
   boundaries — function, class, block — at `chunk_size=1000`,
   `min_chunk_size=300`, `chunk_overlap=300`. A hit is therefore a complete
   semantic unit, never a function body with its signature cut off. Everything
   else in the product is downstream of this choice.
2. **Every chunk is addressable back to source.** Stored fields are `filename`,
   `code`, `embedding`, `start_line`, `end_line`, `language`; rows are keyed on
   `(filename, location)`. A result says "this function, at `foo.rs:412-460`",
   so the agent jumps to the range instead of re-grepping for what it was just
   handed. The deterministic key is also what makes incremental upsert work — a
   changed chunk replaces its own row.
3. **Index-time and query-time embedding are literally the same function.** The
   embedder is wrapped in a shared `transform_flow` and the query path calls
   `.eval(query)` on it. Rules out the drift where the index model is swapped
   and search quality degrades silently.
4. **The economic argument is tokens, not relevance.** "Agents receive the few
   chunks that matter — not whole files"; claimed at ~70% token saving. The
   competitor being displaced is not `rg`, it is the agent's *read-file loop* —
   grep, open the file, open the neighbour, open the caller.
5. **Freshness is guaranteed at query time.** The MCP tool is
   `search(query, limit=5, offset=0, refresh_index=True, languages=None,
   paths=None)` — `refresh_index` defaults to true, so the search call itself
   brings the index current before answering. There is no state in which the
   agent reasons about code as it was ten edits ago.
6. **Scope is part of the query surface.** `languages` and `paths` filters are
   arguments to `search`, not something the agent post-filters.
7. **Two query modes, deliberately.** `ccc search` matches by meaning;
   `ccc grep` matches by syntax-tree shape (ast-grep flavoured). They concede
   that semantic search alone is bad at "every call site of this exact form".
8. **Zero-config local storage.** LMDB plus SQLite under
   `<project>/.cocoindex_code/`, local `Snowflake/snowflake-arctic-embed-xs`
   embeddings, a background daemon, `ccc init|index|status|doctor|reset`. The
   framework demos want Postgres and pgvector; the agent product deliberately
   requires neither, nor an API key.
9. **Distribution is treated as product work.** MCP server (`ccc mcp`) plus a
   Claude Code Skill that teaches the agent when to search and performs
   init/index itself. The Skill is the load-bearing half: it is how a new tool
   gets used without the human remembering to mention it.

## 3. Feature comparison

| Concern | CocoIndex (`ccc`) | GFS |
| --- | --- | --- |
| Corpus | A local working tree, live and mutating | A pinned immutable commit, plus a COW overlay |
| Freshness model | Re-index on demand; `refresh_index=True` on every search | Snapshot is pinned by construction; the query cannot see a different tree than the mount |
| Unit of work reuse | Chunk, keyed `(filename, location)` | Blob, interned once per repository as `blob_key` ([ADR 0004](adr/0004-search-representation.md)) |
| Cross-version reuse | Per-file; a moved file re-embeds under a new key | Content-addressed: a file that moves costs nothing, a blob shared by 200 snapshots is indexed once |
| Per-snapshot cost | Not a concept — one live index | 1.99 MiB manifest per retained snapshot; 200 snapshots = 0.39 GiB |
| Retrieval unit | AST chunk (function/class/block) | Matching line, with context |
| Query modes | Semantic (embeddings) + structural (`ccc grep`) | Literal + regex, ripgrep-exact semantics |
| Ranking | Cosine similarity, top-k | None — exhaustive within budget, order is scan order |
| Completeness contract | None; top-k is inherently partial and unstated | Explicit: complete / no-match / truncated / coverage-gap as distinct exit codes (0/1/3/4) |
| Provenance | `filename` + `start_line`/`end_line` | Blob OID + path + line, and the blob is fetchable |
| Storage | LMDB + SQLite local, or Postgres/pgvector | SQLite: blob registry, postings, per-snapshot Roaring manifests |
| Parsing | Tree-sitter, per language | NUL scan + UTF-8 validation only — no parser to sandbox |
| Code-version invalidation | Memoization key includes transform code; changed code re-runs affected rows | Absent: `blobs.indexed` is a boolean (`store.rs:170`); an extractor change means a full wipe |
| Failure isolation | Per-record; a bad file does not block the pipeline | Per-snapshot: one unreadable object aborts the batch (`registry.rs:304`) and eventually fails the commit |
| Agent interface | New tool the agent must learn (MCP + Skill) | Tools the agent already runs (`PATH` shims for `rg`/`grep`/`find`/`git`) |
| Server component | None — local process | Server holds the object DB and the index; the mount reads nothing |
| Large files | Not addressed | LFS expanded server-side, excluded from the index with a stated reason |

Two rows deserve reading together. **Cross-version reuse** and **per-snapshot
cost** are where GFS is structurally ahead: CocoIndex's incrementality is
"don't redo unchanged files since last run", GFS's is "this blob has been
indexed once, forever, for every commit that will ever contain it". **Code-version
invalidation** and **failure isolation** are where CocoIndex is ahead, and both
are cheap to close.

## 4. What to borrow, and why

### 4.1 Semantic search, keyed by `blob_key` — the substantive one

[ADR 0004 §2](adr/0004-search-representation.md) kept ranked search out of the
MVP on a cost argument: a per-snapshot Tantivy index costs N × 52.1 MiB against
45.9 MiB + N × 0.52 MiB for the trigram-plus-bitmap scheme, so at 200 retained
snapshots that is 10.2 GiB against 150 MiB.

**That argument is about per-snapshot indexes, not about ranking.** A vector
index built the way the trigram index is built does not pay it: embed each
unique blob's chunks once, key the vectors by `blob_key`, and filter candidates
by the snapshot's existing Roaring bitmap. The marginal cost of a new snapshot
stays the manifest already being paid for. The blob economy that made trigram
search affordable across many retained commits makes embedding search
affordable on exactly the same terms — and unlike Tantivy, embeddings give the
agent something trigrams provably cannot: a way to find code whose identifier it
does not know.

This wants its own completion semantics. Top-k is *inherently* partial, and
`SearchOutcome` currently draws a hard line between COMPLETE and TRUNCATED that
a ranked mode cannot honestly sit on either side of. Whatever ADR adopts this
has to answer that before it answers anything about models or dimensions.

### 4.2 AST chunking — needed for 4.1, and it costs something real

Tree-sitter chunking is the input to 4.1 and independently enables a symbol
surface (`gfs symbols`, definition jump) that today costs a full-text scan.

But it directly contradicts a property `registry.rs` states in its module
documentation: *"There is no parser here to sandbox: classification is a NUL
scan plus a UTF-8 validation, and trigram extraction is a three-byte window.
Both are linear, allocate nothing proportional to the input beyond the blob
itself, and cannot recurse."* Tree-sitter is a real parser running per-language
grammars over untrusted repository bytes. `IngestBudget` bounds aggregate bytes
and blob count, which is the right control for a linear scanner and an
insufficient one for a parser with its own recursion and allocation behaviour.

Adopt it, but adopt the consequence with it: that paragraph has to be replaced
with a genuine sandbox story, not amended quietly.

### 4.3 Extractor version in the memoization key — cheap, do it regardless

CocoIndex invalidates on *code* change, not only on data change. GFS cannot:
`blobs.indexed` is a boolean (`store.rs:170`), so changing the trigram
extractor, the classifier, or the corpus policy has no representable
consequence short of wiping the index for every repository.

Making it `indexed_version INTEGER` (and `classified_version` alongside) turns
an extractor upgrade into lazy per-blob re-index, with the interim state
reported as an honest coverage gap rather than a silent wrong answer. This is
the same distinction the schema already draws between "key allocated but not
classified" and "classified but not yet indexed" — a third gradation of the same
idea, in a schema built to express exactly that.

It is also a prerequisite for 4.2: introducing a chunker means the extractor
*will* change repeatedly, and a scheme whose only upgrade path is a global
rebuild will make that painful enough to discourage improving it.

### 4.4 Per-blob failure isolation

`registry.rs:304` reads `let content = source.read(&fact.oid)?;` — one
unreadable object propagates out of the batch, and the snapshot ends `FAILED`
after its attempts are spent. One corrupt object therefore makes an entire
commit unsearchable.

The machinery for the better answer already exists. An `Unreadable` content
class beside `Lfs`, reported as a scoped coverage reason, degrades one *entry*
instead of one *snapshot* — which is the same line ADR 0004 §3 takes everywhere
else: an honest partial answer beats a plausible whole one. CocoIndex's framing
("failure isolation prevents a single bad record from blocking the pipeline") is
the same rule stated for a different substrate.

### 4.5 Scope filters and line-range results on the search API

Small, and worth copying because they are free. `paths` and `languages` as
first-class arguments rather than post-filters; results carrying a line *range*
rather than a line number where the match is a semantic unit. GFS already scopes
coverage to the request — ADR 0004 §3's rule that a query scoped to
`src/vs/editor` reports 4 excluded paths, not the repository's 240 — so the
filter concept is present; it is the ranked mode that will need the range.

## 5. What not to borrow

- **The declarative flow/DAG programming model and the Python API.** They solve
  the general problem: arbitrary user-defined pipelines over heterogeneous
  sources. GFS has one source (Git blobs), one immutability model, and a fixed
  transform. A flow engine here would be ceremony over a straight line.
- **Postgres/pgvector as the lineage and vector store.** The SQLite store's
  transactional coupling of manifest bytes to snapshot state is load-bearing —
  V2's comment is explicit that one transaction makes the state change and the
  bytes it describes durable together, so a `READY` row can never name a
  half-written manifest. Splitting the vector index into a second system
  reintroduces exactly the consistency problem that design avoided.
- **"Sub-second freshness" as a goal.** GFS's freshness unit is a pinned commit,
  deliberately. Continuously re-indexing a moving tip is the opposite of the
  guarantee GFS sells, and `refresh_index=True` is CocoIndex working to recover
  a property GFS has by construction.
- **Chunk keys of the form `(filename, location)`.** Path-keyed rows re-embed a
  file that merely moved. Content-addressing is strictly better here and is
  already the house style.

## 6. Claimed but not evident in the shipped surface

The landing page advertises "call graphs, hierarchies, symbol tables, and
semantic indexes — all kept fresh as the repo changes", "call graphs & blast
radius: trace every caller and callee", and "spot duplicates, understand
architecture across the whole repo". The open-source CLI and MCP surface is
`search`, `grep`, `index`, `status`, `doctor`, `reset`, `daemon` — no
call-graph or symbol query appears in it. Treat the graph story as roadmap, and
do not price GFS work against it.

## 7. The design contrast worth keeping

CocoIndex made a codebase discoverable by **adding a tool the agent must learn**,
and paid for adoption with an MCP server and a Skill that markets itself to the
model. GFS made it discoverable by **intercepting the tools the agent already
runs** — `rg`, `grep`, `find`, `git` shimmed on `PATH` (ADR 0007, ADR 0009).

The GFS approach is strictly better on adoption: it works with an agent that has
never heard of GFS, needs no configuration inside the image, and degrades to the
slow-but-correct path when a flag is unsupported. It is strictly worse on
expressiveness, because `rg`'s argv has nowhere to put *find the retry logic*.

That is the shape of the conclusion: semantic search does not belong behind a
shim, because there is no existing command to shim. It belongs as an explicit
`gfs search --semantic` and an MCP tool, aimed at the question the existing
surface structurally cannot express — while the shims keep answering the
questions it can.

## Sources

- <https://cocoindex.io/> — landing page, feature claims
- <https://github.com/cocoindex-io/cocoindex> — the framework
- <https://github.com/cocoindex-io/cocoindex-code> — the `ccc` CLI/MCP product
- <https://cocoindex.io/cocoindex-code/> — product page, token-saving claims
- <https://cocoindex.io/docs/examples/index-codebase/> — chunking, schema, query code
- <https://github.com/cocoindex-io/realtime-codebase-indexing> — the ~100-line reference flow
- <https://cocoindexio.substack.com/p/index-codebase-with-tree-sitter-and> — tree-sitter chunking parameters
